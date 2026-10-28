use std::collections::HashMap;

use super::super::labels::compute_end_labels_for_edge;
use super::endpoints::edge_endpoint_rects;
use super::fan::edge_rank_span;
use super::path_utils::{
    light_normalize, points_match, ranges_overlap, revalidate_label_anchor, segment_is_axis_aligned,
};
use crate::graph::geometry::{GraphGeometry, LayoutEdge, RoutedEdgeGeometry};
use crate::graph::space::FPoint;
use crate::graph::{Direction, Graph};

pub(super) fn resolve_forward_td_bt_criss_cross_overlaps(
    diagram: &Graph,
    geometry: &GraphGeometry,
    routed: &mut [RoutedEdgeGeometry],
) {
    if !matches!(
        geometry.direction,
        Direction::TopDown | Direction::BottomTop
    ) {
        return;
    }

    let edge_by_index: HashMap<usize, &LayoutEdge> = geometry
        .edges
        .iter()
        .map(|edge| (edge.index, edge))
        .collect();

    const MAX_PASSES: usize = 8;
    for _ in 0..MAX_PASSES {
        let mut changed = false;

        'pair_search: for i in 0..routed.len() {
            for j in (i + 1)..routed.len() {
                if !is_forward_td_bt_criss_cross_overlap_pair(
                    &routed[i],
                    &routed[j],
                    geometry,
                    &edge_by_index,
                ) {
                    continue;
                }

                for reroute_idx in
                    preferred_criss_cross_reroute_order(i, j, routed, geometry, &edge_by_index)
                {
                    let edge = edge_by_index[&routed[reroute_idx].index];
                    let current_path = routed[reroute_idx].path.clone();
                    let current_label_position = routed[reroute_idx].label_position;
                    let candidate = build_forward_td_bt_criss_cross_corridor_path(
                        edge,
                        geometry,
                        &current_path,
                        routed,
                        reroute_idx,
                    );
                    let Some(new_path) = candidate else {
                        continue;
                    };

                    routed[reroute_idx].path = new_path;
                    routed[reroute_idx].label_position =
                        revalidate_label_anchor(current_label_position, &routed[reroute_idx].path);
                    let (head_label_position, tail_label_position) = compute_end_labels_for_edge(
                        diagram,
                        routed[reroute_idx].index,
                        &routed[reroute_idx].path,
                    );
                    routed[reroute_idx].head_label_position = head_label_position;
                    routed[reroute_idx].tail_label_position = tail_label_position;
                    routed[i].preserve_orthogonal_topology = true;
                    routed[j].preserve_orthogonal_topology = true;
                    routed[reroute_idx].preserve_orthogonal_topology = true;
                    changed = true;
                    break 'pair_search;
                }
            }
        }

        if !changed {
            break;
        }
    }
}

pub(super) fn resolve_forward_lr_rl_criss_cross_overlaps(
    diagram: &Graph,
    geometry: &GraphGeometry,
    routed: &mut [RoutedEdgeGeometry],
) {
    if std::env::var("MMDFLUX_DEBUG_DISABLE_LR_CRISS_CROSS_OVERLAP").is_ok() {
        return;
    }
    if !matches!(
        geometry.direction,
        Direction::LeftRight | Direction::RightLeft
    ) {
        return;
    }

    let edge_by_index: HashMap<usize, &LayoutEdge> = geometry
        .edges
        .iter()
        .map(|edge| (edge.index, edge))
        .collect();

    const MAX_PASSES: usize = 8;
    for _ in 0..MAX_PASSES {
        let mut changed = false;

        'pair_search: for i in 0..routed.len() {
            for j in (i + 1)..routed.len() {
                if !is_forward_lr_rl_criss_cross_overlap_pair(
                    &routed[i],
                    &routed[j],
                    geometry,
                    &edge_by_index,
                ) {
                    continue;
                }

                for reroute_idx in preferred_lr_rl_criss_cross_reroute_order(
                    i,
                    j,
                    routed,
                    geometry,
                    &edge_by_index,
                ) {
                    let edge = edge_by_index[&routed[reroute_idx].index];
                    let current_path = light_normalize(&routed[reroute_idx].path);
                    let candidate = build_forward_lr_rl_criss_cross_corridor_path(
                        edge,
                        geometry,
                        &current_path,
                        routed,
                        reroute_idx,
                    );
                    let Some(new_path) = candidate else {
                        continue;
                    };

                    refresh_routed_edge_after_path_change(diagram, routed, reroute_idx, new_path);
                    changed = true;
                    break 'pair_search;
                }
            }
        }

        if !changed {
            break;
        }
    }
}

