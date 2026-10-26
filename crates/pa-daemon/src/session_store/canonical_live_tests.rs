//! Real lease/file regressions for the owner initiated canonical seam.
mod preflight;
mod runtime;
use super::SessionFile;
use pa_core::session::canonical::{
    CanonicalAckPhase, CanonicalAcknowledgement, CanonicalAdmissionPolicy, CanonicalCallerGuard,
    CanonicalCommitDisposition, CanonicalEntryDraft, CanonicalMutationKind, CanonicalOperation,
    CanonicalPrepareMode, CanonicalPrepared, CanonicalSourceRequest,
};
use pa_core::session::manager::{CanonicalLocalAdoption, SessionManager};
use pa_core::session_engine::canonical_live::{
    CanonicalLiveAppendRequest, CanonicalLivePrepareRequest, CanonicalLiveUserRequest,
};
use pa_types::session::FileEntry;
use serde_json::json;
use std::sync::Arc;

fn fixture() -> (tempfile::TempDir, SessionFile) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = SessionFile::create("/isolated", None, 0);
    store.set_path(dir.path().join("session.jsonl"));
    store.rewrite().unwrap();
    store.lease = Some(Arc::new(
        crate::lease::acquire_runtime_session_lease(&store.path, dir.path()).unwrap(),
    ));
    (dir, store)
}

fn request(mode: CanonicalPrepareMode) -> CanonicalLivePrepareRequest {
    CanonicalLivePrepareRequest {
        expected: None,
        source: CanonicalSourceRequest::Full,
        mode,
        admission: CanonicalAdmissionPolicy::Preserve,
    }
}

fn mirror(store: &SessionFile) -> SessionManager {
    SessionManager::in_memory_from_header(
        std::path::Path::new("/isolated"),
        std::path::Path::new("/isolated/artifacts"),
        store.header.clone(),
    )
    .unwrap()
}

fn adopt_prepared(
    store: &mut SessionFile,
    manager: &mut SessionManager,
    prepared: CanonicalPrepared,
) -> CanonicalCallerGuard {
    let binding = manager.canonical_binding().cloned();
    let cursor = prepared.checkpoint.cursor.clone();
    let certificate = manager
        .install_canonical_checkpoint(
            &prepared.operation,
            manager.canonical_local_revision(),
            binding.as_ref(),
            prepared.checkpoint,
            CanonicalLocalAdoption::preserve(),
        )
        .unwrap();
    store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation: prepared.operation,
            cursor,
            phase: CanonicalAckPhase::Prepared,
            adoption: Some(certificate),
        })
        .unwrap();
    prepared.caller
}

fn custom_draft() -> CanonicalEntryDraft {
    CanonicalEntryDraft::new(
        CanonicalMutationKind::OwnerMetadata,
        serde_json::from_value(
            json!({"type":"custom", "customType":"exact", "data":{"native":"AA==","id":"opaque"}}),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn owner_scope_starts_only_at_lease_attach_and_survives_clones() {
    let (_dir, mut store) = fixture();
    assert!(!store.is_canonical_bound());
    let cursor = store.attach_canonical_owner().unwrap();
    let mut clone = store.clone();
    assert_eq!(clone.attach_canonical_owner().unwrap(), cursor);
    let reopened = SessionFile::open(&store.path).unwrap();
    assert!(!reopened.is_canonical_bound());
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    assert!(clone
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_err());
    drop(prepared);
    let error = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .err()
        .expect("abandoned caller must refuse preparation");
    assert_eq!(error.detail, "canonical owner adoption abandoned");
}

#[test]
fn exact_metadata_suffix_blocks_all_writes_until_core_adoption_ack() {
    let (_dir, mut store) = fixture();
    let mut manager = mirror(&store);
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Mutation(
            CanonicalMutationKind::OwnerMetadata,
        )))
        .unwrap();
    let operation = prepared.operation.clone();
    let from = prepared.checkpoint.cursor.clone();
    let caller = adopt_prepared(&mut store, &mut manager, prepared);
    let prefix = std::fs::read(&store.path).unwrap();
    let disposition = store
        .append_canonical(CanonicalLiveAppendRequest {
            context_admission: None,
            run: None,
            operation: operation.clone(),
            expected: from.clone(),
            source: CanonicalSourceRequest::Full,
            draft: custom_draft(),
        })
        .unwrap();
    let CanonicalCommitDisposition::Committed(receipt) = disposition else {
        panic!("commit");
    };
    assert_eq!(receipt.from, from);
    assert_eq!(receipt.rows.len(), 1);
    assert_eq!(receipt.rows[0].byte_offset(), prefix.len() as u64);
    let suffix = receipt.rows[0].raw().to_vec();
    assert_eq!(
        std::fs::read(&store.path).unwrap(),
        [prefix.as_slice(), suffix.as_slice()].concat()
    );
    assert_eq!(receipt.to.canonical_revision, from.canonical_revision + 1);
    assert_eq!(store.rows_written_without_epoch(), 0);
    assert!(store
        .persist_entry("custom", json!({"customType":"bypass"}))
        .is_err());
    assert!(store.rewrite().is_err());
    let error = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .err()
        .expect("pending adoption must refuse preparation");
    assert_eq!(
        error.detail,
        "canonical owner awaiting adoption acknowledgement"
    );
    let cursor = receipt.to.clone();
    let certificate = manager
        .apply_canonical_suffix(
            manager.canonical_local_revision(),
            receipt,
            CanonicalLocalAdoption::preserve(),
        )
        .unwrap();
    assert!(store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation: CanonicalOperation::new(),
            cursor: cursor.clone(),
            phase: CanonicalAckPhase::AppliedReady,
            adoption: Some(certificate.clone())
        })
        .is_err());
    store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation,
            cursor,
            phase: CanonicalAckPhase::AppliedReady,
            adoption: Some(certificate),
        })
        .unwrap();
    caller.complete();
    assert!(store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_ok());
}

