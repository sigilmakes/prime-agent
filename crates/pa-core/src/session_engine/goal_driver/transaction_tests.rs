//! Goal transactions do not publish candidates or alias pending flags before ACK.
use super::*;
use std::sync::Arc;
use tokio::sync::Mutex;

#[test]
fn staging_changes_neither_live_state_nor_pending_flag() {
    let mut session = SessionManager::in_memory(std::path::Path::new("/tmp"));
    let mut driver = GoalDriver::new();
    driver.start(&mut session, "finish work", None).unwrap();
    let before = driver.state().clone();
    let pending = driver.pending_continuation_handle();
    let plan = driver
        .prepare_mutation(|candidate, drafts| candidate.next_continuation_message(drafts, None))
        .unwrap();
    assert_eq!(driver.state(), &before);
    assert!(!pending.load(std::sync::atomic::Ordering::SeqCst));
    assert!(plan.candidate.pending_continuation());
    assert_eq!(plan.drafts.entries.len(), 1);
    assert!(plan
        .drafts
        .entries
        .iter()
        .all(|entry| { entry.id().is_none() && entry.parent_id().is_none() }));
    assert!(driver.ensure_ready().is_err());
    drop(plan);
    assert_eq!(driver.state(), &before);
    assert!(driver.ensure_ready().is_err(), "lost plan stays blocked");
}

struct CheckingWriter {
    driver: Arc<Mutex<GoalDriver>>,
    session: Arc<Mutex<SessionManager>>,
    fail: bool,
}

impl crate::session::canonical::CanonicalMetadataWriter for CheckingWriter {
    fn observe_metadata_source(self: Arc<Self>) -> crate::session::canonical::CanonicalFuture<()> {
        Box::pin(async move {
            // This mock checks pre-policy ordering and released locks only.
            // Physical owner observation/adoption is covered by live-owner tests.
            let driver = self
                .driver
                .try_lock()
                .expect("goal lock released before source observation");
            driver
                .ensure_ready()
                .expect("source observation precedes goal policy preparation");
            assert_eq!(driver.state().status, GoalStatus::Idle);
            drop(driver);
            let _session = self
                .session
                .try_lock()
                .expect("SDK lock released before source observation");
            Ok(())
        })
    }

    fn append_metadata(
        self: Arc<Self>,
        _entry: pa_types::session::FileEntry,
    ) -> crate::session::canonical::CanonicalFuture<String> {
        panic!("precomputed goal policy must use the strict source seam")
    }

    fn append_metadata_from(
        self: Arc<Self>,
        entry: pa_types::session::FileEntry,
        expected: crate::session::canonical::CanonicalCursor,
    ) -> crate::session::canonical::CanonicalFuture<
        crate::session::canonical::CanonicalMetadataReceipt,
    > {
        Box::pin(async move {
            // This seam mock checks transaction locking/publication only.
            // Canonical owner raw adoption + ACK are covered by live-owner tests.
            let driver = self
                .driver
                .try_lock()
                .expect("goal lock released before owner");
            assert!(driver.ensure_ready().is_err());
            assert_eq!(driver.state().status, GoalStatus::Idle);
            drop(driver);
            let session = self
                .session
                .try_lock()
                .expect("SDK lock released before owner");
            assert_eq!(session.canonical_binding(), Some(&expected));
            assert!(entry.id().is_none());
            if self.fail {
                return Err(crate::session::canonical::CanonicalStoreError {
                    kind: crate::session::canonical::CanonicalFailureKind::Io,
                    requested: crate::session::canonical::CanonicalRequestedWrite::Unknown,
                    source: crate::session::canonical::CanonicalSourceDisposition::NeedsReconcile,
                    detail: "injected receipt failure".to_owned(),
                });
            }
            Ok(crate::session::canonical::CanonicalMetadataReceipt {
                entry_id: "actual-owner-id".to_owned(),
                cursor: expected,
            })
        })
    }
}

/// A real no-I/O checkpoint installation, not an invented manager binding.
fn certified_manager() -> SessionManager {
    use crate::session::canonical::*;
    use crate::session::manager::CanonicalLocalAdoption;
    use sha2::{Digest, Sha256};
    let raw = b"{\"type\":\"session\",\"id\":\"goal-transaction\",\"version\":3,\"timestamp\":\"2026-10-24T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n";
    let header = CanonicalJournalRow::parse(Arc::from(raw.as_slice()), 0, 0).unwrap();
    let digest: [u8; 32] = Sha256::digest(header.raw()).into();
    let cursor = CanonicalCursor {
        owner: CanonicalOwnerScope::new(),
        session_id: "goal-transaction".to_owned(),
        source_path: "/tmp/goal-transaction.jsonl".into(),
        raw_source_generation: digest,
        header_certificate: digest,
        canonical_revision: 1,
        branch_revision: 1,
        selected_leaf: None,
        frontier: CanonicalFrontier {
            bytes: raw.len() as u64,
            rows: 1,
            prefix_certificate: digest,
        },
    };
    let pa_types::session::FileEntry::Header { header: parsed } = header.entry() else {
        panic!("header");
    };
    let mut manager = SessionManager::in_memory_from_header(
        std::path::Path::new("/tmp"),
        std::path::Path::new("/tmp"),
        parsed.clone(),
    )
    .unwrap();
    manager
        .install_canonical_checkpoint(
            &CanonicalOperation::new(),
            manager.canonical_local_revision(),
            None,
            CanonicalCheckpoint {
                cursor,
                header,
                source: CanonicalSource::Full(Vec::new()),
            },
            CanonicalLocalAdoption::preserve(),
        )
        .unwrap();
    manager
}

