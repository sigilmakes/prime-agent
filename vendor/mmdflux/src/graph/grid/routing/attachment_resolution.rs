use std::collections::HashMap;

use super::super::attachments::{
    Face as SharedFace, edge_faces as shared_edge_faces,
    plan_attachments as shared_plan_attachments,
};
use super::super::backward::is_backward_edge;
use super::super::bounds::{bounds_for_node_id, resolve_edge_bounds};
use super::super::intersect::{
    NodeFace, calculate_attachment_points, classify_face, face_extent, face_fixed_coord,
    spread_points_on_face,
};
use super::super::layout::{GridLayout, NodeBounds};
use super::types::{AttachmentOverride, EdgeEndpoints, Point};
use crate::graph::attachment::{
    AttachmentPlan, OverflowSide, fan_in_overflow_face_for_slot, fan_in_primary_target_face,
};
use crate::graph::{Direction, Edge, Stroke};

/// Compute pre-assigned attachment points for edges that share a node face.
///
/// Only produces overrides for faces with >1 edge. Single-edge faces
/// use the default intersect_rect() calculation (no override).
pub fn compute_attachment_plan(
    edges: &[Edge],
    layout: &GridLayout,
    direction: Direction,
) -> HashMap<usize, AttachmentOverride> {
    compute_attachment_plan_from_shared_planner(edges, layout, direction)
}