#[test]
fn accepted_user_is_durable_once_and_replayed_only_by_fifo_with_original_timestamp() {
    let (_dir, mut store) = fixture();
    let initial = store.attach_canonical_owner().unwrap();
    let id = store
        .persist_delivered_user(json!({"role":"user","content":"same body","timestamp":7}))
        .unwrap();
    assert!(store
        .prepare_canonical(CanonicalLivePrepareRequest {
            expected: Some(initial),
            source: CanonicalSourceRequest::Full,
            mode: CanonicalPrepareMode::Observe,
            admission: CanonicalAdmissionPolicy::Append(vec!["not-delivered".into()])
        })
        .is_err());
    let delivered = store
        .admit_canonical_user(CanonicalLiveUserRequest {
            expected: Some(store.canonical_snapshot().unwrap().checkpoint.cursor),
            source: CanonicalSourceRequest::Full,
            message: serde_json::from_value(json!({"content":"same body","timestamp":99})).unwrap(),
        })
        .unwrap();
    assert_eq!(delivered.admitted_entry_id, id);
    let rows = match &delivered.prepared.checkpoint.source {
        pa_core::session::canonical::CanonicalSource::Full(rows) => rows,
        pa_core::session::canonical::CanonicalSource::Window(_) => panic!("full"),
    };
    assert_eq!(rows.len(), 1);
    assert!(
        matches!(rows[0].entry(), FileEntry::Message { message: pa_types::session::AgentMessage::User(actual), .. } if actual.timestamp == 7)
    );
    assert_eq!(SessionFile::open(&store.path).unwrap().entries().len(), 1);
    drop(delivered);
    assert!(store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_err());
}

#[test]
fn lost_prepared_caller_and_missing_certificate_fail_closed_without_more_bytes() {
    let (_dir, mut store) = fixture();
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    let bytes = std::fs::read(&store.path).unwrap();
    assert!(store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation: prepared.operation.clone(),
            cursor: prepared.checkpoint.cursor.clone(),
            phase: CanonicalAckPhase::Prepared,
            adoption: None,
        })
        .is_err());
    drop(prepared);
    assert!(store
        .persist_delivered_user(json!({"role":"user","content":"blocked","timestamp":1}))
        .is_err());
    assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
}

#[test]
fn bound_report_keeps_actual_stable_journal_id_and_retry_does_not_append() {
    let (_dir, mut store) = fixture();
    let target = store
        .persist_entry(
            "message",
            json!({"message": {
                "role":"assistant", "content":[], "api":"openai-completions", "provider":"fake",
                "model":"fake", "stopReason":"stop", "timestamp":1,
                "usage": pa_types::ai::Usage::default(),
            }}),
        )
        .unwrap();
    store.attach_canonical_owner().unwrap();
    let usage = pa_types::ai::Usage {
        input: 4,
        ..Default::default()
    };
    store
        .append_child_usage_once("stable-report", &target, usage.clone(), None)
        .unwrap();
    let bytes = std::fs::read(&store.path).unwrap();
    store
        .append_child_usage_once("stable-report", &target, usage.clone(), None)
        .unwrap();
    assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    let rows = match &prepared.checkpoint.source {
        pa_core::session::canonical::CanonicalSource::Full(rows) => rows,
        pa_core::session::canonical::CanonicalSource::Window(_) => panic!("full"),
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.last().unwrap().entry().id(), Some("stable-report"));
    let mut manager = mirror(&store);
    adopt_prepared(&mut store, &mut manager, prepared).complete();
    store
        .append_child_usage_once("stable-report", &target, usage, None)
        .unwrap();
    assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
}

