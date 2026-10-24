//! Real lease/file regressions for the owner initiated canonical seam.
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
    assert!(store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_err());
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
    assert!(store
        .prepare_canonical(request(CanonicalPrepareMode::Observe))
        .is_err());
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
            expected: Some(initial.clone()),
            source: CanonicalSourceRequest::Full,
            mode: CanonicalPrepareMode::Observe,
            admission: CanonicalAdmissionPolicy::Append(vec!["not-delivered".into()])
        })
        .is_err());
    let delivered = store
        .admit_canonical_user(CanonicalLiveUserRequest {
            expected: Some(initial),
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
            expected: Some(cursor.clone()),
            source: CanonicalSourceRequest::Full,
            message: serde_json::from_value(json!({"content":"accepted","timestamp":999})).unwrap(),
        })
        .unwrap();
    assert_eq!(delivered.admitted_entry_id, user);
    let prepared = delivered.prepared;
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
