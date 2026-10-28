use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::layout_stability::mutations;
use crate::graph::GeometryLevel;
use crate::mmds::diff::{ChangeKind, Diff};
use crate::{OutputFormat, RenderConfig};

#[test]
fn mmds_diff_model_identical_outputs_have_no_events() {
    let before = parse_routed("graph TD; A --> B");
    let after = parse_routed("graph TD; A --> B");

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.changes.is_empty(), "{diff:#?}");
    assert_eq!(diff.before_geometry_level, GeometryLevel::Routed);
    assert_eq!(diff.after_geometry_level, GeometryLevel::Routed);

    assert!(!diff.has_change(crate::mmds::diff::ChangeKind::GeometryLevelChanged, ""));
}

#[test]
fn mmds_diff_identity_reports_node_and_edge_additions() {
    let before = parse_routed("graph TD; A --> B");
    let after = parse_routed("graph TD; A --> B; B --> C");

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change(crate::mmds::diff::ChangeKind::NodeAdded, "C"));
    assert!(diff.has_change(crate::mmds::diff::ChangeKind::EdgeAdded, "e1"));
}

#[test]
fn mmds_diff_identity_treats_id_change_as_remove_add() {
    let before = parse_routed("graph TD; Lint --> Build");
    let after = parse_routed("graph TD; Audit --> Build");

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change(crate::mmds::diff::ChangeKind::NodeRemoved, "Lint"));
    assert!(diff.has_change(crate::mmds::diff::ChangeKind::NodeAdded, "Audit"));
}

#[test]
fn mmds_diff_semantic_reports_node_label_and_shape_changes() {
    let before = parse_layout("graph TD; A[Build]");
    let after = parse_layout("graph TD; A{Deploy}");

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change(crate::mmds::diff::ChangeKind::NodeLabelChanged, "A"));
    assert!(diff.has_change(crate::mmds::diff::ChangeKind::NodeShapeChanged, "A"));
}

#[test]
fn mmds_diff_semantic_reports_edge_label_and_style_changes() {
    let before = parse_routed("graph TD; A -->|ok| B");
    let after = parse_routed("graph TD; A -.->|warn| B");

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change(crate::mmds::diff::ChangeKind::EdgeLabelChanged, "e0"));
    assert!(diff.has_change(crate::mmds::diff::ChangeKind::EdgeStyleChanged, "e0"));
}

#[test]
fn mmds_diff_edge_matching_shifted_id_reports_label_change_not_remove_add() {
    let before = parse_routed("graph TD; A --> B; B -->|old| C; C --> D");
    let mut after = before.clone();

    after.edges[0].id = "e1".to_string();
    after.edges[1].id = "e2".to_string();
    after.edges[1].label = Some("new".to_string());
    after.edges[2].id = "e3".to_string();

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change(crate::mmds::diff::ChangeKind::EdgeLabelChanged, "e2"));
    assert!(!diff.has_change(crate::mmds::diff::ChangeKind::EdgeRemoved, "e1"));
    assert!(!diff.has_change(crate::mmds::diff::ChangeKind::EdgeAdded, "e2"));
    assert!(
        diff.changes.iter().any(|event| {
            matches!(
                &event.subject,
                crate::mmds::Subject::Edge(id) if id == "e2"
            ) && event.evidence_mentions("matched_by=fallback")
                && event.evidence_mentions("before_id=e1")
                && event.evidence_mentions("after_id=e2")
        }),
        "{diff:#?}"
    );
}

#[test]
fn mmds_diff_edge_matching_shifted_id_uses_after_id_for_routed_evidence() {
    let before = parse_routed("graph TD; A --> B; B --> C");
    let mut after = before.clone();

    after.edges[0].id = "e1".to_string();
    after.edges[1].id = "e2".to_string();
    after.edges[1].path = Some(vec![[0.0, 0.0], [40.0, 0.0], [40.0, 20.0]]);

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change(crate::mmds::diff::ChangeKind::EdgeRerouted, "e2"));
    assert!(
        diff.changes.iter().any(|event| {
            matches!(
                &event.subject,
                crate::mmds::Subject::Edge(id) if id == "e2"
            ) && event.evidence_mentions("matched_by=fallback")
                && event.evidence_mentions("before_id=e1")
                && event.evidence_mentions("after_id=e2")
        }),
        "{diff:#?}"
    );
}