#[test]
fn navigation_aba_keeps_bytes_and_owner_but_advances_both_revisions() {
    let (_dir, mut store) = fixture();
    let mut manager = mirror(&store);
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    let before = prepared.checkpoint.cursor.clone();
    adopt_prepared(&mut store, &mut manager, prepared).complete();
    let bytes = std::fs::read(&store.path).unwrap();
    store.advance_canonical_branch(None).unwrap();
    store.advance_canonical_branch(None).unwrap();
    let after = store.attach_canonical_owner().unwrap();
    assert_eq!(after.owner, before.owner);
    assert_eq!(after.canonical_revision, before.canonical_revision + 2);
    assert_eq!(after.branch_revision, before.branch_revision + 2);
    assert_eq!(after.frontier, before.frontier);
    assert_eq!(after.raw_source_generation, before.raw_source_generation);
    assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
}

#[test]
fn worker_metadata_entrypoint_refuses_user_assistant_tool_and_context_rows() {
    let (_dir, mut store) = fixture();
    store.attach_canonical_owner().unwrap();
    let bytes = std::fs::read(&store.path).unwrap();
    assert!(store
        .persist_owner_metadata(
            "message",
            json!({"message":{
                "role":"user", "content":"not-metadata", "timestamp":1
            }})
        )
        .is_err());
    assert!(store
        .persist_owner_metadata(
            "compaction",
            json!({
                "summary":"not-metadata", "firstKeptEntryId":"a", "tokensBefore":1
            })
        )
        .is_err());
    assert!(store
        .persist_owner_metadata(
            "message",
            json!({"message":{
                "role":"assistant", "content":[], "api":"openai-completions", "provider":"fake",
                "model":"fake", "stopReason":"stop", "timestamp":1,
                "usage":pa_types::ai::Usage::default(),
            }})
        )
        .is_err());
    assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
}

#[test]
fn strict_source_advance_refuses_requested_row_before_io_without_reparenting() {
    use std::io::Write;
    let (_dir, mut store) = fixture();
    let mut manager = mirror(&store);
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Mutation(
            CanonicalMutationKind::OwnerMetadata,
        )))
        .unwrap();
    let operation = prepared.operation.clone();
    let expected = prepared.checkpoint.cursor.clone();
    let caller = adopt_prepared(&mut store, &mut manager, prepared);
    let foreign = b"{\"type\":\"custom\",\"id\":\"external\",\"parentId\":null,\"timestamp\":\"external\",\"customType\":\"external\"}\n";
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&store.path)
        .unwrap();
    file.write_all(foreign).unwrap();
    file.sync_all().unwrap();
    let before = std::fs::read(&store.path).unwrap();
    let Err(error) = store.append_canonical(CanonicalLiveAppendRequest {
        context_admission: None,
        run: None,
        operation,
        expected,
        source: CanonicalSourceRequest::Full,
        draft: custom_draft(),
    }) else {
        panic!("stale source must not write");
    };
    assert_eq!(
        error.requested,
        pa_core::session::canonical::CanonicalRequestedWrite::NotWritten
    );
    assert_eq!(std::fs::read(&store.path).unwrap(), before);
    caller.complete();
}