pub(super) fn resolve_forward_lr_rl_parallel_vertical_overlaps(
    diagram: &Graph,
    geometry: &GraphGeometry,
    routed: &mut [RoutedEdgeGeometry],
) {
    if std::env::var("MMDFLUX_DEBUG_DISABLE_LR_VERTICAL_OVERLAP").is_ok() {
        return;
    }
    if !matches!(
        geometry.direction,
        Direction::LeftRight | Direction::RightLeft
    ) {
        return;
    }

    let edge_by_index: HashMap<usize, &LayoutEdge> = geometry
        .edges
        .iter()
        .map(|edge| (edge.index, edge))
        .collect();

    const MAX_PASSES: usize = 8;
    const MIN_VERTICAL_CLEARANCE: f64 = 8.0;
    const EPS: f64 = 0.5;

    for _ in 0..MAX_PASSES {
        let mut changed = false;

        'pair_search: for i in 0..routed.len() {
            if routed[i].is_backward {
                continue;
            }
            let path_i = light_normalize(&routed[i].path);
            let verticals_i = interior_vertical_segments(&path_i);
            if verticals_i.is_empty() {
                continue;
            }

            for j in (i + 1)..routed.len() {
                if routed[j].is_backward {
                    continue;
                }
                if routed[i].from == routed[j].from || routed[i].to == routed[j].to {
                    continue;
                }
                let path_j = light_normalize(&routed[j].path);
                let verticals_j = interior_vertical_segments(&path_j);
                if verticals_j.is_empty() {
                    continue;
                }

                for (seg_i, x_i, y_min_i, y_max_i) in &verticals_i {
                    for (seg_j, x_j, y_min_j, y_max_j) in &verticals_j {
                        if !ranges_overlap(*y_min_i, *y_max_i, *y_min_j, *y_max_j) {
                            continue;
                        }

                        let gap = (*x_i - *x_j).abs();
                        if gap + EPS >= MIN_VERTICAL_CLEARANCE {
                            continue;
                        }

                        let needed = MIN_VERTICAL_CLEARANCE - gap;
                        let current_clearance =
                            pairwise_parallel_clearance(&path_i, &path_j).unwrap_or(0.0);
                        let lane_i = VerticalLaneCandidate {
                            route_idx: i,
                            segment_idx: *seg_i,
                            x: *x_i,
                        };
                        let lane_j = VerticalLaneCandidate {
                            route_idx: j,
                            segment_idx: *seg_j,
                            x: *x_j,
                        };
                        for (route_idx, segment_idx, delta_x) in
                            preferred_lr_rl_vertical_shift_attempts(
                                geometry.direction,
                                lane_i,
                                lane_j,
                                needed,
                            )
                        {
                            let edge = edge_by_index[&routed[route_idx].index];
                            let (base_path, other_path) = if route_idx == i {
                                (&path_i, &path_j)
                            } else {
                                (&path_j, &path_i)
                            };
                            let Some(candidate) = shift_interior_vertical_lane(
                                base_path,
                                segment_idx,
                                delta_x,
                                edge,
                                geometry,
                                geometry.direction,
                            ) else {
                                continue;
                            };
                            let new_clearance = pairwise_parallel_clearance(&candidate, other_path)
                                .unwrap_or(f64::INFINITY);
                            if new_clearance + EPS < current_clearance
                                || new_clearance + EPS < MIN_VERTICAL_CLEARANCE
                            {
                                continue;
                            }

                            refresh_routed_edge_after_path_change(
                                diagram, routed, route_idx, candidate,
                            );
                            changed = true;
                            break 'pair_search;
                        }
                    }
                }
            }
        }

        if !changed {
            break;
        }
    }
}

fn refresh_routed_edge_after_path_change(
    diagram: &Graph,
    routed: &mut [RoutedEdgeGeometry],
    routed_idx: usize,
    new_path: Vec<FPoint>,
) {
    let current_label_position = routed[routed_idx].label_position;
    routed[routed_idx].path = new_path;
    routed[routed_idx].label_position =
        revalidate_label_anchor(current_label_position, &routed[routed_idx].path);
    let (head_label_position, tail_label_position) =
        compute_end_labels_for_edge(diagram, routed[routed_idx].index, &routed[routed_idx].path);
    routed[routed_idx].head_label_position = head_label_position;
    routed[routed_idx].tail_label_position = tail_label_position;
    routed[routed_idx].preserve_orthogonal_topology = true;
}

