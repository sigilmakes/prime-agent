//! Issue #271: evaluate whether direct singleton flowchart edges could honor
//! a logical port `fraction == 0.5` more faithfully in text/grid rendering
//! under a narrow safe predicate. Evidence-only; no production code is
//! changed by this module.
//!
//! Each shortlist fixture is solved at `GeometryLevel::Routed`, every graph
//! edge is classified against eleven gates, and survivors get a cross-axis
//! drift measurement (face-perpendicular axis is dropped). The aggregate
//! per-fixture report is checked into
//! `tests/snapshots/internal/singleton_centering_summary.txt` via the
//! project's `GENERATE_*_SNAPSHOTS=1` convention.

use std::collections::BTreeMap;

use crate::diagrams::flowchart::compile_to_graph;
use crate::engines::graph::algorithms::layered::layout_building::layered_config_for_layout;
use crate::engines::graph::contracts::{
    EngineConfig, GraphEngine, GraphGeometryContract, GraphSolveRequest, MeasurementMode,
};
use crate::engines::graph::flux::FluxLayeredEngine;
use crate::graph::attachment::{EdgePort, PortFace};
use crate::graph::geometry::{GraphGeometry, RoutedGraphGeometry};
use crate::graph::grid::{
    AttachDirection, AttachmentOverride, GridLayout, GridLayoutConfig, TextPathFamily,
    compute_attachment_plan, geometry_to_grid_layout_with_routed, route_edge_with_probe,
};
use crate::graph::{Arrow, Edge, GeometryLevel, Graph};
use crate::mermaid::parse_flowchart;

/// 16 fixtures spanning trivial chains, mixed direction, boundary cases, and
/// fan canaries that should reject 100% on `GroupSizeNonOne`.
const SHORTLIST: &[&str] = &[
    "simple.mmd",
    "chain.mmd",
    "simple_cycle.mmd",
    "mixed_shape_chain.mmd",
    "ci_pipeline.mmd",
    "callgraph_feedback_cycle.mmd",
    "left_right.mmd",
    "bottom_top.mmd",
    "direction_override.mmd",
    "subgraph_direction_lr.mmd",
    "labeled_edges.mmd",
    "edge_styles.mmd",
    "bidirectional.mmd",
    "decision.mmd",
    "multi_edge.mmd",
    "fan_in.mmd",
    "fan_out.mmd",
];

const FRACTION_EPS: f64 = 1e-6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum RejectGate {
    SelfEdge,
    MissingRoutedPort,
    SubgraphEndpoint,
    GroupSizeNonOne,
    PortFractionNotCentered,
    Backward,
    CorridorPreserved,
    MarkerOrLabelTerminalShift,
    TextProbeUnavailable,
    TextFaceMismatch,
    TextPathNotDirect,
}