#[test]
fn ack_visibility_is_capped_by_both_actual_delivery_ledgers() {
    let (_dir, mut store) = fixture();
    let before = store
        .persist_entry("custom", json!({"customType":"before-delivery"}))
        .unwrap();
    let user = store
        .persist_delivered_user(json!({"role":"user","content":"accepted","timestamp":7}))
        .unwrap();
    store
        .persist_delivered_custom(json!({"customType":"queued","content":"notice","display":true}))
        .unwrap();
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    let operation = prepared.operation.clone();
    let cursor = prepared.checkpoint.cursor.clone();
    let mut manager = mirror(&store);
    let certificate = manager
        .install_canonical_checkpoint(
            &operation,
            0,
            None,
            prepared.checkpoint,
            CanonicalLocalAdoption::preserve(),
        )
        .unwrap();
    let proof = store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation: operation.clone(),
            cursor: cursor.clone(),
            phase: CanonicalAckPhase::Prepared,
            adoption: Some(certificate),
        })
        .unwrap();
    assert_eq!(
        (proof.operation, proof.cursor, proof.visible_leaf),
        (operation, cursor.clone(), Some(before))
    );
    prepared.caller.complete();
    let delivered = store
        .admit_canonical_user(CanonicalLiveUserRequest {
            expected: Some(cursor),
            source: CanonicalSourceRequest::Full,
            message: serde_json::from_value(json!({"content":"accepted","timestamp":999})).unwrap(),
        })
        .unwrap();
    assert_eq!(delivered.admitted_entry_id, user);
    let prepared = delivered.prepared;
    let cursor = prepared.checkpoint.cursor.clone();
    let binding = manager.canonical_binding().cloned();
    let certificate = manager
        .install_canonical_checkpoint(
            &prepared.operation,
            manager.canonical_local_revision(),
            binding.as_ref(),
            prepared.checkpoint,
            CanonicalLocalAdoption::preserve(),
        )
        .unwrap();
    let proof = store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation: prepared.operation,
            cursor,
            phase: CanonicalAckPhase::Prepared,
            adoption: Some(certificate),
        })
        .unwrap();
    assert_eq!(proof.visible_leaf, Some(user));
    prepared.caller.complete();
}

#[test]
fn ack_refuses_pending_delivery_not_on_actual_selected_branch() {
    let (_dir, mut store) = fixture();
    store
        .persist_delivered_user(json!({"role":"user","content":"unselected","timestamp":1}))
        .unwrap();
    store.branch_to(None).unwrap();
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    let mut manager = mirror(&store);
    let cursor = prepared.checkpoint.cursor.clone();
    let certificate = manager
        .install_canonical_checkpoint(
            &prepared.operation,
            0,
            None,
            prepared.checkpoint,
            CanonicalLocalAdoption::preserve(),
        )
        .unwrap();
    assert!(store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation: prepared.operation,
            cursor,
            phase: CanonicalAckPhase::Prepared,
            adoption: Some(certificate)
        })
        .is_err());
    assert!(store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_err());
    drop(prepared.caller);
}

#[test]
fn prepared_ack_then_explicit_prepared_only_abandon_does_not_require_pending_receipt() {
    let (_dir, mut store) = fixture();
    let mut manager = mirror(&store);
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Mutation(
            CanonicalMutationKind::ContextPolicy,
        )))
        .unwrap();
    let operation = prepared.operation.clone();
    let cursor = prepared.checkpoint.cursor.clone();
    let caller = adopt_prepared(&mut store, &mut manager, prepared);
    let before = std::fs::read(&store.path).unwrap();
    let proof = store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation: operation.clone(),
            cursor: cursor.clone(),
            phase: CanonicalAckPhase::AbandonPreparedOnly,
            adoption: None,
        })
        .unwrap();
    assert_eq!((proof.operation, proof.cursor), (operation, cursor));
    caller.complete();
    assert!(store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_ok());
    assert_eq!(std::fs::read(&store.path).unwrap(), before);
}

#[test]
fn prepared_only_abandon_survives_owner_delivered_agent_message() {
    let (_dir, mut store) = fixture();
    let mut manager = mirror(&store);
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Mutation(
            CanonicalMutationKind::ContextPolicy,
        )))
        .unwrap();
    let operation = prepared.operation.clone();
    let cursor = prepared.checkpoint.cursor.clone();
    let caller = adopt_prepared(&mut store, &mut manager, prepared);
    // Incoming subagent messages persist through another handle while context
    // work is prepared. This advances the owner, not the prepared operation.
    let mut delivery = store.clone();
    let id = delivery
        .persist_delivered_custom(json!({
            "customType":"agent_message",
            "content":"[agent-message from child] done",
            "display":true
        }))
        .unwrap();
    store.leaf_id = delivery.leaf_id.clone();
    let before = std::fs::read(&store.path).unwrap();
    let error = store
        .append_canonical(CanonicalLiveAppendRequest {
            context_admission: None,
            run: None,
            operation: operation.clone(),
            expected: cursor.clone(),
            source: CanonicalSourceRequest::Full,
            draft: custom_draft(),
        })
        .err()
        .expect("stale preparation must not append");
    assert_eq!(
        error.requested,
        pa_core::session::canonical::CanonicalRequestedWrite::NotWritten
    );
    store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation,
            cursor,
            phase: CanonicalAckPhase::AbandonPreparedOnly,
            adoption: None,
        })
        .expect("certified delivery must not strand an unwritten preparation");
    caller.complete();
    assert_eq!(std::fs::read(&store.path).unwrap(), before);
    let snapshot = store.canonical_snapshot().unwrap();
    assert_eq!(
        snapshot.pending_custom,
        std::collections::VecDeque::from([id])
    );
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    adopt_prepared(&mut store, &mut manager, prepared).complete();
    assert_eq!(std::fs::read(&store.path).unwrap(), before);
}