fn is_forward_td_bt_criss_cross_overlap_pair(
    a: &RoutedEdgeGeometry,
    b: &RoutedEdgeGeometry,
    geometry: &GraphGeometry,
    edge_by_index: &HashMap<usize, &LayoutEdge>,
) -> bool {
    const EPS: f64 = 0.5;

    if a.is_backward || b.is_backward || a.from == b.from || a.to == b.to {
        return false;
    }
    if !is_td_bt_v_h_v(&a.path) || !is_td_bt_v_h_v(&b.path) {
        return false;
    }
    if !has_coincident_horizontal_overlap(&a.path, &b.path) {
        return false;
    }

    let Some(edge_a) = edge_by_index.get(&a.index).copied() else {
        return false;
    };
    let Some(edge_b) = edge_by_index.get(&b.index).copied() else {
        return false;
    };
    let Some(rank_span_a) = edge_rank_span(geometry, edge_a) else {
        return false;
    };
    let Some(rank_span_b) = edge_rank_span(geometry, edge_b) else {
        return false;
    };
    if rank_span_a > 2 || rank_span_b > 2 {
        return false;
    }

    let Some((src_a, tgt_a)) = edge_endpoint_rects(geometry, edge_a) else {
        return false;
    };
    let Some((src_b, tgt_b)) = edge_endpoint_rects(geometry, edge_b) else {
        return false;
    };

    let src_a_center_x = src_a.x + src_a.width / 2.0;
    let src_b_center_x = src_b.x + src_b.width / 2.0;
    let tgt_a_center_x = tgt_a.x + tgt_a.width / 2.0;
    let tgt_b_center_x = tgt_b.x + tgt_b.width / 2.0;
    let src_delta = src_a_center_x - src_b_center_x;
    let tgt_delta = tgt_a_center_x - tgt_b_center_x;
    if src_delta.abs() <= EPS || tgt_delta.abs() <= EPS || src_delta * tgt_delta >= 0.0 {
        return false;
    }

    let src_a_center_y = src_a.y + src_a.height / 2.0;
    let src_b_center_y = src_b.y + src_b.height / 2.0;
    let tgt_a_center_y = tgt_a.y + tgt_a.height / 2.0;
    let tgt_b_center_y = tgt_b.y + tgt_b.height / 2.0;
    (src_a_center_y - src_b_center_y).abs() <= EPS && (tgt_a_center_y - tgt_b_center_y).abs() <= EPS
}

fn is_forward_lr_rl_criss_cross_overlap_pair(
    a: &RoutedEdgeGeometry,
    b: &RoutedEdgeGeometry,
    geometry: &GraphGeometry,
    edge_by_index: &HashMap<usize, &LayoutEdge>,
) -> bool {
    const EPS: f64 = 0.5;

    if a.is_backward || b.is_backward || a.from == b.from || a.to == b.to {
        return false;
    }

    let normalized_a = light_normalize(&a.path);
    let normalized_b = light_normalize(&b.path);
    if !is_forward_lr_rl_overlap_candidate_path(&normalized_a, geometry.direction)
        || !is_forward_lr_rl_overlap_candidate_path(&normalized_b, geometry.direction)
    {
        return false;
    }
    if !has_coincident_horizontal_overlap(&normalized_a, &normalized_b) {
        return false;
    }

    let Some(edge_a) = edge_by_index.get(&a.index).copied() else {
        return false;
    };
    let Some(edge_b) = edge_by_index.get(&b.index).copied() else {
        return false;
    };
    let Some(rank_span_a) = edge_rank_span(geometry, edge_a) else {
        return false;
    };
    let Some(rank_span_b) = edge_rank_span(geometry, edge_b) else {
        return false;
    };
    if rank_span_a > 2 || rank_span_b > 2 {
        return false;
    }

    let Some((src_a, tgt_a)) = edge_endpoint_rects(geometry, edge_a) else {
        return false;
    };
    let Some((src_b, tgt_b)) = edge_endpoint_rects(geometry, edge_b) else {
        return false;
    };

    let src_a_center_y = src_a.y + src_a.height / 2.0;
    let src_b_center_y = src_b.y + src_b.height / 2.0;
    let tgt_a_center_y = tgt_a.y + tgt_a.height / 2.0;
    let tgt_b_center_y = tgt_b.y + tgt_b.height / 2.0;
    let src_delta = src_a_center_y - src_b_center_y;
    let tgt_delta = tgt_a_center_y - tgt_b_center_y;
    if src_delta.abs() <= EPS || tgt_delta.abs() <= EPS || src_delta * tgt_delta >= 0.0 {
        return false;
    }

    true
}

