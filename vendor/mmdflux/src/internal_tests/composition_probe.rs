use serde_json::json;

use super::event_change_mapping::model_event_kind_to_change_kind;
use crate::commands::{Command, apply};
use crate::graph::{Arrow, Direction, Shape, Stroke};
use crate::mmds::diff::{Change, ChangeKind, diff_documents};
use crate::mmds::events::{ModelEvent, ModelEventKind};
use crate::mmds::{Document, Subject};
use crate::views::{Selector, ViewEvent, ViewSpec, ViewStatement, project};
use crate::{RenderConfig, materialize_diagram};

#[derive(Debug)]
struct ProbeScenario {
    initial: Document,
    commands: Vec<ProbeCommandStep>,
    view_specs: Vec<ProbeViewSpec>,
}

#[derive(Debug)]
struct ProbeCommandStep {
    name: &'static str,
    command: Command,
    expected_kind: ModelEventKind,
    expected_subject: Subject,
}

#[derive(Debug)]
struct ProbeViewSpec {
    name: &'static str,
    spec: ViewSpec,
}

#[derive(Debug)]
struct CommandProbeResult {
    command_observations: Vec<CommandObservation>,
    unexpected_failures: Vec<String>,
    final_document: Document,
}

#[derive(Debug)]
struct CommandObservation {
    command_name: &'static str,
    returned_event_pairs: Vec<(ModelEventKind, Subject)>,
    returned_expected_primary: bool,
    error: Option<String>,
}

#[derive(Debug)]
struct CompositionProbeResult {
    command_result: CommandProbeResult,
    diff_observations: DiffObservations,
    view_observations: Vec<ViewObservation>,
    composition_observations: CompositionObservations,
}

#[derive(Debug)]
struct ViewObservation {
    name: &'static str,
    node_ids: Vec<String>,
    edge_ids: Vec<String>,
    subgraph_ids: Vec<String>,
    node_count: usize,
    metadata_bounds: (f64, f64),
    events: Vec<ViewEvent>,
    sparse_surviving_edge_ids_preserved: bool,
    error: Option<String>,
}

impl ViewObservation {
    fn contains_node(&self, node_id: &str) -> bool {
        self.node_ids.iter().any(|id| id == node_id)
    }
}

#[derive(Debug)]
struct CompositionObservations {
    unexpected_command_failures: Vec<String>,
    unexpected_missing_semantic_changes: Vec<&'static str>,
    snapshot_diff_collapses: Vec<&'static str>,
    view_materialization_failures: Vec<String>,
    private_internal_requirements: Vec<String>,
    gating_notes: Vec<GatingNote>,
}

impl CompositionObservations {
    fn no_unexpected_failures(&self) -> bool {
        self.unexpected_command_failures.is_empty()
            && self.unexpected_missing_semantic_changes.is_empty()
            && self.view_materialization_failures.is_empty()
    }
}

#[derive(Debug)]
struct GatingNote {
    primitive: &'static str,
    surface: &'static str,
}

#[derive(Debug)]
struct DiffObservations {
    raw_diff_change_pairs: Vec<(ChangeKind, Subject)>,
    command_classifications: Vec<CommandDiffClassification>,
    unexpected_extra_semantic_changes: Vec<(ChangeKind, Subject)>,
    has_expected_stable_headlines: bool,
}

