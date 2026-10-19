//! Cross-crate joint test: the daemon records, Core drains.
//!
//! The ticket is real and comes from Core's owner authority through the
//! non-default `canonical-integration-test-support` feature. The bytes are real
//! (open, write, `sync_all` on a temp file). What this file proves is the joint:
//! one classification per flight, drain pending until the real flight drops,
//! Drained only after the ordered boundary, and a fail-closed path when the
//! recorder is dropped unclassified.
//!
//! It does not prove production wiring: `session_store::write.rs` does not yet
//! take a ticket, so the bytes here go through `canonical_io::append_rows_durable`
//! rather than through the session-store writer's internals.
//!
//! A genuine partial-prefix or fsync failure needs a fault-injection device this
//! tree does not have; that mapping is covered by `canonical_io::tests`, and the
//! fail-closed side is covered here by the unrecorded-drop case.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pa_core::session::canonical::{
    CanonicalCursor, CanonicalEntryDraft, CanonicalFrontier, CanonicalIoStage,
    CanonicalMutationKind, CanonicalOwnerScope,
};
use pa_core::session_engine::canonical_coordinator::{CanonicalTestDrain, CanonicalTestEpoch};

use super::canonical_io::{append_rows_durable, IoOutcome};
use super::canonical_write_seam::commit_canonical_rows;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn source() -> CanonicalCursor {
    CanonicalCursor {
        owner: CanonicalOwnerScope::new(),
        session_id: "joint-session".to_owned(),
        source_path: std::path::PathBuf::from("/private/joint.jsonl"),
        raw_source_generation: [1; 32],
        header_certificate: [2; 32],
        canonical_revision: 0,
        branch_revision: 1,
        selected_leaf: None,
        frontier: CanonicalFrontier {
            bytes: 9,
            rows: 1,
            prefix_certificate: [3; 32],
        },
    }
}

fn draft() -> CanonicalEntryDraft {
    CanonicalEntryDraft::new(
        CanonicalMutationKind::ContextPolicy,
        serde_json::from_value(serde_json::json!({
            "type": "compaction",
            "summary": "joint",
            "firstKeptEntryId": "a",
            "tokensBefore": 1,
            "details": {"bytes": "AA=="}
        }))
        .expect("native entry"),
    )
    .expect("draft")
}

fn epoch() -> CanonicalTestEpoch {
    let mut epoch = CanonicalTestEpoch::new(source()).expect("epoch");
    epoch
        .capture_serving(
            Arc::new(String::from("run")),
            Arc::new(String::from("model")),
        )
        .expect("serving capture");
    epoch
}

fn scratch_path(tag: &str) -> std::path::PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "pa-canonical-joint-{tag}-{}-{unique}",
        std::process::id()
    ))
}

#[test]
fn joint_durable_write_records_known_and_drains_only_after_the_real_flight_drops() {
    let mut epoch = epoch();
    let cursor = epoch.cursor();
    let (operation, ticket) = epoch.final_ticket(draft()).expect("final ticket");
    let path = scratch_path("durable");

    let outcome = commit_canonical_rows(
        ticket,
        CanonicalIoStage::RequestedMutation,
        &cursor,
        &operation,
        &draft(),
        &path,
        b"{\"row\":\"joint\"}\n",
    )
    .expect("daemon write");

    assert_eq!(outcome, IoOutcome::DurableOk);
    assert_eq!(
        std::fs::read(&path).expect("bytes"),
        b"{\"row\":\"joint\"}\n"
    );
    // Still serving: projection is permitted until the close is requested.
    assert!(epoch.projection_allowed());
    std::fs::remove_file(&path).expect("cleanup");

    epoch.request_close().expect("close");
    // The serving anchor is still registered at close time: the drain is Pending
    // on the ordered boundary, not Drained, until the anchor finishes.
    assert_eq!(
        epoch.close_status(),
        CanonicalTestDrain::Pending {
            io: 0,
            transactions: 0,
            anchors: 1
        }
    );
    epoch.finish_anchor();
    assert_eq!(epoch.close_status(), CanonicalTestDrain::Drained);
    // A retired epoch no longer projects: `projection_allowed` is a projection
    // permission, not a readiness or drain predicate.
    assert!(!epoch.projection_allowed());
    epoch.finish_at_external_idle().expect("ordered boundary");
}