fn preferred_criss_cross_reroute_order(
    a_idx: usize,
    b_idx: usize,
    routed: &[RoutedEdgeGeometry],
    geometry: &GraphGeometry,
    edge_by_index: &HashMap<usize, &LayoutEdge>,
) -> [usize; 2] {
    let a_left_to_right = edge_by_index
        .get(&routed[a_idx].index)
        .and_then(|edge| edge_cross_axis_delta(geometry, edge))
        .is_some_and(|delta| delta > 0.0);
    let b_left_to_right = edge_by_index
        .get(&routed[b_idx].index)
        .and_then(|edge| edge_cross_axis_delta(geometry, edge))
        .is_some_and(|delta| delta > 0.0);

    if a_left_to_right != b_left_to_right {
        if a_left_to_right {
            [a_idx, b_idx]
        } else {
            [b_idx, a_idx]
        }
    } else if routed[a_idx].index <= routed[b_idx].index {
        [a_idx, b_idx]
    } else {
        [b_idx, a_idx]
    }
}

fn preferred_lr_rl_criss_cross_reroute_order(
    a_idx: usize,
    b_idx: usize,
    routed: &[RoutedEdgeGeometry],
    geometry: &GraphGeometry,
    edge_by_index: &HashMap<usize, &LayoutEdge>,
) -> [usize; 2] {
    let a_rises = edge_by_index
        .get(&routed[a_idx].index)
        .and_then(|edge| edge_lr_rl_cross_axis_delta(geometry, edge))
        .is_some_and(|delta| delta < 0.0);
    let b_rises = edge_by_index
        .get(&routed[b_idx].index)
        .and_then(|edge| edge_lr_rl_cross_axis_delta(geometry, edge))
        .is_some_and(|delta| delta < 0.0);

    if a_rises != b_rises {
        if a_rises {
            [a_idx, b_idx]
        } else {
            [b_idx, a_idx]
        }
    } else if routed[a_idx].index <= routed[b_idx].index {
        [a_idx, b_idx]
    } else {
        [b_idx, a_idx]
    }
}

