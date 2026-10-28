use std::collections::BTreeSet;
use std::fmt;

use super::mmds_metrics::{self, LayoutMmdsMetrics, QualityOracleMetrics, RoutedMmdsMetrics};
use super::mutations::{DiagramFamily, MutationPair, SemanticChangeKind};
use super::phase_attribution::{self, PhaseAttribution};
use super::render_metrics::{self, RenderChurnClass, RenderMetricDelta};
use super::render_surfaces;

#[derive(Debug)]
pub(crate) struct MutationStabilityReport {
    pub(crate) pair_id: &'static str,
    pub(crate) expected_changes: Vec<SemanticChangeKind>,
    pub(crate) layout: LayoutMmdsMetrics,
    pub(crate) routed: RoutedMmdsMetrics,
    pub(crate) quality: QualityOracleMetrics,
    pub(crate) phase_attribution: PhaseAttribution,
    pub(crate) render_churn: RenderChurnClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChurnExplanation {
    pub(crate) components: Vec<ChurnComponent>,
}

impl ChurnExplanation {
    pub(crate) fn components_sorted_for_test(&self) -> Vec<ChurnComponent> {
        let mut components = self.components.clone();
        components.sort();
        components.dedup();
        components
    }

    pub(crate) fn is_style_only_without_route_geometry(&self) -> bool {
        self.components == [ChurnComponent::Style]
    }

    pub(crate) fn is_label_only_without_route_geometry(&self) -> bool {
        self.has_component(ChurnComponent::LabelRect)
            && !self.has_component(ChurnComponent::RouteGeometry)
            && !self.has_component(ChurnComponent::PortDivergence)
            && !self.has_component(ChurnComponent::Style)
    }

    pub(crate) fn has_route_geometry_cascade(&self) -> bool {
        self.has_component(ChurnComponent::LabelRect)
            && self.has_component(ChurnComponent::RouteGeometry)
            && self.has_component(ChurnComponent::PortDivergence)
    }