#[derive(Debug)]
struct CommandDiffClassification {
    command_name: &'static str,
    expected_kind: ModelEventKind,
    expected_subject: Subject,
    classification: CommandDiffClassificationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandDiffClassificationKind {
    PresentInBoth,
    GeometryOnlySideEffect,
    CollapsedInSnapshotDiff,
    UnexpectedMissing,
}

fn build_probe_scenario() -> ProbeScenario {
    ProbeScenario {
        initial: document_from_mermaid(
            "graph TD
    gateway[Gateway] --> users[User Service]
    users --> db[(Database)]
    gateway --> billing[Billing Service]
    billing --> ledger[(Ledger)]
    docs[Docs Site] --> gateway
",
        ),
        commands: vec![
            ProbeCommandStep {
                name: "add-auth-subgraph",
                command: Command::AddSubgraph {
                    id: "auth".to_string(),
                    title: Some("Auth".to_string()),
                    parent: None,
                    direction: Some(Direction::LeftRight),
                    children: Vec::new(),
                    concurrent_regions: Vec::new(),
                    invisible: false,
                },
                expected_kind: ModelEventKind::SubgraphAdded,
                expected_subject: Subject::Subgraph("auth".to_string()),
            },
            ProbeCommandStep {
                name: "add-login",
                command: Command::AddNode {
                    id: "login".to_string(),
                    label: "Login".to_string(),
                    shape: Shape::Rectangle,
                    parent: Some("auth".to_string()),
                },
                expected_kind: ModelEventKind::NodeAdded,
                expected_subject: Subject::Node("login".to_string()),
            },
            ProbeCommandStep {
                name: "move-users-into-auth",
                command: Command::ChangeSubgraphMembership {
                    subgraph: "auth".to_string(),
                    added_children: vec!["users".to_string()],
                    removed_children: Vec::new(),
                    added_concurrent_regions: Vec::new(),
                    removed_concurrent_regions: Vec::new(),
                },
                expected_kind: ModelEventKind::SubgraphMembershipChanged,
                expected_subject: Subject::Subgraph("auth".to_string()),
            },
            ProbeCommandStep {
                name: "add-login-db-edge",
                command: Command::AddEdge {
                    id: None,
                    source: "login".to_string(),
                    target: "db".to_string(),
                    from_subgraph: None,
                    to_subgraph: None,
                    label: Some("reads".to_string()),
                    stroke: Stroke::Solid,
                    arrow_start: Arrow::None,
                    arrow_end: Arrow::Normal,
                    minlen: 1,
                },
                expected_kind: ModelEventKind::EdgeAdded,
                expected_subject: Subject::Edge("e5".to_string()),
            },
            ProbeCommandStep {
                name: "change-login-label",
                command: Command::ChangeNodeLabel {
                    node: "login".to_string(),
                    label: "Login Service :8443".to_string(),
                },
                expected_kind: ModelEventKind::NodeLabelChanged,
                expected_subject: Subject::Node("login".to_string()),
            },
            ProbeCommandStep {
                name: "set-review-profile",
                command: Command::SetProfiles {
                    profiles: vec!["composition-probe".to_string()],
                },
                expected_kind: ModelEventKind::ProfileChanged,
                expected_subject: Subject::Document,
            },
            ProbeCommandStep {
                name: "style-login",
                command: Command::SetNodeStyleExtension {
                    node: "login".to_string(),
                    value: json!({
                        "fill": "#fdf6e3",
                        "stroke": "#586e75",
                    }),
                },
                expected_kind: ModelEventKind::NodeStyleChanged,
                expected_subject: Subject::Node("login".to_string()),
            },
        ],
        view_specs: vec![
            ProbeViewSpec {
                name: "auth-subgraph",
                spec: auth_view_spec(),
            },
            ProbeViewSpec {
                name: "full-diagram",
                spec: full_view_spec(),
            },
        ],
    }
}

fn document_from_mermaid(input: &str) -> Document {
    materialize_diagram(input, &RenderConfig::default()).expect("scenario should materialize")
}

fn auth_view_spec() -> ViewSpec {
    ViewSpec {
        statements: vec![ViewStatement::Include(Selector::SubgraphDescendants(
            "auth".to_string(),
        ))],
        ..ViewSpec::default()
    }
}

fn full_view_spec() -> ViewSpec {
    ViewSpec {
        statements: vec![ViewStatement::Include(Selector::All)],
        ..ViewSpec::default()
    }
}

fn run_command_probe(scenario: &ProbeScenario) -> CommandProbeResult {
    let mut document = scenario.initial.clone();
    let mut command_observations = Vec::new();
    let mut unexpected_failures = Vec::new();

    for step in &scenario.commands {
        match apply(&step.command, &mut document) {
            Ok(events) => {
                let returned_expected_primary =
                    has_model_event_pair(&events, step.expected_kind, &step.expected_subject);
                command_observations.push(CommandObservation {
                    command_name: step.name,
                    returned_event_pairs: model_event_pairs(&events),
                    returned_expected_primary,
                    error: None,
                });
            }
            Err(error) => {
                let message = format!("{} failed: {error:?}", step.name);
                unexpected_failures.push(message.clone());
                command_observations.push(CommandObservation {
                    command_name: step.name,
                    returned_event_pairs: Vec::new(),
                    returned_expected_primary: false,
                    error: Some(message),
                });
            }
        }
    }

    CommandProbeResult {
        command_observations,
        unexpected_failures,
        final_document: document,
    }
}

fn run_composed_probe(scenario: &ProbeScenario) -> CompositionProbeResult {
    let command_result = run_command_probe(scenario);
    let diff = diff_documents(&scenario.initial, &command_result.final_document);
    let raw_diff_change_pairs = change_pairs(&diff.changes);
    let semantic_diff_change_pairs = semantic_change_pairs(&diff.changes);
    let command_expected_pairs: Vec<_> = scenario
        .commands
        .iter()
        .map(|step| {
            (
                model_event_kind_to_change_kind(step.expected_kind),
                step.expected_subject.clone(),
            )
        })
        .collect();
    let mut created_subjects = Vec::new();
    let mut command_classifications = Vec::new();

    for step in &scenario.commands {
        let classification = classify_command_diff_relationship(
            step,
            &semantic_diff_change_pairs,
            &diff.changes,
            &created_subjects,
        );

        if matches!(
            model_event_kind_to_change_kind(step.expected_kind),
            ChangeKind::NodeAdded | ChangeKind::EdgeAdded | ChangeKind::SubgraphAdded
        ) {
            created_subjects.push(step.expected_subject.clone());
        }

        command_classifications.push(CommandDiffClassification {
            command_name: step.name,
            expected_kind: step.expected_kind,
            expected_subject: step.expected_subject.clone(),
            classification,
        });
    }

    let unexpected_extra_semantic_changes = semantic_diff_change_pairs
        .iter()
        .filter(|(kind, subject)| {
            !command_expected_pairs
                .iter()
                .any(|(expected_kind, expected_subject)| {
                    kind == expected_kind && subject == expected_subject
                })
        })
        .cloned()
        .collect();
    let has_expected_stable_headlines = command_classifications.iter().all(|classification| {
        !matches!(
            classification.classification,
            CommandDiffClassificationKind::UnexpectedMissing
        )
    });

    let diff_observations = DiffObservations {
        raw_diff_change_pairs,
        command_classifications,
        unexpected_extra_semantic_changes,
        has_expected_stable_headlines,
    };
    let view_observations = collect_view_observations(&command_result.final_document, scenario);
    let composition_observations =
        summarize_composition(&command_result, &diff_observations, &view_observations);

    CompositionProbeResult {
        command_result,
        diff_observations,
        view_observations,
        composition_observations,
    }
}

fn summarize_composition(
    command_result: &CommandProbeResult,
    diff_observations: &DiffObservations,
    view_observations: &[ViewObservation],
) -> CompositionObservations {
    CompositionObservations {
        unexpected_command_failures: command_result.unexpected_failures.clone(),
        unexpected_missing_semantic_changes: diff_observations
            .command_classifications
            .iter()
            .filter(|classification| {
                matches!(
                    classification.classification,
                    CommandDiffClassificationKind::UnexpectedMissing
                )
            })
            .map(|classification| classification.command_name)
            .collect(),
        snapshot_diff_collapses: diff_observations
            .command_classifications
            .iter()
            .filter(|classification| {
                matches!(
                    classification.classification,
                    CommandDiffClassificationKind::CollapsedInSnapshotDiff
                )
            })
            .map(|classification| classification.command_name)
            .collect(),
        view_materialization_failures: view_observations
            .iter()
            .filter_map(|observation| {
                observation
                    .error
                    .as_ref()
                    .map(|error| format!("{} failed: {error}", observation.name))
            })
            .collect(),
        private_internal_requirements: Vec::new(),
        gating_notes: vec![
            GatingNote {
                primitive: "views",
                surface: "public",
            },
            GatingNote {
                primitive: "diff",
                surface: "public",
            },
            GatingNote {
                primitive: "commands",
                surface: "public",
            },
        ],
    }
}

fn collect_view_observations(
    final_document: &Document,
    scenario: &ProbeScenario,
) -> Vec<ViewObservation> {
    scenario
        .view_specs
        .iter()
        .map(
            |probe_view| match project(final_document, &probe_view.spec) {
                Ok((view, events)) => {
                    let canonical_edge_ids = edge_ids(final_document);
                    let retained_edge_ids = edge_ids(&view);
                    ViewObservation {
                        name: probe_view.name,
                        node_ids: node_ids(&view),
                        edge_ids: retained_edge_ids.clone(),
                        subgraph_ids: subgraph_ids(&view),
                        node_count: view.nodes.len(),
                        metadata_bounds: (view.metadata.bounds.width, view.metadata.bounds.height),
                        events,
                        sparse_surviving_edge_ids_preserved: retained_edge_ids
                            .iter()
                            .all(|id| canonical_edge_ids.contains(id))
                            && retained_edge_ids
                                .iter()
                                .enumerate()
                                .any(|(index, id)| id != &format!("e{index}")),
                        error: None,
                    }
                }
                Err(error) => ViewObservation {
                    name: probe_view.name,
                    node_ids: Vec::new(),
                    edge_ids: Vec::new(),
                    subgraph_ids: Vec::new(),
                    node_count: 0,
                    metadata_bounds: (0.0, 0.0),
                    events: Vec::new(),
                    sparse_surviving_edge_ids_preserved: false,
                    error: Some(format!("{error:?}")),
                },
            },
        )
        .collect()
}

fn classify_command_diff_relationship(
    step: &ProbeCommandStep,
    semantic_diff_change_pairs: &[(ChangeKind, Subject)],
    raw_diff_events: &[Change],
    created_subjects: &[Subject],
) -> CommandDiffClassificationKind {
    if event_pair_is_present(
        semantic_diff_change_pairs,
        model_event_kind_to_change_kind(step.expected_kind),
        &step.expected_subject,
    ) {
        return CommandDiffClassificationKind::PresentInBoth;
    }

    if raw_diff_events
        .iter()
        .any(|event| event.kind.is_geometry() && event.subject == step.expected_subject)
    {
        return CommandDiffClassificationKind::GeometryOnlySideEffect;
    }

    if created_subjects
        .iter()
        .any(|subject| subject == &step.expected_subject)
    {
        return CommandDiffClassificationKind::CollapsedInSnapshotDiff;
    }

    CommandDiffClassificationKind::UnexpectedMissing
}

fn has_model_event_pair(events: &[ModelEvent], kind: ModelEventKind, subject: &Subject) -> bool {
    events
        .iter()
        .any(|event| event.kind == kind && &event.subject == subject)
}

fn model_event_pairs(events: &[ModelEvent]) -> Vec<(ModelEventKind, Subject)> {
    events
        .iter()
        .map(|event| (event.kind, event.subject.clone()))
        .collect()
}

fn change_pairs(changes: &[Change]) -> Vec<(ChangeKind, Subject)> {
    changes
        .iter()
        .map(|change| (change.kind, change.subject.clone()))
        .collect()
}

fn semantic_change_pairs(changes: &[Change]) -> Vec<(ChangeKind, Subject)> {
    change_pairs(changes)
        .into_iter()
        .filter(|(kind, _)| !kind.is_geometry())
        .collect()
}

fn event_pair_is_present(
    pairs: &[(ChangeKind, Subject)],
    kind: ChangeKind,
    subject: &Subject,
) -> bool {
    pairs
        .iter()
        .any(|(event_kind, event_subject)| *event_kind == kind && event_subject == subject)
}

fn node_ids(document: &Document) -> Vec<String> {
    document.nodes.iter().map(|node| node.id.clone()).collect()
}

fn edge_ids(document: &Document) -> Vec<String> {
    document.edges.iter().map(|edge| edge.id.clone()).collect()
}

fn subgraph_ids(document: &Document) -> Vec<String> {
    document
        .subgraphs
        .iter()
        .map(|subgraph| subgraph.id.clone())
        .collect()
}

#[test]
fn composition_probe_scenario_covers_required_surfaces() {
    let scenario = build_probe_scenario();

    assert!(scenario.initial.nodes.len() >= 5);
    assert!(
        scenario
            .commands
            .iter()
            .any(|step| matches!(step.command, Command::AddNode { .. }))
    );
    assert!(
        scenario
            .commands
            .iter()
            .any(|step| matches!(step.command, Command::AddSubgraph { .. }))
    );
    assert!(
        scenario
            .commands
            .iter()
            .any(|step| matches!(step.command, Command::ChangeSubgraphMembership { .. }))
    );
    assert!(
        scenario
            .commands
            .iter()
            .any(|step| matches!(step.command, Command::AddEdge { .. }))
    );
    assert!(
        scenario
            .commands
            .iter()
            .any(|step| matches!(step.command, Command::ChangeNodeLabel { .. }))
    );
    assert!(
        scenario
            .commands
            .iter()
            .any(|step| matches!(step.command, Command::SetProfiles { .. }))
    );
    assert!(
        scenario
            .commands
            .iter()
            .any(|step| matches!(step.command, Command::SetNodeStyleExtension { .. }))
    );
    assert_eq!(scenario.view_specs.len(), 2);

    let add_login = scenario
        .commands
        .iter()
        .find(|step| step.name == "add-login")
        .expect("add-login command exists");
    assert_eq!(add_login.expected_kind, ModelEventKind::NodeAdded);
    assert_eq!(
        add_login.expected_subject,
        Subject::Node("login".to_string())
    );
}

#[test]
fn composition_probe_command_log_records_expected_primary_events() {
    let scenario = build_probe_scenario();
    let result = run_command_probe(&scenario);

    assert!(result.unexpected_failures.is_empty(), "{result:#?}");
    assert!(
        result
            .final_document
            .nodes
            .iter()
            .any(|node| node.id == "login")
    );
    for observation in &result.command_observations {
        assert!(!observation.command_name.is_empty());
        assert!(observation.error.is_none(), "{observation:#?}");
        assert!(!observation.returned_event_pairs.is_empty());
        assert!(observation.returned_expected_primary, "{observation:#?}");
    }
}

#[test]
fn composition_probe_snapshot_diff_classifies_command_log_relationship() {
    let scenario = build_probe_scenario();
    let result = run_composed_probe(&scenario);

    assert!(
        result.diff_observations.has_expected_stable_headlines,
        "{result:#?}"
    );
    assert!(!result.diff_observations.raw_diff_change_pairs.is_empty());
    assert!(
        result
            .diff_observations
            .unexpected_extra_semantic_changes
            .iter()
            .any(|(kind, subject)| {
                *kind == ChangeKind::ExtensionChanged && *subject == Subject::Document
            })
    );
    assert!(
        result
            .diff_observations
            .command_classifications
            .iter()
            .any(|classification| classification.command_name == "change-login-label"),
        "{result:#?}"
    );
    let label_classification = result
        .diff_observations
        .command_classifications
        .iter()
        .find(|classification| classification.command_name == "change-login-label")
        .expect("change-login-label classification exists");
    assert_eq!(
        label_classification.expected_kind,
        ModelEventKind::NodeLabelChanged
    );
    assert_eq!(
        label_classification.expected_subject,
        Subject::Node("login".to_string())
    );
}

#[test]
fn composition_probe_view_materializes_filtered_and_full_views() {
    let scenario = build_probe_scenario();
    let result = run_composed_probe(&scenario);

    assert_eq!(result.view_observations.len(), 2, "{result:#?}");
    assert!(
        result
            .view_observations
            .iter()
            .any(|view| { view.name == "auth-subgraph" && view.contains_node("login") })
    );
    let auth_view = result
        .view_observations
        .iter()
        .find(|view| view.name == "auth-subgraph")
        .expect("auth-subgraph view exists");
    assert!(auth_view.edge_ids.is_empty());
    assert_eq!(auth_view.subgraph_ids, vec!["auth".to_string()]);
    assert!(auth_view.metadata_bounds.0 > 0.0);
    assert!(auth_view.metadata_bounds.1 > 0.0);
    assert!(!auth_view.events.is_empty());
    assert!(!auth_view.sparse_surviving_edge_ids_preserved);
    assert!(result.view_observations.iter().any(|view| {
        view.name == "full-diagram"
            && view.node_count == result.command_result.final_document.nodes.len()
    }));
}

#[test]
fn composition_probe_observations_reflect_public_surfaces() {
    let scenario = build_probe_scenario();
    let result = run_composed_probe(&scenario);

    assert!(
        result.composition_observations.no_unexpected_failures(),
        "{result:#?}"
    );
    assert!(
        result
            .composition_observations
            .gating_notes
            .iter()
            .any(|note| note.primitive == "views" && note.surface == "public")
    );
    assert!(
        result
            .composition_observations
            .gating_notes
            .iter()
            .any(|note| note.primitive == "diff" && note.surface == "public")
    );
    assert!(
        result
            .composition_observations
            .gating_notes
            .iter()
            .any(|note| note.primitive == "commands" && note.surface == "public")
    );
    assert!(
        result
            .composition_observations
            .snapshot_diff_collapses
            .contains(&"change-login-label")
    );
    assert!(
        result
            .composition_observations
            .private_internal_requirements
            .is_empty()
    );

    eprintln!("{result:#?}");
}