fn build_forward_td_bt_criss_cross_corridor_path(
    edge: &LayoutEdge,
    geometry: &GraphGeometry,
    path: &[FPoint],
    routed: &[RoutedEdgeGeometry],
    reroute_idx: usize,
) -> Option<Vec<FPoint>> {
    const EPS: f64 = 0.000_001;
    const NODE_CLEARANCE: f64 = 8.0;
    const MIN_CORRIDOR_OFFSET: f64 = 8.0;
    const INTRUSION_MARGIN: f64 = -0.5;
    const SOURCE_STEM_CANDIDATES: [f64; 4] = [4.0, 8.0, 12.0, 16.0];
    const TARGET_STEM_CANDIDATES: [f64; 4] = [8.0, 12.0, 16.0, 20.0];

    if !is_td_bt_v_h_v(path) {
        return None;
    }

    let p0 = path[0];
    let p3 = path[3];
    let flow_sign = if p3.y >= p0.y { 1.0 } else { -1.0 };
    let y_min = p0.y.min(p3.y);
    let y_max = p0.y.max(p3.y);
    let preferred_corridor_x = (p0.x + p3.x) / 2.0;
    let mut candidates = vec![preferred_corridor_x];
    for (node_id, node) in &geometry.nodes {
        if node_id == &edge.from || node_id == &edge.to {
            continue;
        }
        let rect = node.rect;
        if !ranges_overlap(y_min, y_max, rect.y, rect.y + rect.height) {
            continue;
        }
        candidates.push(rect.x - NODE_CLEARANCE);
        candidates.push(rect.x + rect.width + NODE_CLEARANCE);
    }
    candidates.sort_by(|a, b| a.total_cmp(b));
    candidates.dedup_by(|a, b| (*a - *b).abs() <= 0.5);

    let mut best: Option<(f64, f64, Vec<FPoint>)> = None;
    for corridor_x in candidates {
        if (corridor_x - p0.x).abs() < MIN_CORRIDOR_OFFSET
            || (corridor_x - p3.x).abs() < MIN_CORRIDOR_OFFSET
        {
            continue;
        }

        for source_stem in SOURCE_STEM_CANDIDATES {
            let source_support_y = p0.y + flow_sign * source_stem;
            for target_stem in TARGET_STEM_CANDIDATES {
                let terminal_support_y = p3.y - flow_sign * target_stem;
                if (terminal_support_y - source_support_y) * flow_sign <= EPS {
                    continue;
                }

                let route = vec![
                    p0,
                    FPoint::new(p0.x, source_support_y),
                    FPoint::new(corridor_x, source_support_y),
                    FPoint::new(corridor_x, terminal_support_y),
                    FPoint::new(p3.x, terminal_support_y),
                    p3,
                ];
                let mut deduped: Vec<FPoint> = Vec::with_capacity(route.len());
                for point in route {
                    if deduped
                        .last()
                        .is_none_or(|prev| !points_match(*prev, point))
                    {
                        deduped.push(point);
                    }
                }
                let candidate = light_normalize(&deduped);
                if candidate.len() < 4
                    || !candidate
                        .windows(2)
                        .all(|segment| segment_is_axis_aligned(segment[0], segment[1]))
                {
                    continue;
                }
                if candidate.windows(2).any(|segment| {
                    super::collision::segment_crosses_any_other_node_interior(
                        edge,
                        geometry,
                        segment[0],
                        segment[1],
                        INTRUSION_MARGIN,
                    )
                }) {
                    continue;
                }
                if path_has_coincident_overlap_with_existing(&candidate, routed, reroute_idx) {
                    continue;
                }

                let clearance = path_parallel_clearance(&candidate, routed, reroute_idx);
                let center_penalty = (corridor_x - preferred_corridor_x).abs();
                let should_replace = match &best {
                    Some((best_clearance, _best_penalty, _))
                        if clearance + EPS < *best_clearance =>
                    {
                        false
                    }
                    Some((best_clearance, best_penalty, _))
                        if (clearance - *best_clearance).abs() <= EPS
                            && center_penalty >= *best_penalty - EPS =>
                    {
                        false
                    }
                    _ => true,
                };
                if should_replace {
                    best = Some((clearance, center_penalty, candidate));
                }
            }
        }
    }

    best.map(|(_, _, route)| route)
}

fn build_forward_lr_rl_criss_cross_corridor_path(
    edge: &LayoutEdge,
    geometry: &GraphGeometry,
    path: &[FPoint],
    routed: &[RoutedEdgeGeometry],
    reroute_idx: usize,
) -> Option<Vec<FPoint>> {
    const EPS: f64 = 0.000_001;
    const NODE_CLEARANCE: f64 = 8.0;
    const MIN_CORRIDOR_OFFSET: f64 = 8.0;
    const INTRUSION_MARGIN: f64 = -0.5;
    const SOURCE_STEM_CANDIDATES: [f64; 4] = [4.0, 8.0, 12.0, 16.0];
    const TARGET_STEM_CANDIDATES: [f64; 4] = [8.0, 12.0, 16.0, 20.0];

    if !is_forward_lr_rl_overlap_candidate_path(path, geometry.direction) {
        return None;
    }

    let p0 = path[0];
    let p_end = *path.last()?;
    let flow_sign = if p_end.x >= p0.x { 1.0 } else { -1.0 };
    let x_min = p0.x.min(p_end.x);
    let x_max = p0.x.max(p_end.x);
    let preferred_corridor_y = (p0.y + p_end.y) / 2.0;
    let mut candidates = vec![preferred_corridor_y];
    for (node_id, node) in &geometry.nodes {
        if node_id == &edge.from || node_id == &edge.to {
            continue;
        }
        let rect = node.rect;
        if !ranges_overlap(x_min, x_max, rect.x, rect.x + rect.width) {
            continue;
        }
        candidates.push(rect.y - NODE_CLEARANCE);
        candidates.push(rect.y + rect.height + NODE_CLEARANCE);
    }
    candidates.sort_by(|a, b| a.total_cmp(b));
    candidates.dedup_by(|a, b| (*a - *b).abs() <= 0.5);

    let mut best: Option<(f64, f64, Vec<FPoint>)> = None;
    for corridor_y in candidates {
        if (corridor_y - p0.y).abs() < MIN_CORRIDOR_OFFSET
            || (corridor_y - p_end.y).abs() < MIN_CORRIDOR_OFFSET
        {
            continue;
        }

        for source_stem in SOURCE_STEM_CANDIDATES {
            let source_support_x = p0.x + flow_sign * source_stem;
            for target_stem in TARGET_STEM_CANDIDATES {
                let terminal_support_x = p_end.x - flow_sign * target_stem;
                if (terminal_support_x - source_support_x) * flow_sign <= EPS {
                    continue;
                }

                let route = vec![
                    p0,
                    FPoint::new(source_support_x, p0.y),
                    FPoint::new(source_support_x, corridor_y),
                    FPoint::new(terminal_support_x, corridor_y),
                    FPoint::new(terminal_support_x, p_end.y),
                    p_end,
                ];
                let mut deduped: Vec<FPoint> = Vec::with_capacity(route.len());
                for point in route {
                    if deduped
                        .last()
                        .is_none_or(|prev| !points_match(*prev, point))
                    {
                        deduped.push(point);
                    }
                }
                let candidate = light_normalize(&deduped);
                if candidate.len() < 4
                    || !candidate
                        .windows(2)
                        .all(|segment| segment_is_axis_aligned(segment[0], segment[1]))
                {
                    continue;
                }
                if super::forward::has_forward_primary_axis_reversal(&candidate, geometry.direction)
                {
                    continue;
                }
                if candidate.windows(2).any(|segment| {
                    super::collision::segment_crosses_any_other_node_interior(
                        edge,
                        geometry,
                        segment[0],
                        segment[1],
                        INTRUSION_MARGIN,
                    )
                }) {
                    continue;
                }
                if path_has_coincident_overlap_with_existing(&candidate, routed, reroute_idx) {
                    continue;
                }

                let clearance = path_parallel_clearance(&candidate, routed, reroute_idx);
                let center_penalty = (corridor_y - preferred_corridor_y).abs();
                let should_replace = match &best {
                    Some((best_clearance, _best_penalty, _))
                        if clearance + EPS < *best_clearance =>
                    {
                        false
                    }
                    Some((best_clearance, best_penalty, _))
                        if (clearance - *best_clearance).abs() <= EPS
                            && center_penalty >= *best_penalty - EPS =>
                    {
                        false
                    }
                    _ => true,
                };
                if should_replace {
                    best = Some((clearance, center_penalty, candidate));
                }
            }
        }
    }

    best.map(|(_, _, route)| route)
}