    fn has_component(&self, component: ChurnComponent) -> bool {
        self.components.contains(&component)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ChurnComponent {
    Style,
    LabelRect,
    Bounds,
    RouteGeometry,
    PortDivergence,
}

impl MutationStabilityReport {
    pub(crate) fn phase_attribution(&self) -> &PhaseAttribution {
        &self.phase_attribution
    }

    pub(crate) fn churn_explanation(&self) -> ChurnExplanation {
        let mut components = Vec::new();
        if self.render_churn == RenderChurnClass::StyleOnly {
            components.push(ChurnComponent::Style);
        }
        if !self.routed.label_rect_changes.is_empty() {
            components.push(ChurnComponent::LabelRect);
        }
        if self.layout.bounds_delta.changed || self.routed.routed_bounds_delta.changed {
            components.push(ChurnComponent::Bounds);
        }
        if self.routed.changed_path_geometry_count() > 0 {
            components.push(ChurnComponent::RouteGeometry);
        }
        if !self.routed.path_port_divergence.is_empty() {
            components.push(ChurnComponent::PortDivergence);
        }
        components.sort();
        components.dedup();

        ChurnExplanation { components }
    }

    pub(crate) fn summary(&self) -> String {
        let explanation = self.churn_explanation();
        let first_divergence = self.phase_attribution.first_divergence();
        let phase_components = self.phase_attribution.phase_components();
        format!(
            "{}: changes={:?}; layout_nodes +{}/-{}; sizes={}; layout_bounds={}; routed_bounds={}; subgraphs={}; label_sides={}; routed_edges +{}/-{}; paths_compared={}; paths_changed={}; labels={}; port_divergence={}; route_len_delta={:.2}; bend_delta={}; components={:?}; churn={:?}; first_divergence={:?}; phase_components={:?}; route_only_paths={}; layout_amplified_paths={}",
            self.pair_id,
            self.expected_changes,
            self.layout.added_nodes.len(),
            self.layout.removed_nodes.len(),
            self.layout.node_size_changes.len(),
            self.layout.bounds_delta.changed,
            self.routed.routed_bounds_delta.changed,
            self.layout.subgraph_membership_changes.len(),
            self.layout.label_side_changes.len(),
            self.routed.added_edges.len(),
            self.routed.removed_edges.len(),
            self.routed.path_metrics.len(),
            self.routed.changed_path_geometry_count(),
            self.routed.label_rect_changes.len(),
            self.routed.path_port_divergence.len(),
            self.quality.route_length_total_delta,
            self.quality.bend_count_total_delta,
            explanation.components,
            self.render_churn,
            first_divergence,
            phase_components,
            self.phase_attribution.route.route_only_paths.len(),
            self.phase_attribution.route.layout_amplified_paths.len()
        )
    }
}

#[derive(Debug)]
pub(crate) enum ReportError {
    Render {
        pair_id: &'static str,
        message: String,
    },
    DirectImpact {
        pair_id: &'static str,
        subject_kind: &'static str,
        subject_id: String,
    },
    ExcludedFamily {
        pair_id: &'static str,
        family: DiagramFamily,
    },
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReportError::Render { pair_id, message } => {
                write!(f, "failed to render report for {pair_id}: {message}")
            }
            ReportError::DirectImpact {
                pair_id,
                subject_kind,
                subject_id,
            } => write!(
                f,
                "direct-impact {subject_kind} {subject_id:?} for {pair_id} was not found in before or after MMDS"
            ),
            ReportError::ExcludedFamily { pair_id, family } => {
                write!(
                    f,
                    "{pair_id} is excluded from graph-family reports: {family:?}"
                )
            }
        }
    }
}

pub(crate) fn run_pair(
    pair: &'static MutationPair,
) -> Result<MutationStabilityReport, ReportError> {
    if pair.family == DiagramFamily::TimelineExcluded || !pair.include_in_graph_stability_metrics {
        return Err(ReportError::ExcludedFamily {
            pair_id: pair.id,
            family: pair.family,
        });
    }

    let rendered = render_surfaces::render_pair(pair).map_err(|error| ReportError::Render {
        pair_id: pair.id,
        message: error.to_string(),
    })?;
    validate_direct_impacts(pair, &rendered)?;

    let layout = mmds_metrics::compare_layout_mmds(
        pair,
        &rendered.before.layout_mmds,
        &rendered.after.layout_mmds,
    );
    let routed = mmds_metrics::compare_routed_mmds(
        pair,
        &rendered.before.routed_mmds,
        &rendered.after.routed_mmds,
    );
    let quality = mmds_metrics::collect_quality_oracle_metrics(
        pair,
        &rendered.before.routed_mmds,
        &rendered.after.routed_mmds,
    );
    let mut phase_attribution = phase_attribution::compare_phase_traces(
        &rendered.before.phase_trace,
        &rendered.after.phase_trace,
    );
    phase_attribution::attribute_routed_metrics(&mut phase_attribution, &routed);
    let render_delta = render_delta(&rendered, &routed);
    let render_churn = render_metrics::classify_render_churn(&render_delta);

    Ok(MutationStabilityReport {
        pair_id: pair.id,
        expected_changes: pair.expected_changes.to_vec(),
        layout,
        routed,
        quality,
        phase_attribution,
        render_churn,
    })
}

pub(crate) fn run_corpus(
    pairs: &'static [MutationPair],
) -> Result<Vec<MutationStabilityReport>, ReportError> {
    let mut reports = pairs
        .iter()
        .map(run_pair)
        .collect::<Result<Vec<_>, ReportError>>()?;
    reports.sort_by_key(|report| report.pair_id);
    Ok(reports)
}

pub(crate) fn format_reports(reports: &[MutationStabilityReport]) -> String {
    let mut summaries = reports
        .iter()
        .map(MutationStabilityReport::summary)
        .collect::<Vec<_>>();
    summaries.sort();
    summaries.join("\n")
}

fn validate_direct_impacts(
    pair: &MutationPair,
    rendered: &render_surfaces::RenderedPair,
) -> Result<(), ReportError> {
    let node_ids = rendered
        .before
        .layout_mmds
        .nodes
        .iter()
        .chain(&rendered.after.layout_mmds.nodes)
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    let edge_ids = rendered
        .before
        .routed_mmds
        .edges
        .iter()
        .chain(&rendered.after.routed_mmds.edges)
        .map(|edge| edge.id.as_str())
        .collect::<BTreeSet<_>>();

    for id in pair.direct_nodes {
        if !node_ids.contains(id) {
            return Err(ReportError::DirectImpact {
                pair_id: pair.id,
                subject_kind: "node",
                subject_id: (*id).to_string(),
            });
        }
    }
    for id in pair.direct_edges {
        if !edge_ids.contains(id) {
            return Err(ReportError::DirectImpact {
                pair_id: pair.id,
                subject_kind: "edge",
                subject_id: (*id).to_string(),
            });
        }
    }

    Ok(())
}

fn render_delta(
    rendered: &render_surfaces::RenderedPair,
    routed: &RoutedMmdsMetrics,
) -> RenderMetricDelta {
    let before_text = render_metrics::collect_text_metrics(&rendered.before.text);
    let after_text = render_metrics::collect_text_metrics(&rendered.after.text);
    let before_svg = render_metrics::collect_svg_metrics(&rendered.before.svg);
    let after_svg = render_metrics::collect_svg_metrics(&rendered.after.svg);

    RenderMetricDelta {
        svg_viewbox_changed: before_svg.viewbox_width != after_svg.viewbox_width
            || before_svg.viewbox_height != after_svg.viewbox_height,
        svg_path_count_changed: before_svg.path_count != after_svg.path_count,
        path_topology_changed: routed
            .path_metrics
            .iter()
            .any(|metric| metric.point_count_delta != 0 || metric.bend_count_delta != 0),
        label_rect_changed: !routed.label_rect_changes.is_empty(),
        endpoint_face_changed: !routed.endpoint_face_changes.is_empty(),
        style_only_changed: rendered
            .after
            .routed_mmds
            .edges
            .iter()
            .zip(&rendered.before.routed_mmds.edges)
            .any(|(after, before)| {
                after.stroke != before.stroke
                    || after.arrow_start != before.arrow_start
                    || after.arrow_end != before.arrow_end
            }),
        text_dimensions_changed: before_text != after_text,
    }
}

#[test]
#[ignore]
fn print_tier_a_stability_report() {
    let reports = run_corpus(super::mutations::tier_a_pairs()).unwrap();
    println!("{}", format_reports(&reports));
}