#[tokio::test]
async fn owner_failure_preserves_old_state_and_blocks_next_mutation() {
    let driver = Arc::new(Mutex::new(GoalDriver::new()));
    let session = Arc::new(Mutex::new(certified_manager()));
    let writer: Arc<dyn crate::session::canonical::CanonicalMetadataWriter> =
        Arc::new(CheckingWriter {
            driver: driver.clone(),
            session: session.clone(),
            fail: true,
        });
    session
        .lock()
        .await
        .bind_canonical_metadata_writer(Arc::downgrade(&writer));
    let result = persist_goal_mutation(&driver, &session, |driver, drafts| {
        driver.start(drafts, "finish work", None)
    })
    .await;
    assert!(result.is_err());
    let driver = driver.lock().await;
    assert_eq!(driver.state(), &empty_goal_state());
    assert!(driver.ensure_ready().is_err());
}

#[tokio::test]
async fn successful_writer_installs_candidate_and_keeps_pending_handle_identity() {
    let driver = Arc::new(Mutex::new(GoalDriver::new()));
    let session = Arc::new(Mutex::new(certified_manager()));
    let pending = driver.lock().await.pending_continuation_handle();
    let writer: Arc<dyn crate::session::canonical::CanonicalMetadataWriter> =
        Arc::new(CheckingWriter {
            driver: driver.clone(),
            session: session.clone(),
            fail: false,
        });
    session
        .lock()
        .await
        .bind_canonical_metadata_writer(Arc::downgrade(&writer));
    let state = persist_goal_mutation(&driver, &session, |driver, drafts| {
        driver.start(drafts, "finish work", None)
    })
    .await
    .unwrap();
    let driver = driver.lock().await;
    assert_eq!(driver.state(), &state);
    driver.ensure_ready().unwrap();
    assert!(Arc::ptr_eq(&pending, &driver.pending_continuation_handle()));
}

#[test]
fn pending_transaction_refuses_branch_reload_without_changing_live_state() {
    let session = SessionManager::in_memory(std::path::Path::new("/tmp"));
    let mut driver = GoalDriver::new();
    let before = driver.state().clone();
    let _plan = driver
        .prepare_mutation(|candidate, drafts| candidate.start(drafts, "finish work", None))
        .unwrap();
    assert!(driver
        .reload_from_branch(&session, GoalBranchReload::FaithfulBranch)
        .is_err());
    assert_eq!(driver.state(), &before);
}

#[tokio::test]
async fn uncertified_live_writer_refuses_before_running_goal_policy() {
    let driver = Arc::new(Mutex::new(GoalDriver::new()));
    let session = Arc::new(Mutex::new(SessionManager::in_memory(std::path::Path::new(
        "/tmp",
    ))));
    let writer: Arc<dyn crate::session::canonical::CanonicalMetadataWriter> =
        Arc::new(CheckingWriter {
            driver: driver.clone(),
            session: session.clone(),
            fail: false,
        });
    session
        .lock()
        .await
        .bind_canonical_metadata_writer(Arc::downgrade(&writer));
    let result = persist_goal_mutation::<()>(&driver, &session, |_, _| {
        panic!("uncertified policy must not run")
    })
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("certified canonical source"));
    driver.lock().await.ensure_ready().unwrap();
}

#[test]
fn multiple_state_transitions_keep_ordered_envelope_free_drafts() {
    let mut session = SessionManager::in_memory(std::path::Path::new("/tmp"));
    let mut driver = GoalDriver::new();
    driver.start(&mut session, "finish work", None).unwrap();
    let before = driver.state().clone();
    let plan = driver
        .prepare_mutation(|candidate, drafts| {
            candidate.pause(drafts, "pause first")?;
            candidate.resume(drafts)?;
            Ok(())
        })
        .unwrap();
    let states: Vec<GoalState> = plan
        .drafts
        .entries
        .iter()
        .map(|entry| {
            let pa_types::session::FileEntry::Custom { payload, .. } = entry else {
                panic!("goal state draft");
            };
            assert!(entry.id().is_none());
            serde_json::from_value(payload.data.clone().unwrap()).unwrap()
        })
        .collect();
    assert_eq!(
        states.iter().map(|state| state.status).collect::<Vec<_>>(),
        vec![GoalStatus::Paused, GoalStatus::Active]
    );
    assert_eq!(driver.state(), &before);
}

#[test]
fn unresolved_handoff_replaces_pending_plan_token_without_mutating_goal() {
    let mut driver = GoalDriver::new();
    let before = driver.state().clone();
    let plan = driver
        .prepare_mutation(|candidate, drafts| candidate.start(drafts, "finish work", None))
        .unwrap();
    driver.block_unresolved_handoff();
    assert!(!Arc::ptr_eq(
        driver.mutation_flight.as_ref().unwrap(),
        &plan.flight
    ));
    assert_eq!(driver.state(), &before);
    assert!(!driver.pending_continuation());
    assert!(driver.ensure_ready().is_err());
}
