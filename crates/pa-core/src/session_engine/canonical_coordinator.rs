//! Staged canonical transactions. No runtime or context callback is wired here.
//!
//! The local adapter is bound to the actual captured serving run. It performs
//! whole-candidate NOIO validation. Publication needs a separate owned grant;
//! receiving or adopting a raw receipt never proves permission to publish.

pub use writer_hook::{CanonicalWriteCertainty, CanonicalWriteRecorder, CanonicalWriteTicket};
mod writer_hook;

#[cfg(feature = "canonical-integration-test-support")]
pub use integration_test_support::{CanonicalTestDrain, CanonicalTestEpoch};
#[cfg(feature = "canonical-integration-test-support")]
pub mod integration_test_support;

use std::sync::Arc;

mod epoch;
pub use epoch::{
    CanonicalAdmittedTool, CanonicalAppendGrant, CanonicalClosing, CanonicalDrainView,
    CanonicalEpoch, CanonicalOwnerEpoch, CanonicalOwnerRun, CanonicalProviderStep,
    CanonicalRetiredEpoch, MetadataAction,
};

use crate::session::canonical::{
    CanonicalAckPhase, CanonicalAcknowledgement, CanonicalAdmissionPolicy,
    CanonicalAdoptionCertificate, CanonicalAppendReceipt, CanonicalCallerGuard,
    CanonicalCheckpoint, CanonicalCommitDisposition, CanonicalCommitRequest, CanonicalCursor,
    CanonicalEntryDraft, CanonicalFailureKind, CanonicalMutationKind, CanonicalOperation,
    CanonicalPrepareMode, CanonicalPrepareRequest, CanonicalPrepared, CanonicalRequestedWrite,
    CanonicalSessionStore, CanonicalSourceDisposition, CanonicalSourceRequest, CanonicalStoreError,
    CanonicalWritePermit,
};

/// Owner-approved policy. Only successful matching preparation creates this value.
/// The local adapter additionally binds its actual captured run/serving epoch.
pub struct CanonicalPreparedApproval {
    operation: CanonicalOperation,
    cursor: CanonicalCursor,
    admission: CanonicalAdmissionPolicy,
}
impl CanonicalPreparedApproval {
    #[must_use]
    pub fn operation(&self) -> &CanonicalOperation {
        &self.operation
    }
    #[must_use]
    pub fn cursor(&self) -> &CanonicalCursor {
        &self.cursor
    }
    #[must_use]
    pub fn admission(&self) -> &CanonicalAdmissionPolicy {
        &self.admission
    }
}

/// Synchronous local gate. Implementations release all SDK/registry/Agent locks
/// before returning. No method may perform journal I/O or invoke policy callbacks.
pub trait CanonicalLocalState: Send + Sync + 'static {
    /// Actual owned publication authority, not a bool or a raw adoption certificate.
    type PublicationGrant: Send;
    /// Sealed non-Clone local view/admission plan, minted only after owner success.
    type PreparedPlan: Send;
    /// Bind successful matching owner approval to the actual local serving run.
    /// This executes before moving the certified checkpoint into NOIO adoption.
    /// # Errors
    /// Rejects a mismatched policy/operation/cursor or expired local run.
    fn approve_prepared(
        &self,
        owner: CanonicalPreparedApproval,
        prepared: &CanonicalPrepared,
    ) -> Result<Self::PreparedPlan, CanonicalStoreError>;
    /// Validate owner approval, raw source, view, FIFO/seen ledger and local CAS;
    /// install the complete candidate atomically or change nothing.
    /// # Errors
    /// Refuses stale, malformed, foreign, or unauthorized local candidates.
    fn install_prepared(
        &self,
        approval: Self::PreparedPlan,
        checkpoint: CanonicalCheckpoint,
    ) -> Result<CanonicalAdoptionCertificate, CanonicalStoreError>;
    /// Adopt exact raw data and permitted accounting only. This does NOT publish.
    /// # Errors
    /// Refuses a stale or invalid complete suffix before any local mutation.
    fn adopt_receipt(
        &self,
        receipt: CanonicalAppendReceipt,
    ) -> Result<CanonicalAdoptionCertificate, CanonicalStoreError>;
    /// The actual serving-epoch gate consumes the grant and performs permitted
    /// publication atomically. Closing must refuse, never silently reopen.
    /// # Errors
    /// Refuses expired authority, local CAS conflict, or disallowed publication.
    fn publish(
        &self,
        adoption: CanonicalAdoptionCertificate,
        grant: Self::PublicationGrant,
    ) -> Result<CanonicalAdoptionCertificate, CanonicalStoreError>;
    /// Quarantine this local run/view. Must not acquire `FCore` or call observers.
    fn block(&self, error: &CanonicalStoreError);
}