#[test]
fn prepared_only_abandon_survives_child_usage_attribution() {
    let (_dir, mut store) = fixture();
    let target = store
        .persist_entry(
            "message",
            json!({"message": {
                "role":"assistant", "content":[], "api":"openai-completions", "provider":"fake",
                "model":"fake", "stopReason":"stop", "timestamp":1,
                "usage": pa_types::ai::Usage::default(),
            }}),
        )
        .unwrap();
    let mut manager = mirror(&store);
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Mutation(
            CanonicalMutationKind::OwnerMetadata,
        )))
        .unwrap();
    let operation = prepared.operation.clone();
    let cursor = prepared.checkpoint.cursor.clone();
    let caller = adopt_prepared(&mut store, &mut manager, prepared);
    store
        .append_child_usage_once(
            "child-report",
            &target,
            pa_types::ai::Usage::default(),
            None,
        )
        .unwrap();
    let before = std::fs::read(&store.path).unwrap();
    store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation,
            cursor,
            phase: CanonicalAckPhase::AbandonPreparedOnly,
            adoption: None,
        })
        .expect("child usage must not strand an unwritten preparation");
    caller.complete();
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    adopt_prepared(&mut store, &mut manager, prepared).complete();
    assert_eq!(std::fs::read(&store.path).unwrap(), before);
    assert_eq!(store.entries().last().unwrap().id, "child-report");
}

#[test]
fn prepared_only_abandon_refuses_unowned_source_change() {
    use std::io::Write;
    let (_dir, mut store) = fixture();
    let mut manager = mirror(&store);
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Mutation(
            CanonicalMutationKind::ContextPolicy,
        )))
        .unwrap();
    let operation = prepared.operation.clone();
    let cursor = prepared.checkpoint.cursor.clone();
    let caller = adopt_prepared(&mut store, &mut manager, prepared);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&store.path)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    let before = std::fs::read(&store.path).unwrap();
    assert!(store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation,
            cursor,
            phase: CanonicalAckPhase::AbandonPreparedOnly,
            adoption: None,
        })
        .is_err());
    drop(caller);
    assert!(store.canonical_snapshot().is_err());
    assert_eq!(std::fs::read(&store.path).unwrap(), before);
}

#[test]
fn exact_unwritten_observation_can_be_abandoned_after_caller_drop() {
    let (_dir, mut store) = fixture();
    let before = std::fs::read(&store.path).unwrap();
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    let ack = CanonicalAcknowledgement {
        operation: prepared.operation.clone(),
        cursor: prepared.checkpoint.cursor.clone(),
        phase: CanonicalAckPhase::AbandonPreparedOnly,
        adoption: None,
    };
    let wrong = CanonicalAcknowledgement {
        operation: CanonicalOperation::new(),
        cursor: ack.cursor.clone(),
        phase: ack.phase,
        adoption: None,
    };
    assert!(store.acknowledge_canonical(wrong).is_err());
    drop(prepared);
    store.acknowledge_canonical(ack).unwrap();
    assert_eq!(std::fs::read(&store.path).unwrap(), before);
    let next = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    assert_eq!(next.checkpoint.cursor.frontier.bytes, before.len() as u64);
}

#[test]
fn pending_mutation_cannot_use_observation_abandon() {
    let (_dir, mut store) = fixture();
    let before = std::fs::read(&store.path).unwrap();
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Mutation(
            CanonicalMutationKind::OwnerMetadata,
        )))
        .unwrap();
    assert!(store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation: prepared.operation.clone(),
            cursor: prepared.checkpoint.cursor.clone(),
            phase: CanonicalAckPhase::AbandonPreparedOnly,
            adoption: None,
        })
        .is_err());
    drop(prepared);
    assert!(store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_err());
    assert_eq!(std::fs::read(&store.path).unwrap(), before);
}