impl RejectGate {
    fn as_str(&self) -> &'static str {
        match self {
            Self::SelfEdge => "SelfEdge",
            Self::MissingRoutedPort => "MissingRoutedPort",
            Self::SubgraphEndpoint => "SubgraphEndpoint",
            Self::GroupSizeNonOne => "GroupSizeNonOne",
            Self::PortFractionNotCentered => "PortFractionNotCentered",
            Self::Backward => "Backward",
            Self::CorridorPreserved => "CorridorPreserved",
            Self::MarkerOrLabelTerminalShift => "MarkerOrLabelTerminalShift",
            Self::TextProbeUnavailable => "TextProbeUnavailable",
            Self::TextFaceMismatch => "TextFaceMismatch",
            Self::TextPathNotDirect => "TextPathNotDirect",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Verdict {
    Survivor,
    Rejected(RejectGate),
}

#[derive(Debug, Clone)]
struct EdgeRow {
    #[allow(dead_code)]
    fixture: String,
    from: String,
    to: String,
    verdict: Verdict,
    /// Populated for survivors only (the only verdict that emits detail rows).
    /// For all rejected verdicts these fields are None.
    delta_cross_cells: Option<i64>,
    source_face: Option<String>,
    target_face: Option<String>,
    source_fraction: Option<f64>,
    target_fraction: Option<f64>,
}

#[derive(Debug, Clone)]
struct FixtureSummary {
    fixture: String,
    total: usize,
    survivors: usize,
    delta0: usize,
    delta1: usize,
    delta2plus: usize,
    rejections: BTreeMap<RejectGate, usize>,
    detail_rows: Vec<EdgeRow>,
}

struct RoutedHarnessContext {
    diagram: Graph,
    #[allow(dead_code)]
    geometry: GraphGeometry,
    routed: RoutedGraphGeometry,
    layout: GridLayout,
    attachment_plan: std::collections::HashMap<usize, AttachmentOverride>,
}

struct TextEndpointSummary {
    face: Option<String>,
    cell: Option<(usize, usize)>,
}

struct TextProbeResult {
    source: Option<TextEndpointSummary>,
    target: Option<TextEndpointSummary>,
    path_family: Option<TextPathFamily>,
}

fn fixture_path(fixture: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("flowchart")
        .join(fixture)
}

fn build_routed_harness_context(fixture: &str) -> RoutedHarnessContext {
    let path = fixture_path(fixture);
    let input =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let flowchart =
        parse_flowchart(&input).unwrap_or_else(|e| panic!("parse flowchart {fixture}: {e:?}"));
    let diagram = compile_to_graph(&flowchart);
    let grid_config = GridLayoutConfig::default();
    let engine = FluxLayeredEngine::text();
    let request = GraphSolveRequest::new(
        MeasurementMode::Grid,
        GraphGeometryContract::Canonical,
        GeometryLevel::Routed,
        None,
        Default::default(),
    );
    let result = engine
        .solve(
            &diagram,
            &EngineConfig::Layered(layered_config_for_layout(&diagram, &grid_config)),
            &request,
        )
        .unwrap_or_else(|e| panic!("routed solve {fixture}: {e:?}"));
    let routed = result
        .routed
        .unwrap_or_else(|| panic!("routed level should populate for {fixture}"));
    let layout = geometry_to_grid_layout_with_routed(
        &diagram,
        &result.geometry,
        Some(&routed),
        &grid_config,
    );
    let attachment_plan = compute_attachment_plan(&diagram.edges, &layout, diagram.direction);
    RoutedHarnessContext {
        diagram,
        geometry: result.geometry,
        routed,
        layout,
        attachment_plan,
    }
}

fn source_face_from_connection(connection: AttachDirection) -> &'static str {
    match connection {
        AttachDirection::Top => "bottom",
        AttachDirection::Bottom => "top",
        AttachDirection::Left => "right",
        AttachDirection::Right => "left",
    }
}

fn target_face_from_entry(entry: AttachDirection) -> &'static str {
    match entry {
        AttachDirection::Top => "top",
        AttachDirection::Bottom => "bottom",
        AttachDirection::Left => "left",
        AttachDirection::Right => "right",
    }
}

fn probe_text_edge(ctx: &RoutedHarnessContext, edge: &Edge) -> TextProbeResult {
    let edge_dir = ctx
        .layout
        .effective_edge_direction(&edge.from, &edge.to, ctx.diagram.direction);
    // Mirror the production `route_all_edges` flow: pull the per-edge
    // override from the attachment plan and pass it through. Without this
    // the probe takes a no-override fallback path and observes un-centered
    // cells for cross-direction-override-boundary edges, even though
    // production text rendering centers them correctly.
    let plan_entry = ctx.attachment_plan.get(&edge.index);
    let (src_override, tgt_override, src_first_vertical) = plan_entry
        .map(|ov| (ov.source, ov.target, ov.source_first_vertical))
        .unwrap_or((None, None, false));
    let Some(result) = route_edge_with_probe(
        edge,
        &ctx.layout,
        edge_dir,
        src_override,
        tgt_override,
        src_first_vertical,
    ) else {
        return TextProbeResult {
            source: None,
            target: None,
            path_family: None,
        };
    };
    TextProbeResult {
        source: Some(TextEndpointSummary {
            face: result
                .routed
                .source_connection
                .map(source_face_from_connection)
                .map(ToOwned::to_owned),
            cell: Some((result.routed.start.x, result.routed.start.y)),
        }),
        target: Some(TextEndpointSummary {
            face: Some(target_face_from_entry(result.routed.entry_direction).to_string()),
            cell: Some((result.routed.end.x, result.routed.end.y)),
        }),
        path_family: Some(result.probe.path_family),
    }
}