/// Coordinator has no callback, model, tool or retry facility.
pub struct CanonicalCoordinator<L: CanonicalLocalState> {
    store: Arc<dyn CanonicalSessionStore>,
    local: Arc<L>,
}
impl<L: CanonicalLocalState> CanonicalCoordinator<L> {
    #[must_use]
    pub fn new(store: Arc<dyn CanonicalSessionStore>, local: Arc<L>) -> Self {
        Self { store, local }
    }

    /// Prepare, install the exact approved source/view/admission candidate, then ACK.
    /// No lock acquired by the local adapter survives an awaited owner operation.
    /// # Errors
    /// Any owner/adoption/ACK failure blocks this run and preserves caller loss.
    pub async fn prepare(
        &self,
        request: CanonicalPrepareRequest,
    ) -> Result<CanonicalPreparation<L>, CanonicalStoreError> {
        let mode = request.mode;
        if let CanonicalPrepareMode::Mutation(kind) = mode {
            if request.permit.kind() != kind {
                return Err(self.block(CanonicalStoreError {
                    kind: CanonicalFailureKind::InvalidRequest,
                    requested: CanonicalRequestedWrite::NotRequested,
                    source: CanonicalSourceDisposition::Unchanged,
                    detail: "prepare permit kind differs from requested mutation mode".to_owned(),
                }));
            }
        }
        let admission = request.admission.clone();
        let expected = request.expected.clone();
        let mut prepared = self
            .store
            .prepare(request)
            .await
            .map_err(|error| self.block(error))?;
        if let Some(expected) = expected.as_ref() {
            check_scope(expected, &prepared.checkpoint.cursor)
                .map_err(|error| self.block(error))?;
        }
        let operation = prepared.operation.clone();
        let cursor = prepared.checkpoint.cursor.clone();
        let approval = CanonicalPreparedApproval {
            operation: operation.clone(),
            cursor: cursor.clone(),
            admission,
        };
        let local_plan = self
            .local
            .approve_prepared(approval, &prepared)
            .map_err(|error| self.block(error))?;
        let certificate = self
            .local
            .install_prepared(local_plan, prepared.checkpoint)
            .map_err(|error| self.block(error))?;
        check_certificate(
            &operation,
            &cursor,
            &certificate,
            CanonicalRequestedWrite::NotRequested,
        )
        .map_err(|error| self.block(error))?;
        self.store
            .acknowledge(CanonicalAcknowledgement {
                operation: operation.clone(),
                cursor: cursor.clone(),
                phase: CanonicalAckPhase::Prepared,
                adoption: Some(certificate),
            })
            .await
            .map_err(|error| self.block(error))?;
        match mode {
            CanonicalPrepareMode::Observe => {
                prepared.caller.complete();
                Ok(CanonicalPreparation::Observed(cursor))
            }
            CanonicalPrepareMode::Mutation(kind) => {
                prepared
                    .caller
                    .prepared_acknowledged()
                    .map_err(|error| self.block(error))?;
                Ok(CanonicalPreparation::Mutation(CanonicalMutationTicket {
                    store: self.store.clone(),
                    local: self.local.clone(),
                    operation,
                    cursor,
                    kind,
                    caller: prepared.caller,
                }))
            }
        }
    }

    fn block(&self, error: CanonicalStoreError) -> CanonicalStoreError {
        self.local.block(&error);
        error
    }
}

/// Observation is terminal. Mutation remains uniquely armed across private work.
pub enum CanonicalPreparation<L: CanonicalLocalState> {
    Observed(CanonicalCursor),
    Mutation(CanonicalMutationTicket<L>),
}