pub(super) fn compute_attachment_plan_from_shared_planner(
    edges: &[Edge],
    layout: &GridLayout,
    direction: Direction,
) -> HashMap<usize, AttachmentOverride> {
    let shared = shared_plan_attachments(edges, layout, direction);

    // Detect face saturation and compute overflow corrections.
    let overflow = compute_grid_face_overflow(edges, layout, direction, &shared);

    let mut overrides: HashMap<usize, AttachmentOverride> = HashMap::new();

    for edge in edges {
        if edge.from == edge.to || edge.stroke == Stroke::Invisible {
            continue;
        }

        let src_id = edge.from_subgraph.as_deref().unwrap_or(edge.from.as_str());
        let tgt_id = edge.to_subgraph.as_deref().unwrap_or(edge.to.as_str());

        let Some(attachments) = shared.edge(edge.index) else {
            continue;
        };

        let entry = overrides.entry(edge.index).or_insert(AttachmentOverride {
            source: None,
            target: None,
            target_face: None,
            source_first_vertical: false,
        });

        // Singletons usually skip override emission and let the consumer fall
        // back to dynamic-intersection / waypoint clamp; that path produces the
        // visual face center for non-boundary edges. But when an edge crosses
        // a direction-override subgraph boundary in TD/BT layouts the waypoint
        // clamp pulls the cross-axis cell off-center (issue #275). Emit the
        // singleton override only in that narrow case so non-boundary fixtures
        // keep their existing snapshots.
        let src_node_dir = layout
            .node_directions
            .get(&edge.from)
            .copied()
            .unwrap_or(direction);
        let tgt_node_dir = layout
            .node_directions
            .get(&edge.to)
            .copied()
            .unwrap_or(direction);
        let cross_override_boundary = src_node_dir != tgt_node_dir;
        let td_or_bt = matches!(direction, Direction::TopDown | Direction::BottomTop);
        let emit_singleton_for_boundary = cross_override_boundary
            && td_or_bt
            && edge.from_subgraph.is_none()
            && edge.to_subgraph.is_none();

        if let Some(source_attachment) = attachments.source
            && let Some(src_bounds) = bounds_for_node_id(layout, src_id)
        {
            let group_size = shared.group_size(src_id, source_attachment.face);
            if group_size > 1 || (group_size == 1 && emit_singleton_for_boundary) {
                entry.source = Some(point_on_face_grid(
                    &src_bounds,
                    source_attachment.face.to_node_face(),
                    source_attachment.fraction,
                    group_size,
                ));
            }
        }

        // Apply overflow correction if present, otherwise use shared plan.
        if let Some(&(face, fraction, group_size)) = overflow.get(&edge.index) {
            if let Some(tgt_bounds) = bounds_for_node_id(layout, tgt_id) {
                let node_face = face.to_node_face();
                entry.target = Some(point_on_face_grid(
                    &tgt_bounds,
                    node_face,
                    fraction,
                    group_size,
                ));
                // Tag overflow edges (non-primary face) so routing honors the
                // face instead of inferring from approach direction.
                let primary = fan_in_primary_target_face(direction);
                if face != primary {
                    entry.target_face = Some(node_face);
                }
            }
        } else if let Some(target_attachment) = attachments.target
            && let Some(tgt_bounds) = bounds_for_node_id(layout, tgt_id)
        {
            let group_size = shared.group_size(tgt_id, target_attachment.face);
            if group_size > 1 || (group_size == 1 && emit_singleton_for_boundary) {
                entry.target = Some(point_on_face_grid(
                    &tgt_bounds,
                    target_attachment.face.to_node_face(),
                    target_attachment.fraction,
                    group_size,
                ));
            }
        }
    }

    let flow_face = match direction {
        Direction::TopDown => Some(SharedFace::Bottom),
        Direction::BottomTop => Some(SharedFace::Top),
        _ => None,
    };
    if let Some(flow_face) = flow_face {
        let mut side_lanes: HashMap<(String, i8), Vec<(usize, f64)>> = HashMap::new();
        let mut override_side_lanes: HashMap<(String, i8), Vec<(usize, f64)>> = HashMap::new();
        for edge in edges {
            if edge.from == edge.to || edge.stroke == Stroke::Invisible {
                continue;
            }
            let has_waypoints = edge.from_subgraph.is_none()
                && edge.to_subgraph.is_none()
                && layout
                    .edge_waypoints
                    .get(&edge.index)
                    .is_some_and(|wps| !wps.is_empty());
            let Some(source_attachment) = shared.edge(edge.index).and_then(|a| a.source) else {
                continue;
            };
            if source_attachment.face != flow_face {
                continue;
            }
            let Some((src_bounds, tgt_bounds)) = resolve_edge_bounds(layout, edge) else {
                continue;
            };
            let cross = if has_waypoints {
                let Some(first_wp) = layout
                    .edge_waypoints
                    .get(&edge.index)
                    .and_then(|wps| wps.first())
                    .copied()
                else {
                    continue;
                };
                match source_attachment.face {
                    SharedFace::Top | SharedFace::Bottom => first_wp.0 as f64,
                    SharedFace::Left | SharedFace::Right => first_wp.1 as f64,
                }
            } else {
                match source_attachment.face {
                    SharedFace::Top | SharedFace::Bottom => tgt_bounds.center_x() as f64,
                    SharedFace::Left | SharedFace::Right => tgt_bounds.center_y() as f64,
                }
            };
            let center_cross = match source_attachment.face {
                SharedFace::Top | SharedFace::Bottom => src_bounds.center_x() as f64,
                SharedFace::Left | SharedFace::Right => src_bounds.center_y() as f64,
            };
            let src_id = edge.from_subgraph.as_deref().unwrap_or(edge.from.as_str());
            let side = if cross >= center_cross { 1 } else { -1 };
            if has_waypoints {
                side_lanes
                    .entry((src_id.to_string(), side))
                    .or_default()
                    .push((edge.index, cross));
            } else {
                let target_in_override = layout
                    .node_directions
                    .get(&edge.to)
                    .is_some_and(|d| *d != direction);
                if target_in_override {
                    override_side_lanes
                        .entry((src_id.to_string(), side))
                        .or_default()
                        .push((edge.index, cross));
                }
            }
        }

        for ((_node_id, _side), mut lanes) in side_lanes {
            if lanes.len() <= 1 {
                continue;
            }
            lanes.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            for (idx, (edge_index, _)) in lanes.into_iter().enumerate() {
                if let Some(entry) = overrides.get_mut(&edge_index) {
                    entry.source_first_vertical = idx % 2 == 1;
                }
            }
        }

        for ((_node_id, _side), mut lanes) in override_side_lanes {
            if lanes.len() <= 1 {
                continue;
            }
            lanes.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            for (idx, (edge_index, _)) in lanes.into_iter().enumerate() {
                if let Some(entry) = overrides.get_mut(&edge_index) {
                    entry.source_first_vertical = idx % 2 == 0;
                }
            }
        }
    }

    overrides.retain(|_, ov| ov.source.is_some() || ov.target.is_some());
    overrides
}