/// Gate verdict produced by `classify_edge`. Survivors carry the cross-axis
/// drift on each end; rejections carry only the firing gate.
struct ClassifyOutcome {
    verdict: Verdict,
    /// Populated only for survivors.
    delta_cross_cells: Option<i64>,
    source_face: Option<String>,
    target_face: Option<String>,
    source_fraction: Option<f64>,
    target_fraction: Option<f64>,
}

fn rejected(gate: RejectGate) -> ClassifyOutcome {
    ClassifyOutcome {
        verdict: Verdict::Rejected(gate),
        delta_cross_cells: None,
        source_face: None,
        target_face: None,
        source_fraction: None,
        target_fraction: None,
    }
}

fn classify_edge(ctx: &RoutedHarnessContext, edge: &Edge) -> ClassifyOutcome {
    // Gate 1: SelfEdge — short-circuit before any routed-edge lookup.
    if edge.from == edge.to {
        return rejected(RejectGate::SelfEdge);
    }

    // Pre-step: routed-edge lookup by index. RoutedGraphGeometry.edges is not
    // a Vec indexed by edge.index — self-edges are filtered and order is not
    // preserved. Use find().
    let routed_edge = ctx.routed.edges.iter().find(|e| e.index == edge.index);

    // Gate 2: MissingRoutedPort — covers (a) no routed entry, (b) source_port
    // None, (c) target_port None. All three must be guarded before any later
    // gate dereferences source_port/target_port.
    let routed = match routed_edge {
        Some(r) if r.source_port.is_some() && r.target_port.is_some() => r,
        _ => return rejected(RejectGate::MissingRoutedPort),
    };
    let source_port = routed.source_port.as_ref().unwrap();
    let target_port = routed.target_port.as_ref().unwrap();

    // Gate 3: SubgraphEndpoint — safe now that gate 2 confirmed the entry.
    if routed.from_subgraph.is_some() || routed.to_subgraph.is_some() {
        return rejected(RejectGate::SubgraphEndpoint);
    }

    // Gate 4: GroupSizeNonOne — canary for fan_in.mmd / fan_out.mmd.
    if source_port.group_size != 1 || target_port.group_size != 1 {
        return rejected(RejectGate::GroupSizeNonOne);
    }

    // Gate 5: PortFractionNotCentered — the whole experiment's selector.
    if (source_port.fraction - 0.5).abs() > FRACTION_EPS
        || (target_port.fraction - 0.5).abs() > FRACTION_EPS
    {
        return rejected(RejectGate::PortFractionNotCentered);
    }

    // Gate 6: Backward.
    if routed.is_backward {
        return rejected(RejectGate::Backward);
    }

    // Gate 7: CorridorPreserved — deliberate de-overlap corridor.
    if routed.preserve_orthogonal_topology {
        return rejected(RejectGate::CorridorPreserved);
    }

    // Gate 8: MarkerOrLabelTerminalShift — placed before the text-probe gates
    // so labeled edges are bucketed by their actual constraint rather than
    // re-bucketed downstream as TextPathNotDirect.
    let has_marker_or_label = edge.label.is_some()
        || !matches!(edge.arrow_start, Arrow::None)
        || routed.label_geometry.is_some();
    if has_marker_or_label {
        return rejected(RejectGate::MarkerOrLabelTerminalShift);
    }

    // Probe text. Must run before gates 9-11.
    let text = probe_text_edge(ctx, edge);

    // Gate 9: TextProbeUnavailable — must precede TextPathNotDirect so a
    // None path_family lands here, not in TextPathNotDirect.
    if text.path_family.is_none() || text.source.is_none() || text.target.is_none() {
        return rejected(RejectGate::TextProbeUnavailable);
    }
    let text_source = text.source.as_ref().unwrap();
    let text_target = text.target.as_ref().unwrap();

    // Gate 10: TextFaceMismatch — routed face must agree with observed text
    // face on the canonical "top"/"bottom"/"left"/"right" form.
    if text_source.face.as_deref() != Some(source_port.face.as_str())
        || text_target.face.as_deref() != Some(target_port.face.as_str())
    {
        return rejected(RejectGate::TextFaceMismatch);
    }

    // Gate 11: TextPathNotDirect — only fires when probe succeeded but chose
    // a non-direct family (Some(non_direct)).
    if !matches!(text.path_family, Some(TextPathFamily::Direct)) {
        return rejected(RejectGate::TextPathNotDirect);
    }

    // Survivor — compute cross-axis delta on each end, take max abs.
    let source_delta = cross_axis_delta(
        ctx,
        &edge.from,
        source_port,
        text_source.cell.expect("survivor has source cell"),
    );
    let target_delta = cross_axis_delta(
        ctx,
        &edge.to,
        target_port,
        text_target.cell.expect("survivor has target cell"),
    );
    let delta = source_delta.abs().max(target_delta.abs());

    ClassifyOutcome {
        verdict: Verdict::Survivor,
        delta_cross_cells: Some(delta),
        source_face: Some(source_port.face.as_str().to_string()),
        target_face: Some(target_port.face.as_str().to_string()),
        source_fraction: Some(source_port.fraction),
        target_fraction: Some(target_port.fraction),
    }
}

