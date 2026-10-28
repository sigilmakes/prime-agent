use std::collections::{BTreeMap, BTreeSet};

use super::mmds_metrics::{COORD_EPS, EdgePathMetric, RoutedMmdsMetrics};
use crate::engines::graph::algorithms::layered::kernel::trace::{
    DummyTraceKey, LayeredPhaseTrace, TraceDummySnapshot, TraceNodeSnapshot, TraceStage,
    TraceStageSnapshot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PhaseName {
    Acyclic,
    Rank,
    Normalize,
    Order,
    Position,
    Route,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PhaseAttribution {
    pub(crate) rank: NodePhaseDelta,
    pub(crate) normalize: NormalizePhaseDelta,
    pub(crate) order: NodePhaseDelta,
    pub(crate) position: NodePhaseDelta,
    pub(crate) route: RoutePhaseDelta,
}

impl PhaseAttribution {
    pub(crate) fn first_divergence(&self) -> Option<PhaseName> {
        if !self.rank.changed_nodes.is_empty() {
            return Some(PhaseName::Rank);
        }
        if self.normalize.has_changes() {
            return Some(PhaseName::Normalize);
        }
        if !self.order.changed_nodes.is_empty() {
            return Some(PhaseName::Order);
        }
        if !self.position.changed_nodes.is_empty() {
            return Some(PhaseName::Position);
        }
        if self.route.has_changes() {
            return Some(PhaseName::Route);
        }
        None
    }

    pub(crate) fn phase_components(&self) -> Vec<PhaseName> {
        let mut components = Vec::new();
        if !self.rank.changed_nodes.is_empty() {
            components.push(PhaseName::Rank);
        }
        if self.normalize.has_changes() {
            components.push(PhaseName::Normalize);
        }
        if !self.order.changed_nodes.is_empty() {
            components.push(PhaseName::Order);
        }
        if !self.position.changed_nodes.is_empty() {
            components.push(PhaseName::Position);
        }
        if self.route.has_changes() {
            components.push(PhaseName::Route);
        }
        components
    }

    pub(crate) fn phase_components_for_test(&self) -> Vec<PhaseName> {
        self.phase_components()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct NodePhaseDelta {
    pub(crate) changed_nodes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct NormalizePhaseDelta {
    pub(crate) added_dummy_chains: Vec<DummyTraceKey>,
    pub(crate) removed_dummy_chains: Vec<DummyTraceKey>,
    pub(crate) changed_dummy_chains: Vec<DummyTraceKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RoutePhaseDelta {
    pub(crate) changed_paths: Vec<String>,
    pub(crate) route_only_paths: Vec<String>,
    pub(crate) layout_amplified_paths: Vec<String>,
    pub(crate) port_divergence_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteMetricAttribution {
    pub(crate) phase: PhaseName,
    pub(crate) endpoint_layout_changed: bool,
}

impl NormalizePhaseDelta {
    fn has_changes(&self) -> bool {
        !self.added_dummy_chains.is_empty()
            || !self.removed_dummy_chains.is_empty()
            || !self.changed_dummy_chains.is_empty()
    }
}

impl RoutePhaseDelta {
    fn has_changes(&self) -> bool {
        !self.changed_paths.is_empty() || self.port_divergence_count > 0
    }
}

pub(crate) fn compare_phase_traces(
    before: &LayeredPhaseTrace,
    after: &LayeredPhaseTrace,
) -> PhaseAttribution {
    PhaseAttribution {
        rank: compare_node_phase(before, after, TraceStage::Rank, node_rank_changed),
        normalize: compare_normalize_phase(before, after),
        order: compare_node_phase(before, after, TraceStage::Order, node_order_changed),
        position: compare_node_phase(before, after, TraceStage::Position, node_position_changed),
        route: RoutePhaseDelta::default(),
    }
}

pub(crate) fn attribute_route_metric(
    metric: &EdgePathMetric,
    attribution: &PhaseAttribution,
) -> RouteMetricAttribution {
    let moved_nodes = layout_changed_nodes(attribution);
    RouteMetricAttribution {
        phase: PhaseName::Route,
        endpoint_layout_changed: moved_nodes
            .iter()
            .any(|node_id| metric.subject_mentions_node(node_id)),
    }
}

pub(crate) fn attribute_routed_metrics(
    attribution: &mut PhaseAttribution,
    routed: &RoutedMmdsMetrics,
) {
    let mut changed_paths = Vec::new();
    let mut route_only_paths = Vec::new();
    let mut layout_amplified_paths = Vec::new();

    for metric in routed
        .path_metrics
        .iter()
        .filter(|metric| metric.geometry_changed())
    {
        changed_paths.push(metric.edge_id.clone());
        let route_attribution = attribute_route_metric(metric, attribution);
        if route_attribution.endpoint_layout_changed {
            layout_amplified_paths.push(metric.edge_id.clone());
        } else {
            route_only_paths.push(metric.edge_id.clone());
        }
    }

    changed_paths.sort();
    route_only_paths.sort();
    layout_amplified_paths.sort();

    attribution.route = RoutePhaseDelta {
        changed_paths,
        route_only_paths,
        layout_amplified_paths,
        port_divergence_count: routed.path_port_divergence.len(),
    };
}

fn compare_node_phase(
    before: &LayeredPhaseTrace,
    after: &LayeredPhaseTrace,
    stage: TraceStage,
    changed: fn(&TraceNodeSnapshot, &TraceNodeSnapshot) -> bool,
) -> NodePhaseDelta {
    let Some(before_stage) = stage_snapshot(before, stage) else {
        return NodePhaseDelta::default();
    };
    let Some(after_stage) = stage_snapshot(after, stage) else {
        return NodePhaseDelta::default();
    };

    let before_nodes = nodes_by_id(before_stage);
    let after_nodes = nodes_by_id(after_stage);
    let mut changed_nodes = before_nodes
        .iter()
        .filter_map(|(id, before_node)| {
            let after_node = after_nodes.get(id)?;
            changed(before_node, after_node).then(|| id.clone())
        })
        .collect::<Vec<_>>();
    changed_nodes.sort();

    NodePhaseDelta { changed_nodes }
}

fn stage_snapshot(trace: &LayeredPhaseTrace, stage: TraceStage) -> Option<&TraceStageSnapshot> {
    trace.stages.iter().find(|snapshot| snapshot.stage == stage)
}

fn nodes_by_id(stage: &TraceStageSnapshot) -> BTreeMap<String, &TraceNodeSnapshot> {
    stage
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node))
        .collect()
}

fn layout_changed_nodes(attribution: &PhaseAttribution) -> BTreeSet<&str> {
    attribution
        .rank
        .changed_nodes
        .iter()
        .chain(attribution.order.changed_nodes.iter())
        .chain(attribution.position.changed_nodes.iter())
        .map(String::as_str)
        .collect()
}

fn compare_normalize_phase(
    before: &LayeredPhaseTrace,
    after: &LayeredPhaseTrace,
) -> NormalizePhaseDelta {
    let Some(before_stage) = stage_snapshot(before, TraceStage::Normalize) else {
        return NormalizePhaseDelta::default();
    };
    let Some(after_stage) = stage_snapshot(after, TraceStage::Normalize) else {
        return NormalizePhaseDelta::default();
    };

    let before_dummies = dummies_by_key(before_stage);
    let after_dummies = dummies_by_key(after_stage);

    let mut added_dummy_chains = after_dummies
        .keys()
        .filter(|key| !before_dummies.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();
    let mut removed_dummy_chains = before_dummies
        .keys()
        .filter(|key| !after_dummies.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();
    let mut changed_dummy_chains = before_dummies
        .iter()
        .filter_map(|(key, before_dummy)| {
            let after_dummy = after_dummies.get(key)?;
            dummy_changed(before_dummy, after_dummy).then(|| key.clone())
        })
        .collect::<Vec<_>>();

    added_dummy_chains.sort();
    removed_dummy_chains.sort();
    changed_dummy_chains.sort();

    NormalizePhaseDelta {
        added_dummy_chains,
        removed_dummy_chains,
        changed_dummy_chains,
    }
}

fn dummies_by_key(stage: &TraceStageSnapshot) -> BTreeMap<DummyTraceKey, &TraceDummySnapshot> {
    stage
        .dummies
        .iter()
        .map(|dummy| (dummy.key.clone(), dummy))
        .collect()
}

fn dummy_changed(before: &TraceDummySnapshot, after: &TraceDummySnapshot) -> bool {
    before.rank != after.rank || before.order != after.order
}

fn node_rank_changed(before: &TraceNodeSnapshot, after: &TraceNodeSnapshot) -> bool {
    before.rank != after.rank
}

fn node_order_changed(before: &TraceNodeSnapshot, after: &TraceNodeSnapshot) -> bool {
    before.order != after.order
}

fn node_position_changed(before: &TraceNodeSnapshot, after: &TraceNodeSnapshot) -> bool {
    coordinate_changed(before.x, after.x) || coordinate_changed(before.y, after.y)
}

fn coordinate_changed(before: Option<f64>, after: Option<f64>) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => (before - after).abs() > COORD_EPS,
        _ => before != after,
    }
}

#[test]
fn phase_names_include_future_attribution_boundaries() {
    assert_eq!(
        [PhaseName::Acyclic, PhaseName::Normalize, PhaseName::Route].len(),
        3
    );
}