/// Holds no prepare authority lease or SDK lock across arbitrary private work.
/// Dropping the unique caller lets the owner classify safe prepared abandonment.
pub struct CanonicalMutationTicket<L: CanonicalLocalState> {
    store: Arc<dyn CanonicalSessionStore>,
    local: Arc<L>,
    operation: CanonicalOperation,
    cursor: CanonicalCursor,
    kind: CanonicalMutationKind,
    caller: CanonicalCallerGuard,
}
impl<L: CanonicalLocalState> CanonicalMutationTicket<L> {
    #[must_use]
    pub fn operation(&self) -> &CanonicalOperation {
        &self.operation
    }
    #[must_use]
    pub fn cursor(&self) -> &CanonicalCursor {
        &self.cursor
    }

    /// A NEW permit must be acquired against the same captured authority.
    /// This function never retries a draft, callback, model or tool.
    /// # Errors
    /// Refuses kind mismatch, owner failure, invalid receipt, or local conflict.
    pub async fn commit(
        self,
        draft: CanonicalEntryDraft,
        permit: CanonicalWritePermit,
        source: CanonicalSourceRequest,
    ) -> Result<CanonicalCommitStage<L>, CanonicalStoreError> {
        if draft.kind() != self.kind || permit.kind() != self.kind {
            return Err(block_local(
                &*self.local,
                invalid(
                    "commit purpose differs from prepared mode",
                    CanonicalRequestedWrite::NotWritten,
                    CanonicalSourceDisposition::Unchanged,
                ),
            ));
        }
        let disposition = self
            .store
            .commit(CanonicalCommitRequest {
                operation: self.operation.clone(),
                expected: self.cursor.clone(),
                source,
                draft,
                permit,
            })
            .await
            .map_err(|error| block_local(&*self.local, error))?;
        let (receipt, committed) = match disposition {
            CanonicalCommitDisposition::Committed(receipt) => (receipt, true),
            CanonicalCommitDisposition::NeedsCatchUp(receipt) => (receipt, false),
        };
        let requested = if committed {
            CanonicalRequestedWrite::Committed
        } else {
            CanonicalRequestedWrite::NotWritten
        };
        check_receipt(&self.operation, &self.cursor, &receipt, committed)
            .map_err(|error| block_local(&*self.local, error))?;
        let cursor = receipt.to.clone();
        let committed_entry_id = receipt.committed_entry_id.clone();
        let certificate = self
            .local
            .adopt_receipt(receipt)
            .map_err(|error| block_local(&*self.local, error))?;
        check_certificate(&self.operation, &cursor, &certificate, requested)
            .map_err(|error| block_local(&*self.local, error))?;
        let pending = CanonicalPendingPublication {
            store: self.store,
            local: self.local,
            operation: self.operation,
            cursor,
            certificate,
            caller: self.caller,
            committed_entry_id,
        };
        Ok(if committed {
            CanonicalCommitStage::Committed(pending)
        } else {
            CanonicalCommitStage::NeedsCatchUp(pending)
        })
    }

    /// Only a prepared operation with no unadopted/uncertain I/O may abandon.
    /// The owner must verify that condition; this is not a write retry.
    /// # Errors
    /// An ACK failure keeps the caller armed and source blocked as appropriate.
    pub async fn abandon(self) -> Result<(), CanonicalStoreError> {
        self.store
            .acknowledge(CanonicalAcknowledgement {
                operation: self.operation,
                cursor: self.cursor,
                phase: CanonicalAckPhase::AbandonPreparedOnly,
                adoption: None,
            })
            .await
            .map_err(|error| block_local(&*self.local, error))?;
        self.caller.complete();
        Ok(())
    }
}

/// Catchup is explicit known backpressure. It is never converted into a retry.
pub enum CanonicalCommitStage<L: CanonicalLocalState> {
    Committed(CanonicalPendingPublication<L>),
    NeedsCatchUp(CanonicalPendingPublication<L>),
}