#[test]
fn joint_held_flight_keeps_the_scope_pending_until_the_ticket_drops() {
    let mut epoch = epoch();
    let cursor = epoch.cursor();
    let (operation, ticket) = epoch.final_ticket(draft()).expect("final ticket");
    let flight = ticket
        .permit
        .enter_io(
            CanonicalIoStage::RequestedMutation,
            &cursor,
            &operation,
            Some(&draft()),
        )
        .expect("flight");
    let recorder = ticket
        .recorder(CanonicalIoStage::RequestedMutation)
        .expect("recorder");

    epoch.request_close().expect("close");
    assert_eq!(
        epoch.close_status(),
        CanonicalTestDrain::Pending {
            io: 1,
            transactions: 1,
            anchors: 1
        }
    );

    recorder
        .record(pa_core::session_engine::canonical_coordinator::CanonicalWriteCertainty::Known)
        .expect("record");
    drop(flight);
    drop(ticket);
    epoch.finish_anchor();
    assert_eq!(epoch.close_status(), CanonicalTestDrain::Drained);
    epoch.finish_at_external_idle().expect("ordered boundary");
}

#[test]
fn joint_unrecorded_recorder_drop_blocks_the_epoch_and_denies_projection() {
    let mut epoch = epoch();
    let cursor = epoch.cursor();
    let (operation, ticket) = epoch.final_ticket(draft()).expect("final ticket");
    let flight = ticket
        .permit
        .enter_io(
            CanonicalIoStage::RequestedMutation,
            &cursor,
            &operation,
            Some(&draft()),
        )
        .expect("flight");
    let recorder = ticket
        .recorder(CanonicalIoStage::RequestedMutation)
        .expect("recorder");
    drop(recorder);
    drop(flight);
    drop(ticket);

    epoch.request_close().expect("close");
    assert_eq!(epoch.close_status(), CanonicalTestDrain::Blocked);
    assert!(!epoch.projection_allowed());
}

#[test]
fn joint_second_recorder_for_the_same_stage_is_refused() {
    let epoch = epoch();
    let cursor = epoch.cursor();
    let (operation, ticket) = epoch.final_ticket(draft()).expect("final ticket");
    let flight = ticket
        .permit
        .enter_io(
            CanonicalIoStage::RequestedMutation,
            &cursor,
            &operation,
            Some(&draft()),
        )
        .expect("flight");
    let first = ticket
        .recorder(CanonicalIoStage::RequestedMutation)
        .expect("recorder");
    let second = ticket.recorder(CanonicalIoStage::RequestedMutation);
    assert!(
        second.is_err(),
        "a second recorder for one flight must refuse"
    );
    first
        .record(pa_core::session_engine::canonical_coordinator::CanonicalWriteCertainty::Known)
        .expect("record");
    drop(flight);
    drop(ticket);
}

#[test]
fn joint_refusal_before_the_first_byte_is_known_and_does_not_block_drain() {
    let mut epoch = epoch();
    let cursor = epoch.cursor();
    let (operation, ticket) = epoch.final_ticket(draft()).expect("final ticket");
    let path = scratch_path("refused").join("missing").join("rows.jsonl");

    let outcome = commit_canonical_rows(
        ticket,
        CanonicalIoStage::RequestedMutation,
        &cursor,
        &operation,
        &draft(),
        &path,
        b"row\n",
    )
    .expect("daemon write");

    assert_eq!(outcome, IoOutcome::RefusedBeforeFirstByte);
    assert!(!path.exists());
    epoch.request_close().expect("close");
    epoch.finish_anchor();
    assert_eq!(epoch.close_status(), CanonicalTestDrain::Drained);
    epoch.finish_at_external_idle().expect("ordered boundary");
}

#[test]
fn joint_real_bytes_land_before_the_flight_drops() {
    let path = scratch_path("ordering");
    let outcome = append_rows_durable(&path, b"first\n");
    assert_eq!(outcome, IoOutcome::DurableOk);
    assert_eq!(std::fs::read(&path).expect("bytes"), b"first\n");
    std::fs::remove_file(&path).expect("cleanup");
}
