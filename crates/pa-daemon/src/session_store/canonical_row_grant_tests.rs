//! The store's canonical admission boundary: one grant per row, one count when
//! there is no epoch.
//!
//! These tests do not re-prove Core's authority. They prove the seam this store
//! owns: a row written with no epoch moves `rows_written_without_epoch`, a turn's
//! user row rides a real prepare-phase flight (so the count does not move and the
//! epoch counts the write), and an ordinary row takes a requested-mutation grant.

use std::sync::Arc;

use pa_core::session::canonical::{CanonicalCursor, CanonicalFrontier, CanonicalOwnerScope};
use pa_core::session_engine::canonical_coordinator::CanonicalEpoch;
use serde_json::json;

use super::canonical_adapter::EpochHandle;
use super::SessionFile;

fn cursor() -> CanonicalCursor {
    CanonicalCursor {
        owner: CanonicalOwnerScope::new(),
        session_id: "row-grant-session".to_owned(),
        source_path: std::path::PathBuf::from("/private/row-grant.jsonl"),
        raw_source_generation: [7; 32],
        header_certificate: [8; 32],
        canonical_revision: 0,
        branch_revision: 1,
        selected_leaf: None,
        frontier: CanonicalFrontier {
            bytes: 9,
            rows: 1,
            prefix_certificate: [9; 32],
        },
    }
}

fn fixture() -> (tempfile::TempDir, SessionFile) {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = SessionFile::create("/synthetic", None, 0);
    store.set_path(dir.path().join("session.jsonl"));
    store.rewrite().expect("rewrite");
    (dir, store)
}

fn epoch() -> Arc<CanonicalEpoch> {
    Arc::new(
        CanonicalEpoch::genesis(
            &cursor(),
            Arc::new(String::from("run")),
            Arc::new(String::from("model")),
        )
        .expect("epoch"),
    )
}

fn user_row() -> serde_json::Value {
    json!({
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
            "timestamp": 1
        }
    })
}

fn assistant_row() -> serde_json::Value {
    json!({
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "api": "openai-completions",
            "provider": "synthetic",
            "model": "synthetic",
            "stopReason": "stop",
            "timestamp": 2,
            "usage": serde_json::to_value(pa_types::ai::Usage::default()).expect("usage")
        }
    })
}

/// No epoch attached: the row lands and the counter states the fact.
#[test]
fn a_row_written_with_no_epoch_is_counted_once() {
    let (_dir, mut store) = fixture();
    let before = store.rows_written_without_epoch();
    store.persist_entry("message", user_row()).expect("row");
    assert_eq!(store.rows_written_without_epoch(), before + 1);
}

/// The turn's user row rides a prepare-phase flight: it is covered, so the
/// counter does not move, and the epoch does not poison.
#[test]
fn a_turn_user_row_does_not_increment_the_without_epoch_count() {
    let (_dir, mut store) = fixture();
    let epoch = epoch();
    store.epoch = Some(EpochHandle(Arc::clone(&epoch)));
    store
        .persist_entry("message", user_row())
        .expect("user row");
    assert_eq!(
        store.rows_written_without_epoch(),
        0,
        "the user row is covered by a prepare-phase flight"
    );
    assert!(!epoch.poisoned(), "a classified flight must not poison");
    let closing = epoch.request_close();
    assert_eq!(
        closing.view(),
        pa_core::session_engine::canonical_coordinator::CanonicalDrainView::Pending {
            io: 0,
            transactions: 0,
            anchors: 1,
        },
        "the flight settled, so only the live serving anchor remains"
    );
}

/// A context-policy row takes a real requested-mutation grant.
#[test]
fn a_compaction_row_takes_a_requested_mutation_grant_and_is_not_counted() {
    let (_dir, mut store) = fixture();
    let epoch = epoch();
    store.epoch = Some(EpochHandle(Arc::clone(&epoch)));
    store
        .persist_entry(
            "compaction",
            json!({
                "summary": "row-grant",
                "firstKeptEntryId": "a",
                "tokensBefore": 1,
                "details": {"bytes": "AA=="}
            }),
        )
        .expect("compaction row");
    assert_eq!(store.rows_written_without_epoch(), 0);
    assert!(!epoch.poisoned());
}

/// An assistant row is the native final output: it takes the provider step's
/// terminal grant, is NOT counted, and leaves the epoch clean - with exactly one
/// flight, which a close observes.
#[test]
fn an_assistant_row_takes_the_provider_step_grant() {
    let (_dir, mut store) = fixture();
    let epoch = epoch();
    store.epoch = Some(EpochHandle(Arc::clone(&epoch)));
    let step = epoch
        .admit_provider_step(Arc::new(String::from("actual provider step")))
        .expect("admitted step");
    let before = store.rows_written_without_epoch();
    store
        .persist_message_with_step(assistant_row(), &step)
        .expect("assistant row");
    assert_eq!(
        store.rows_written_without_epoch(),
        before,
        "the assistant row is covered by the step's grant, so the count does not move"
    );
    assert!(
        !epoch.poisoned(),
        "a settled terminal flight must not poison"
    );
    let closing = epoch.request_close();
    assert_eq!(
        closing.view(),
        pa_core::session_engine::canonical_coordinator::CanonicalDrainView::Pending {
            io: 0,
            transactions: 0,
            anchors: 1,
        },
        "the terminal flight settled; only the live serving anchor remains"
    );
}