/// Raw adoption alone keeps owner/SDK work blocked. There is no ready constructor.
pub struct CanonicalPendingPublication<L: CanonicalLocalState> {
    store: Arc<dyn CanonicalSessionStore>,
    local: Arc<L>,
    operation: CanonicalOperation,
    cursor: CanonicalCursor,
    certificate: CanonicalAdoptionCertificate,
    caller: CanonicalCallerGuard,
    committed_entry_id: Option<String>,
}
impl<L: CanonicalLocalState> CanonicalPendingPublication<L> {
    /// Consume actual allowed publication authority, then ACK, then complete caller.
    /// # Errors
    /// Any failure leaves the unique caller armed; no raw-adopt-to-ready shortcut.
    pub async fn publish(
        self,
        grant: L::PublicationGrant,
    ) -> Result<CanonicalCompletion, CanonicalStoreError> {
        let certificate = self
            .local
            .publish(self.certificate, grant)
            .map_err(|error| block_local(&*self.local, error))?;
        check_certificate(
            &self.operation,
            &self.cursor,
            &certificate,
            if self.committed_entry_id.is_some() {
                CanonicalRequestedWrite::Committed
            } else {
                CanonicalRequestedWrite::NotWritten
            },
        )
        .map_err(|error| block_local(&*self.local, error))?;
        self.store
            .acknowledge(CanonicalAcknowledgement {
                operation: self.operation,
                cursor: self.cursor.clone(),
                phase: CanonicalAckPhase::AppliedReady,
                adoption: Some(certificate),
            })
            .await
            .map_err(|error| block_local(&*self.local, error))?;
        self.caller.complete();
        Ok(CanonicalCompletion {
            cursor: self.cursor,
            committed_entry_id: self.committed_entry_id,
            projection_blocked: false,
        })
    }

    /// Closing cleanup can adopt/account/ACK but cannot reopen old projection.
    /// # Errors
    /// Failed cleanup ACK keeps the caller armed and requires reconciliation.
    pub async fn cleanup(self) -> Result<CanonicalCompletion, CanonicalStoreError> {
        self.store
            .acknowledge(CanonicalAcknowledgement {
                operation: self.operation,
                cursor: self.cursor.clone(),
                phase: CanonicalAckPhase::ReconciledBlocked,
                adoption: Some(self.certificate),
            })
            .await
            .map_err(|error| block_local(&*self.local, error))?;
        self.caller.complete();
        Ok(CanonicalCompletion {
            cursor: self.cursor,
            committed_entry_id: self.committed_entry_id,
            projection_blocked: true,
        })
    }
}

pub struct CanonicalCompletion {
    pub cursor: CanonicalCursor,
    pub committed_entry_id: Option<String>,
    pub projection_blocked: bool,
}