/// Grid face capacity: how many distinct attachment positions a face can hold.
///
/// For Left/Right faces this is the node height (rows); for Top/Bottom it is
/// the node width (columns).
fn grid_face_capacity(bounds: &NodeBounds, face: SharedFace) -> usize {
    match face {
        SharedFace::Left | SharedFace::Right => bounds.height,
        SharedFace::Top | SharedFace::Bottom => bounds.width,
    }
}

/// Detect target face groups that exceed grid capacity and compute overflow
/// corrections.  Returns a map from edge index to (new_face, new_fraction,
/// new_group_size) for every edge in a saturated group — both the edges that
/// stay on the primary face (with corrected fractions) and the ones that
/// overflow to an adjacent face.
fn compute_grid_face_overflow(
    edges: &[Edge],
    layout: &GridLayout,
    direction: Direction,
    shared: &AttachmentPlan,
) -> HashMap<usize, (SharedFace, f64, usize)> {
    let primary_face = fan_in_primary_target_face(direction);

    // Collect (edge_index, source_cross_axis) per (target_node, target_face).
    let mut groups: HashMap<(String, SharedFace), Vec<(usize, f64)>> = HashMap::new();
    for edge in edges {
        if edge.from == edge.to || edge.stroke == Stroke::Invisible {
            continue;
        }
        let tgt_id = edge.to_subgraph.as_deref().unwrap_or(edge.to.as_str());
        let Some(attachments) = shared.edge(edge.index) else {
            continue;
        };
        let Some(target) = attachments.target else {
            continue;
        };
        // Only check the primary inbound face for overflow.
        if target.face != primary_face {
            continue;
        }
        let Some((src_bounds, _)) = resolve_edge_bounds(layout, edge) else {
            continue;
        };
        // Cross-axis: for LR primary face (Left) the cross axis is y.
        let source_cross = match direction {
            Direction::LeftRight | Direction::RightLeft => src_bounds.center_y() as f64,
            Direction::TopDown | Direction::BottomTop => src_bounds.center_x() as f64,
        };
        groups
            .entry((tgt_id.to_string(), target.face))
            .or_default()
            .push((edge.index, source_cross));
    }

    let mut corrections: HashMap<usize, (SharedFace, f64, usize)> = HashMap::new();

    for ((tgt_id, face), mut group) in groups {
        let Some(tgt_bounds) = bounds_for_node_id(layout, &tgt_id) else {
            continue;
        };
        let capacity = grid_face_capacity(&tgt_bounds, face);
        // Only overflow when the face is fully saturated — every position
        // would have at least one overlap.  Mild overlap (e.g. 5 edges on
        // a 3-row face) is preferable to routing detours.
        if group.len() < 2 * capacity {
            continue;
        }

        // Sort by cross-axis so spatially adjacent sources stay together.
        group.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        let primary_count = capacity;
        let target_cross = match direction {
            Direction::LeftRight | Direction::RightLeft => tgt_bounds.center_y() as f64,
            Direction::TopDown | Direction::BottomTop => tgt_bounds.center_x() as f64,
        };

        // Edges that stay on the primary face — assign new fractions.
        for (idx, &(edge_index, _)) in group[..primary_count].iter().enumerate() {
            let fraction = if primary_count <= 1 {
                0.5
            } else {
                idx as f64 / (primary_count - 1) as f64
            };
            corrections.insert(edge_index, (face, fraction, primary_count));
        }

        // Overflow edges — assign to Top or Bottom (for LR) based on source
        // position relative to the target center.
        let mut top_edges: Vec<usize> = Vec::new();
        let mut bottom_edges: Vec<usize> = Vec::new();
        for &(edge_index, source_cross) in &group[primary_count..] {
            if source_cross < target_cross {
                top_edges.push(edge_index);
            } else {
                bottom_edges.push(edge_index);
            }
        }

        // If all overflow went to one side, split evenly.
        if top_edges.is_empty() && !bottom_edges.is_empty() && bottom_edges.len() > 1 {
            let half = bottom_edges.len() / 2;
            top_edges = bottom_edges.drain(..half).collect();
        } else if bottom_edges.is_empty() && !top_edges.is_empty() && top_edges.len() > 1 {
            let half = top_edges.len() / 2;
            bottom_edges = top_edges.split_off(top_edges.len() - half);
        }

        let top_face = fan_in_overflow_face_for_slot(direction, OverflowSide::LeftOrTop);
        let bottom_face = fan_in_overflow_face_for_slot(direction, OverflowSide::RightOrBottom);

        for (idx, &edge_index) in top_edges.iter().enumerate() {
            let fraction = if top_edges.len() <= 1 {
                0.5
            } else {
                idx as f64 / (top_edges.len() - 1) as f64
            };
            corrections.insert(edge_index, (top_face, fraction, top_edges.len()));
        }
        for (idx, &edge_index) in bottom_edges.iter().enumerate() {
            let fraction = if bottom_edges.len() <= 1 {
                0.5
            } else {
                idx as f64 / (bottom_edges.len() - 1) as f64
            };
            corrections.insert(edge_index, (bottom_face, fraction, bottom_edges.len()));
        }
    }

    corrections
}

