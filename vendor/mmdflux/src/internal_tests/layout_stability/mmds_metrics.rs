use std::collections::{BTreeMap, BTreeSet};

use super::{geometry_metrics, mutations};
use crate::graph::Direction;
use crate::graph::attachment::PortFace;
use crate::graph::geometry::{EdgeLabelSide, FPoint, FRect};
use crate::mmds;

pub(crate) const COORD_EPS: f64 = 0.01;
pub(crate) const DISPLAY_EPS: f64 = 1.0;

#[derive(Debug, Clone)]
pub(crate) struct LayoutMmdsMetrics {
    pub(crate) coord_eps: f64,
    pub(crate) added_nodes: BTreeSet<String>,
    pub(crate) removed_nodes: BTreeSet<String>,
    pub(crate) inferred_renames: Vec<InferredRename>,
    pub(crate) direct_impact_nodes: BTreeSet<String>,
    pub(crate) unchanged_node_count: usize,
    pub(crate) unchanged_node_displacements: BTreeMap<String, Displacement>,
    pub(crate) node_size_changes: BTreeMap<String, SizeDelta>,
    pub(crate) bounds_delta: BoundsDelta,
    pub(crate) subgraph_membership_changes: Vec<SubgraphMembershipDelta>,
    pub(crate) label_side_changes: Vec<LabelSideDelta>,
    pub(crate) global_shift: GlobalShiftSummary,
}

#[derive(Debug, Clone)]
pub(crate) struct RoutedMmdsMetrics {
    pub(crate) added_edges: BTreeSet<String>,
    pub(crate) removed_edges: BTreeSet<String>,
    pub(crate) path_metrics: Vec<EdgePathMetric>,
    pub(crate) label_rect_changes: Vec<LabelRectDelta>,
    pub(crate) port_intent_changes: Vec<PortIntentDelta>,
    pub(crate) endpoint_face_changes: Vec<EndpointFaceDelta>,
    pub(crate) path_port_divergence: Vec<PathPortDivergence>,
    pub(crate) routed_bounds_delta: BoundsDelta,
}