fn block_local(
    local: &impl CanonicalLocalState,
    error: CanonicalStoreError,
) -> CanonicalStoreError {
    local.block(&error);
    error
}
fn invalid(
    detail: &str,
    requested: CanonicalRequestedWrite,
    source: CanonicalSourceDisposition,
) -> CanonicalStoreError {
    CanonicalStoreError {
        kind: CanonicalFailureKind::AdoptionConflict,
        requested,
        source,
        detail: detail.to_owned(),
    }
}
fn check_scope(
    expected: &CanonicalCursor,
    actual: &CanonicalCursor,
) -> Result<(), CanonicalStoreError> {
    if expected.owner != actual.owner
        || expected.session_id != actual.session_id
        || expected.source_path != actual.source_path
        || expected.header_certificate != actual.header_certificate
        || expected.branch_revision != actual.branch_revision
        || actual.canonical_revision < expected.canonical_revision
    {
        return Err(invalid(
            "prepared owner/header/path/branch changed",
            CanonicalRequestedWrite::NotRequested,
            CanonicalSourceDisposition::NeedsReconcile,
        ));
    }
    Ok(())
}
fn check_certificate(
    operation: &CanonicalOperation,
    cursor: &CanonicalCursor,
    certificate: &CanonicalAdoptionCertificate,
    requested: CanonicalRequestedWrite,
) -> Result<(), CanonicalStoreError> {
    if certificate.operation() != operation || certificate.cursor() != cursor {
        return Err(invalid(
            "adoption certificate does not match operation/cursor",
            requested,
            CanonicalSourceDisposition::NeedsReconcile,
        ));
    }
    Ok(())
}
fn check_receipt(
    operation: &CanonicalOperation,
    expected: &CanonicalCursor,
    receipt: &CanonicalAppendReceipt,
    committed: bool,
) -> Result<(), CanonicalStoreError> {
    let requested = if committed {
        CanonicalRequestedWrite::Committed
    } else {
        CanonicalRequestedWrite::NotWritten
    };
    if &receipt.operation != operation || &receipt.from != expected {
        return Err(invalid(
            "receipt does not match exact prepared operation/cursor",
            requested,
            CanonicalSourceDisposition::NeedsReconcile,
        ));
    }
    check_scope(expected, &receipt.to).map_err(|mut error| {
        error.requested = requested;
        error
    })?;
    if committed {
        let Some(id) = receipt
            .committed_entry_id
            .as_deref()
            .filter(|id| !id.is_empty())
        else {
            return Err(invalid(
                "committed receipt lacks exact canonical ID",
                requested,
                CanonicalSourceDisposition::NeedsReconcile,
            ));
        };
        if receipt
            .rows
            .iter()
            .filter(|row| row.entry().id() == Some(id))
            .count()
            != 1
        {
            return Err(invalid(
                "committed ID is not exactly one raw suffix row",
                requested,
                CanonicalSourceDisposition::NeedsReconcile,
            ));
        }
    } else if receipt.committed_entry_id.is_some() || receipt.to == receipt.from {
        return Err(invalid(
            "catchup must be known-unwritten and advance the source",
            requested,
            CanonicalSourceDisposition::NeedsReconcile,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::canonical::{
        CanonicalCallerNotifier, CanonicalCallerSignal, CanonicalCallerState, CanonicalFrontier,
        CanonicalFuture, CanonicalIoFlight, CanonicalIoStage, CanonicalJournalRow,
        CanonicalOwnerScope, CanonicalSource, CanonicalWriteAuthority,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

    fn cursor() -> CanonicalCursor {
        CanonicalCursor {
            owner: CanonicalOwnerScope::new(),
            session_id: "session".into(),
            source_path: "/private/session.jsonl".into(),
            raw_source_generation: [1; 32],
            header_certificate: [2; 32],
            canonical_revision: 3,
            branch_revision: 4,
            selected_leaf: None,
            frontier: CanonicalFrontier {
                bytes: 1,
                rows: 1,
                prefix_certificate: [3; 32],
            },
        }
    }
    fn failure() -> CanonicalStoreError {
        CanonicalStoreError {
            kind: CanonicalFailureKind::Io,
            requested: CanonicalRequestedWrite::Unknown,
            source: CanonicalSourceDisposition::NeedsReconcile,
            detail: "injected".into(),
        }
    }
    struct Notify;
    impl CanonicalCallerNotifier for Notify {
        fn changed(&self) {}
    }
    struct Authority;
    impl CanonicalWriteAuthority for Authority {
        fn enter_io(
            &self,
            _: CanonicalIoStage,
            _: &CanonicalCursor,
            _: &CanonicalOperation,
            _: Option<&CanonicalEntryDraft>,
        ) -> Result<CanonicalIoFlight, CanonicalStoreError> {
            Ok(CanonicalIoFlight::new(()))
        }
    }
    fn permit(kind: CanonicalMutationKind) -> CanonicalWritePermit {
        CanonicalWritePermit::new(kind, Arc::new(Authority))
    }
    fn draft() -> CanonicalEntryDraft {
        let entry = serde_json::from_value(serde_json::json!({"type":"custom",
            "customType":"test","data":{"opaque":"AA=="}}))
        .expect("draft");
        CanonicalEntryDraft::new(CanonicalMutationKind::OwnerMetadata, entry).expect("payload")
    }
    struct Store {
        actual: CanonicalCursor,
        prepare_error: bool,
        ack_error: bool,
        signal: Mutex<Option<CanonicalCallerSignal>>,
        commits: AtomicUsize,
        acks: Mutex<Vec<CanonicalAckPhase>>,
    }
    impl Store {
        fn new(actual: CanonicalCursor) -> Self {
            Self {
                actual,
                prepare_error: false,
                ack_error: false,
                signal: Mutex::new(None),
                commits: AtomicUsize::new(0),
                acks: Mutex::new(Vec::new()),
            }
        }
        fn caller_state(&self) -> CanonicalCallerState {
            self.signal
                .lock()
                .expect("signal lock")
                .as_ref()
                .expect("caller signal")
                .state()
        }
    }
    impl CanonicalSessionStore for Store {
        fn prepare(&self, _: CanonicalPrepareRequest) -> CanonicalFuture<CanonicalPrepared> {
            if self.prepare_error {
                return Box::pin(async { Err(failure()) });
            }
            let (caller, signal) = CanonicalCallerGuard::new(Arc::new(Notify));
            *self.signal.lock().expect("signal lock") = Some(signal);
            let raw: Arc<[u8]> = Arc::from(&b"{\"type\":\"session\",\"version\":3,\"id\":\"session\",\"timestamp\":\"t\",\"cwd\":\"/private\"}\n"[..]);
            let header = CanonicalJournalRow::parse(raw, 0, 0).expect("physical header");
            let prepared = CanonicalPrepared {
                caller,
                operation: CanonicalOperation::new(),
                checkpoint: CanonicalCheckpoint {
                    cursor: self.actual.clone(),
                    header,
                    source: CanonicalSource::Full(Vec::new()),
                },
            };
            Box::pin(async move { Ok(prepared) })
        }
        fn commit(&self, _: CanonicalCommitRequest) -> CanonicalFuture<CanonicalCommitDisposition> {
            self.commits.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(failure()) })
        }
        fn acknowledge(&self, request: CanonicalAcknowledgement) -> CanonicalFuture<()> {
            self.acks.lock().expect("acks lock").push(request.phase);
            let fail = self.ack_error;
            Box::pin(async move {
                if fail {
                    Err(failure())
                } else {
                    Ok(())
                }
            })
        }
    }
    #[derive(Default)]
    struct Local {
        approvals: AtomicUsize,
        installs: AtomicUsize,
        blocks: AtomicUsize,
    }
    impl CanonicalLocalState for Local {
        type PublicationGrant = ();
        type PreparedPlan = ();
        fn approve_prepared(
            &self,
            owner: CanonicalPreparedApproval,
            prepared: &CanonicalPrepared,
        ) -> Result<(), CanonicalStoreError> {
            assert_eq!(owner.operation(), &prepared.operation);
            assert_eq!(owner.cursor(), &prepared.checkpoint.cursor);
            assert_eq!(owner.admission(), &CanonicalAdmissionPolicy::Preserve);
            self.approvals.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn install_prepared(
            &self,
            (): (),
            _: CanonicalCheckpoint,
        ) -> Result<CanonicalAdoptionCertificate, CanonicalStoreError> {
            self.installs.fetch_add(1, Ordering::SeqCst);
            Err(invalid(
                "local injected adoption conflict",
                CanonicalRequestedWrite::NotRequested,
                CanonicalSourceDisposition::NeedsReconcile,
            ))
        }
        fn adopt_receipt(
            &self,
            _: CanonicalAppendReceipt,
        ) -> Result<CanonicalAdoptionCertificate, CanonicalStoreError> {
            Err(failure())
        }
        fn publish(
            &self,
            _: CanonicalAdoptionCertificate,
            (): (),
        ) -> Result<CanonicalAdoptionCertificate, CanonicalStoreError> {
            Err(failure())
        }
        fn block(&self, _: &CanonicalStoreError) {
            self.blocks.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn request(expected: CanonicalCursor) -> CanonicalPrepareRequest {
        CanonicalPrepareRequest {
            permit: permit(CanonicalMutationKind::OwnerMetadata),
            expected: Some(expected),
            source: CanonicalSourceRequest::Full,
            mode: CanonicalPrepareMode::Mutation(CanonicalMutationKind::OwnerMetadata),
            admission: CanonicalAdmissionPolicy::Preserve,
        }
    }
    fn ticket(store: Arc<Store>, local: Arc<Local>) -> CanonicalMutationTicket<Local> {
        let (mut caller, signal) = CanonicalCallerGuard::new(Arc::new(Notify));
        caller.prepared_acknowledged().expect("prepared ACK");
        *store.signal.lock().expect("signal lock") = Some(signal);
        CanonicalMutationTicket {
            operation: CanonicalOperation::new(),
            cursor: store.actual.clone(),
            store,
            local,
            caller,
            kind: CanonicalMutationKind::OwnerMetadata,
        }
    }

    #[tokio::test]
    async fn owner_uncertainty_is_not_retained_success_or_local_adoption() {
        let expected = cursor();
        let mut backend = Store::new(expected.clone());
        backend.prepare_error = true;
        let store = Arc::new(backend);
        let local = Arc::new(Local::default());
        let result = CanonicalCoordinator::new(store, local.clone())
            .prepare(request(expected))
            .await;
        let Err(error) = result else {
            panic!("uncertainty accepted");
        };
        assert_eq!(error.requested, CanonicalRequestedWrite::Unknown);
        assert_eq!(error.source, CanonicalSourceDisposition::NeedsReconcile);
        assert_eq!(local.installs.load(Ordering::SeqCst), 0);
        assert_eq!(local.blocks.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn foreign_prepared_owner_is_rejected_before_local_candidate_or_ack() {
        let expected = cursor();
        let mut actual = expected.clone();
        actual.owner = CanonicalOwnerScope::new();
        let store = Arc::new(Store::new(actual));
        let local = Arc::new(Local::default());
        assert!(CanonicalCoordinator::new(store.clone(), local.clone())
            .prepare(request(expected))
            .await
            .is_err());
        assert_eq!(local.approvals.load(Ordering::SeqCst), 0);
        assert!(store.acks.lock().expect("acks lock").is_empty());
        assert_eq!(store.caller_state(), CanonicalCallerState::Abandoned);
    }
    #[tokio::test]
    async fn own_preflight_head_advance_is_not_generic_authority_failure() {
        let expected = cursor();
        let mut actual = expected.clone();
        actual.canonical_revision += 1;
        actual.raw_source_generation = [9; 32];
        actual.frontier.bytes += 4;
        actual.frontier.rows += 1;
        let store = Arc::new(Store::new(actual));
        let local = Arc::new(Local::default());
        assert!(CanonicalCoordinator::new(store.clone(), local.clone())
            .prepare(request(expected))
            .await
            .is_err());
        assert_eq!(local.approvals.load(Ordering::SeqCst), 1);
        assert_eq!(local.installs.load(Ordering::SeqCst), 1);
        assert!(store.acks.lock().expect("acks lock").is_empty());
        assert_eq!(store.caller_state(), CanonicalCallerState::Abandoned);
    }
    #[tokio::test]
    async fn commit_kind_mismatch_refuses_before_owner_mutation() {
        let store = Arc::new(Store::new(cursor()));
        let local = Arc::new(Local::default());
        let result = ticket(store.clone(), local)
            .commit(
                draft(),
                permit(CanonicalMutationKind::ContextPolicy),
                CanonicalSourceRequest::Full,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(store.commits.load(Ordering::SeqCst), 0);
        assert_eq!(store.caller_state(), CanonicalCallerState::Abandoned);
    }
    #[tokio::test]
    async fn commit_failure_keeps_caller_armed_until_drop_not_complete() {
        let store = Arc::new(Store::new(cursor()));
        let local = Arc::new(Local::default());
        let result = ticket(store.clone(), local)
            .commit(
                draft(),
                permit(CanonicalMutationKind::OwnerMetadata),
                CanonicalSourceRequest::Full,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(store.commits.load(Ordering::SeqCst), 1);
        assert_eq!(store.caller_state(), CanonicalCallerState::Abandoned);
        assert!(store.acks.lock().expect("acks lock").is_empty());
    }
    #[tokio::test]
    async fn lost_abandon_ack_cannot_complete_unique_caller() {
        let mut backend = Store::new(cursor());
        backend.ack_error = true;
        let store = Arc::new(backend);
        let local = Arc::new(Local::default());
        assert!(ticket(store.clone(), local).abandon().await.is_err());
        assert_eq!(store.caller_state(), CanonicalCallerState::Abandoned);
    }
    #[tokio::test]
    async fn known_prepared_abandon_completes_only_after_matching_ack() {
        let store = Arc::new(Store::new(cursor()));
        let local = Arc::new(Local::default());
        ticket(store.clone(), local)
            .abandon()
            .await
            .expect("known abandon ACK");
        assert_eq!(store.caller_state(), CanonicalCallerState::Completed);
        assert_eq!(
            *store.acks.lock().expect("acks lock"),
            vec![CanonicalAckPhase::AbandonPreparedOnly]
        );
    }
    #[test]
    fn wrong_operation_or_base_cursor_receipt_never_applies() {
        let from = cursor();
        let operation = CanonicalOperation::new();
        let mut to = from.clone();
        to.canonical_revision += 1;
        let mut receipt = CanonicalAppendReceipt {
            operation: CanonicalOperation::new(),
            from: from.clone(),
            to,
            rows: Vec::new(),
            committed_entry_id: None,
            replacement_window: None,
        };
        assert!(check_receipt(&operation, &from, &receipt, false).is_err());
        receipt.operation = operation.clone();
        receipt.from.branch_revision += 1;
        assert!(check_receipt(&operation, &from, &receipt, false).is_err());
    }
    #[test]
    fn catchup_cannot_claim_requested_id_or_no_advance() {
        let from = cursor();
        let operation = CanonicalOperation::new();
        let mut receipt = CanonicalAppendReceipt {
            operation: operation.clone(),
            from: from.clone(),
            to: from.clone(),
            rows: Vec::new(),
            committed_entry_id: None,
            replacement_window: None,
        };
        assert!(check_receipt(&operation, &from, &receipt, false).is_err());
        receipt.to.canonical_revision += 1;
        receipt.committed_entry_id = Some("fake".into());
        assert!(check_receipt(&operation, &from, &receipt, false).is_err());
    }
    #[test]
    fn committed_id_must_identify_exactly_one_original_raw_row() {
        let from = cursor();
        let operation = CanonicalOperation::new();
        let mut to = from.clone();
        to.canonical_revision += 1;
        let raw: Arc<[u8]> =
            Arc::from(&b"{\"type\":\"custom\",\"id\":\"actual\",\"customType\":\"test\"}\n"[..]);
        let row = CanonicalJournalRow::parse(raw, 1, 1).expect("raw row");
        let mut receipt = CanonicalAppendReceipt {
            operation: operation.clone(),
            from: from.clone(),
            to,
            rows: vec![row.clone()],
            committed_entry_id: Some("actual".into()),
            replacement_window: None,
        };
        assert!(check_receipt(&operation, &from, &receipt, true).is_ok());
        receipt.rows.push(row);
        assert!(check_receipt(&operation, &from, &receipt, true).is_err());
    }
    #[test]
    fn receipt_branch_error_preserves_known_requested_write_certainty() {
        let from = cursor();
        let operation = CanonicalOperation::new();
        let mut to = from.clone();
        to.branch_revision += 1;
        let receipt = CanonicalAppendReceipt {
            operation: operation.clone(),
            from: from.clone(),
            to,
            rows: Vec::new(),
            committed_entry_id: Some("actual".into()),
            replacement_window: None,
        };
        let error = check_receipt(&operation, &from, &receipt, true).expect_err("foreign branch");
        assert_eq!(error.requested, CanonicalRequestedWrite::Committed);
        assert_eq!(error.source, CanonicalSourceDisposition::NeedsReconcile);
    }
    #[tokio::test]
    async fn prepare_mode_kind_mismatch_refuses_before_local_work() {
        let expected = cursor();
        let store = Arc::new(Store::new(expected.clone()));
        let local = Arc::new(Local::default());
        let mut request = request(expected);
        request.permit = permit(CanonicalMutationKind::ContextPolicy);
        let result = CanonicalCoordinator::new(store.clone(), local.clone())
            .prepare(request)
            .await;
        let Err(error) = result else {
            panic!("kind mismatch accepted")
        };
        assert_eq!(error.kind, CanonicalFailureKind::InvalidRequest);
        assert_eq!(error.requested, CanonicalRequestedWrite::NotRequested);
        assert_eq!(local.approvals.load(Ordering::SeqCst), 0);
        assert!(store.signal.lock().expect("signal lock").is_none());
    }
}