/// A missing step under a LIVE epoch is a bug to surface, not a row to write:
/// the refusal names the bug, nothing is written, and the count does not move.
#[test]
fn an_assistant_row_without_a_step_is_refused_under_a_live_epoch() {
    let (_dir, mut store) = fixture();
    let epoch = epoch();
    store.epoch = Some(EpochHandle(Arc::clone(&epoch)));
    let before = store.rows_written_without_epoch();
    let refused = store
        .persist_entry("message", assistant_row())
        .expect_err("an assistant row without a provider step must not be written");
    let message = format!("{refused:#}");
    assert!(
        message.contains("admitted no provider step"),
        "the refusal names the bug, got: {message}"
    );
    assert_eq!(
        store.rows_written_without_epoch(),
        before,
        "a refused row is not counted as written"
    );
    assert!(
        !epoch.poisoned(),
        "no flight was opened, so nothing can poison"
    );
}

/// A branch summary is context policy at HEAD: `Purpose::ContextPolicy` accepts
/// `Compaction | BranchSummary` and no metadata action matches it, so it takes the
/// same requested-mutation grant as a compaction row and is not counted.
#[test]
fn a_branch_summary_row_takes_a_requested_mutation_grant_and_is_not_counted() {
    let (_dir, mut store) = fixture();
    let epoch = epoch();
    store.epoch = Some(EpochHandle(Arc::clone(&epoch)));
    store
        .persist_entry(
            "branch_summary",
            json!({"fromId": "a", "summary": "branch"}),
        )
        .expect("branch summary row");
    assert_eq!(store.rows_written_without_epoch(), 0);
    assert!(!epoch.poisoned());
}

/// The sealed entry and the persisted row are the SAME payload.
///
/// Byte equality is impossible by design: the draft rejects a supplied envelope
/// (`id`, `parentId`, `timestamp`), and the store mints those at write time. So the
/// comparison removes exactly those three keys from BOTH sides and requires the
/// remainder to be equal as a whole value - field-by-field assertions pass when a
/// payload field is dropped, because nothing looks for the field that vanished.
fn assert_same_payload(draft_entry: &serde_json::Value, persisted: &serde_json::Value) {
    let mut left = draft_entry.clone();
    let mut right = persisted.clone();
    let mut removed_left = 0;
    let mut removed_right = 0;
    for key in ["id", "parentId", "timestamp"] {
        if left.as_object_mut().expect("object").remove(key).is_some() {
            removed_left += 1;
        }
        if right.as_object_mut().expect("object").remove(key).is_some() {
            removed_right += 1;
        }
    }
    assert_eq!(
        removed_left, 3,
        "the draft must carry exactly three envelope keys to strip"
    );
    assert_eq!(
        removed_right, 3,
        "the persisted row must carry exactly three envelope keys; a new one fails here instead of being compared"
    );
    assert_eq!(left, right, "the payload must be identical on both sides");
}

/// CASE A: a row with a parent. The persisted row carries `parentId` and it is
/// non-empty, and the payload matches the sealed draft exactly.
#[test]
fn the_sealed_entry_matches_the_persisted_row_for_a_parented_assistant_row() {
    let (_dir, mut store) = fixture();
    let epoch = epoch();
    store.epoch = Some(EpochHandle(Arc::clone(&epoch)));
    // A parent the assistant row can hang from.
    store
        .persist_entry("message", user_row())
        .expect("user row");
    let step = epoch
        .admit_provider_step(Arc::new(String::from("actual provider step")))
        .expect("admitted step");
    store
        .persist_message_with_step(assistant_row(), &step)
        .expect("assistant row");
    let entry = store.entries.last().expect("persisted row").clone();
    let persisted = serde_json::to_value(&entry).expect("row value");
    let draft = super::canonical_adapter::draft_for_row(&entry).expect("draft");
    let draft_entry = serde_json::to_value(draft.entry()).expect("draft value");
    assert_same_payload(&draft_entry, &persisted);
    let parent = persisted["parentId"].as_str().expect("parented row");
    assert!(!parent.is_empty(), "a parented row's parentId is non-empty");
    assert!(
        persisted["id"].as_str().is_some_and(|id| !id.is_empty()),
        "the store assigned an id"
    );
    assert!(
        persisted["timestamp"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "the store assigned a timestamp"
    );
}

/// CASE B: the session's FIRST row. `parentId` is absent or null - asserted as the
/// expected shape rather than skipped, so a conditional cannot become a loophole.
#[test]
fn the_sealed_entry_matches_the_persisted_row_for_the_first_row() {
    let (_dir, mut store) = fixture();
    let epoch = epoch();
    store.epoch = Some(EpochHandle(Arc::clone(&epoch)));
    let step = epoch
        .admit_provider_step(Arc::new(String::from("actual provider step")))
        .expect("admitted step");
    store
        .persist_message_with_step(assistant_row(), &step)
        .expect("first row");
    let entry = store.entries.last().expect("persisted row").clone();
    let persisted = serde_json::to_value(&entry).expect("row value");
    let draft = super::canonical_adapter::draft_for_row(&entry).expect("draft");
    let draft_entry = serde_json::to_value(draft.entry()).expect("draft value");
    assert_same_payload(&draft_entry, &persisted);
    assert!(
        persisted
            .get("parentId")
            .is_none_or(serde_json::Value::is_null),
        "the session's first row has no parent, and that absence is the asserted shape"
    );
    assert!(
        persisted["id"].as_str().is_some_and(|id| !id.is_empty()),
        "the store assigned an id"
    );
}
