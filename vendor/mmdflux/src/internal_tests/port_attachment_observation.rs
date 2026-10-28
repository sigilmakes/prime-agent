use std::fs;
use std::path::Path;

use crate::diagrams::flowchart::compile_to_graph;
use crate::engines::graph::algorithms::layered::layout_building::layered_config_for_layout;
use crate::engines::graph::contracts::{
    EngineConfig, GraphEngine, GraphGeometryContract, GraphSolveRequest, MeasurementMode,
};
use crate::engines::graph::flux::FluxLayeredEngine;
use crate::graph::grid::{
    AttachDirection, GridLayout, GridLayoutConfig, TextPathFamily,
    geometry_to_grid_layout_with_routed, route_edge_with_probe,
};
use crate::graph::{Edge, GeometryLevel, Graph};
use crate::mermaid::parse_flowchart;
use crate::mmds::{Document, Edge as MmdsEdge, Node as MmdsNode, Port};
use crate::{OutputFormat, RenderConfig};

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct PortObservation {
    fixture: String,
    from: String,
    to: String,
    source_port: EndpointObservation,
    target_port: EndpointObservation,
    path_source: EndpointObservation,
    path_target: EndpointObservation,
    svg_source: Option<EndpointObservation>,
    svg_target: Option<EndpointObservation>,
    svg_rejection_reason: Option<String>,
    text_source: Option<TextEndpointObservation>,
    text_target: Option<TextEndpointObservation>,
    text_path_family: Option<TextPathFamily>,
    text_rejection_reason: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
struct EndpointObservation {
    face: Option<String>,
    fraction: Option<f64>,
    position: Option<(f64, f64)>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct TextEndpointObservation {
    face: Option<String>,
    cell: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
struct ObservationMatrix {
    observations: Vec<PortObservation>,
}

#[derive(Debug, Clone, Copy)]
struct NodeRect {
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
}

#[test]
fn port_attachment_observation_reports_simple_td_edge() {
    let obs = observe_fixture_edge("subgraph_direction_lr.mmd", "Start", "A");

    assert_eq!(obs.source_port.face.as_deref(), Some("bottom"));
    assert_eq!(obs.target_port.face.as_deref(), Some("top"));
    assert_eq!(obs.path_source.face.as_deref(), Some("bottom"));
    assert_eq!(obs.path_target.face.as_deref(), Some("top"));
    assert_fraction_near(obs.source_port.fraction, 0.5, "source_port.fraction");
    assert_fraction_near(obs.target_port.fraction, 0.5, "target_port.fraction");
}

#[test]
fn port_attachment_observation_probe_covers_required_direction_override_fixtures() {
    let matrix = observe_direction_override_probe_matrix();

    assert!(matrix.contains_edge("subgraph_direction_lr.mmd", "C", "End"));
    assert!(matrix.contains_edge("direction_override.mmd", "C", "End"));
    assert!(matrix.contains_edge("subgraph_direction_cross_boundary.mmd", "B", "F"));
    assert!(matrix.contains_edge("subgraph_direction_cross_boundary.mmd", "B", "D"));
    assert!(matrix.contains_edge("subgraph_direction_nested_both.mmd", "A", "B"));
    assert!(matrix.contains_edge("subgraph_direction_nested_both.mmd", "C", "A"));
}

#[test]
fn port_attachment_observation_internal_override_edges_keep_lr_ports() {
    let matrix = observe_direction_override_probe_matrix();

    for (from, to) in [("A", "B"), ("B", "C")] {
        let obs = matrix
            .edge("subgraph_direction_lr.mmd", from, to)
            .unwrap_or_else(|| panic!("missing probe observation for {from} -> {to}"));
        assert_eq!(obs.source_port.face.as_deref(), Some("right"));
        assert_eq!(obs.target_port.face.as_deref(), Some("left"));
        assert_visible_shape_preserved(obs);
    }
}

#[test]
fn port_attachment_observation_visible_shape_guard_accepts_probe_matrix() {
    let matrix = observe_direction_override_probe_matrix();

    for obs in &matrix.observations {
        assert_visible_shape_preserved(obs);
    }
}

#[test]
fn mmds_logical_ports_use_root_direction_for_subgraph_direction_lr_exit() {
    let obs = observe_fixture_edge("subgraph_direction_lr.mmd", "C", "End");

    assert_eq!(obs.source_port.face.as_deref(), Some("bottom"));
    assert_eq!(obs.target_port.face.as_deref(), Some("top"));
    assert_eq!(obs.path_source.face.as_deref(), Some("bottom"));
    assert_eq!(obs.path_target.face.as_deref(), Some("top"));

    assert_visible_shape_preserved(&obs);
}

#[test]
fn mmds_logical_ports_use_root_direction_for_cross_boundary_override_exits() {
    for (from, to) in [("B", "F"), ("B", "D")] {
        let obs = observe_fixture_edge("subgraph_direction_cross_boundary.mmd", from, to);

        assert_eq!(
            obs.source_port.face.as_deref(),
            Some("bottom"),
            "{from} -> {to} source logical port should use root TD exit face"
        );
        assert_eq!(
            obs.target_port.face.as_deref(),
            Some("top"),
            "{from} -> {to} target logical port should use root TD entry face"
        );
        assert_eq!(obs.path_source.face.as_deref(), Some("bottom"));
        assert_eq!(obs.path_target.face.as_deref(), Some("top"));

        assert_visible_shape_preserved(&obs);
    }
}

#[test]
fn nested_override_probe_classifies_confirmed_edges_without_mandating_a_to_b_fix() {
    let c_to_a = observe_fixture_edge("subgraph_direction_nested_both.mmd", "C", "A");
    assert_eq!(c_to_a.source_port.face.as_deref(), Some("right"));
    assert_eq!(c_to_a.target_port.face.as_deref(), Some("left"));
    assert_eq!(c_to_a.path_source.face.as_deref(), Some("right"));
    assert_eq!(c_to_a.path_target.face.as_deref(), Some("left"));

    let d_to_c = observe_fixture_edge("subgraph_direction_nested_both.mmd", "D", "C");
    assert_eq!(d_to_c.source_port.face.as_deref(), Some("bottom"));
    assert_eq!(d_to_c.target_port.face.as_deref(), Some("top"));

    let a_to_b = observe_fixture_edge("subgraph_direction_nested_both.mmd", "A", "B");
    assert_nested_known_unknown("A -> B", &a_to_b);
}

#[test]
fn mmds_logical_port_fix_preserves_target_visible_geometry() {
    for (fixture, from, to) in [
        ("subgraph_direction_lr.mmd", "C", "End"),
        ("subgraph_direction_cross_boundary.mmd", "B", "F"),
        ("subgraph_direction_cross_boundary.mmd", "B", "D"),
    ] {
        let obs = observe_fixture_edge(fixture, from, to);

        assert_eq!(
            obs.path_source.face.as_deref(),
            Some("bottom"),
            "{fixture} {from} -> {to} should keep bottom-side visible source path"
        );
        assert_eq!(
            obs.path_target.face.as_deref(),
            Some("top"),
            "{fixture} {from} -> {to} should keep top-side visible target path"
        );
        assert_visible_shape_preserved(&obs);
    }
}

const DIRECTION_OVERRIDE_PROBE_EDGES: &[(&str, &[(&str, &str)])] = &[
    (
        "subgraph_direction_lr.mmd",
        &[("Start", "A"), ("A", "B"), ("B", "C"), ("C", "End")],
    ),
    ("direction_override.mmd", &[("Start", "A"), ("C", "End")]),
    (
        "subgraph_direction_cross_boundary.mmd",
        &[("E", "A"), ("C", "A"), ("B", "F"), ("B", "D")],
    ),
    (
        "subgraph_direction_nested_both.mmd",
        &[("A", "B"), ("C", "A"), ("D", "C")],
    ),
];

impl ObservationMatrix {
    fn contains_edge(&self, fixture: &str, from: &str, to: &str) -> bool {
        self.edge(fixture, from, to).is_some()
    }

    fn edge(&self, fixture: &str, from: &str, to: &str) -> Option<&PortObservation> {
        self.observations
            .iter()
            .find(|obs| obs.fixture == fixture && obs.from == from && obs.to == to)
    }
}

fn observe_direction_override_probe_matrix() -> ObservationMatrix {
    let observations = DIRECTION_OVERRIDE_PROBE_EDGES
        .iter()
        .flat_map(|(fixture, edges)| observe_fixture_edges(fixture, edges))
        .collect();
    ObservationMatrix { observations }
}

fn observe_fixture_edge(fixture: &str, from: &str, to: &str) -> PortObservation {
    observe_fixture_edges(fixture, &[(from, to)])
        .into_iter()
        .next()
        .expect("single-edge fixture observation should be present")
}

fn observe_fixture_edges(fixture: &str, edges: &[(&str, &str)]) -> Vec<PortObservation> {
    let input = load_flowchart_fixture(fixture);
    let output = render_routed_mmds(&input);
    let text_context = build_text_context(&input);

    edges
        .iter()
        .map(|(from, to)| observe_output_edge(fixture, &output, &text_context, from, to))
        .collect()
}

fn observe_output_edge(
    fixture: &str,
    output: &Document,
    text_context: &TextContext,
    from: &str,
    to: &str,
) -> PortObservation {
    let edge = find_edge(output, from, to);
    let source_node = find_node(output, from);
    let target_node = find_node(output, to);
    let text = observe_text_edge(text_context, from, to);

    let path = edge
        .path
        .as_ref()
        .unwrap_or_else(|| panic!("edge {from} -> {to} should include routed path points"));
    let path_source = path
        .first()
        .unwrap_or_else(|| panic!("edge {from} -> {to} should include a path source point"));
    let path_target = path
        .last()
        .unwrap_or_else(|| panic!("edge {from} -> {to} should include a path target point"));

    PortObservation {
        fixture: fixture.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        source_port: edge
            .source_port
            .as_ref()
            .map(port_observation)
            .unwrap_or_default(),
        target_port: edge
            .target_port
            .as_ref()
            .map(port_observation)
            .unwrap_or_default(),
        path_source: infer_endpoint_observation((path_source[0], path_source[1]), source_node),
        path_target: infer_endpoint_observation((path_target[0], path_target[1]), target_node),
        svg_source: None,
        svg_target: None,
        svg_rejection_reason: Some(
            "SVG endpoint observation unavailable: probe has no stable SVG edge-to-path mapping"
                .to_string(),
        ),
        text_source: text.source,
        text_target: text.target,
        text_path_family: text.path_family,
        text_rejection_reason: text.rejection_reason,
    }
}

fn load_flowchart_fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("flowchart")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read fixture {}: {err}", path.display()))
}

fn render_routed_mmds(input: &str) -> Document {
    let config = RenderConfig {
        geometry_level: GeometryLevel::Routed,
        ..RenderConfig::default()
    };
    let json = crate::render_diagram(input, OutputFormat::Mmds, &config)
        .expect("routed MMDS render should succeed");
    serde_json::from_str(&json).expect("routed MMDS output should deserialize")
}

struct TextContext {
    diagram: Graph,
    layout: GridLayout,
}

#[derive(Debug, Clone)]
struct TextObservationResult {
    source: Option<TextEndpointObservation>,
    target: Option<TextEndpointObservation>,
    path_family: Option<TextPathFamily>,
    rejection_reason: Option<String>,
}

fn build_text_context(input: &str) -> TextContext {
    let flowchart = parse_flowchart(input).expect("flowchart fixture should parse");
    let diagram = compile_to_graph(&flowchart);
    let grid_config = GridLayoutConfig::default();
    let engine = FluxLayeredEngine::text();
    let request = GraphSolveRequest::new(
        MeasurementMode::Grid,
        GraphGeometryContract::Canonical,
        GeometryLevel::Layout,
        None,
        Default::default(),
    );
    let result = engine
        .solve(
            &diagram,
            &EngineConfig::Layered(layered_config_for_layout(&diagram, &grid_config)),
            &request,
        )
        .expect("text observation layout solve should succeed");
    let layout = geometry_to_grid_layout_with_routed(
        &diagram,
        &result.geometry,
        result.routed.as_ref(),
        &grid_config,
    );

    TextContext { diagram, layout }
}

fn observe_text_edge(context: &TextContext, from: &str, to: &str) -> TextObservationResult {
    let edge = find_graph_edge(&context.diagram, from, to);
    let Some(result) = route_edge_with_probe(
        edge,
        &context.layout,
        context.diagram.direction,
        None,
        None,
        false,
    ) else {
        return TextObservationResult {
            source: None,
            target: None,
            path_family: None,
            rejection_reason: Some("route_edge_with_probe returned None".to_string()),
        };
    };

    TextObservationResult {
        source: Some(TextEndpointObservation {
            face: result
                .routed
                .source_connection
                .map(source_face_from_connection)
                .map(ToOwned::to_owned),
            cell: Some((result.routed.start.x, result.routed.start.y)),
        }),
        target: Some(TextEndpointObservation {
            face: Some(target_face_from_entry(result.routed.entry_direction).to_string()),
            cell: Some((result.routed.end.x, result.routed.end.y)),
        }),
        path_family: Some(result.probe.path_family),
        rejection_reason: result
            .probe
            .rejection_reason
            .map(|reason| format!("{reason:?}")),
    }
}

fn find_graph_edge<'a>(diagram: &'a Graph, from: &str, to: &str) -> &'a Edge {
    let mut matches = diagram
        .edges
        .iter()
        .filter(|edge| edge.from == from && edge.to == to);
    let edge = matches
        .next()
        .unwrap_or_else(|| panic!("missing graph edge {from} -> {to}"));
    assert!(
        matches.next().is_none(),
        "duplicate graph edges found for {from} -> {to}; add an edge-indexed observation helper"
    );
    edge
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

fn find_edge<'a>(output: &'a Document, from: &str, to: &str) -> &'a MmdsEdge {
    let mut matches = output
        .edges
        .iter()
        .filter(|edge| edge.source == from && edge.target == to);
    let edge = matches
        .next()
        .unwrap_or_else(|| panic!("missing edge {from} -> {to}"));
    assert!(
        matches.next().is_none(),
        "duplicate edges found for {from} -> {to}; add an edge-indexed observation helper"
    );
    edge
}