/// Cross-axis drift in integer grid cells between the actual text endpoint
/// and the rect's geometric center. Face-perpendicular axis is dropped —
/// `offset_for_face` only affects that axis, so the router's one-cell
/// offset cancels out.
///
/// Expected center is `NodeBounds::center_x()` / `center_y()` (the rect's
/// geometric midpoint). Note that production's `point_on_face_grid` lays
/// out singleton overrides at the midpoint of `face_extent` (which excludes
/// the corner cells); for even-width / even-height rects that midpoint is
/// 1 cell off `bounds.center_x()` / `center_y()`. So a fully-fixed
/// even-width override-boundary edge can show `delta_cross == 1`, and that
/// is visually correct — the residual is the fundamental representation
/// gap between a 10-wide rect's geometric center (5.0) and integer cells.
fn cross_axis_delta(
    ctx: &RoutedHarnessContext,
    node_id: &str,
    port: &EdgePort,
    actual_cell: (usize, usize),
) -> i64 {
    let bounds = ctx
        .layout
        .get_bounds(node_id)
        .unwrap_or_else(|| panic!("routed node {node_id} should be present in grid layout"));
    match port.face {
        PortFace::Top | PortFace::Bottom => bounds.center_x() as i64 - actual_cell.0 as i64,
        PortFace::Left | PortFace::Right => bounds.center_y() as i64 - actual_cell.1 as i64,
    }
}

fn observe_singleton_centering_report() -> Vec<FixtureSummary> {
    SHORTLIST
        .iter()
        .map(|fixture| observe_fixture(fixture))
        .collect()
}