#[test]
fn mmds_diff_edge_matching_m01_preserves_split_and_matches_downstream_shift() {
    let before = parse_routed(include_str!("../../tests/fixtures/flowchart/chain.mmd"));
    let after = parse_routed(
        "graph TD
    A[Step 1] --> B[Step 2] --> X[Inserted] --> C[Step 3] --> D[Step 4]
",
    );

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change(crate::mmds::diff::ChangeKind::NodeAdded, "X"));
    assert!(diff.has_change(crate::mmds::diff::ChangeKind::EdgeRemoved, "e1"));
    assert!(diff.has_change(crate::mmds::diff::ChangeKind::EdgeAdded, "e1"));
    assert!(diff.has_change(crate::mmds::diff::ChangeKind::EdgeAdded, "e2"));
    assert!(!diff.has_change(crate::mmds::diff::ChangeKind::EdgeAdded, "e3"));
}

#[test]
fn mmds_diff_edge_matching_keeps_existing_node_reconnect_as_reconnected() {
    let before = parse_routed("graph TD; A --> B; C[Alternative]");
    let after = parse_routed("graph TD; A --> C; B[Target]");

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change(crate::mmds::diff::ChangeKind::EdgeReconnected, "e0"));
    assert!(!diff.has_change(crate::mmds::diff::ChangeKind::EdgeRemoved, "e0"));
    assert!(!diff.has_change(crate::mmds::diff::ChangeKind::EdgeAdded, "e0"));
}

#[test]
fn mmds_diff_edge_matching_parallel_edges_prefers_label_tiebreaker() {
    let before = parse_routed("graph TD; A -->|alpha| B; A -->|beta| B");
    let mut after = before.clone();

    after.edges[0].id = "e1".to_string();
    after.edges[0].label = Some("beta".to_string());
    after.edges[1].id = "e2".to_string();
    after.edges[1].label = Some("alpha changed".to_string());

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change(crate::mmds::diff::ChangeKind::EdgeLabelChanged, "e2"));
    assert!(!diff.has_change(crate::mmds::diff::ChangeKind::EdgeLabelChanged, "e1"));
    assert!(
        diff.changes.iter().any(|event| {
            matches!(
                &event.subject,
                crate::mmds::Subject::Edge(id) if id == "e2"
            ) && event.evidence_mentions("matched_by=fallback")
                && event.evidence_mentions("before_id=e0")
                && event.evidence_mentions("after_id=e2")
        }),
        "{diff:#?}"
    );
}

#[test]
fn mmds_diff_edge_matching_parallel_edges_falls_back_to_declaration_order() {
    let before = parse_routed("graph TD; A --> B; A --> B");
    let mut after = before.clone();

    after.edges[0].id = "e1".to_string();
    after.edges[0].path = Some(vec![[0.0, 0.0], [40.0, 0.0], [40.0, 20.0]]);
    after.edges[1].id = "e2".to_string();

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change(crate::mmds::diff::ChangeKind::EdgeRerouted, "e1"));
    assert!(
        diff.changes.iter().any(|event| {
            matches!(
                &event.subject,
                crate::mmds::Subject::Edge(id) if id == "e1"
            ) && event.evidence_mentions("matched_by=fallback")
                && event.evidence_mentions("before_id=e0")
                && event.evidence_mentions("after_id=e1")
        }),
        "{diff:#?}"
    );
}

#[test]
fn mmds_diff_semantic_reports_subgraph_title_direction_and_membership_changes() {
    let before = parse_layout(
        "graph TD
        subgraph sg [Pipeline]
            direction TB
            A
        end",
    );
    let after = parse_layout(
        "graph TD
        subgraph sg [Release]
            direction LR
            A
            B
        end",
    );

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change(crate::mmds::diff::ChangeKind::SubgraphTitleChanged, "sg"));
    assert!(diff.has_change(
        crate::mmds::diff::ChangeKind::SubgraphDirectionChanged,
        "sg"
    ));
    assert!(diff.has_change(
        crate::mmds::diff::ChangeKind::SubgraphMembershipChanged,
        "sg"
    ));
}

