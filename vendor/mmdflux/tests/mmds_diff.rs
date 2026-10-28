use mmdflux::mmds::Subject;
use mmdflux::mmds::diff::{ChangeKind, diff_documents};
use mmdflux::{RenderConfig, materialize_diagram};

fn materialize(source: &str) -> mmdflux::mmds::Document {
    materialize_diagram(source, &RenderConfig::default()).expect("diagram should materialize")
}

#[test]
fn public_mmds_diff_exposes_changes_and_geometry_levels() {
    let before = materialize(
        r#"
graph TD
    A[Alpha] --> B[Beta]
"#,
    );
    let after = materialize(
        r#"
graph TD
    A[Alpine] --> B[Beta]
"#,
    );

    let diff = diff_documents(&before, &after);

    assert_eq!(diff.before_geometry_level, before.geometry_level);
    assert_eq!(diff.after_geometry_level, after.geometry_level);
    assert!(diff.changes.iter().any(|event| {
        event.kind == ChangeKind::NodeLabelChanged
            && matches!(&event.subject, Subject::Node(id) if id == "A")
    }));
    assert!(!ChangeKind::NodeLabelChanged.is_geometry());
    assert!(ChangeKind::NodeLabelChanged.is_model());
}

#[test]
fn public_mmds_diff_docs_name_snapshot_diff_contract() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mmds/diff.rs"),
    )
    .expect("diff source should be readable");

    for required in [
        "snapshot diff",
        "diagnostic and not format-stable",
        "secondary semantic effects",
        "related_change_ids",
    ] {
        assert!(
            source.contains(required),
            "diff rustdoc should document: {required}"
        );
    }
}