fn edge_cross_axis_delta(geometry: &GraphGeometry, edge: &LayoutEdge) -> Option<f64> {
    let (source_rect, target_rect) = edge_endpoint_rects(geometry, edge)?;
    Some((target_rect.x + target_rect.width / 2.0) - (source_rect.x + source_rect.width / 2.0))
}

fn edge_lr_rl_cross_axis_delta(geometry: &GraphGeometry, edge: &LayoutEdge) -> Option<f64> {
    let (source_rect, target_rect) = edge_endpoint_rects(geometry, edge)?;
    Some((target_rect.y + target_rect.height / 2.0) - (source_rect.y + source_rect.height / 2.0))
}

fn is_td_bt_v_h_v(path: &[FPoint]) -> bool {
    const EPS: f64 = 0.000_001;
    if path.len() != 4 {
        return false;
    }
    let first_vertical =
        (path[0].x - path[1].x).abs() <= EPS && (path[0].y - path[1].y).abs() > EPS;
    let middle_horizontal =
        (path[1].y - path[2].y).abs() <= EPS && (path[1].x - path[2].x).abs() > EPS;
    let terminal_vertical =
        (path[2].x - path[3].x).abs() <= EPS && (path[2].y - path[3].y).abs() > EPS;
    first_vertical && middle_horizontal && terminal_vertical
}

fn is_forward_lr_rl_overlap_candidate_path(path: &[FPoint], direction: Direction) -> bool {
    if path.len() < 4 {
        return false;
    }
    if path
        .windows(2)
        .any(|segment| !segment_is_axis_aligned(segment[0], segment[1]))
    {
        return false;
    }
    if super::forward::has_forward_primary_axis_reversal(path, direction) {
        return false;
    }

    let has_horizontal = path.windows(2).any(|segment| {
        let a = segment[0];
        let b = segment[1];
        (a.y - b.y).abs() <= 0.5 && (a.x - b.x).abs() > 0.5
    });
    let has_vertical = path.windows(2).any(|segment| {
        let a = segment[0];
        let b = segment[1];
        (a.x - b.x).abs() <= 0.5 && (a.y - b.y).abs() > 0.5
    });

    has_horizontal && has_vertical
}

