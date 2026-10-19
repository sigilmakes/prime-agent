//! Persistent session regression: depth repair must not erase opaque metadata.
use pa_core::session::manager::SessionManager;
use pa_types::session::{FileEntry, GitContext};
use serde_json::{json, Value};
use std::path::Path;

const FIXTURE: &str = include_str!("fixtures/session_nested_metadata.jsonl");

fn read_rows(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn persistent_roundtrip(fixture: &str) {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original.jsonl");
    let working = dir.path().join("working.jsonl");
    std::fs::write(&original, fixture).unwrap();
    std::fs::copy(&original, &working).unwrap();
    let mut expected = read_rows(&original);
    // Missing depth on a child header forces the real full-journal rewrite.
    expected[0]["rlmDepth"] = json!(0);
    let mut manager = SessionManager::open(dir.path(), dir.path(), &working);
    assert!(manager.is_persisted());
    assert!(matches!(
        manager.retained_entries()[0],
        FileEntry::Header { .. }
    ));
    assert!(matches!(
        manager.retained_entries()[1],
        FileEntry::GitState { .. }
    ));
    assert_eq!(read_rows(&working), expected);
    for (entry, row) in manager.retained_entries().iter().zip(&expected) {
        assert_eq!(
            matches!(entry, FileEntry::Unknown { .. }),
            row["type"] == "future_entry",
            "known rows must retain their typed behavior"
        );
    }
    let appended_id = manager.append_session_info("Rust roundtrip").unwrap();
    drop(manager);
    let after_append = read_rows(&working);
    assert_eq!(&after_append[..expected.len()], expected);
    assert_eq!(after_append.len(), expected.len() + 1);
    let last = after_append.last().unwrap();
    assert_eq!(last["id"], appended_id);
    assert_eq!(last["type"], "session_info");
    assert_eq!(last["name"], "Rust roundtrip");
    let reopened = SessionManager::open(dir.path(), dir.path(), &working);
    assert_eq!(reopened.retained_entries().len(), after_append.len());
    assert_eq!(read_rows(&working), after_append);
    assert_eq!(std::fs::read_to_string(original).unwrap(), fixture);
}

#[test]
fn persistent_open_repair_append_reopen_preserves_nested_metadata() {
    persistent_roundtrip(FIXTURE);
}

#[test]
fn persistent_open_repair_preserves_all_audited_nested_metadata() {
    persistent_roundtrip(include_str!("fixtures/session_nested_metadata_all.jsonl"));
}

#[test]
fn empty_unknown_maps_keep_existing_wire_bytes() {
    for line in include_str!("fixtures/session_nested_empty_extras.jsonl").lines() {
        let entry: FileEntry = serde_json::from_str(line).unwrap();
        assert!(!matches!(entry, FileEntry::Unknown { .. }));
        assert_eq!(serde_json::to_string(&entry).unwrap(), line);
    }
}

#[test]
fn git_identity_ignores_preserved_metadata_but_records_known_changes() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original.jsonl");
    let working = dir.path().join("working.jsonl");
    let fixture = FIXTURE.lines().take(2).collect::<Vec<_>>().join("\n") + "\n";
    std::fs::write(&original, &fixture).unwrap();
    std::fs::copy(&original, &working).unwrap();
    let mut manager = SessionManager::open(dir.path(), dir.path(), &working);
    manager.append_session_info("git identity fixture").unwrap();
    let before = read_rows(&working);
    let before_bytes = std::fs::read(&working).unwrap();
    let captured: GitContext = serde_json::from_value(json!({"branch": "feature"})).unwrap();
    let historical: GitContext = serde_json::from_value(before[1]["git"].clone()).unwrap();
    assert_ne!(
        historical, captured,
        "canonical equality must still see metadata"
    );
    assert_eq!(manager.record_git_state_if_changed(captured), None);
    manager.flush_now().unwrap();
    assert_eq!(std::fs::read(&working).unwrap(), before_bytes);
    assert_eq!(read_rows(&working), before);

    let changed: GitContext = serde_json::from_value(json!({"branch": "changed"})).unwrap();
    assert!(manager.record_git_state_if_changed(changed).is_some());
    manager.flush_now().unwrap();
    let after = read_rows(&working);
    assert_eq!(&after[..before.len()], before);
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(after.last().unwrap()["type"], "git_state");
    assert_eq!(after.last().unwrap()["git"], json!({"branch": "changed"}));
    assert_eq!(std::fs::read_to_string(original).unwrap(), fixture);
}
