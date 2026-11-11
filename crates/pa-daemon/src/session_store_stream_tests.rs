//! Native log replay agrees with the read-only SDK reader and refuses damaged bytes.

use super::{fold_child_usage_attributions, SessionEntry, SessionFile};
use serde_json::{json, Value};

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let rows = [
        json!({"type":"session","id":"s","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp","version":3}),
        json!({"type":"message","id":"a","timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"one"}}),
        json!({"type":"message","id":"b","parentId":"a","timestamp":"2026-01-01T00:00:02Z","message":{"role":"assistant","content":"two"}}),
        json!({"type":"session_info","id":"c","parentId":"b","timestamp":"2026-01-01T00:00:03Z","name":"wire bytes"}),
    ];
    super::log_reader_tests::write_jsonl_fixture(
        &path,
        &rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    );
    (dir, path)
}

#[test]
fn native_replay_matches_the_read_only_sdk_row_for_row() {
    let (_dir, path) = fixture();
    let before = std::fs::read(&path).unwrap();
    let reference = pa_core::session::read::read_entries(&path).unwrap();
    let mut reference: Vec<SessionEntry> = reference
        .into_iter()
        .skip(1)
        .map(|entry| serde_json::from_value(serde_json::to_value(entry).unwrap()).unwrap())
        .collect();
    fold_child_usage_attributions(&mut reference);
    let replay = SessionFile::open(&path).unwrap();
    assert_eq!(
        serde_json::to_value(replay.entries()).unwrap(),
        serde_json::to_value(reference).unwrap()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn damaged_native_rows_are_refused_without_repair() {
    // A same-length edit is not damage any more: a bare row carries no
    // checksum, so its bytes are its own and nothing can tell it from an
    // original. Only shapes that are not a row at all are refused.
    for damage in ["blank", "malformed", "unterminated", "invalid-utf8"] {
        let (_dir, path) = fixture();
        let mut bytes = std::fs::read(&path).unwrap();
        match damage {
            "blank" => bytes.extend_from_slice(b"\n"),
            "malformed" => bytes.extend_from_slice(b"{not json}\n"),
            "unterminated" => {
                bytes.pop();
            }
            "invalid-utf8" => bytes.extend_from_slice(b"\xff\xfe\n"),
            _ => unreachable!(),
        }
        std::fs::write(&path, &bytes).unwrap();
        assert!(SessionFile::open(&path).is_err(), "{damage}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "{damage}: no implicit repair"
        );
    }
}

#[test]
fn legacy_raw_rows_are_refused_without_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.jsonl");
    let bytes = b"{\"type\":\"session\",\"id\":\"s\",\"cwd\":\"/tmp\",\"timestamp\":\"t\"}\r\n\r\n{not json}\r\n";
    std::fs::write(&path, bytes).unwrap();
    assert!(SessionFile::open(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn native_open_keeps_the_empty_and_missing_header_errors() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.jsonl");
    std::fs::write(&empty, b"").unwrap();
    assert!(SessionFile::open(&empty).is_err());
    let wrong_header = dir.path().join("wrong-header.jsonl");
    super::log_reader_tests::write_jsonl_fixture(&wrong_header, &json!({"type":"message","id":"a","timestamp":"t","message":{"role":"user","content":"no header"}}).to_string());
    let before = std::fs::read(&wrong_header).unwrap();
    assert!(SessionFile::open(&wrong_header).is_err());
    assert_eq!(std::fs::read(&wrong_header).unwrap(), before);
}
