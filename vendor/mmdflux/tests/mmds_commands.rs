use mmdflux::commands::{Command, CommandApplyError, EdgeSelector, apply, apply_with_config};
use mmdflux::graph::Shape;
use mmdflux::mmds::Subject;
use mmdflux::mmds::diff::{ChangeKind, diff_documents};
use mmdflux::mmds::events::ModelEventKind;
use mmdflux::{EngineAlgorithmId, RenderConfig, materialize_diagram};

fn materialize(source: &str) -> mmdflux::mmds::Document {
    materialize_diagram(source, &RenderConfig::default()).expect("diagram should materialize")
}

fn add_gamma_command() -> Command {
    Command::AddNode {
        id: "C".to_string(),
        label: "Gamma".to_string(),
        shape: Shape::Rectangle,
        parent: None,
    }
}

fn vertical_distance(document: &mmdflux::mmds::Document, source: &str, target: &str) -> f64 {
    let source = document
        .nodes
        .iter()
        .find(|node| node.id == source)
        .expect("source node should exist");
    let target = document
        .nodes
        .iter()
        .find(|node| node.id == target)
        .expect("target node should exist");
    (target.position.y - source.position.y).abs()
}

#[test]
fn public_mmds_commands_expose_apply_and_structured_errors() {
    let mut document = materialize(
        r#"
graph TD
    A[Alpha] --> B[Beta]
"#,
    );

    let error = apply(
        &Command::RemoveNode {
            id: "missing".to_string(),
        },
        &mut document,
    )
    .expect_err("missing node should produce a structured error");

    assert!(matches!(error, CommandApplyError::NodeNotFound { .. }));
    let _: Box<dyn std::error::Error> = Box::new(error);

    let _selector = EdgeSelector::Id("e0".to_string());
}

#[test]
fn public_mmds_commands_apply_with_config_uses_caller_engine_when_document_has_none() {
    let mut document = materialize(
        r#"
graph TD
    A[Alpha] --> B[Beta]
"#,
    );
    document.metadata.engine = None;

    let config = RenderConfig {
        layout_engine: Some(EngineAlgorithmId::parse("mermaid-layered").unwrap()),
        ..RenderConfig::default()
    };

    apply_with_config(&add_gamma_command(), &mut document, &config)
        .expect("add node should apply with caller config");

    assert_eq!(document.metadata.engine.as_deref(), Some("mermaid-layered"));
}

#[test]
fn public_mmds_commands_apply_with_config_document_engine_overrides_caller_engine() {
    let mut document = materialize(
        r#"
graph TD
    A[Alpha] --> B[Beta]
"#,
    );
    document.metadata.engine = Some("flux-layered".to_string());

    let config = RenderConfig {
        layout_engine: Some(EngineAlgorithmId::parse("mermaid-layered").unwrap()),
        ..RenderConfig::default()
    };

    apply_with_config(&add_gamma_command(), &mut document, &config)
        .expect("add node should apply with caller config");

    assert_eq!(document.metadata.engine.as_deref(), Some("flux-layered"));
}

#[test]
fn public_mmds_commands_apply_with_config_preserves_non_engine_caller_config() {
    let source = r#"
graph TD
    A[Alpha] --> B[Beta]
"#;
    let mut default_document = materialize(source);
    let mut wide_document = materialize(source);

    apply(&add_gamma_command(), &mut default_document).expect("default add node should apply");

    let mut wide_config = RenderConfig::default();
    wide_config.layout.rank_sep = 220.0;

    apply_with_config(&add_gamma_command(), &mut wide_document, &wide_config)
        .expect("add node should apply with caller config");

    let default_distance = vertical_distance(&default_document, "A", "B");
    let wide_distance = vertical_distance(&wide_document, "A", "B");

    assert!(
        wide_distance > default_distance + 100.0,
        "caller rank_sep should flow through relayout: default={default_distance}, wide={wide_distance}"
    );
}

#[test]
fn public_mmds_commands_events_are_not_snapshot_diff_entries() {
    let before = materialize(
        r#"
graph TD
    A[Alpha] --> B[Beta]
"#,
    );
    let mut document = before.clone();

    apply(&add_gamma_command(), &mut document).expect("add node should apply");
    let model_events = apply(
        &Command::ChangeNodeLabel {
            node: "C".to_string(),
            label: "Gamma prime".to_string(),
        },
        &mut document,
    )
    .expect("label change should apply");
    let snapshot_diff = diff_documents(&before, &document);

    assert!(model_events.iter().any(|event| {
        event.kind == ModelEventKind::NodeLabelChanged
            && matches!(&event.subject, Subject::Node(id) if id == "C")
    }));
    assert!(snapshot_diff.changes.iter().any(|event| {
        event.kind == ChangeKind::NodeAdded
            && matches!(&event.subject, Subject::Node(id) if id == "C")
    }));
    assert!(!snapshot_diff.changes.iter().any(|event| {
        event.kind == ChangeKind::NodeLabelChanged
            && matches!(&event.subject, Subject::Node(id) if id == "C")
    }));
}

#[test]
fn public_mmds_commands_docs_name_event_snapshot_diff_contract() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands.rs"),
    )
    .expect("commands source should be readable");

    for required in [
        "model events",
        "snapshot diff",
        "apply_with_config",
        "document-owned",
        "EdgeSelector::Semantic",
        "AddEdge.id",
        "wraps non-object",
        "typed Rust enums",
        "std::error::Error",
    ] {
        assert!(
            source.contains(required),
            "commands rustdoc should document: {required}"
        );
    }
}
