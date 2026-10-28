use std::collections::{BTreeMap, BTreeSet};

use super::{mmds_metrics, mutations, phase_attribution, render_surfaces, report};
use crate::graph::geometry::EdgeLabelSide;
use crate::graph::routing::EdgeRouting;
use crate::graph::routing::trace::{
    LabelLaneEdgeSnapshot, LabelLaneTraceSnapshot, RouteEdgeInput, RouteInputSnapshot,
    RouteLabelInput, RouteNodeInput, RoutePortInput,
};
use crate::graph::space::{FPoint, FRect};
use crate::graph::{Direction, Shape};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteDeterminismReport {
    pub(crate) run_count: usize,
    pub(crate) unique_signature_count: usize,
    pub(crate) signatures: Vec<String>,
}

impl RouteDeterminismReport {
    pub(crate) fn is_deterministic(&self) -> bool {
        self.run_count > 0 && self.unique_signature_count == 1
    }
}

pub(crate) fn determinism_report_from_signatures(
    signatures: Vec<String>,
) -> RouteDeterminismReport {
    let unique_signature_count = signatures.iter().collect::<BTreeSet<_>>().len();
    RouteDeterminismReport {
        run_count: signatures.len(),
        unique_signature_count,
        signatures,
    }
}

pub(crate) fn measure_pair_determinism(
    pair: &mutations::MutationPair,
    iterations: usize,
) -> Result<RouteDeterminismReport, render_surfaces::RenderSurfaceError> {
    let mut signatures = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let rendered = render_surfaces::render_pair(pair)?;
        signatures.push(routed_mmds_signature(&rendered.after.routed_mmds));
    }
    Ok(determinism_report_from_signatures(signatures))
}