fn interior_vertical_segments(path: &[FPoint]) -> Vec<(usize, f64, f64, f64)> {
    const EPS: f64 = 0.5;
    if path.len() < 3 {
        return Vec::new();
    }

    let mut segments = Vec::new();
    for idx in 0..path.len().saturating_sub(1) {
        let a = path[idx];
        let b = path[idx + 1];
        let is_vertical = (a.x - b.x).abs() <= EPS && (a.y - b.y).abs() > EPS;
        if !is_vertical {
            continue;
        }

        let has_horizontal_before = idx > 0 && {
            let prev = path[idx - 1];
            (prev.y - a.y).abs() <= EPS && (prev.x - a.x).abs() > EPS
        };
        let has_horizontal_after = idx + 2 < path.len() && {
            let next = path[idx + 2];
            (b.y - next.y).abs() <= EPS && (b.x - next.x).abs() > EPS
        };
        if has_horizontal_before || has_horizontal_after {
            segments.push((idx, a.x, a.y.min(b.y), a.y.max(b.y)));
        }
    }
    segments
}

#[derive(Clone, Copy)]
struct VerticalLaneCandidate {
    route_idx: usize,
    segment_idx: usize,
    x: f64,
}

fn preferred_lr_rl_vertical_shift_attempts(
    direction: Direction,
    lane_a: VerticalLaneCandidate,
    lane_b: VerticalLaneCandidate,
    needed: f64,
) -> [(usize, usize, f64); 4] {
    let (left_lane, right_lane) = if lane_a.x <= lane_b.x {
        (lane_a, lane_b)
    } else {
        (lane_b, lane_a)
    };

    match direction {
        Direction::LeftRight => [
            (right_lane.route_idx, right_lane.segment_idx, needed),
            (left_lane.route_idx, left_lane.segment_idx, -needed),
            (left_lane.route_idx, left_lane.segment_idx, needed),
            (right_lane.route_idx, right_lane.segment_idx, -needed),
        ],
        Direction::RightLeft => [
            (left_lane.route_idx, left_lane.segment_idx, -needed),
            (right_lane.route_idx, right_lane.segment_idx, needed),
            (right_lane.route_idx, right_lane.segment_idx, -needed),
            (left_lane.route_idx, left_lane.segment_idx, needed),
        ],
        _ => [
            (right_lane.route_idx, right_lane.segment_idx, needed),
            (left_lane.route_idx, left_lane.segment_idx, -needed),
            (left_lane.route_idx, left_lane.segment_idx, needed),
            (right_lane.route_idx, right_lane.segment_idx, -needed),
        ],
    }
}

fn shift_interior_vertical_lane(
    path: &[FPoint],
    segment_idx: usize,
    delta_x: f64,
    edge: &LayoutEdge,
    geometry: &GraphGeometry,
    direction: Direction,
) -> Option<Vec<FPoint>> {
    const INTRUSION_MARGIN: f64 = -0.5;

    if segment_idx == 0 || segment_idx + 1 >= path.len().saturating_sub(1) {
        return None;
    }

    let mut candidate = path.to_vec();
    candidate[segment_idx].x += delta_x;
    candidate[segment_idx + 1].x += delta_x;
    let candidate = light_normalize(&candidate);
    if candidate.len() < 4
        || candidate
            .windows(2)
            .any(|segment| !segment_is_axis_aligned(segment[0], segment[1]))
    {
        return None;
    }
    if super::forward::has_forward_primary_axis_reversal(&candidate, direction) {
        return None;
    }
    if candidate.windows(2).any(|segment| {
        super::collision::segment_crosses_any_other_node_interior(
            edge,
            geometry,
            segment[0],
            segment[1],
            INTRUSION_MARGIN,
        )
    }) {
        return None;
    }

    Some(candidate)
}

fn path_has_coincident_overlap_with_existing(
    candidate: &[FPoint],
    routed: &[RoutedEdgeGeometry],
    reroute_idx: usize,
) -> bool {
    routed.iter().enumerate().any(|(idx, edge)| {
        idx != reroute_idx
            && (has_coincident_horizontal_overlap(candidate, &edge.path)
                || has_coincident_vertical_overlap(candidate, &edge.path))
    })
}

fn path_parallel_clearance(
    candidate: &[FPoint],
    routed: &[RoutedEdgeGeometry],
    reroute_idx: usize,
) -> f64 {
    let mut best = f64::INFINITY;
    for (idx, edge) in routed.iter().enumerate() {
        if idx == reroute_idx {
            continue;
        }
        if let Some(clearance) = pairwise_parallel_clearance(candidate, &edge.path) {
            best = best.min(clearance);
        }
    }
    if best.is_finite() { best } else { 10_000.0 }
}