#[test]
fn mmds_diff_routed_separates_path_from_port_intent() {
    let before = parse_routed("graph TD; A --> B");
    let after = parse_routed("graph LR; A --> B");

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change_kind(crate::mmds::diff::ChangeKind::EdgeRerouted));
    assert!(diff.has_change_kind(crate::mmds::diff::ChangeKind::EndpointFaceChanged));
    assert!(diff.changes.iter().all(|event| {
        event.kind != crate::mmds::diff::ChangeKind::PortIntentChanged
            || event.evidence_mentions("logical")
    }));
}

#[test]
fn mmds_diff_geometry_reports_label_rect_change() {
    let before = render_pair_before_routed("M14");
    let after = render_pair_after_routed("M14");

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change_kind(crate::mmds::diff::ChangeKind::LabelResized));
}

#[test]
fn mmds_diff_reflow_suppresses_tiny_unchanged_node_movement() {
    let before = parse_routed("graph TD; A --> B");
    let after = output_with_node_shift(&before, "A", 0.5, 0.0);

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(!diff.has_change_kind(crate::mmds::diff::ChangeKind::NodeMoved));
    assert!(!diff.has_change_kind(crate::mmds::diff::ChangeKind::GlobalReflowDetected));
}

#[test]
fn mmds_diff_reflow_reports_many_unchanged_nodes_moving() {
    let before = render_pair_before_routed("M14");
    let after = render_pair_after_routed("M14");

    let diff = crate::mmds::diff::diff_documents(&before, &after);

    assert!(diff.has_change_kind(crate::mmds::diff::ChangeKind::EdgeLabelChanged));
    assert!(
        diff.has_related_geometry_for("e0")
            || diff.has_change_kind(crate::mmds::diff::ChangeKind::EdgeRerouted)
    );
}

#[test]
fn mmds_diff_tier_a_reports_expected_event_families() {
    let summaries = run_tier_a_diff_summaries();

    assert!(summaries["M01"].has_change_kind(ChangeKind::NodeAdded));
    assert!(summaries["M05"].has_change_kind(ChangeKind::NodeLabelChanged));
    assert!(summaries["M07"].has_change_kind(ChangeKind::EdgeStyleChanged));
    assert!(!summaries["M07"].has_change_kind(ChangeKind::EdgeRerouted));
    assert!(summaries["M10"].has_change_kind(ChangeKind::SubgraphDirectionChanged));
}

#[test]
fn mmds_diff_edge_matching_tier_a_controls_stay_calibrated() {
    let summaries = run_tier_a_diff_summaries();

    assert!(summaries["M01"].has_change(ChangeKind::NodeAdded, "X"));
    assert!(summaries["M01"].has_change(ChangeKind::EdgeRemoved, "e1"));
    assert!(summaries["M01"].has_change(ChangeKind::EdgeAdded, "e1"));
    assert!(summaries["M01"].has_change(ChangeKind::EdgeAdded, "e2"));

    for pair_id in ["M04", "M11", "M19", "M20", "M21"] {
        assert!(
            summaries[pair_id].has_change_kind(ChangeKind::EdgeAdded),
            "{pair_id} should stay an edge-addition control"
        );
        assert!(
            !summaries[pair_id].has_change_kind(ChangeKind::EdgeRemoved),
            "{pair_id} should not report fallback-induced edge removals"
        );
    }

    assert!(summaries["M05"].has_change_kind(ChangeKind::NodeLabelChanged));
    assert!(summaries["M07"].has_change_kind(ChangeKind::EdgeStyleChanged));
    assert!(!summaries["M07"].has_change_kind(ChangeKind::EdgeRerouted));
    assert!(summaries["S02"].has_change_kind(ChangeKind::EdgeLabelChanged));
    assert!(!summaries["S02"].has_change_kind(ChangeKind::EdgeRerouted));
    assert!(summaries["S03"].has_change_kind(ChangeKind::EdgeStyleChanged));
    assert!(!summaries["S03"].has_change_kind(ChangeKind::EdgeRerouted));

    assert!(summaries["M08"].has_change_kind(ChangeKind::SubgraphAdded));
    assert!(summaries["M09"].has_change_kind(ChangeKind::SubgraphMembershipChanged));
    assert!(summaries["M10"].has_change_kind(ChangeKind::SubgraphDirectionChanged));
}