fn point_on_face_grid(
    bounds: &NodeBounds,
    face: NodeFace,
    fraction: f64,
    group_size: usize,
) -> (usize, usize) {
    if group_size == 0 {
        return (bounds.center_x(), bounds.center_y());
    }

    let points = spread_points_on_face(
        face,
        face_fixed_coord(bounds, &face),
        face_extent(bounds, &face),
        group_size,
    );
    if group_size == 1 {
        return points[0];
    }

    let fraction = fraction.clamp(0.0, 1.0);
    let rank = ((group_size - 1) as f64 * fraction).round() as usize;
    points[rank.min(group_size - 1)]
}

/// Resolve attachment points, using overrides when provided, falling back to
/// `calculate_attachment_points()` for non-overridden sides.
pub(super) fn resolve_attachment_points(
    src_override: Option<(usize, usize)>,
    tgt_override: Option<(usize, usize)>,
    ep: &EdgeEndpoints,
    waypoints: &[(usize, usize)],
    direction: Direction,
) -> ((usize, usize), (usize, usize)) {
    let from_bounds = ep.from_bounds;
    let to_bounds = ep.to_bounds;

    let is_backward = is_backward_edge(&from_bounds, &to_bounds, direction);

    match direction {
        Direction::LeftRight | Direction::RightLeft => {
            if is_backward
                && let (Some(&first_wp), Some(&last_wp)) = (waypoints.first(), waypoints.last())
            {
                let src_face = classify_face(&from_bounds, first_wp, ep.from_shape);
                let tgt_face = classify_face(&to_bounds, last_wp, ep.to_shape);
                let src =
                    src_override.unwrap_or_else(|| clamp_to_face(&from_bounds, src_face, first_wp));
                let tgt =
                    tgt_override.unwrap_or_else(|| clamp_to_face(&to_bounds, tgt_face, last_wp));
                return (src, tgt);
            }

            let flows_right = matches!(direction, Direction::LeftRight) != is_backward;
            let y = if is_backward {
                from_bounds.center_y()
            } else {
                consensus_y(&from_bounds, &to_bounds)
            };
            let tgt_y = if is_backward { to_bounds.center_y() } else { y };
            let (src, tgt) = if flows_right {
                (
                    src_override.unwrap_or((from_bounds.x + from_bounds.width - 1, y)),
                    tgt_override.unwrap_or((to_bounds.x, tgt_y)),
                )
            } else {
                (
                    src_override.unwrap_or((from_bounds.x, y)),
                    tgt_override.unwrap_or((to_bounds.x + to_bounds.width - 1, tgt_y)),
                )
            };
            return (src, tgt);
        }
        _ => {}
    }

    if matches!(direction, Direction::TopDown | Direction::BottomTop)
        && let (Some(&first_wp), Some(&last_wp)) = (waypoints.first(), waypoints.last())
    {
        let (src_face, tgt_face) = if is_backward && waypoints.len() <= 1 {
            let (default_src_face, default_tgt_face) = edge_faces(direction, is_backward);
            let inferred_src_face = classify_face(&from_bounds, first_wp, ep.from_shape);
            let inferred_tgt_face = classify_face(&to_bounds, last_wp, ep.to_shape);
            (
                if matches!(inferred_src_face, NodeFace::Left | NodeFace::Right) {
                    inferred_src_face
                } else {
                    default_src_face
                },
                if matches!(inferred_tgt_face, NodeFace::Left | NodeFace::Right) {
                    inferred_tgt_face
                } else {
                    default_tgt_face
                },
            )
        } else {
            edge_faces(direction, is_backward)
        };
        let src = src_override.unwrap_or_else(|| clamp_to_face(&from_bounds, src_face, first_wp));
        let tgt = tgt_override.unwrap_or_else(|| clamp_to_face(&to_bounds, tgt_face, last_wp));
        return (src, tgt);
    }

    let fallback = || {
        calculate_attachment_points(
            &from_bounds,
            ep.from_shape,
            &to_bounds,
            ep.to_shape,
            waypoints,
        )
    };
    let src = src_override.unwrap_or_else(|| fallback().0);
    let tgt = tgt_override.unwrap_or_else(|| fallback().1);
    (src, tgt)
}