fn find_node<'a>(output: &'a Document, id: &str) -> &'a MmdsNode {
    output
        .nodes
        .iter()
        .find(|node| node.id == id)
        .unwrap_or_else(|| panic!("missing node {id}"))
}

fn assert_visible_shape_preserved(obs: &PortObservation) {
    assert_endpoint_available(&obs.path_source, obs, "path source");
    assert_endpoint_available(&obs.path_target, obs, "path target");

    if obs.svg_source.is_none() || obs.svg_target.is_none() {
        assert!(
            obs.svg_source.is_none() && obs.svg_target.is_none(),
            "partial SVG endpoint observation for {} {} -> {}",
            obs.fixture,
            obs.from,
            obs.to
        );
        assert!(
            obs.svg_rejection_reason.is_some(),
            "SVG endpoint observation unavailable for {} {} -> {}, but no reason was recorded",
            obs.fixture,
            obs.from,
            obs.to
        );
    }

    if obs.text_source.is_none() || obs.text_target.is_none() {
        assert!(
            obs.text_source.is_none() && obs.text_target.is_none(),
            "partial text endpoint observation for {} {} -> {}; reason: {}",
            obs.fixture,
            obs.from,
            obs.to,
            obs.text_rejection_reason
                .as_deref()
                .unwrap_or("no text rejection reason recorded")
        );
        assert!(
            obs.text_rejection_reason.is_some(),
            "text endpoint observation unavailable for {} {} -> {}, but no reason was recorded",
            obs.fixture,
            obs.from,
            obs.to
        );
    }
}

