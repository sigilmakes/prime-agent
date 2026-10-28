use std::collections::{BTreeMap, BTreeSet};

use super::{mutations, render_surfaces, report, route_diagnostics};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InputMagnitude {
    pub(crate) label_dimension: Option<LabelDimensionMagnitude>,
    pub(crate) edge_insertion: Option<EdgeInsertionMagnitude>,
    pub(crate) node_insertion: Option<NodeInsertionMagnitude>,
    pub(crate) subgraph_change: Option<SubgraphChangeMagnitude>,
}

impl InputMagnitude {
    pub(crate) fn non_zero_axes(&self) -> Vec<InputMagnitudeAxis> {
        let mut axes = Vec::new();
        if self
            .label_dimension
            .as_ref()
            .is_some_and(LabelDimensionMagnitude::is_non_zero)
        {
            axes.push(InputMagnitudeAxis::LabelDimensionUnits);
        }
        if self
            .edge_insertion
            .as_ref()
            .is_some_and(EdgeInsertionMagnitude::is_non_zero)
        {
            axes.push(InputMagnitudeAxis::EdgeInsertion);
        }
        if self
            .node_insertion
            .as_ref()
            .is_some_and(NodeInsertionMagnitude::is_non_zero)
        {
            axes.push(InputMagnitudeAxis::NodeInsertionArea);
        }
        if self
            .subgraph_change
            .as_ref()
            .is_some_and(SubgraphChangeMagnitude::is_non_zero)
        {
            axes.push(InputMagnitudeAxis::SubgraphChange);
        }
        axes
    }