pub(crate) fn pairwise_parallel_clearance(path_a: &[FPoint], path_b: &[FPoint]) -> Option<f64> {
    const EPS: f64 = 0.5;
    let mut best: Option<f64> = None;

    for seg_a in path_a.windows(2) {
        let a0 = seg_a[0];
        let a1 = seg_a[1];
        let a_is_horizontal = (a0.y - a1.y).abs() <= EPS && (a0.x - a1.x).abs() > EPS;
        let a_is_vertical = (a0.x - a1.x).abs() <= EPS && (a0.y - a1.y).abs() > EPS;
        let a_min_x = a0.x.min(a1.x);
        let a_max_x = a0.x.max(a1.x);
        let a_min_y = a0.y.min(a1.y);
        let a_max_y = a0.y.max(a1.y);

        for seg_b in path_b.windows(2) {
            let b0 = seg_b[0];
            let b1 = seg_b[1];
            let b_is_horizontal = (b0.y - b1.y).abs() <= EPS && (b0.x - b1.x).abs() > EPS;
            let b_is_vertical = (b0.x - b1.x).abs() <= EPS && (b0.y - b1.y).abs() > EPS;
            let b_min_x = b0.x.min(b1.x);
            let b_max_x = b0.x.max(b1.x);
            let b_min_y = b0.y.min(b1.y);
            let b_max_y = b0.y.max(b1.y);

            let clearance = if a_is_horizontal && b_is_horizontal {
                if a_max_x.min(b_max_x) - a_min_x.max(b_min_x) > EPS {
                    Some((a0.y - b0.y).abs())
                } else {
                    None
                }
            } else if a_is_vertical && b_is_vertical {
                if a_max_y.min(b_max_y) - a_min_y.max(b_min_y) > EPS {
                    Some((a0.x - b0.x).abs())
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(clearance) = clearance.filter(|value| *value > EPS) {
                best = Some(best.map_or(clearance, |current| current.min(clearance)));
            }
        }
    }

    best
}

fn has_coincident_horizontal_overlap(path_a: &[FPoint], path_b: &[FPoint]) -> bool {
    const EPS: f64 = 0.5;
    for seg_a in path_a.windows(2) {
        let a0 = seg_a[0];
        let a1 = seg_a[1];
        let a_is_horizontal = (a0.y - a1.y).abs() <= EPS && (a0.x - a1.x).abs() > EPS;
        if !a_is_horizontal {
            continue;
        }
        let a_min_x = a0.x.min(a1.x);
        let a_max_x = a0.x.max(a1.x);
        for seg_b in path_b.windows(2) {
            let b0 = seg_b[0];
            let b1 = seg_b[1];
            let b_is_horizontal = (b0.y - b1.y).abs() <= EPS && (b0.x - b1.x).abs() > EPS;
            if !b_is_horizontal || (a0.y - b0.y).abs() > EPS {
                continue;
            }
            let b_min_x = b0.x.min(b1.x);
            let b_max_x = b0.x.max(b1.x);
            if a_max_x.min(b_max_x) - a_min_x.max(b_min_x) > EPS {
                return true;
            }
        }
    }
    false
}

fn has_coincident_vertical_overlap(path_a: &[FPoint], path_b: &[FPoint]) -> bool {
    const EPS: f64 = 0.5;
    for seg_a in path_a.windows(2) {
        let a0 = seg_a[0];
        let a1 = seg_a[1];
        let a_is_vertical = (a0.x - a1.x).abs() <= EPS && (a0.y - a1.y).abs() > EPS;
        if !a_is_vertical {
            continue;
        }
        let a_min_y = a0.y.min(a1.y);
        let a_max_y = a0.y.max(a1.y);
        for seg_b in path_b.windows(2) {
            let b0 = seg_b[0];
            let b1 = seg_b[1];
            let b_is_vertical = (b0.x - b1.x).abs() <= EPS && (b0.y - b1.y).abs() > EPS;
            if !b_is_vertical || (a0.x - b0.x).abs() > EPS {
                continue;
            }
            let b_min_y = b0.y.min(b1.y);
            let b_max_y = b0.y.max(b1.y);
            if a_max_y.min(b_max_y) - a_min_y.max(b_min_y) > EPS {
                return true;
            }
        }
    }
    false
}