fn assert_nested_known_unknown(edge_label: &str, obs: &PortObservation) {
    assert_eq!(obs.fixture, "subgraph_direction_nested_both.mmd");
    assert_eq!(format!("{} -> {}", obs.from, obs.to), edge_label);
    assert_endpoint_available(&obs.source_port, obs, "logical source port");
    assert_endpoint_available(&obs.target_port, obs, "logical target port");
    assert_endpoint_available(&obs.path_source, obs, "path source");
    assert_endpoint_available(&obs.path_target, obs, "path target");
    assert_visible_shape_preserved(obs);
    assert!(
        obs.text_source.is_some() && obs.text_target.is_some() && obs.text_path_family.is_some(),
        "nested known-unknown {edge_label} should capture text endpoint/path-family evidence"
    );
    assert!(
        obs.svg_rejection_reason.is_some(),
        "nested known-unknown {edge_label} should record why SVG endpoint evidence is unavailable"
    );
}

fn assert_fraction_near(actual: Option<f64>, expected: f64, label: &str) {
    let actual = actual.unwrap_or_else(|| panic!("{label} should be present"));
    assert!(
        (actual - expected).abs() <= 1e-6,
        "{label} should be near {expected}, got {actual}"
    );
}

fn assert_endpoint_available(
    endpoint: &EndpointObservation,
    obs: &PortObservation,
    endpoint_name: &str,
) {
    assert!(
        endpoint.face.is_some(),
        "{endpoint_name} face missing for {} {} -> {}",
        obs.fixture,
        obs.from,
        obs.to
    );
    assert!(
        endpoint.fraction.is_some(),
        "{endpoint_name} fraction missing for {} {} -> {}",
        obs.fixture,
        obs.from,
        obs.to
    );
    assert!(
        endpoint.position.is_some(),
        "{endpoint_name} position missing for {} {} -> {}",
        obs.fixture,
        obs.from,
        obs.to
    );
}