pub(crate) fn determinism_summary_for_pair_ids(
    pair_ids: &[&str],
    iterations: usize,
) -> Result<String, String> {
    let mut lines = Vec::with_capacity(pair_ids.len());
    for pair_id in pair_ids {
        let pair =
            mutations::pair_by_id(pair_id).ok_or_else(|| format!("unknown pair id {pair_id}"))?;
        let report =
            measure_pair_determinism(pair, iterations).map_err(|error| error.to_string())?;
        lines.push(format!(
            "{pair_id}: route_deterministic={}; route_signature_variants={}/{}",
            report.is_deterministic(),
            report.unique_signature_count,
            report.run_count
        ));
    }
    Ok(lines.join("\n"))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RouteInputDiff {
    pub(crate) added_edges: Vec<RouteEdgeInputDelta>,
    pub(crate) removed_edges: Vec<RouteEdgeInputDelta>,
    pub(crate) changed_labels: Vec<RouteLabelInputDelta>,
    pub(crate) changed_ports: Vec<RoutePortInputDelta>,
    pub(crate) changed_edges: Vec<RouteEdgeInputDelta>,
    pub(crate) changed_nodes: Vec<RouteNodeInputDelta>,
    pub(crate) incidental_label_changes: Vec<RouteIncidentalLabelDelta>,
}

impl RouteInputDiff {
    pub(crate) fn has_changes(&self) -> bool {
        !self.added_edges.is_empty()
            || !self.removed_edges.is_empty()
            || !self.changed_labels.is_empty()
            || !self.changed_ports.is_empty()
            || !self.changed_edges.is_empty()
            || !self.changed_nodes.is_empty()
            || !self.incidental_label_changes.is_empty()
    }

    pub(crate) fn decision_input_change_count(&self) -> usize {
        self.added_edges.len()
            + self.removed_edges.len()
            + self.changed_labels.len()
            + self.changed_ports.len()
            + self.changed_edges.len()
            + self.changed_nodes.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteInputChangeKind {
    DecisionRelevant,
    Incidental,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RouteLabelInputDelta {
    pub(crate) edge_index: usize,
    pub(crate) width_delta: f64,
    pub(crate) height_delta: f64,
    pub(crate) kind: RouteInputChangeKind,
}

impl RouteLabelInputDelta {
    pub(crate) fn is_decision_relevant(&self) -> bool {
        self.kind == RouteInputChangeKind::DecisionRelevant
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RoutePortInputDelta {
    pub(crate) edge_index: usize,
    pub(crate) before_fraction: Option<f64>,
    pub(crate) after_fraction: Option<f64>,
    pub(crate) kind: RouteInputChangeKind,
}

impl RoutePortInputDelta {
    pub(crate) fn is_decision_relevant(&self) -> bool {
        self.kind == RouteInputChangeKind::DecisionRelevant
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RouteEdgeInputDelta {
    pub(crate) edge_index: usize,
    pub(crate) kind: RouteInputChangeKind,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RouteNodeInputDelta {
    pub(crate) node_id: String,
    pub(crate) kind: RouteInputChangeKind,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RouteIncidentalLabelDelta {
    pub(crate) edge_index: usize,
    pub(crate) kind: RouteInputChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LabelLaneDeltaClass {
    CompartmentMembership,
    SubclusterMembership,
    SortOrder,
    Track,
    TrackCenterOnly,
    LabelStepOnly,
    LabelGeometryOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LabelLaneEdgeDelta {
    pub(crate) class: LabelLaneDeltaClass,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LabelLaneSnapshotDiff {
    pub(crate) edge_deltas: BTreeMap<String, LabelLaneEdgeDelta>,
}

impl LabelLaneSnapshotDiff {
    pub(crate) fn total_edges(&self) -> usize {
        self.edge_deltas.len()
    }

    pub(crate) fn histogram(&self) -> BTreeMap<LabelLaneDeltaClass, usize> {
        let mut histogram = BTreeMap::new();
        for delta in self.edge_deltas.values() {
            *histogram.entry(delta.class).or_insert(0) += 1;
        }
        histogram
    }

    pub(crate) fn histogram_total(&self) -> usize {
        self.histogram().values().sum()
    }

    pub(crate) fn structural_total(&self) -> usize {
        self.edge_deltas
            .values()
            .filter(|delta| delta.class != LabelLaneDeltaClass::LabelGeometryOnly)
            .count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathShapeCorrelationOutcome {
    MostlyOverlapsLabelGeometry,
    MostlyDisjointFromLabelGeometry,
    Mixed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathShapeCorrelation {
    pub(crate) label_geometry_count: usize,
    pub(crate) path_shape_count: usize,
    pub(crate) overlap_count: usize,
    pub(crate) outcome: PathShapeCorrelationOutcome,
}

#[derive(Debug, Clone)]
pub(crate) struct RoutePairDiagnostic {
    pub(crate) pair_id: &'static str,
    pub(crate) route_input_diff: RouteInputDiff,
    pub(crate) phase_attribution: phase_attribution::PhaseAttribution,
    pub(crate) route_decisions: RouteDecisionBreakdown,
    pub(crate) label_lane_decomposition: Option<LabelLaneSnapshotDiff>,
    pub(crate) path_shape_correlation: Option<PathShapeCorrelation>,
    pub(crate) label_lane_non_applicability_reason: Option<String>,
}

impl RoutePairDiagnostic {
    pub(crate) fn summary(&self) -> String {
        format!(
            "{}: route_input_diffs={}; decision_input_changes={}; added_edges={}; changed_labels={}; incidental_label_changes={}; route_decisions={:?}; label_lane_decomposition={:?}; path_shape_correlation={:?}; label_lane_non_applicability={:?}; route_input_correlation={}; first_divergence={:?}",
            self.pair_id,
            self.route_input_diff.total_change_count(),
            self.route_input_diff.decision_input_change_count(),
            self.route_input_diff.added_edges.len(),
            self.route_input_diff.changed_labels.len(),
            self.route_input_diff.incidental_label_changes.len(),
            self.route_decisions.primary_histogram(),
            self.label_lane_decomposition
                .as_ref()
                .map(LabelLaneSnapshotDiff::histogram)
                .unwrap_or_default(),
            self.path_shape_correlation,
            self.label_lane_non_applicability_reason,
            self.route_decisions
                .correlate_with_route_inputs(&self.route_input_diff),
            self.phase_attribution.first_divergence()
        )
    }
}

pub(crate) fn run_pair_route_diagnostics(
    pair: &'static mutations::MutationPair,
) -> Result<RoutePairDiagnostic, String> {
    let rendered = render_surfaces::render_pair(pair).map_err(|error| error.to_string())?;
    let before = rendered
        .before
        .route_trace
        .input()
        .ok_or_else(|| format!("{} before route input trace missing", pair.id))?;
    let after = rendered
        .after
        .route_trace
        .input()
        .ok_or_else(|| format!("{} after route input trace missing", pair.id))?;
    let report = report::run_pair(pair).map_err(|error| error.to_string())?;
    let route_decisions =
        classify_route_decisions(route_decision_input_from_metrics(&report.routed));
    let label_geometry_edge_ids = label_geometry_change_edge_ids(&report.routed);
    let label_lane_edge_ids = route_decisions.edge_ids_for_class(RouteDecisionClass::LabelLane);
    let label_lane_non_applicability_reason = label_lane_edge_ids
        .is_empty()
        .then(|| format!("{} has no LabelLane route decision edges", pair.id));
    let label_lane_decomposition = (!label_lane_edge_ids.is_empty())
        .then(|| {
            rendered
                .before
                .route_trace
                .label_lanes()
                .zip(rendered.after.route_trace.label_lanes())
                .map(|(before, after)| {
                    decompose_label_lane_changes(
                        diff_label_lane_snapshots(before, after),
                        &route_decisions,
                        &label_geometry_edge_ids,
                    )
                })
        })
        .flatten();
    let path_shape_correlation = label_lane_decomposition.as_ref().map(|decomposition| {
        correlate_path_shape_with_label_geometry(decomposition, &route_decisions)
    });

    Ok(RoutePairDiagnostic {
        pair_id: pair.id,
        route_input_diff: diff_route_inputs(before, after),
        route_decisions,
        label_lane_decomposition,
        path_shape_correlation,
        label_lane_non_applicability_reason,
        phase_attribution: report.phase_attribution().clone(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RouteDecisionClass {
    Port,
    EndpointFace,
    LabelLane,
    BendTopology,
    PathShape,
    LengthOnly,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RouteDecisionInput {
    by_edge: BTreeMap<String, RouteDecisionFacts>,
}

impl RouteDecisionInput {
    pub(crate) fn with_changed_port(
        mut self,
        edge_id: &str,
        endpoint: super::mmds_metrics::Endpoint,
        before_face: &str,
        after_face: &str,
    ) -> Self {
        self.facts_mut(edge_id)
            .port_changes
            .push(RoutePortDecisionDelta {
                endpoint,
                before_face: before_face.to_string(),
                after_face: after_face.to_string(),
            });
        self
    }

    pub(crate) fn with_bend_delta(mut self, edge_id: &str, bend_delta: isize) -> Self {
        self.facts_mut(edge_id).bend_delta = bend_delta;
        self
    }

    pub(crate) fn with_length_delta(mut self, edge_id: &str, length_delta: f64) -> Self {
        self.facts_mut(edge_id).length_delta = length_delta;
        self
    }

    pub(crate) fn with_label_track_delta(
        mut self,
        edge_id: &str,
        before_track: i32,
        after_track: i32,
    ) -> Self {
        self.facts_mut(edge_id).label_track_delta = Some((before_track, after_track));
        self
    }

    pub(crate) fn with_path_shape_changed(mut self, edge_id: &str) -> Self {
        self.facts_mut(edge_id).path_shape_changed = true;
        self
    }

    fn facts_mut(&mut self, edge_id: &str) -> &mut RouteDecisionFacts {
        self.by_edge.entry(edge_id.to_string()).or_default()
    }
}

#[derive(Debug, Clone, Default)]
struct RouteDecisionFacts {
    port_changes: Vec<RoutePortDecisionDelta>,
    endpoint_face_changed: bool,
    label_track_delta: Option<(i32, i32)>,
    label_lane_changed: bool,
    bend_delta: isize,
    point_count_delta: isize,
    path_shape_changed: bool,
    length_delta: f64,
}

#[derive(Debug, Clone)]
struct RoutePortDecisionDelta {
    endpoint: super::mmds_metrics::Endpoint,
    before_face: String,
    after_face: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RouteDecisionBreakdown {
    pub(crate) by_edge: BTreeMap<String, RoutePathDecision>,
}

impl RouteDecisionBreakdown {
    pub(crate) fn primary_histogram(&self) -> BTreeMap<RouteDecisionClass, usize> {
        let mut histogram = BTreeMap::new();
        for decision in self.by_edge.values() {
            *histogram.entry(decision.primary_class).or_insert(0) += 1;
        }
        histogram
    }

    pub(crate) fn primary_histogram_total(&self) -> usize {
        self.primary_histogram().values().sum()
    }

    pub(crate) fn edge_ids_for_class(&self, class: RouteDecisionClass) -> BTreeSet<String> {
        self.by_edge
            .iter()
            .filter(|(_edge_id, decision)| decision.classes.contains(&class))
            .map(|(edge_id, _decision)| edge_id.clone())
            .collect()
    }

    pub(crate) fn correlate_with_route_inputs(&self, input_diff: &RouteInputDiff) -> String {
        if input_diff.decision_input_change_count() == 0 {
            "no_decision_input_change".to_string()
        } else {
            format!(
                "decision_input_changes={}",
                input_diff.decision_input_change_count()
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoutePathDecision {
    pub(crate) primary_class: RouteDecisionClass,
    pub(crate) classes: Vec<RouteDecisionClass>,
}

pub(crate) fn synthetic_route_decision_input() -> RouteDecisionInput {
    RouteDecisionInput::default()
}

pub(crate) fn classify_route_decisions(input: RouteDecisionInput) -> RouteDecisionBreakdown {
    let mut by_edge = BTreeMap::new();
    for (edge_id, facts) in input.by_edge {
        let classes = decision_classes(&facts);
        let Some(primary_class) = classes.first().copied() else {
            continue;
        };
        by_edge.insert(
            edge_id,
            RoutePathDecision {
                primary_class,
                classes,
            },
        );
    }
    RouteDecisionBreakdown { by_edge }
}

pub(crate) fn classify_path_shape_correlation(overlap_count: usize) -> PathShapeCorrelationOutcome {
    if overlap_count >= 5 {
        PathShapeCorrelationOutcome::MostlyOverlapsLabelGeometry
    } else if overlap_count <= 1 {
        PathShapeCorrelationOutcome::MostlyDisjointFromLabelGeometry
    } else {
        PathShapeCorrelationOutcome::Mixed
    }
}

fn route_decision_input_from_metrics(
    metrics: &mmds_metrics::RoutedMmdsMetrics,
) -> RouteDecisionInput {
    let changed_edge_ids = metrics
        .path_metrics
        .iter()
        .filter(|metric| metric.geometry_changed())
        .map(|metric| metric.edge_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut input = RouteDecisionInput::default();

    for metric in metrics
        .path_metrics
        .iter()
        .filter(|metric| metric.geometry_changed())
    {
        let facts = input.facts_mut(&metric.edge_id);
        facts.bend_delta = metric.bend_count_delta;
        facts.point_count_delta = metric.point_count_delta;
        facts.path_shape_changed = metric.envelope_delta.changed;
        facts.length_delta = metric.length_delta;
    }

    for delta in &metrics.port_intent_changes {
        if !changed_edge_ids.contains(delta.edge_id.as_str()) {
            continue;
        }
        input
            .facts_mut(&delta.edge_id)
            .port_changes
            .push(RoutePortDecisionDelta {
                endpoint: delta.endpoint,
                before_face: delta
                    .before_face
                    .map(|face| face.as_str().to_string())
                    .unwrap_or_else(|| "none".to_string()),
                after_face: delta
                    .after_face
                    .map(|face| face.as_str().to_string())
                    .unwrap_or_else(|| "none".to_string()),
            });
    }

    for delta in &metrics.endpoint_face_changes {
        if changed_edge_ids.contains(delta.edge_id.as_str()) {
            input.facts_mut(&delta.edge_id).endpoint_face_changed = true;
        }
    }

    for delta in &metrics.label_rect_changes {
        if changed_edge_ids.contains(delta.edge_id.as_str()) {
            input.facts_mut(&delta.edge_id).label_lane_changed = true;
        }
    }

    input
}

pub(crate) fn diff_route_inputs(
    before: &RouteInputSnapshot,
    after: &RouteInputSnapshot,
) -> RouteInputDiff {
    let mut added_edges = after
        .edges
        .iter()
        .filter(|after_edge| {
            !before
                .edges
                .iter()
                .any(|before_edge| same_edge_identity(before_edge, after_edge))
        })
        .map(|edge| RouteEdgeInputDelta {
            edge_index: edge.index,
            kind: RouteInputChangeKind::DecisionRelevant,
        })
        .collect::<Vec<_>>();
    added_edges.sort_by_key(|edge| edge.edge_index);

    let mut removed_edges = before
        .edges
        .iter()
        .filter(|before_edge| {
            !after
                .edges
                .iter()
                .any(|after_edge| same_edge_identity(before_edge, after_edge))
        })
        .map(|edge| RouteEdgeInputDelta {
            edge_index: edge.index,
            kind: RouteInputChangeKind::DecisionRelevant,
        })
        .collect::<Vec<_>>();
    removed_edges.sort_by_key(|edge| edge.edge_index);

    let mut changed_labels = Vec::new();
    let mut incidental_label_changes = Vec::new();
    for before_label in &before.labels {
        let Some(after_label) = after
            .labels
            .iter()
            .find(|label| label.edge_index == before_label.edge_index)
        else {
            continue;
        };
        let width_delta = after_label.width - before_label.width;
        let height_delta = after_label.height - before_label.height;
        if changed_float(width_delta) || changed_float(height_delta) {
            changed_labels.push(RouteLabelInputDelta {
                edge_index: before_label.edge_index,
                width_delta,
                height_delta,
                kind: RouteInputChangeKind::DecisionRelevant,
            });
        } else if before_label.label_text != after_label.label_text {
            incidental_label_changes.push(RouteIncidentalLabelDelta {
                edge_index: before_label.edge_index,
                kind: RouteInputChangeKind::Incidental,
            });
        }
    }

    let mut changed_ports = Vec::new();
    for before_edge in &before.edges {
        let Some(after_edge) = after
            .edges
            .iter()
            .find(|edge| edge.index == before_edge.index)
        else {
            continue;
        };
        if before_edge.source_port != after_edge.source_port {
            changed_ports.push(RoutePortInputDelta {
                edge_index: before_edge.index,
                before_fraction: before_edge.source_port.as_ref().map(|port| port.fraction),
                after_fraction: after_edge.source_port.as_ref().map(|port| port.fraction),
                kind: RouteInputChangeKind::DecisionRelevant,
            });
        }
    }

    RouteInputDiff {
        added_edges,
        removed_edges,
        changed_labels,
        changed_ports,
        changed_edges: Vec::new(),
        changed_nodes: Vec::new(),
        incidental_label_changes,
    }
}

pub(crate) fn diff_label_lane_snapshots(
    before: &LabelLaneTraceSnapshot,
    after: &LabelLaneTraceSnapshot,
) -> LabelLaneSnapshotDiff {
    let before_by_edge = before
        .edges
        .iter()
        .map(|edge| (edge.mmds_edge_id.as_str(), edge))
        .collect::<BTreeMap<_, _>>();
    let after_by_edge = after
        .edges
        .iter()
        .map(|edge| (edge.mmds_edge_id.as_str(), edge))
        .collect::<BTreeMap<_, _>>();
    let mut edge_deltas = BTreeMap::new();

    for (edge_id, before_edge) in before_by_edge {
        let Some(after_edge) = after_by_edge.get(edge_id) else {
            continue;
        };
        let Some(class) = label_lane_delta_class(before_edge, after_edge) else {
            continue;
        };
        edge_deltas.insert(edge_id.to_string(), LabelLaneEdgeDelta { class });
    }

    LabelLaneSnapshotDiff { edge_deltas }
}

fn decompose_label_lane_changes(
    diff: LabelLaneSnapshotDiff,
    route_decisions: &RouteDecisionBreakdown,
    label_geometry_edge_ids: &BTreeSet<String>,
) -> LabelLaneSnapshotDiff {
    let mut edge_deltas = BTreeMap::new();

    for (edge_id, decision) in &route_decisions.by_edge {
        if !decision.classes.contains(&RouteDecisionClass::LabelLane) {
            continue;
        }
        if let Some(delta) = diff.edge_deltas.get(edge_id) {
            edge_deltas.insert(edge_id.clone(), delta.clone());
        } else if label_geometry_edge_ids.contains(edge_id) {
            edge_deltas.insert(
                edge_id.clone(),
                LabelLaneEdgeDelta {
                    class: LabelLaneDeltaClass::LabelGeometryOnly,
                },
            );
        }
    }

    LabelLaneSnapshotDiff { edge_deltas }
}

fn label_geometry_change_edge_ids(metrics: &mmds_metrics::RoutedMmdsMetrics) -> BTreeSet<String> {
    metrics
        .label_rect_changes
        .iter()
        .map(|delta| delta.edge_id.clone())
        .collect()
}

fn correlate_path_shape_with_label_geometry(
    decomposition: &LabelLaneSnapshotDiff,
    route_decisions: &RouteDecisionBreakdown,
) -> PathShapeCorrelation {
    let label_geometry_edge_ids = decomposition
        .edge_deltas
        .iter()
        .filter(|(_edge_id, delta)| delta.class == LabelLaneDeltaClass::LabelGeometryOnly)
        .map(|(edge_id, _delta)| edge_id.clone())
        .collect::<BTreeSet<_>>();
    let path_shape_edge_ids = route_decisions.edge_ids_for_class(RouteDecisionClass::PathShape);
    let overlap_count = label_geometry_edge_ids
        .intersection(&path_shape_edge_ids)
        .count();

    PathShapeCorrelation {
        label_geometry_count: label_geometry_edge_ids.len(),
        path_shape_count: path_shape_edge_ids.len(),
        overlap_count,
        outcome: classify_path_shape_correlation(overlap_count),
    }
}

impl RouteInputDiff {
    fn total_change_count(&self) -> usize {
        self.added_edges.len()
            + self.removed_edges.len()
            + self.changed_labels.len()
            + self.changed_ports.len()
            + self.changed_edges.len()
            + self.changed_nodes.len()
            + self.incidental_label_changes.len()
    }
}

pub(crate) fn synthetic_route_input_with_label(
    edge_index: usize,
    width: f64,
    height: f64,
) -> RouteInputSnapshot {
    synthetic_route_input(edge_index, None, width, height, None)
}

pub(crate) fn synthetic_route_input_with_label_text(
    edge_index: usize,
    label_text: &str,
    width: f64,
    height: f64,
) -> RouteInputSnapshot {
    synthetic_route_input(edge_index, Some(label_text), width, height, None)
}

pub(crate) fn synthetic_route_input_with_source_port(
    edge_index: usize,
    face: &str,
    fraction: f64,
) -> RouteInputSnapshot {
    synthetic_route_input(
        edge_index,
        None,
        80.0,
        20.0,
        Some(RoutePortInput {
            face: face.to_string(),
            fraction,
            group_size: 1,
        }),
    )
}

pub(crate) fn synthetic_label_lane_snapshot(edge_id: &str) -> LabelLaneTraceSnapshot {
    LabelLaneTraceSnapshot {
        edges: vec![LabelLaneEdgeSnapshot {
            edge_index: edge_index_from_mmds_id(edge_id),
            mmds_edge_id: edge_id.to_string(),
            compartment_id: format!("scope:none|members:{edge_id}"),
            subcluster_id: format!("scope:none|members:{edge_id}|cluster:{edge_id}"),
            sort_position: 0,
            track: 0,
            track_center: 0.0,
            label_step: 32.0,
            label_rect: FRect::new(10.0, 20.0, 40.0, 16.0),
        }],
        compartments: Vec::new(),
        subclusters: Vec::new(),
    }
}

impl LabelLaneTraceSnapshot {
    pub(crate) fn with_compartment(mut self, compartment_id: &str) -> Self {
        self.edges[0].compartment_id = compartment_id.to_string();
        self
    }

    pub(crate) fn with_track(mut self, track: i32) -> Self {
        self.edges[0].track = track;
        self
    }

    pub(crate) fn with_label_step(mut self, label_step: f64) -> Self {
        self.edges[0].label_step = label_step;
        self
    }
}

fn routed_mmds_signature(output: &crate::mmds::Document) -> String {
    serde_json::to_string(output).expect("MMDS output should serialize")
}

fn synthetic_route_input(
    edge_index: usize,
    label_text: Option<&str>,
    width: f64,
    height: f64,
    source_port: Option<RoutePortInput>,
) -> RouteInputSnapshot {
    RouteInputSnapshot {
        edge_routing: EdgeRouting::PolylineRoute,
        nodes: vec![RouteNodeInput {
            id: "A".to_string(),
            rect: FRect::new(10.0, 20.0, 40.0, 30.0),
            shape: Shape::Rectangle,
            label: "A".to_string(),
            parent: None,
            direction: Direction::TopDown,
        }],
        edges: vec![RouteEdgeInput {
            index: edge_index,
            from: "A".to_string(),
            to: "B".to_string(),
            waypoints: Vec::new(),
            layout_path_hint: None,
            label_position: Some(FPoint::new(10.0, 35.0)),
            label_side: Some(EdgeLabelSide::Above),
            source_port,
            target_port: None,
            is_backward: false,
            preserve_orthogonal_topology: false,
        }],
        labels: vec![RouteLabelInput {
            edge_index,
            label_text: label_text.map(str::to_string),
            width,
            height,
            axis_min: 35.0 - height / 2.0,
            axis_max: 35.0 + height / 2.0,
            cross_min: 10.0 - width / 2.0,
            cross_max: 10.0 + width / 2.0,
            side: Some(EdgeLabelSide::Above),
            direction_sign: 1,
            scope_parent: None,
            midpoint: FPoint::new(10.0, 35.0),
        }],
    }
}

fn changed_float(value: f64) -> bool {
    value.abs() > super::mmds_metrics::COORD_EPS
}

fn label_lane_delta_class(
    before: &LabelLaneEdgeSnapshot,
    after: &LabelLaneEdgeSnapshot,
) -> Option<LabelLaneDeltaClass> {
    if before.compartment_id != after.compartment_id {
        return Some(LabelLaneDeltaClass::CompartmentMembership);
    }
    if before.subcluster_id != after.subcluster_id {
        return Some(LabelLaneDeltaClass::SubclusterMembership);
    }
    if before.sort_position != after.sort_position {
        return Some(LabelLaneDeltaClass::SortOrder);
    }
    if before.track != after.track {
        return Some(LabelLaneDeltaClass::Track);
    }
    if changed_float(after.track_center - before.track_center) {
        return Some(LabelLaneDeltaClass::TrackCenterOnly);
    }
    if changed_float(after.label_step - before.label_step) {
        return Some(LabelLaneDeltaClass::LabelStepOnly);
    }
    if changed_rect(&before.label_rect, &after.label_rect) {
        return Some(LabelLaneDeltaClass::LabelGeometryOnly);
    }
    None
}

fn changed_rect(before: &FRect, after: &FRect) -> bool {
    changed_float(after.x - before.x)
        || changed_float(after.y - before.y)
        || changed_float(after.width - before.width)
        || changed_float(after.height - before.height)
}

fn edge_index_from_mmds_id(edge_id: &str) -> usize {
    edge_id
        .strip_prefix('e')
        .and_then(|suffix| suffix.parse().ok())
        .unwrap_or_default()
}

fn same_edge_identity(before: &RouteEdgeInput, after: &RouteEdgeInput) -> bool {
    before.index == after.index && before.from == after.from && before.to == after.to
}

fn decision_classes(facts: &RouteDecisionFacts) -> Vec<RouteDecisionClass> {
    let mut classes = Vec::new();
    if facts.port_changes.iter().any(|delta| {
        delta.before_face != delta.after_face
            || matches!(
                delta.endpoint,
                super::mmds_metrics::Endpoint::Source | super::mmds_metrics::Endpoint::Target
            )
    }) {
        classes.push(RouteDecisionClass::Port);
    }
    if facts.endpoint_face_changed {
        classes.push(RouteDecisionClass::EndpointFace);
    }
    if facts
        .label_track_delta
        .is_some_and(|(before, after)| before != after)
        || facts.label_lane_changed
    {
        classes.push(RouteDecisionClass::LabelLane);
    }
    if facts.bend_delta != 0 {
        classes.push(RouteDecisionClass::BendTopology);
    }
    if facts.point_count_delta != 0 || facts.path_shape_changed {
        classes.push(RouteDecisionClass::PathShape);
    }
    if facts.length_delta.abs() > super::mmds_metrics::DISPLAY_EPS {
        classes.push(RouteDecisionClass::LengthOnly);
    }
    classes
}