fn observe_fixture(fixture: &str) -> FixtureSummary {
    let ctx = build_routed_harness_context(fixture);
    let mut rows: Vec<EdgeRow> = ctx
        .diagram
        .edges
        .iter()
        .map(|edge| {
            let outcome = classify_edge(&ctx, edge);
            EdgeRow {
                fixture: fixture.to_string(),
                from: edge.from.clone(),
                to: edge.to.clone(),
                verdict: outcome.verdict,
                delta_cross_cells: outcome.delta_cross_cells,
                source_face: outcome.source_face,
                target_face: outcome.target_face,
                source_fraction: outcome.source_fraction,
                target_fraction: outcome.target_fraction,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));

    let total = rows.len();
    let mut survivors = 0usize;
    let mut delta0 = 0usize;
    let mut delta1 = 0usize;
    let mut delta2plus = 0usize;
    let mut rejections: BTreeMap<RejectGate, usize> = BTreeMap::new();
    let mut detail_rows = Vec::new();

    for row in &rows {
        match row.verdict {
            Verdict::Survivor => {
                survivors += 1;
                let abs = row.delta_cross_cells.unwrap_or(0).unsigned_abs();
                match abs {
                    0 => delta0 += 1,
                    1 => delta1 += 1,
                    _ => delta2plus += 1,
                }
                detail_rows.push(row.clone());
            }
            Verdict::Rejected(gate) => {
                *rejections.entry(gate).or_insert(0) += 1;
            }
        }
    }

    FixtureSummary {
        fixture: fixture.to_string(),
        total,
        survivors,
        delta0,
        delta1,
        delta2plus,
        rejections,
        detail_rows,
    }
}

fn render_summary_report(summaries: &[FixtureSummary]) -> String {
    let mut out = String::new();
    out.push_str(
        "# Singleton port-fraction centering report (issue #271)\n\
         #\n\
         # Each fixture: total / survivors / delta histogram / rejected histogram.\n\
         # 'delta_cross' is integer-cell drift on the face-perpendicular-dropped axis\n\
         # between the routed text endpoint and the rect-midpoint a fraction==0.5 port\n\
         # would project to. Detail rows: survivors only.\n\
         #\n\
         # Reviewer flags (per plan risks):\n\
         # - MarkerOrLabelTerminalShift dominating labeled fixtures => gate too coarse.\n\
         # - TextProbeUnavailable >10% of total => probe issues, not the predicate.\n\
         # - TextFaceMismatch firing => routed/observed face disagreement, log on #270.\n\
         #\n\n",
    );

    let mut totals_edges = 0usize;
    let mut totals_survivors = 0usize;
    let mut totals_delta0 = 0usize;
    let mut totals_delta1 = 0usize;
    let mut totals_delta2plus = 0usize;
    let mut totals_rejections: BTreeMap<RejectGate, usize> = BTreeMap::new();

    let mut sorted: Vec<&FixtureSummary> = summaries.iter().collect();
    sorted.sort_by(|a, b| a.fixture.cmp(&b.fixture));

    for s in sorted {
        out.push_str(&format!("== {} ==\n", s.fixture));
        out.push_str(&format!(
            "  edges: {}   survivors: {}   delta0: {}  delta1: {}  delta2+: {}   rejected: {}\n",
            s.total,
            s.survivors,
            s.delta0,
            s.delta1,
            s.delta2plus,
            format_rejections(&s.rejections),
        ));
        if s.detail_rows.is_empty() {
            out.push_str("  (no survivors)\n");
        } else {
            out.push_str("  survivors:\n");
            for row in &s.detail_rows {
                out.push_str(&format!(
                    "    {} -> {}    delta_cross={}  faces=({},{})  fractions=({:.3},{:.3})\n",
                    row.from,
                    row.to,
                    row.delta_cross_cells.unwrap_or(0),
                    row.source_face.as_deref().unwrap_or("?"),
                    row.target_face.as_deref().unwrap_or("?"),
                    row.source_fraction.unwrap_or(f64::NAN),
                    row.target_fraction.unwrap_or(f64::NAN),
                ));
            }
        }

        totals_edges += s.total;
        totals_survivors += s.survivors;
        totals_delta0 += s.delta0;
        totals_delta1 += s.delta1;
        totals_delta2plus += s.delta2plus;
        for (gate, count) in &s.rejections {
            *totals_rejections.entry(*gate).or_insert(0) += count;
        }
    }

    out.push_str(&format!(
        "\nTOTALS: edges={} survivors={} delta0={} delta1={} delta2+={} rejected={}\n",
        totals_edges,
        totals_survivors,
        totals_delta0,
        totals_delta1,
        totals_delta2plus,
        format_rejections(&totals_rejections),
    ));

    out
}

fn format_rejections(rejections: &BTreeMap<RejectGate, usize>) -> String {
    if rejections.is_empty() {
        return "{}".to_string();
    }
    let inner = rejections
        .iter()
        .map(|(gate, count)| format!("{}: {}", gate.as_str(), count))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{{}}}", inner)
}

