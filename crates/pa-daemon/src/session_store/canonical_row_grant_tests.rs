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

/// An assistant row is DECLARED OUTSIDE and counted: it is written, but no grant
/// covers it, because a terminal row is only admissible through
/// `ProviderStep::seal_assistant` - a provider-step seal the daemon's generic row
/// loop cannot produce. Downgrading it to a prepare flight would vouch for content
/// nobody admitted, so the counter states the fact instead.
#[test]
fn an_assistant_row_is_declared_outside_and_counted() {
    let (_dir, mut store) = fixture();
    let epoch = epoch();
    store.epoch = Some(EpochHandle(Arc::clone(&epoch)));
    store
        .persist_entry("message", assistant_row())
        .expect("assistant row still lands");
    assert_eq!(
        store.rows_written_without_epoch(),
        1,
        "an assistant row is declared outside, so the count moves by exactly one"
    );
    assert!(
        !epoch.poisoned(),
        "nothing poisoned the epoch: no flight was opened"
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