#[test]
fn durable_user_pending_cannot_be_abandoned_as_unwritten_observation() {
    let (_dir, mut store) = fixture();
    let delivered = store
        .admit_canonical_user(CanonicalLiveUserRequest {
            expected: None,
            source: CanonicalSourceRequest::Full,
            message: serde_json::from_value(json!({"content":"durable","timestamp":7})).unwrap(),
        })
        .unwrap();
    let bytes = std::fs::read(&store.path).unwrap();
    let id = delivered.admitted_entry_id.clone();
    assert!(store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation: delivered.prepared.operation.clone(),
            cursor: delivered.prepared.checkpoint.cursor.clone(),
            phase: CanonicalAckPhase::AbandonPreparedOnly,
            adoption: None,
        })
        .is_err());
    drop(delivered);
    assert!(store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_err());
    assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
    assert_eq!(SessionFile::open(&store.path).unwrap().entries()[0].id, id);
}

#[test]
fn staged_observation_with_admission_cannot_be_abandoned() {
    let (_dir, mut store) = fixture();
    store.attach_canonical_owner().unwrap();
    let id = store
        .persist_delivered_user(json!({"role":"user","content":"staged","timestamp":7}))
        .unwrap();
    let prepared = store
        .prepare_canonical(CanonicalLivePrepareRequest {
            expected: None,
            source: CanonicalSourceRequest::Full,
            mode: CanonicalPrepareMode::Observe,
            admission: CanonicalAdmissionPolicy::Append(vec![id]),
        })
        .unwrap();
    let bytes = std::fs::read(&store.path).unwrap();
    assert!(store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation: prepared.operation.clone(),
            cursor: prepared.checkpoint.cursor.clone(),
            phase: CanonicalAckPhase::AbandonPreparedOnly,
            adoption: None,
        })
        .is_err());
    drop(prepared);
    assert!(store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_err());
    assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
}

#[test]
fn unwritten_observation_abandon_refuses_external_source_change() {
    use std::io::Write;
    let (_dir, mut store) = fixture();
    let prepared = store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&store.path)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    let changed = std::fs::read(&store.path).unwrap();
    assert!(store
        .acknowledge_canonical(CanonicalAcknowledgement {
            operation: prepared.operation.clone(),
            cursor: prepared.checkpoint.cursor.clone(),
            phase: CanonicalAckPhase::AbandonPreparedOnly,
            adoption: None,
        })
        .is_err());
    drop(prepared);
    assert!(store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_err());
    assert_eq!(std::fs::read(&store.path).unwrap(), changed);
}

#[test]
fn immutable_snapshot_drop_and_rejection_leave_writer_usable_and_bytes_exact() {
    let (_dir, mut store) = fixture();
    let before = store.attach_canonical_owner().unwrap();
    let bytes = std::fs::read(&store.path).unwrap();
    for _ in 0..3 {
        let snapshot = store.canonical_snapshot().unwrap();
        assert_eq!(snapshot.checkpoint.cursor, before);
        assert!(snapshot.pending_users.is_empty());
        assert!(snapshot.pending_custom.is_empty());
        assert!(snapshot.admitted_users.is_empty());
        drop(snapshot);
    }
    assert!(!store.canonical_source_uncertain());
    let id = store
        .persist_owner_metadata(
            "custom",
            json!({"customType":"snapshot-drop","data":{"unknown":[1,true]}}),
        )
        .unwrap();
    let snapshot = store.canonical_snapshot().unwrap();
    assert_eq!(
        snapshot.checkpoint.cursor.canonical_revision,
        before.canonical_revision + 1
    );
    assert_eq!(snapshot.safe_visible_leaf, Some(id));
    assert!(std::fs::read(&store.path).unwrap().starts_with(&bytes));
    drop(snapshot);
    assert!(!store.canonical_source_uncertain());
}