fn parse_layout(source: &str) -> crate::mmds::Document {
    let config = RenderConfig {
        geometry_level: GeometryLevel::Layout,
        ..RenderConfig::default()
    };
    let json = crate::render_diagram(source, OutputFormat::Mmds, &config)
        .expect("layout MMDS should render");
    crate::mmds::parse_input(&json).expect("rendered MMDS should parse")
}

fn output_with_node_shift(
    output: &crate::mmds::Document,
    node_id: &str,
    dx: f64,
    dy: f64,
) -> crate::mmds::Document {
    let mut shifted = output.clone();
    let node = shifted
        .nodes
        .iter_mut()
        .find(|node| node.id == node_id)
        .expect("node should exist");
    node.position.x += dx;
    node.position.y += dy;
    shifted
}

fn run_tier_a_diff_summaries() -> BTreeMap<&'static str, Diff> {
    mutations::tier_a_pairs()
        .iter()
        .map(|pair| {
            let before = mutation_input_source(pair.id, "before", pair.base);
            let after = mutation_input_source(pair.id, "after", pair.mutated);

            (
                pair.id,
                crate::mmds::diff::diff_documents(&parse_routed(&before), &parse_routed(&after)),
            )
        })
        .collect()
}

fn mutation_input_source(
    pair_id: &'static str,
    side: &'static str,
    input: mutations::MutationInput,
) -> String {
    match input {
        mutations::MutationInput::Inline(source) => source.to_string(),
        mutations::MutationInput::Fixture { family, name } => {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join(family)
                .join(name);
            fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!(
                    "failed to read {side} fixture for {pair_id} from {}: {error}",
                    path.display()
                )
            })
        }
    }
}

fn render_pair_before_routed(pair_id: &str) -> crate::mmds::Document {
    match pair_id {
        "M14" => parse_routed(include_str!(
            "../../tests/fixtures/flowchart/inline_label_flowchart.mmd"
        )),
        _ => panic!("unsupported pair id {pair_id}"),
    }
}

fn render_pair_after_routed(pair_id: &str) -> crate::mmds::Document {
    match pair_id {
        "M14" => parse_routed(M14_AFTER),
        _ => panic!("unsupported pair id {pair_id}"),
    }
}

const M14_AFTER: &str = "flowchart TD
  start((Start)) --> ingest[Ingest Request]
  ingest --> parse[Parse Payload]
  parse --> validate{Valid?}

  validate -- no --> reject[Reject]
  reject -.-> notify[Notify User]
  reject --> metrics[Emit Metrics]

  validate -- yes --> route{Route Type}
  route -- sync --> sync[Sync Pipeline]
  route -- async --> queue[Enqueue Job]

  queue --> worker[Worker Pool]
  worker --> process[Process Job]
  process --> success{Success?}

  success -- retry later --> retry[Retry]
  retry ==> queue

  success -- yes --> persist[Persist Result]
  sync --> persist
  persist --> metrics

  parse --> cache[Lookup Cache]
  cache -- hit --> fastpath[Serve Cached]
  fastpath --> metrics
  cache -- miss --> validate

  ingest --> audit[Audit Log]
  audit --> metrics

  process -- warn --> alert[Page On-call]
  alert -.-> metrics

  metrics --> End((Done))
";

fn parse_routed(source: &str) -> crate::mmds::Document {
    let config = RenderConfig {
        geometry_level: GeometryLevel::Routed,
        ..RenderConfig::default()
    };
    let json = crate::render_diagram(source, OutputFormat::Mmds, &config)
        .expect("routed MMDS should render");
    crate::mmds::parse_input(&json).expect("rendered MMDS should parse")
}