impl RoutedMmdsMetrics {
    pub(crate) fn changed_path_geometry_count(&self) -> usize {
        self.path_metrics
            .iter()
            .filter(|metric| metric.geometry_changed())
            .count()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct QualityOracleMetrics {
    pub(crate) route_length_total_delta: f64,
    pub(crate) bend_count_total_delta: isize,
    pub(crate) label_rect_overlaps: Vec<LabelRectOverlap>,
    pub(crate) label_path_drift: Vec<LabelPathDrift>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InferredRename {
    pub(crate) before_id: String,
    pub(crate) after_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Displacement {
    pub(crate) dx: f64,
    pub(crate) dy: f64,
    pub(crate) distance: f64,
    pub(crate) changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SizeDelta {
    pub(crate) width_delta: f64,
    pub(crate) height_delta: f64,
    pub(crate) area_delta: f64,
    pub(crate) changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BoundsDelta {
    pub(crate) width_delta: f64,
    pub(crate) height_delta: f64,
    pub(crate) area_delta: f64,
    pub(crate) changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubgraphMembershipDelta {
    pub(crate) id: String,
    pub(crate) before_children: Vec<String>,
    pub(crate) after_children: Vec<String>,
    pub(crate) before_parent: Option<String>,
    pub(crate) after_parent: Option<String>,
    pub(crate) before_direction: Option<Direction>,
    pub(crate) after_direction: Option<Direction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LabelSideDelta {
    pub(crate) edge_id: String,
    pub(crate) before: Option<EdgeLabelSide>,
    pub(crate) after: Option<EdgeLabelSide>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EdgePathMetric {
    pub(crate) edge_id: String,
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) point_count_delta: isize,
    pub(crate) bend_count_delta: isize,
    pub(crate) length_delta: f64,
    pub(crate) envelope_delta: BoundsDelta,
}

impl EdgePathMetric {
    pub(crate) fn geometry_changed(&self) -> bool {
        self.point_count_delta != 0
            || self.bend_count_delta != 0
            || self.length_delta.abs() > DISPLAY_EPS
            || self.envelope_delta.changed
    }

    pub(crate) fn subject_mentions_node(&self, node_id: &str) -> bool {
        self.source == node_id || self.target == node_id
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LabelRectDelta {
    pub(crate) edge_id: String,
    pub(crate) center_dx: f64,
    pub(crate) center_dy: f64,
    pub(crate) width_delta: f64,
    pub(crate) height_delta: f64,
    pub(crate) before_present: bool,
    pub(crate) after_present: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PortIntentDelta {
    pub(crate) edge_id: String,
    pub(crate) endpoint: Endpoint,
    pub(crate) before_face: Option<PortFace>,
    pub(crate) after_face: Option<PortFace>,
    pub(crate) before_fraction: Option<f64>,
    pub(crate) after_fraction: Option<f64>,
    pub(crate) before_group_size: Option<usize>,
    pub(crate) after_group_size: Option<usize>,
    pub(crate) source: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EndpointFaceDelta {
    pub(crate) edge_id: String,
    pub(crate) endpoint: Endpoint,
    pub(crate) before: DerivedEndpointFace,
    pub(crate) after: DerivedEndpointFace,
    pub(crate) source: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PathPortDivergence {
    pub(crate) edge_id: String,
    pub(crate) endpoint: Endpoint,
    pub(crate) path_face: DerivedEndpointFace,
    pub(crate) port_face: Option<PortFace>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LabelRectOverlap {
    pub(crate) first_edge_id: String,
    pub(crate) second_edge_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LabelPathDrift {
    pub(crate) edge_id: String,
    pub(crate) distance: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endpoint {
    Source,
    Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DerivedEndpointFace {
    Face(String),
    Ambiguous,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GlobalShiftSummary {
    pub(crate) anchor_count: usize,
    pub(crate) dx: f64,
    pub(crate) dy: f64,
    pub(crate) confidence: ShiftConfidence,
    pub(crate) collapsed_movement: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShiftConfidence {
    High,
    Low,
}

pub(crate) fn compare_layout_mmds(
    pair: &mutations::MutationPair,
    before: &mmds::Document,
    after: &mmds::Document,
) -> LayoutMmdsMetrics {
    let before_nodes = nodes_by_id(before);
    let after_nodes = nodes_by_id(after);
    let direct_impact_nodes = direct_impact_nodes(pair);
    let added_nodes = difference(after_nodes.keys(), &before_nodes);
    let removed_nodes = difference(before_nodes.keys(), &after_nodes);
    let mut unchanged_node_displacements = BTreeMap::new();
    let mut node_size_changes = BTreeMap::new();

    for (id, before_node) in &before_nodes {
        let Some(after_node) = after_nodes.get(id) else {
            continue;
        };
        if direct_impact_nodes.contains(id) {
            continue;
        }

        unchanged_node_displacements.insert((*id).clone(), position_delta(before_node, after_node));
        let size = size_delta(before_node, after_node);
        if size.changed {
            node_size_changes.insert((*id).clone(), size);
        }
    }

    let unchanged_node_count = unchanged_node_displacements.len();
    let global_shift = global_shift_summary(&unchanged_node_displacements);

    LayoutMmdsMetrics {
        coord_eps: COORD_EPS,
        added_nodes,
        removed_nodes,
        inferred_renames: Vec::new(),
        direct_impact_nodes,
        unchanged_node_count,
        unchanged_node_displacements,
        node_size_changes,
        bounds_delta: bounds_delta(&before.metadata.bounds, &after.metadata.bounds),
        subgraph_membership_changes: subgraph_membership_changes(before, after),
        label_side_changes: label_side_changes(before, after),
        global_shift,
    }
}

pub(crate) fn compare_routed_mmds(
    pair: &mutations::MutationPair,
    before: &mmds::Document,
    after: &mmds::Document,
) -> RoutedMmdsMetrics {
    let before_edges = edges_by_id(before);
    let after_edges = edges_by_id(after);
    let before_nodes = nodes_by_id(before);
    let after_nodes = nodes_by_id(after);
    let direct_nodes = direct_impact_nodes(pair);
    let mut added_edges = edge_difference(after_edges.keys(), &before_edges);
    let mut removed_edges = edge_difference(before_edges.keys(), &after_edges);
    let mut path_metrics = Vec::new();
    let mut label_rect_changes = Vec::new();
    let mut port_intent_changes = Vec::new();
    let mut endpoint_face_changes = Vec::new();
    let mut path_port_divergences = Vec::new();

    for (edge_id, before_edge) in &before_edges {
        let Some(after_edge) = after_edges.get(edge_id) else {
            continue;
        };
        if endpoint_identity_changed_across_direct_node(before_edge, after_edge, &direct_nodes) {
            removed_edges.insert((*edge_id).clone());
            added_edges.insert((*edge_id).clone());
            continue;
        }

        path_metrics.push(edge_path_metric(edge_id, before_edge, after_edge));
        if let Some(delta) = label_rect_delta(edge_id, before_edge, after_edge) {
            label_rect_changes.push(delta);
        }
        port_intent_changes.extend(port_intent_deltas(edge_id, before_edge, after_edge));
        endpoint_face_changes.extend(endpoint_face_deltas(
            edge_id,
            before_edge,
            after_edge,
            &before_nodes,
            &after_nodes,
        ));
        path_port_divergences.extend(path_port_divergence(edge_id, after_edge, &after_nodes));
    }

    RoutedMmdsMetrics {
        added_edges,
        removed_edges,
        path_metrics,
        label_rect_changes,
        port_intent_changes,
        endpoint_face_changes,
        path_port_divergence: path_port_divergences,
        routed_bounds_delta: bounds_delta(&before.metadata.bounds, &after.metadata.bounds),
    }
}

pub(crate) fn collect_quality_oracle_metrics(
    pair: &mutations::MutationPair,
    before: &mmds::Document,
    after: &mmds::Document,
) -> QualityOracleMetrics {
    let routed = compare_routed_mmds(pair, before, after);

    QualityOracleMetrics {
        route_length_total_delta: routed
            .path_metrics
            .iter()
            .map(|metric| metric.length_delta)
            .sum(),
        bend_count_total_delta: routed
            .path_metrics
            .iter()
            .map(|metric| metric.bend_count_delta)
            .sum(),
        label_rect_overlaps: label_rect_overlaps(after),
        label_path_drift: label_path_drift(after),
    }
}

fn nodes_by_id(output: &mmds::Document) -> BTreeMap<String, &mmds::Node> {
    output
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node))
        .collect()
}

fn direct_impact_nodes(pair: &mutations::MutationPair) -> BTreeSet<String> {
    pair.direct_nodes
        .iter()
        .map(|id| (*id).to_string())
        .collect()
}

fn difference<'a>(
    candidates: impl Iterator<Item = &'a String>,
    other: &BTreeMap<String, &mmds::Node>,
) -> BTreeSet<String> {
    candidates
        .filter(|id| !other.contains_key(*id))
        .cloned()
        .collect()
}

fn edge_difference<'a>(
    candidates: impl Iterator<Item = &'a String>,
    other: &BTreeMap<String, &mmds::Edge>,
) -> BTreeSet<String> {
    candidates
        .filter(|id| !other.contains_key(*id))
        .cloned()
        .collect()
}

fn position_delta(before: &mmds::Node, after: &mmds::Node) -> Displacement {
    let dx = after.position.x - before.position.x;
    let dy = after.position.y - before.position.y;
    let distance = dx.hypot(dy);

    Displacement {
        dx,
        dy,
        distance,
        changed: distance > COORD_EPS,
    }
}

fn size_delta(before: &mmds::Node, after: &mmds::Node) -> SizeDelta {
    let width_delta = after.size.width - before.size.width;
    let height_delta = after.size.height - before.size.height;
    let before_area = before.size.width * before.size.height;
    let after_area = after.size.width * after.size.height;
    let area_delta = after_area - before_area;

    SizeDelta {
        width_delta,
        height_delta,
        area_delta,
        changed: width_delta.abs() > COORD_EPS
            || height_delta.abs() > COORD_EPS
            || area_delta.abs() > COORD_EPS,
    }
}

fn bounds_delta(before: &mmds::Bounds, after: &mmds::Bounds) -> BoundsDelta {
    let width_delta = after.width - before.width;
    let height_delta = after.height - before.height;
    let area_delta = after.width * after.height - before.width * before.height;

    BoundsDelta {
        width_delta,
        height_delta,
        area_delta,
        changed: width_delta.abs() > COORD_EPS
            || height_delta.abs() > COORD_EPS
            || area_delta.abs() > COORD_EPS,
    }
}

fn subgraph_membership_changes(
    before: &mmds::Document,
    after: &mmds::Document,
) -> Vec<SubgraphMembershipDelta> {
    let before_subgraphs = subgraphs_by_id(before);
    let after_subgraphs = subgraphs_by_id(after);
    let ids = before_subgraphs
        .keys()
        .chain(after_subgraphs.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut changes = Vec::new();

    for id in ids {
        let before = before_subgraphs.get(&id);
        let after = after_subgraphs.get(&id);
        let before_children = before
            .map(|subgraph| sorted_children(&subgraph.children))
            .unwrap_or_default();
        let after_children = after
            .map(|subgraph| sorted_children(&subgraph.children))
            .unwrap_or_default();
        let before_parent = before.and_then(|subgraph| subgraph.parent.clone());
        let after_parent = after.and_then(|subgraph| subgraph.parent.clone());
        let before_direction = before.and_then(|subgraph| subgraph.direction);
        let after_direction = after.and_then(|subgraph| subgraph.direction);

        if before_children != after_children
            || before_parent != after_parent
            || before_direction != after_direction
        {
            changes.push(SubgraphMembershipDelta {
                id,
                before_children,
                after_children,
                before_parent,
                after_parent,
                before_direction,
                after_direction,
            });
        }
    }

    changes
}

fn subgraphs_by_id(output: &mmds::Document) -> BTreeMap<String, &mmds::Subgraph> {
    output
        .subgraphs
        .iter()
        .map(|subgraph| (subgraph.id.clone(), subgraph))
        .collect()
}

fn sorted_children(children: &[String]) -> Vec<String> {
    let mut sorted = children.to_vec();
    sorted.sort();
    sorted
}

fn label_side_changes(before: &mmds::Document, after: &mmds::Document) -> Vec<LabelSideDelta> {
    let before_edges = edges_by_id(before);
    let after_edges = edges_by_id(after);
    let mut changes = Vec::new();

    for (id, before_edge) in before_edges {
        let Some(after_edge) = after_edges.get(&id) else {
            continue;
        };
        if before_edge.label_side != after_edge.label_side {
            changes.push(LabelSideDelta {
                edge_id: id,
                before: before_edge.label_side,
                after: after_edge.label_side,
            });
        }
    }

    changes
}

fn edges_by_id(output: &mmds::Document) -> BTreeMap<String, &mmds::Edge> {
    output
        .edges
        .iter()
        .map(|edge| (edge.id.clone(), edge))
        .collect()
}

fn endpoint_identity_changed_across_direct_node(
    before: &mmds::Edge,
    after: &mmds::Edge,
    direct_nodes: &BTreeSet<String>,
) -> bool {
    (before.source != after.source || before.target != after.target)
        && (edge_mentions_direct_node(before, direct_nodes)
            || edge_mentions_direct_node(after, direct_nodes))
}

fn edge_mentions_direct_node(edge: &mmds::Edge, direct_nodes: &BTreeSet<String>) -> bool {
    direct_nodes.contains(&edge.source) || direct_nodes.contains(&edge.target)
}

fn edge_path_metric(edge_id: &str, before: &mmds::Edge, after: &mmds::Edge) -> EdgePathMetric {
    let before_path = before.path.as_deref().unwrap_or(&[]);
    let after_path = after.path.as_deref().unwrap_or(&[]);
    let before_points = points_from_mmds_path(before_path);
    let after_points = points_from_mmds_path(after_path);

    EdgePathMetric {
        edge_id: edge_id.to_string(),
        source: before.source.clone(),
        target: before.target.clone(),
        point_count_delta: after_path.len() as isize - before_path.len() as isize,
        bend_count_delta: geometry_metrics::bend_count(&after_points) as isize
            - geometry_metrics::bend_count(&before_points) as isize,
        length_delta: geometry_metrics::polyline_length(&after_points)
            - geometry_metrics::polyline_length(&before_points),
        envelope_delta: path_envelope_delta(&before_points, &after_points),
    }
}

fn points_from_mmds_path(path: &[[f64; 2]]) -> Vec<FPoint> {
    path.iter()
        .map(|point| FPoint::new(point[0], point[1]))
        .collect()
}

fn path_envelope_delta(before: &[FPoint], after: &[FPoint]) -> BoundsDelta {
    let before = bounds_from_rect(geometry_metrics::route_envelope(before));
    let after = bounds_from_rect(geometry_metrics::route_envelope(after));

    bounds_delta(&before, &after)
}

fn bounds_from_rect(rect: Option<FRect>) -> mmds::Bounds {
    match rect {
        Some(rect) => mmds::Bounds {
            width: rect.width,
            height: rect.height,
        },
        None => mmds::Bounds {
            width: 0.0,
            height: 0.0,
        },
    }
}

fn label_rect_delta(
    edge_id: &str,
    before: &mmds::Edge,
    after: &mmds::Edge,
) -> Option<LabelRectDelta> {
    if before.label_rect.is_none() && after.label_rect.is_none() {
        return None;
    }

    let before_center = before.label_rect.as_ref().map(rect_center);
    let after_center = after.label_rect.as_ref().map(rect_center);
    let center_dx = match (before_center, after_center) {
        (Some(before), Some(after)) => after.0 - before.0,
        _ => 0.0,
    };
    let center_dy = match (before_center, after_center) {
        (Some(before), Some(after)) => after.1 - before.1,
        _ => 0.0,
    };
    let width_delta = match (&before.label_rect, &after.label_rect) {
        (Some(before), Some(after)) => after.width - before.width,
        _ => 0.0,
    };
    let height_delta = match (&before.label_rect, &after.label_rect) {
        (Some(before), Some(after)) => after.height - before.height,
        _ => 0.0,
    };

    Some(LabelRectDelta {
        edge_id: edge_id.to_string(),
        center_dx,
        center_dy,
        width_delta,
        height_delta,
        before_present: before.label_rect.is_some(),
        after_present: after.label_rect.is_some(),
    })
}

fn rect_center(rect: &mmds::Rect) -> (f64, f64) {
    (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0)
}

fn label_rect_overlaps(output: &mmds::Document) -> Vec<LabelRectOverlap> {
    let rects = output
        .edges
        .iter()
        .filter_map(|edge| {
            edge.label_rect
                .as_ref()
                .map(|rect| (edge.id.as_str(), rect_from_mmds(rect)))
        })
        .collect::<Vec<_>>();
    let mut overlaps = Vec::new();

    for (index, (first_id, first)) in rects.iter().enumerate() {
        for (second_id, second) in rects.iter().skip(index + 1) {
            if geometry_metrics::rects_overlap(*first, *second) {
                overlaps.push(LabelRectOverlap {
                    first_edge_id: (*first_id).to_string(),
                    second_edge_id: (*second_id).to_string(),
                });
            }
        }
    }

    overlaps
}

fn label_path_drift(output: &mmds::Document) -> Vec<LabelPathDrift> {
    output
        .edges
        .iter()
        .filter_map(|edge| {
            let rect = edge.label_rect.as_ref()?;
            let path = edge.path.as_deref()?;
            let center = rect_from_mmds(rect).center();
            let points = points_from_mmds_path(path);

            Some(LabelPathDrift {
                edge_id: edge.id.clone(),
                distance: geometry_metrics::distance_point_to_path(center, &points),
            })
        })
        .collect()
}

fn rect_from_mmds(rect: &mmds::Rect) -> FRect {
    FRect::new(rect.x, rect.y, rect.width, rect.height)
}

fn port_intent_deltas(
    edge_id: &str,
    before: &mmds::Edge,
    after: &mmds::Edge,
) -> Vec<PortIntentDelta> {
    [
        (
            Endpoint::Source,
            before.source_port.as_ref(),
            after.source_port.as_ref(),
        ),
        (
            Endpoint::Target,
            before.target_port.as_ref(),
            after.target_port.as_ref(),
        ),
    ]
    .into_iter()
    .filter_map(|(endpoint, before, after)| {
        if same_port_intent(before, after) {
            return None;
        }
        Some(PortIntentDelta {
            edge_id: edge_id.to_string(),
            endpoint,
            before_face: before.map(|port| port.face),
            after_face: after.map(|port| port.face),
            before_fraction: before.map(|port| port.fraction),
            after_fraction: after.map(|port| port.fraction),
            before_group_size: before.map(|port| port.group_size),
            after_group_size: after.map(|port| port.group_size),
            source: "port",
        })
    })
    .collect()
}

fn same_port_intent(before: Option<&mmds::Port>, after: Option<&mmds::Port>) -> bool {
    match (before, after) {
        (None, None) => true,
        (Some(before), Some(after)) => {
            before.face == after.face
                && (before.fraction - after.fraction).abs() <= COORD_EPS
                && before.group_size == after.group_size
        }
        _ => false,
    }
}

fn endpoint_face_deltas(
    edge_id: &str,
    before_edge: &mmds::Edge,
    after_edge: &mmds::Edge,
    before_nodes: &BTreeMap<String, &mmds::Node>,
    after_nodes: &BTreeMap<String, &mmds::Node>,
) -> Vec<EndpointFaceDelta> {
    let endpoints = [
        (
            Endpoint::Source,
            endpoint_face(before_edge, before_nodes, Endpoint::Source),
            endpoint_face(after_edge, after_nodes, Endpoint::Source),
        ),
        (
            Endpoint::Target,
            endpoint_face(before_edge, before_nodes, Endpoint::Target),
            endpoint_face(after_edge, after_nodes, Endpoint::Target),
        ),
    ];

    endpoints
        .into_iter()
        .filter_map(|(endpoint, before, after)| {
            (before != after).then(|| EndpointFaceDelta {
                edge_id: edge_id.to_string(),
                endpoint,
                before,
                after,
                source: "path",
            })
        })
        .collect()
}

fn path_port_divergence(
    edge_id: &str,
    edge: &mmds::Edge,
    nodes: &BTreeMap<String, &mmds::Node>,
) -> Vec<PathPortDivergence> {
    [
        (
            Endpoint::Source,
            endpoint_face(edge, nodes, Endpoint::Source),
            edge.source_port.as_ref().map(|port| port.face),
        ),
        (
            Endpoint::Target,
            endpoint_face(edge, nodes, Endpoint::Target),
            edge.target_port.as_ref().map(|port| port.face),
        ),
    ]
    .into_iter()
    .filter_map(|(endpoint, path_face, port_face)| {
        let diverged = match (&path_face, &port_face) {
            (DerivedEndpointFace::Face(path), Some(port)) => path != port.as_str(),
            (DerivedEndpointFace::Ambiguous, Some(_)) => true,
            _ => false,
        };
        diverged.then(|| PathPortDivergence {
            edge_id: edge_id.to_string(),
            endpoint,
            path_face,
            port_face,
        })
    })
    .collect()
}

fn endpoint_face(
    edge: &mmds::Edge,
    nodes: &BTreeMap<String, &mmds::Node>,
    endpoint: Endpoint,
) -> DerivedEndpointFace {
    let Some(path) = &edge.path else {
        return DerivedEndpointFace::Missing;
    };
    let point = match endpoint {
        Endpoint::Source => path.first(),
        Endpoint::Target => path.last(),
    };
    let Some(point) = point else {
        return DerivedEndpointFace::Missing;
    };
    let node_id = match endpoint {
        Endpoint::Source => &edge.source,
        Endpoint::Target => &edge.target,
    };
    let Some(node) = nodes.get(node_id) else {
        return DerivedEndpointFace::Missing;
    };

    derive_face_for_point(*point, node)
}

fn derive_face_for_point(point: [f64; 2], node: &mmds::Node) -> DerivedEndpointFace {
    let left = node.position.x - node.size.width / 2.0;
    let right = node.position.x + node.size.width / 2.0;
    let top = node.position.y - node.size.height / 2.0;
    let bottom = node.position.y + node.size.height / 2.0;
    let candidates = [
        ("left", (point[0] - left).abs()),
        ("right", (point[0] - right).abs()),
        ("top", (point[1] - top).abs()),
        ("bottom", (point[1] - bottom).abs()),
    ]
    .into_iter()
    .filter(|(_, distance)| *distance <= COORD_EPS)
    .map(|(face, _)| face)
    .collect::<Vec<_>>();

    match candidates.as_slice() {
        [] => DerivedEndpointFace::Missing,
        [face] => DerivedEndpointFace::Face((*face).to_string()),
        _ => DerivedEndpointFace::Ambiguous,
    }
}

fn global_shift_summary(displacements: &BTreeMap<String, Displacement>) -> GlobalShiftSummary {
    let anchor_count = displacements.len();
    if anchor_count < 2 {
        return GlobalShiftSummary {
            anchor_count,
            dx: 0.0,
            dy: 0.0,
            confidence: ShiftConfidence::Low,
            collapsed_movement: false,
        };
    }

    let dx = median(displacements.values().map(|delta| delta.dx).collect());
    let dy = median(displacements.values().map(|delta| delta.dy).collect());
    let collapsed_movement = displacements.values().all(|delta| {
        let residual_dx = delta.dx - dx;
        let residual_dy = delta.dy - dy;
        residual_dx.hypot(residual_dy) <= DISPLAY_EPS
    });

    GlobalShiftSummary {
        anchor_count,
        dx,
        dy,
        confidence: ShiftConfidence::High,
        collapsed_movement,
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;

    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}