fn port_observation(port: &Port) -> EndpointObservation {
    EndpointObservation {
        face: Some(port.face.as_str().to_string()),
        fraction: Some(port.fraction),
        position: Some((port.position.x, port.position.y)),
    }
}

fn infer_endpoint_observation(point: (f64, f64), node: &MmdsNode) -> EndpointObservation {
    let rect = node_rect(node);
    let candidates = [
        ("top", (point.1 - rect.top).abs()),
        ("bottom", (point.1 - rect.bottom).abs()),
        ("left", (point.0 - rect.left).abs()),
        ("right", (point.0 - rect.right).abs()),
    ];
    let (face, _) = candidates
        .into_iter()
        .min_by(|(_, lhs), (_, rhs)| lhs.total_cmp(rhs))
        .expect("face candidates should be non-empty");
    let fraction = match face {
        "top" | "bottom" => fraction(point.0, rect.left, rect.right),
        "left" | "right" => fraction(point.1, rect.top, rect.bottom),
        _ => unreachable!("known face"),
    };

    EndpointObservation {
        face: Some(face.to_string()),
        fraction: Some(fraction),
        position: Some(point),
    }
}

fn node_rect(node: &MmdsNode) -> NodeRect {
    let half_width = node.size.width / 2.0;
    let half_height = node.size.height / 2.0;
    NodeRect {
        left: node.position.x - half_width,
        right: node.position.x + half_width,
        top: node.position.y - half_height,
        bottom: node.position.y + half_height,
    }
}

fn fraction(value: f64, start: f64, end: f64) -> f64 {
    let span = end - start;
    if span.abs() <= f64::EPSILON {
        0.5
    } else {
        ((value - start) / span).clamp(0.0, 1.0)
    }
}