#[test]
fn completed_stages_allow_snapshots_but_pending_fifos_cap_visibility_and_navigation() {
    let (_dir, mut store) = fixture();
    let before = store.attach_canonical_owner().unwrap();
    let user = store
        .persist_delivered_user(json!({"role":"user","content":"accepted","timestamp":7}))
        .unwrap();
    let custom = store
        .persist_delivered_custom(json!({"customType":"queued","content":"note","display":true}))
        .unwrap();
    let snapshot = store.canonical_snapshot().unwrap();
    assert_eq!(
        snapshot.pending_users,
        std::collections::VecDeque::from([user.clone()])
    );
    assert_eq!(
        snapshot.pending_custom,
        std::collections::VecDeque::from([custom])
    );
    assert_eq!(snapshot.safe_visible_leaf, None);
    assert_eq!(
        snapshot.checkpoint.cursor.canonical_revision,
        before.canonical_revision + 2
    );
    let bytes = std::fs::read(&store.path).unwrap();
    assert!(store.advance_canonical_branch(None).is_err());
    let delivered = store
        .admit_canonical_user(CanonicalLiveUserRequest {
            expected: Some(snapshot.checkpoint.cursor.clone()),
            source: CanonicalSourceRequest::Full,
            message: serde_json::from_value(json!({"content":"accepted","timestamp":99})).unwrap(),
        })
        .unwrap();
    assert_eq!(delivered.admitted_entry_id, user);
    assert_eq!(
        delivered.prepared.checkpoint.cursor.canonical_revision,
        snapshot.checkpoint.cursor.canonical_revision + 1
    );
    assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
    // Actual admission reply debt remains guarded, unlike an immutable snapshot.
    assert!(store.canonical_snapshot().is_err());
    drop(delivered);
    assert!(store.canonical_snapshot().is_err());
}

#[test]
fn newly_written_user_admission_advances_once_not_twice() {
    let (_dir, mut store) = fixture();
    let before = store.attach_canonical_owner().unwrap();
    let delivered = store
        .admit_canonical_user(CanonicalLiveUserRequest {
            expected: Some(before.clone()),
            source: CanonicalSourceRequest::Full,
            message: serde_json::from_value(json!({"content":"new","timestamp":7})).unwrap(),
        })
        .unwrap();
    assert_eq!(
        delivered.prepared.checkpoint.cursor.canonical_revision,
        before.canonical_revision + 1
    );
    let pa_core::session::canonical::CanonicalSource::Full(rows) =
        &delivered.prepared.checkpoint.source
    else {
        panic!("full")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].entry().id(),
        Some(delivered.admitted_entry_id.as_str())
    );
}

#[test]
fn initial_sdk_bootstrap_allows_attached_staging_but_not_admission_transfer() {
    let (_dir, mut store) = fixture();
    store.attach_canonical_owner().unwrap();
    store
        .persist_delivered_user(json!({"role":"user","content":"initial","timestamp":7}))
        .unwrap();
    store.validate_initial_canonical_sdk().unwrap();
    let snapshot = store.canonical_snapshot().unwrap();
    let prepared = store
        .prepare_canonical(CanonicalLivePrepareRequest {
            expected: Some(snapshot.checkpoint.cursor),
            source: CanonicalSourceRequest::Full,
            mode: CanonicalPrepareMode::Observe,
            admission: CanonicalAdmissionPolicy::Replace(
                snapshot.pending_users.into_iter().collect(),
            ),
        })
        .unwrap();
    assert!(store.validate_initial_canonical_sdk().is_err());
    drop(prepared);
    assert!(store.validate_initial_canonical_sdk().is_err());
}

#[test]
fn custom_fifo_exact_adoption_keeps_original_row_and_next_snapshot() {
    use pa_core::session_engine::canonical_live::CanonicalLiveCustomRequest;
    let (_dir, mut store) = fixture();
    let mut manager = mirror(&store);
    store.attach_canonical_owner().unwrap();
    let fields = json!({"customType":"queued", "content":[{"type":"text","text":"note"}],
        "display":true, "details":{"opaque":"AA=="}, "futureField":{"ordered":[3,1,2]}});
    let first = store.persist_delivered_custom(fields.clone()).unwrap();
    let second = store.persist_delivered_custom(fields.clone()).unwrap();
    let before = store.canonical_snapshot().unwrap();
    let bytes = std::fs::read(&store.path).unwrap();
    let delivered = store
        .admit_canonical_custom(CanonicalLiveCustomRequest {
            expected: Some(before.checkpoint.cursor.clone()),
            source: CanonicalSourceRequest::Full,
            message: serde_json::from_value({
                let mut message = fields;
                message["timestamp"] = json!(999);
                message
            })
            .unwrap(),
        })
        .unwrap();
    assert_eq!(delivered.admitted_entry_id, first);
    assert_eq!(
        delivered.prepared.checkpoint.cursor.canonical_revision,
        before.checkpoint.cursor.canonical_revision + 1
    );
    let pa_core::session::canonical::CanonicalSource::Full(original) = &before.checkpoint.source
    else {
        panic!("full");
    };
    let pa_core::session::canonical::CanonicalSource::Full(prepared_rows) =
        &delivered.prepared.checkpoint.source
    else {
        panic!("full");
    };
    assert_eq!(
        prepared_rows
            .iter()
            .map(pa_core::session::canonical::CanonicalJournalRow::raw)
            .collect::<Vec<_>>(),
        original
            .iter()
            .map(pa_core::session::canonical::CanonicalJournalRow::raw)
            .collect::<Vec<_>>()
    );
    assert!(store.canonical_snapshot().is_err());
    adopt_prepared(&mut store, &mut manager, delivered.prepared).complete();
    let after = store.canonical_snapshot().unwrap();
    assert_eq!(
        after.pending_custom,
        std::collections::VecDeque::from([second])
    );
    assert_eq!(after.safe_visible_leaf, Some(first));
    let pa_core::session::canonical::CanonicalSource::Full(after_rows) = &after.checkpoint.source
    else {
        panic!("full");
    };
    assert_eq!(
        after_rows
            .iter()
            .map(pa_core::session::canonical::CanonicalJournalRow::raw)
            .collect::<Vec<_>>(),
        original
            .iter()
            .map(pa_core::session::canonical::CanonicalJournalRow::raw)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        after.checkpoint.cursor.canonical_revision,
        before.checkpoint.cursor.canonical_revision + 1
    );
    assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
}