pub(super) fn clamp_to_face(
    bounds: &NodeBounds,
    face: NodeFace,
    waypoint: (usize, usize),
) -> (usize, usize) {
    let (min, max) = face_extent(bounds, &face);
    let fixed = face_fixed_coord(bounds, &face);
    match face {
        NodeFace::Top | NodeFace::Bottom => (waypoint.0.clamp(min, max), fixed),
        NodeFace::Left | NodeFace::Right => (fixed, waypoint.1.clamp(min, max)),
    }
}

pub(super) fn infer_face_from_attachment(
    bounds: &NodeBounds,
    attach: (usize, usize),
    fallback: NodeFace,
) -> NodeFace {
    let left = bounds.x;
    let right = bounds.x + bounds.width.saturating_sub(1);
    let top = bounds.y;
    let bottom = bounds.y + bounds.height.saturating_sub(1);

    if attach.0 == left {
        NodeFace::Left
    } else if attach.0 == right {
        NodeFace::Right
    } else if attach.1 == top {
        NodeFace::Top
    } else if attach.1 == bottom {
        NodeFace::Bottom
    } else {
        fallback
    }
}

fn consensus_y(a: &NodeBounds, b: &NodeBounds) -> usize {
    let avg = (a.center_y() + b.center_y()) / 2;
    avg.max(a.y)
        .min(a.y + a.height - 1)
        .max(b.y)
        .min(b.y + b.height - 1)
}

pub(super) fn clamp_to_boundary(point: (usize, usize), bounds: &NodeBounds) -> Point {
    let (x, y) = point;
    let left = bounds.x;
    let right = bounds.x + bounds.width - 1;
    let top = bounds.y;
    let bottom = bounds.y + bounds.height - 1;

    Point::new(x.clamp(left, right), y.clamp(top, bottom))
}

/// Clamp only the face's fixed axis to the boundary, letting the cross-axis
/// extend freely.  This supports dense fan-in spreads where attachment points
/// are placed beyond the node's face extent to maintain minimum spacing.
pub(super) fn clamp_to_face_axis(
    point: (usize, usize),
    bounds: &NodeBounds,
    face: NodeFace,
) -> Point {
    let (x, y) = point;
    let left = bounds.x;
    let right = bounds.x + bounds.width - 1;
    let top = bounds.y;
    let bottom = bounds.y + bounds.height - 1;

    match face {
        NodeFace::Top => Point::new(x, top),
        NodeFace::Bottom => Point::new(x, bottom),
        NodeFace::Left => Point::new(left, y),
        NodeFace::Right => Point::new(right, y),
    }
}

pub(super) fn edge_faces(direction: Direction, is_backward: bool) -> (NodeFace, NodeFace) {
    let (src, tgt) = shared_edge_faces(direction, is_backward);
    (src.to_node_face(), tgt.to_node_face())
}

pub(super) fn offset_for_face(point: (usize, usize), face: NodeFace) -> Point {
    let (x, y) = point;
    match face {
        NodeFace::Top => Point::new(x, y.saturating_sub(1)),
        NodeFace::Bottom => Point::new(x, y + 1),
        NodeFace::Left => Point::new(x.saturating_sub(1), y),
        NodeFace::Right => Point::new(x + 1, y),
    }
}