    pub(crate) fn scalar_denominator(&self) -> Option<f64> {
        match self.non_zero_axes().as_slice() {
            [InputMagnitudeAxis::LabelDimensionUnits] => self
                .label_dimension
                .as_ref()
                .map(|magnitude| magnitude.total_axis_delta),
            [InputMagnitudeAxis::NodeInsertionArea] => self
                .node_insertion
                .as_ref()
                .map(|magnitude| magnitude.bbox_area_added),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LabelDimensionMagnitude {
    pub(crate) changed_labels: usize,
    pub(crate) max_axis_delta: f64,
    pub(crate) total_axis_delta: f64,
}

impl LabelDimensionMagnitude {
    fn is_non_zero(&self) -> bool {
        self.changed_labels > 0
            || self.max_axis_delta.abs() > f64::EPSILON
            || self.total_axis_delta.abs() > f64::EPSILON
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeInsertionMagnitude {
    pub(crate) semantic_added_edge_ids: Vec<String>,
    pub(crate) route_input_added_edges: usize,
}

impl EdgeInsertionMagnitude {
    fn is_non_zero(&self) -> bool {
        !self.semantic_added_edge_ids.is_empty() || self.route_input_added_edges > 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeInsertionMagnitude {
    pub(crate) added_node_ids: Vec<String>,
    pub(crate) bbox_area_added: f64,
}

impl NodeInsertionMagnitude {
    fn is_non_zero(&self) -> bool {
        !self.added_node_ids.is_empty() || self.bbox_area_added.abs() > f64::EPSILON
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubgraphChangeMagnitude {
    pub(crate) membership_delta_count: usize,
    pub(crate) direction_changed: bool,
    pub(crate) scope_parent_id: Option<String>,
}

impl SubgraphChangeMagnitude {
    fn is_non_zero(&self) -> bool {
        self.membership_delta_count > 0 || self.direction_changed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputMagnitudeAxis {
    LabelDimensionUnits,
    EdgeInsertion,
    NodeInsertionArea,
    SubgraphChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputMagnitudeBasis {
    LabelDimensionUnits,
    EdgeInsertionContext,
    NodeInsertionArea,
    SubgraphChange,
    MultiAxis,
    None,
}

impl InputMagnitudeBasis {
    pub(crate) fn for_pair_id(pair_id: &str) -> Self {
        match pair_id {
            "M06" | "M14" | "S02" => Self::LabelDimensionUnits,
            "M02" | "M03" | "M04" | "M11" | "M19" | "M20" | "M21" | "S01" => {
                Self::EdgeInsertionContext
            }
            "M05-ID" => Self::NodeInsertionArea,
            "M08" | "M09" | "M10" | "S04" => Self::SubgraphChange,
            "M15" => Self::MultiAxis,
            _ => Self::None,
        }
    }
}

pub(crate) fn pure_edge_control_pair_ids() -> [&'static str; 4] {
    ["M04", "M19", "M20", "M21"]
}

pub(crate) fn node_plus_edge_control_pair_ids() -> [&'static str; 2] {
    ["M02", "M03"]
}

pub(crate) fn input_magnitude_for_pair(
    pair: &'static mutations::MutationPair,
) -> Result<InputMagnitude, String> {
    let rendered = render_surfaces::render_pair(pair)
        .map_err(|error| format!("{} input magnitude render failed: {error}", pair.id))?;
    let route_diagnostic = route_diagnostics::run_pair_route_diagnostics(pair)?;

    Ok(InputMagnitude {
        label_dimension: label_dimension_magnitude(&route_diagnostic.route_input_diff),
        edge_insertion: edge_insertion_magnitude(pair, &route_diagnostic.route_input_diff),
        node_insertion: node_insertion_magnitude(pair, &rendered),
        subgraph_change: subgraph_change_magnitude(pair, &rendered),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PathOutputMagnitude {
    pub(crate) edge_id: String,
    pub(crate) signed_route_len_delta: f64,
    pub(crate) abs_route_len_delta: f64,
    pub(crate) bend_count_delta: isize,
    pub(crate) abs_bend_count_delta: usize,
    pub(crate) port_face_changed: bool,
    pub(crate) endpoint_face_changed: bool,
    pub(crate) endpoint_moved: bool,
    pub(crate) decision_classes: Vec<route_diagnostics::RouteDecisionClass>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RouteOutputMagnitudeSummary {
    pub(crate) changed_path_count: usize,
    pub(crate) signed_route_len_delta_sum: f64,
    pub(crate) sum_abs_route_len_delta: f64,
    pub(crate) median_abs_route_len_delta: f64,
    pub(crate) p75_abs_route_len_delta: f64,
    pub(crate) max_abs_route_len_delta: f64,
    pub(crate) abs_bend_count_delta_sum: usize,
    pub(crate) port_face_change_count: usize,
    pub(crate) endpoint_face_change_count: usize,
    pub(crate) layout_amplified_path_count: usize,
}

impl RouteOutputMagnitudeSummary {
    pub(crate) fn from_abs_lengths(lengths: Vec<f64>) -> Self {
        Self::from_paths(
            lengths
                .into_iter()
                .enumerate()
                .map(|(index, length)| PathOutputMagnitude {
                    edge_id: format!("path-{index}"),
                    signed_route_len_delta: length,
                    abs_route_len_delta: length.abs(),
                    bend_count_delta: 0,
                    abs_bend_count_delta: 0,
                    port_face_changed: false,
                    endpoint_face_changed: false,
                    endpoint_moved: false,
                    decision_classes: Vec::new(),
                })
                .collect(),
        )
    }

    pub(crate) fn from_paths(paths: Vec<PathOutputMagnitude>) -> Self {
        if paths.is_empty() {
            return Self {
                changed_path_count: 0,
                signed_route_len_delta_sum: 0.0,
                sum_abs_route_len_delta: 0.0,
                median_abs_route_len_delta: 0.0,
                p75_abs_route_len_delta: 0.0,
                max_abs_route_len_delta: 0.0,
                abs_bend_count_delta_sum: 0,
                port_face_change_count: 0,
                endpoint_face_change_count: 0,
                layout_amplified_path_count: 0,
            };
        }

        let mut abs_lengths = paths
            .iter()
            .map(|path| path.abs_route_len_delta)
            .collect::<Vec<_>>();
        abs_lengths.sort_by(f64::total_cmp);

        Self {
            changed_path_count: paths.len(),
            signed_route_len_delta_sum: paths.iter().map(|path| path.signed_route_len_delta).sum(),
            sum_abs_route_len_delta: abs_lengths.iter().sum(),
            median_abs_route_len_delta: nearest_rank(&abs_lengths, 0.5),
            p75_abs_route_len_delta: nearest_rank(&abs_lengths, 0.75),
            max_abs_route_len_delta: *abs_lengths.last().unwrap_or(&0.0),
            abs_bend_count_delta_sum: paths.iter().map(|path| path.abs_bend_count_delta).sum(),
            port_face_change_count: paths.iter().filter(|path| path.port_face_changed).count(),
            endpoint_face_change_count: paths
                .iter()
                .filter(|path| path.endpoint_face_changed)
                .count(),
            layout_amplified_path_count: paths.iter().filter(|path| path.endpoint_moved).count(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProportionalityClass {
    Proportionate,
    Disproportionate,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CalibrationSource {
    Seeded,
    ControlDerived,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CalibrationProfile {
    pub(crate) label_ratio_proportionate_max: f64,
    pub(crate) label_ratio_disproportionate_min: f64,
    pub(crate) event_control_p75_abs_route_len_delta: Option<f64>,
    pub(crate) source: CalibrationSource,
    pub(crate) control_pair_ids: Vec<&'static str>,
}

impl CalibrationProfile {
    pub(crate) fn seeded() -> Self {
        Self {
            label_ratio_proportionate_max: 2.0,
            label_ratio_disproportionate_min: 3.0,
            event_control_p75_abs_route_len_delta: None,
            source: CalibrationSource::Seeded,
            control_pair_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProportionalitySummary {
    pub(crate) pair_id: &'static str,
    pub(crate) basis: InputMagnitudeBasis,
    pub(crate) input: InputMagnitude,
    pub(crate) output: RouteOutputMagnitudeSummary,
    pub(crate) aggregate_ratio: Option<f64>,
    pub(crate) max_path_ratio: Option<f64>,
    pub(crate) classification: Option<ProportionalityClass>,
}

impl ProportionalitySummary {
    pub(crate) fn from_parts(
        pair_id: &'static str,
        basis: InputMagnitudeBasis,
        input: InputMagnitude,
        output: RouteOutputMagnitudeSummary,
    ) -> Result<Self, String> {
        let (aggregate_ratio, max_path_ratio) = match basis {
            InputMagnitudeBasis::LabelDimensionUnits => {
                let denominator = input.scalar_denominator().ok_or_else(|| {
                    format!("{pair_id} label dimension summary needs a non-zero denominator")
                })?;
                if denominator.abs() <= f64::EPSILON {
                    return Err(format!(
                        "{pair_id} label dimension summary needs a non-zero denominator"
                    ));
                }
                (
                    Some(output.sum_abs_route_len_delta / denominator),
                    Some(output.max_abs_route_len_delta / denominator),
                )
            }
            _ => (None, None),
        };

        Ok(Self {
            pair_id,
            basis,
            input,
            output,
            aggregate_ratio,
            max_path_ratio,
            classification: None,
        })
    }
}

pub(crate) fn classify_label_ratio(
    aggregate_ratio: f64,
    max_path_ratio: f64,
    calibration: &CalibrationProfile,
) -> ProportionalityClass {
    if aggregate_ratio <= calibration.label_ratio_proportionate_max
        && max_path_ratio <= calibration.label_ratio_proportionate_max
    {
        ProportionalityClass::Proportionate
    } else if aggregate_ratio >= calibration.label_ratio_disproportionate_min
        || max_path_ratio >= calibration.label_ratio_disproportionate_min
    {
        ProportionalityClass::Disproportionate
    } else {
        ProportionalityClass::Mixed
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ControlCalibrationPoint {
    pub(crate) pair_id: &'static str,
    pub(crate) p75_abs_route_len_delta: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PureEdgeClusterSummary {
    pub(crate) control_pair_ids: Vec<&'static str>,
    pub(crate) p75_min: f64,
    pub(crate) p75_max: f64,
    pub(crate) spread_ratio: Option<f64>,
    pub(crate) status: PureEdgeClusterStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PureEdgeClusterStatus {
    OneCluster,
    Split { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlCalibrationStatus {
    Consistent,
    Inconsistent { reason: String },
}

pub(crate) fn control_calibration_status(
    controls: &[ControlCalibrationPoint],
) -> ControlCalibrationStatus {
    let Some((min, max, _spread_ratio)) = p75_spread(controls) else {
        return ControlCalibrationStatus::Consistent;
    };
    let non_zero = non_zero_p75_controls(controls);
    if non_zero.len() < 2 {
        return ControlCalibrationStatus::Consistent;
    }

    if max / min <= 4.0 {
        ControlCalibrationStatus::Consistent
    } else {
        let pair_ids = non_zero
            .iter()
            .map(|control| control.pair_id)
            .collect::<Vec<_>>()
            .join(", ");
        ControlCalibrationStatus::Inconsistent {
            reason: format!(
                "control p75 spread is too wide for one event threshold ({pair_ids}: {min:.2}..{max:.2})"
            ),
        }
    }
}

pub(crate) fn pure_edge_cluster_status(
    controls: &[ControlCalibrationPoint],
) -> PureEdgeClusterStatus {
    let Some((min, max, _spread_ratio)) = p75_spread(controls) else {
        return PureEdgeClusterStatus::OneCluster;
    };

    if max / min <= 4.0 {
        PureEdgeClusterStatus::OneCluster
    } else {
        let pair_ids = controls
            .iter()
            .filter(|control| control.p75_abs_route_len_delta > f64::EPSILON)
            .map(|control| control.pair_id)
            .collect::<Vec<_>>()
            .join(", ");
        PureEdgeClusterStatus::Split {
            reason: format!(
                "pure-edge control p75 spread is too wide for one cluster ({pair_ids}: {min:.2}..{max:.2})"
            ),
        }
    }
}

fn pure_edge_cluster_summary(
    controls: &[ControlCalibrationPoint],
) -> Option<PureEdgeClusterSummary> {
    if controls.is_empty() {
        return None;
    }

    let (p75_min, p75_max, spread_ratio) = p75_spread(controls).unwrap_or((0.0, 0.0, None));

    Some(PureEdgeClusterSummary {
        control_pair_ids: controls.iter().map(|control| control.pair_id).collect(),
        p75_min,
        p75_max,
        spread_ratio,
        status: pure_edge_cluster_status(controls),
    })
}

fn p75_spread(controls: &[ControlCalibrationPoint]) -> Option<(f64, f64, Option<f64>)> {
    let non_zero = non_zero_p75_controls(controls);
    if non_zero.is_empty() {
        return None;
    }

    let min = non_zero
        .iter()
        .map(|control| control.p75_abs_route_len_delta)
        .min_by(f64::total_cmp)
        .unwrap_or(0.0);
    let max = non_zero
        .iter()
        .map(|control| control.p75_abs_route_len_delta)
        .max_by(f64::total_cmp)
        .unwrap_or(0.0);
    let spread_ratio = (min > f64::EPSILON).then_some(max / min);

    Some((min, max, spread_ratio))
}

fn non_zero_p75_controls(controls: &[ControlCalibrationPoint]) -> Vec<&ControlCalibrationPoint> {
    controls
        .iter()
        .filter(|control| control.p75_abs_route_len_delta > f64::EPSILON)
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RouteProportionalityReport {
    pub(crate) calibration: CalibrationProfile,
    pub(crate) control_calibration_status: ControlCalibrationStatus,
    pub(crate) pure_edge_cluster: Option<PureEdgeClusterSummary>,
    pub(crate) summaries: Vec<CanaryProportionalitySummary>,
}

impl RouteProportionalityReport {
    pub(crate) fn summary_for(&self, pair_id: &str) -> Option<&CanaryProportionalitySummary> {
        self.summaries
            .iter()
            .find(|summary| summary.pair_id == pair_id)
    }

    pub(crate) fn summary(&self) -> String {
        let control_status = match &self.control_calibration_status {
            ControlCalibrationStatus::Consistent => "consistent".to_string(),
            ControlCalibrationStatus::Inconsistent { reason } => {
                format!("inconsistent ({reason})")
            }
        };
        let mut lines = vec![format!(
            "calibration_source={:?}; label_ratio_proportionate_max={:.2}; label_ratio_disproportionate_min={:.2}; event_control_p75_abs_route_len_delta={:?}; control_calibration_status={control_status}",
            self.calibration.source,
            self.calibration.label_ratio_proportionate_max,
            self.calibration.label_ratio_disproportionate_min,
            self.calibration.event_control_p75_abs_route_len_delta,
        )];

        for summary in &self.summaries {
            lines.push(format!(
                "{}: basis={:?}; changed_paths={}; sum_abs_route_len_delta={:.2}; p75_abs_route_len_delta={:.2}; max_abs_route_len_delta={:.2}; aggregate_ratio={:?}; max_path_ratio={:?}; classification={:?}; classification_reason={:?}; additive_path_shape_count={:?}; label_geometry_overlap_count={:?}",
                summary.pair_id,
                summary.basis,
                summary.output.changed_path_count,
                summary.output.sum_abs_route_len_delta,
                summary.output.p75_abs_route_len_delta,
                summary.output.max_abs_route_len_delta,
                summary.aggregate_ratio,
                summary.max_path_ratio,
                summary.classification,
                summary.classification_reason,
                summary.additive_path_shape_count,
                summary.label_geometry_overlap_count,
            ));
        }

        lines.join("\n")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CanaryProportionalitySummary {
    pub(crate) pair_id: &'static str,
    pub(crate) basis: InputMagnitudeBasis,
    pub(crate) input: InputMagnitude,
    pub(crate) output: RouteOutputMagnitudeSummary,
    pub(crate) aggregate_ratio: Option<f64>,
    pub(crate) max_path_ratio: Option<f64>,
    pub(crate) classification: Option<ProportionalityClass>,
    pub(crate) classification_reason: Option<String>,
    pub(crate) additive_path_shape_count: Option<usize>,
    pub(crate) label_geometry_overlap_count: Option<usize>,
}

pub(crate) fn run_route_proportionality_report() -> Result<RouteProportionalityReport, String> {
    let primary_pair_ids = ["M14", "M11"];
    let mut control_points = Vec::new();
    let mut pure_edge_points = Vec::new();
    let mut summaries = Vec::new();

    for pair_id in node_plus_edge_control_pair_ids() {
        let pair =
            mutations::pair_by_id(pair_id).ok_or_else(|| format!("unknown pair id {pair_id}"))?;
        let output = route_output_magnitude_for_pair(pair)?;
        control_points.push(ControlCalibrationPoint {
            pair_id,
            p75_abs_route_len_delta: output.summary.p75_abs_route_len_delta,
        });
        summaries.push(summary_for_pair(
            pair,
            output,
            &CalibrationProfile::seeded(),
            None,
        )?);
    }

    for pair_id in pure_edge_control_pair_ids() {
        let pair =
            mutations::pair_by_id(pair_id).ok_or_else(|| format!("unknown pair id {pair_id}"))?;
        let output = route_output_magnitude_for_pair(pair)?;
        let control_point = ControlCalibrationPoint {
            pair_id,
            p75_abs_route_len_delta: output.summary.p75_abs_route_len_delta,
        };
        control_points.push(control_point.clone());
        pure_edge_points.push(control_point);
        summaries.push(summary_for_pair(
            pair,
            output,
            &CalibrationProfile::seeded(),
            None,
        )?);
    }

    let control_calibration_status = control_calibration_status(&control_points);
    let pure_edge_cluster = pure_edge_cluster_summary(&pure_edge_points);
    let calibration = CalibrationProfile::seeded();

    for pair_id in primary_pair_ids {
        let pair =
            mutations::pair_by_id(pair_id).ok_or_else(|| format!("unknown pair id {pair_id}"))?;
        let output = route_output_magnitude_for_pair(pair)?;
        summaries.push(summary_for_pair(
            pair,
            output,
            &calibration,
            pure_edge_cluster.as_ref(),
        )?);
    }

    summaries.sort_by_key(|summary| summary.pair_id);

    Ok(RouteProportionalityReport {
        calibration,
        control_calibration_status,
        pure_edge_cluster,
        summaries,
    })
}

fn summary_for_pair(
    pair: &'static mutations::MutationPair,
    output: RouteOutputMagnitude,
    calibration: &CalibrationProfile,
    pure_edge_cluster: Option<&PureEdgeClusterSummary>,
) -> Result<CanaryProportionalitySummary, String> {
    let basis = InputMagnitudeBasis::for_pair_id(pair.id);
    let input = input_magnitude_for_pair(pair)?;
    let proportionality =
        ProportionalitySummary::from_parts(pair.id, basis, input.clone(), output.summary.clone())?;
    let diagnostic = route_diagnostics::run_pair_route_diagnostics(pair)?;
    let additive_path_shape_count =
        output.decision_edge_count(route_diagnostics::RouteDecisionClass::PathShape);
    let (classification, classification_reason) =
        classify_summary(&proportionality, calibration, pure_edge_cluster);

    Ok(CanaryProportionalitySummary {
        pair_id: pair.id,
        basis,
        input,
        output: output.summary,
        aggregate_ratio: proportionality.aggregate_ratio,
        max_path_ratio: proportionality.max_path_ratio,
        classification,
        classification_reason,
        additive_path_shape_count: Some(additive_path_shape_count),
        label_geometry_overlap_count: diagnostic
            .path_shape_correlation
            .as_ref()
            .map(|correlation| correlation.overlap_count),
    })
}

fn classify_summary(
    summary: &ProportionalitySummary,
    calibration: &CalibrationProfile,
    pure_edge_cluster: Option<&PureEdgeClusterSummary>,
) -> (Option<ProportionalityClass>, Option<String>) {
    match summary.basis {
        InputMagnitudeBasis::LabelDimensionUnits => (
            Some(classify_label_ratio(
                summary.aggregate_ratio.unwrap_or_default(),
                summary.max_path_ratio.unwrap_or_default(),
                calibration,
            )),
            None,
        ),
        InputMagnitudeBasis::EdgeInsertionContext if summary.pair_id == "M11" => {
            classify_edge_insertion_context(summary, pure_edge_cluster)
        }
        _ => (None, None),
    }
}

fn classify_edge_insertion_context(
    summary: &ProportionalitySummary,
    pure_edge_cluster: Option<&PureEdgeClusterSummary>,
) -> (Option<ProportionalityClass>, Option<String>) {
    let Some(cluster) = pure_edge_cluster else {
        return (
            None,
            Some("pure-edge cluster unavailable for M11 classification".to_string()),
        );
    };

    match &cluster.status {
        PureEdgeClusterStatus::OneCluster => {
            let p75 = summary.output.p75_abs_route_len_delta;
            if p75 <= cluster.p75_max {
                (Some(ProportionalityClass::Proportionate), None)
            } else if p75 > cluster.p75_max * 4.0 {
                (Some(ProportionalityClass::Disproportionate), None)
            } else {
                (Some(ProportionalityClass::Mixed), None)
            }
        }
        PureEdgeClusterStatus::Split { reason } => (
            None,
            Some(format!(
                "pure-edge cluster split; sub-band classification needs at least two matching controls ({reason})"
            )),
        ),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RouteOutputMagnitude {
    pub(crate) paths: Vec<PathOutputMagnitude>,
    pub(crate) summary: RouteOutputMagnitudeSummary,
}

impl RouteOutputMagnitude {
    pub(crate) fn decision_edge_count(
        &self,
        class: route_diagnostics::RouteDecisionClass,
    ) -> usize {
        self.paths
            .iter()
            .filter(|path| path.decision_classes.contains(&class))
            .count()
    }
}

pub(crate) fn route_output_magnitude_for_pair(
    pair: &'static mutations::MutationPair,
) -> Result<RouteOutputMagnitude, String> {
    let report = report::run_pair(pair).map_err(|error| error.to_string())?;
    let route_diagnostic = route_diagnostics::run_pair_route_diagnostics(pair)?;
    let port_face_changes = report
        .routed
        .port_intent_changes
        .iter()
        .map(|delta| delta.edge_id.as_str())
        .collect::<BTreeSet<_>>();
    let endpoint_face_changes = report
        .routed
        .endpoint_face_changes
        .iter()
        .map(|delta| delta.edge_id.as_str())
        .collect::<BTreeSet<_>>();
    let layout_amplified_paths = report
        .phase_attribution()
        .route
        .layout_amplified_paths
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();

    let mut paths = report
        .routed
        .path_metrics
        .iter()
        .filter(|metric| metric.geometry_changed())
        .map(|metric| PathOutputMagnitude {
            edge_id: metric.edge_id.clone(),
            signed_route_len_delta: metric.length_delta,
            abs_route_len_delta: metric.length_delta.abs(),
            bend_count_delta: metric.bend_count_delta,
            abs_bend_count_delta: metric.bend_count_delta.unsigned_abs(),
            port_face_changed: port_face_changes.contains(metric.edge_id.as_str()),
            endpoint_face_changed: endpoint_face_changes.contains(metric.edge_id.as_str()),
            endpoint_moved: layout_amplified_paths.contains(metric.edge_id.as_str()),
            decision_classes: route_diagnostic
                .route_decisions
                .by_edge
                .get(&metric.edge_id)
                .map(|decision| decision.classes.clone())
                .unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    paths.sort_by(|a, b| a.edge_id.cmp(&b.edge_id));
    let summary = RouteOutputMagnitudeSummary::from_paths(paths.clone());

    Ok(RouteOutputMagnitude { paths, summary })
}

fn nearest_rank(sorted_values: &[f64], percentile: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }
    let rank = (percentile.clamp(0.0, 1.0) * sorted_values.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted_values.len() - 1);
    sorted_values[index]
}

fn label_dimension_magnitude(
    route_input_diff: &route_diagnostics::RouteInputDiff,
) -> Option<LabelDimensionMagnitude> {
    let mut max_axis_delta = 0.0_f64;
    let mut total_axis_delta = 0.0_f64;
    for delta in &route_input_diff.changed_labels {
        let axis_delta = delta.width_delta.abs().max(delta.height_delta.abs());
        max_axis_delta = max_axis_delta.max(axis_delta);
        total_axis_delta += axis_delta;
    }

    (!route_input_diff.changed_labels.is_empty()).then_some(LabelDimensionMagnitude {
        changed_labels: route_input_diff.changed_labels.len(),
        max_axis_delta,
        total_axis_delta,
    })
}

fn edge_insertion_magnitude(
    pair: &mutations::MutationPair,
    route_input_diff: &route_diagnostics::RouteInputDiff,
) -> Option<EdgeInsertionMagnitude> {
    let mut semantic_added_edge_ids = if pair
        .expected_changes
        .contains(&mutations::SemanticChangeKind::EdgeAdded)
    {
        pair.direct_edges
            .iter()
            .map(|id| (*id).to_string())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    semantic_added_edge_ids.sort();

    let magnitude = EdgeInsertionMagnitude {
        semantic_added_edge_ids,
        route_input_added_edges: route_input_diff.added_edges.len(),
    };

    magnitude.is_non_zero().then_some(magnitude)
}

fn node_insertion_magnitude(
    pair: &mutations::MutationPair,
    rendered: &render_surfaces::RenderedPair,
) -> Option<NodeInsertionMagnitude> {
    if !pair
        .expected_changes
        .contains(&mutations::SemanticChangeKind::NodeAdded)
    {
        return None;
    }

    let before_node_ids = rendered
        .before
        .layout_mmds
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut added_node_ids = Vec::new();
    let mut bbox_area_added = 0.0;

    for node in &rendered.after.layout_mmds.nodes {
        if before_node_ids.contains(node.id.as_str()) {
            continue;
        }
        added_node_ids.push(node.id.clone());
        bbox_area_added += node.size.width * node.size.height;
    }
    added_node_ids.sort();

    let magnitude = NodeInsertionMagnitude {
        added_node_ids,
        bbox_area_added,
    };

    magnitude.is_non_zero().then_some(magnitude)
}

fn subgraph_change_magnitude(
    pair: &mutations::MutationPair,
    rendered: &render_surfaces::RenderedPair,
) -> Option<SubgraphChangeMagnitude> {
    if !pair.expected_changes.iter().any(|change| {
        matches!(
            change,
            mutations::SemanticChangeKind::SubgraphAdded
                | mutations::SemanticChangeKind::SubgraphMembershipChanged
                | mutations::SemanticChangeKind::SubgraphDirectionChanged
        )
    }) {
        return None;
    }

    let before = rendered
        .before
        .layout_mmds
        .subgraphs
        .iter()
        .map(|subgraph| (subgraph.id.as_str(), subgraph))
        .collect::<BTreeMap<_, _>>();
    let after = rendered
        .after
        .layout_mmds
        .subgraphs
        .iter()
        .map(|subgraph| (subgraph.id.as_str(), subgraph))
        .collect::<BTreeMap<_, _>>();
    let ids = before
        .keys()
        .chain(after.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut membership_delta_count = 0;
    let mut direction_changed = false;
    let mut scope_parent_id = None;

    for id in ids {
        let before_subgraph = before.get(id).copied();
        let after_subgraph = after.get(id).copied();
        let before_children = before_subgraph
            .map(|subgraph| sorted_strings(&subgraph.children))
            .unwrap_or_default();
        let after_children = after_subgraph
            .map(|subgraph| sorted_strings(&subgraph.children))
            .unwrap_or_default();
        if before_children != after_children {
            membership_delta_count += before_children.len().abs_diff(after_children.len()).max(1);
        }
        let before_direction = before_subgraph.and_then(|subgraph| subgraph.direction.as_ref());
        let after_direction = after_subgraph.and_then(|subgraph| subgraph.direction.as_ref());
        direction_changed |= before_direction != after_direction;
        if scope_parent_id.is_none() {
            scope_parent_id = after_subgraph
                .and_then(|subgraph| subgraph.parent.clone())
                .or_else(|| before_subgraph.and_then(|subgraph| subgraph.parent.clone()));
        }
    }

    let magnitude = SubgraphChangeMagnitude {
        membership_delta_count,
        direction_changed,
        scope_parent_id,
    };

    magnitude.is_non_zero().then_some(magnitude)
}

fn sorted_strings(values: &[String]) -> Vec<String> {
    let mut sorted = values.to_vec();
    sorted.sort();
    sorted
}