#[test]
fn successful_stage_write_with_failed_capture_keeps_debt_and_exact_bytes() {
    let (_dir, mut store) = fixture();
    store.attach_canonical_owner().unwrap();
    let prefix = std::fs::read(&store.path).unwrap();
    store.fail_next_stage_capture();
    assert!(store
        .persist_delivered_custom(json!({
            "customType":"capture-failure", "content":"once", "display":true
        }))
        .is_err());
    let written = std::fs::read(&store.path).unwrap();
    assert!(written.starts_with(&prefix));
    let reopened = SessionFile::open(&store.path).unwrap();
    assert_eq!(reopened.entries().len(), 1);
    assert!(store.canonical_snapshot().is_err());
    assert!(store
        .persist_delivered_custom(json!({
            "customType":"capture-failure", "content":"once", "display":true
        }))
        .is_err());
    assert!(store
        .persist_owner_metadata("custom", json!({"customType":"blocked"}))
        .is_err());
    assert!(store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_err());
    assert_eq!(std::fs::read(&store.path).unwrap(), written);
}

#[test]
fn completed_owner_retirement_preserves_native_fifos_and_retires_old_scope() {
    let (_dir, mut store) = fixture();
    store.attach_canonical_owner().unwrap();
    let user = store
        .persist_delivered_user(json!({"role":"user","content":"pending","timestamp":7}))
        .unwrap();
    let custom = store.persist_delivered_custom(json!({"customType":"pending", "content":"note", "display":true, "unknown":{"raw":[3,1]}})).unwrap();
    let snapshot = store.canonical_snapshot().unwrap();
    let mut old = store.clone();
    let bytes = std::fs::read(&store.path).unwrap();
    store
        .retire_completed_canonical_runtime(&snapshot.checkpoint.cursor)
        .unwrap();
    assert!(old.canonical_snapshot().is_err());
    assert!(old
        .persist_delivered_user(json!({"role":"user","content":"late","timestamp":8}))
        .is_err());
    let fresh = store.attach_canonical_owner().unwrap();
    assert_ne!(fresh.owner, snapshot.checkpoint.cursor.owner);
    store.validate_initial_canonical_sdk().unwrap();
    let after = store.canonical_snapshot().unwrap();
    assert_eq!(after.pending_users, snapshot.pending_users);
    assert_eq!(after.pending_custom, snapshot.pending_custom);
    assert_eq!(
        after.pending_users.into_iter().collect::<Vec<_>>(),
        vec![user]
    );
    assert_eq!(
        after.pending_custom.into_iter().collect::<Vec<_>>(),
        vec![custom]
    );
    assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
}

#[test]
fn completed_owner_retirement_refuses_unacknowledged_mutation() {
    let (_dir, mut store) = fixture();
    let cursor = store.attach_canonical_owner().unwrap();
    let pending = store
        .prepare_canonical(request(CanonicalPrepareMode::Mutation(
            CanonicalMutationKind::OwnerMetadata,
        )))
        .unwrap();
    assert!(store.retire_completed_canonical_runtime(&cursor).is_err());
    drop(pending);
    assert!(store.retire_completed_canonical_runtime(&cursor).is_err());
    assert!(store.attach_canonical_owner().is_err());
}