fn singleton_snapshot_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join("internal")
        .join("singleton_centering_summary.txt")
}

#[test]
fn singleton_centering_report_matches_snapshot() {
    let summaries = observe_singleton_centering_report();
    let actual = render_summary_report(&summaries);
    let path = singleton_snapshot_path();
    if std::env::var("GENERATE_SINGLETON_CENTERING_SNAPSHOT").is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create snapshot dir");
        }
        std::fs::write(&path, &actual).expect("write snapshot");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing snapshot at {}: {e}. Run with GENERATE_SINGLETON_CENTERING_SNAPSHOT=1 to create it.",
            path.display()
        )
    });
    assert_eq!(actual, expected, "singleton centering summary mismatch");
}

#[test]
fn singleton_centering_fan_canaries_fully_rejected_by_group_size() {
    let summaries = observe_singleton_centering_report();
    for fixture in ["fan_in.mmd", "fan_out.mmd"] {
        let s = summaries
            .iter()
            .find(|s| s.fixture == fixture)
            .unwrap_or_else(|| panic!("fixture {fixture} should be in shortlist"));
        assert_eq!(
            s.survivors, 0,
            "{fixture} should have zero survivors; got {} of {}",
            s.survivors, s.total
        );
        let g = s
            .rejections
            .get(&RejectGate::GroupSizeNonOne)
            .copied()
            .unwrap_or(0);
        assert_eq!(
            g, s.total,
            "{fixture}: GroupSizeNonOne should account for every edge ({}); got {}",
            s.total, g
        );
    }
}

/// Regression guard for #275. Cross-direction-override-boundary singleton
/// edges previously rendered with `delta_cross == 3` (off-center by 3
/// cells in the visible text grid). After the targeted attachment-plan
/// fix they render at or within 1 cell of the rect geometric center;
/// the remaining 1-cell residual on even-width rects is the fundamental
/// integer-grid representation gap (a w=10 rect's center sits between
/// cells 5 and 6) and is visually correct.
#[test]
fn override_boundary_singleton_endpoints_are_centered() {
    const MAX_DELTA: i64 = 1;
    let cases: &[(&str, &str, &str)] = &[
        ("subgraph_direction_lr.mmd", "Start", "A"),
        ("subgraph_direction_lr.mmd", "C", "End"),
        ("direction_override.mmd", "Start", "A"),
        ("direction_override.mmd", "C", "End"),
    ];
    let summaries = observe_singleton_centering_report();
    let mut failures = Vec::new();
    for (fixture, from, to) in cases {
        let summary = summaries
            .iter()
            .find(|s| s.fixture == *fixture)
            .unwrap_or_else(|| panic!("{fixture} should be in shortlist"));
        let row = summary
            .detail_rows
            .iter()
            .find(|r| r.from == *from && r.to == *to)
            .unwrap_or_else(|| {
                panic!("{fixture}: {from} -> {to} should be a survivor with detail row")
            });
        let delta = row
            .delta_cross_cells
            .expect("survivor row should carry delta");
        if delta.abs() > MAX_DELTA {
            failures.push(format!("{fixture}: {from} -> {to} delta_cross={delta}"));
        }
    }
    assert!(
        failures.is_empty(),
        "expected |delta_cross| <= {MAX_DELTA} on all four override-boundary singletons; got:\n  {}",
        failures.join("\n  ")
    );
}
