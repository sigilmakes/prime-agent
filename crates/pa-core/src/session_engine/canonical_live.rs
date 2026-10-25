//! Live owner-initiated canonical receipts. The owner alone admits work and writes bytes.
//! Core adopts exact raw checkpoints/suffixes without I/O, then ACKs before continuing.

use crate::session::canonical::CanonicalCheckpoint;
use crate::session::canonical::{
    CanonicalAckPhase, CanonicalAcknowledgement, CanonicalAdmissionPolicy, CanonicalCallerGuard,
    CanonicalCommitDisposition, CanonicalCursor, CanonicalEntryDraft, CanonicalFailureKind,
    CanonicalFuture, CanonicalMetadataReceipt, CanonicalMetadataWriter, CanonicalMutationKind,
    CanonicalOperation, CanonicalPrepareMode, CanonicalPrepared, CanonicalRequestedWrite,
    CanonicalSource, CanonicalSourceDisposition, CanonicalSourceRequest, CanonicalStoreError,
};
use crate::session::manager::{CanonicalLocalAdoption, SessionManager};
use pa_types::session::{AgentMessage, EntryBase, FileEntry};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Immutable owner-certified observation. This carries no execution or write authority.
/// Membership is not replay order. Only the local SDK owns its execution queue.
pub struct CanonicalLiveSnapshot {
    pub checkpoint: CanonicalCheckpoint,
    pub admitted_users: std::collections::HashSet<String>,
    pub pending_users: std::collections::VecDeque<String>,
    pub pending_custom: std::collections::VecDeque<String>,
    pub safe_visible_leaf: Option<String>,
}

/// Owner-side preparation; admission and write leases remain with the actual owner.
pub struct CanonicalLivePrepareRequest {
    pub expected: Option<CanonicalCursor>,
    pub source: CanonicalSourceRequest,
    pub mode: CanonicalPrepareMode,
    pub admission: CanonicalAdmissionPolicy,
}

/// One envelope-free mutation against a previously certified source.
pub struct CanonicalLiveAppendRequest {
    pub run: Option<Arc<CanonicalLiveRun>>,
    pub context_admission: Option<Arc<CanonicalLiveContextAdmission>>,
    pub operation: CanonicalOperation,
    pub expected: CanonicalCursor,
    pub source: CanonicalSourceRequest,
    pub draft: CanonicalEntryDraft,
}

/// Exact selected-registration authority for one actual context-policy draft.
/// Only Core's policy path can construct this capability. The owner invokes it
/// synchronously around the real RequestedMutation/write-attempt admission.
pub struct CanonicalLiveContextAdmission {
    plugins: Arc<super::plugins::SessionPlugins>,
    registration: super::plugins::ContextRegistration,
    strategy: crate::plugins::Contribution,
    compactor: crate::plugins::Contribution,
    draft: CanonicalEntryDraft,
    used: AtomicBool,
}
impl CanonicalLiveContextAdmission {
    pub(super) fn new(
        plugins: Arc<super::plugins::SessionPlugins>,
        registration: super::plugins::ContextRegistration,
        strategy: crate::plugins::Contribution,
        compactor: crate::plugins::Contribution,
        draft: CanonicalEntryDraft,
    ) -> Arc<Self> {
        Arc::new(Self {
            plugins,
            registration,
            strategy,
            compactor,
            draft,
            used: AtomicBool::new(false),
        })
    }

    /// Admit exactly this draft under the actual selected registration and both
    /// current contribution generations. No guard escapes this synchronous call.
    ///
    /// # Errors
    /// Rejects a changed draft, retired registration/generation, reused capability,
    /// or the actual owner's grant/write-attempt admission error.
    pub fn with_admission<T>(
        &self,
        draft: &CanonicalEntryDraft,
        admit: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if draft.kind() != self.draft.kind() || draft.entry() != self.draft.entry() {
            anyhow::bail!("context admission draft differs from actual policy intent");
        }
        self.plugins.with_registered_context_authority(
            &self.registration,
            &self.strategy,
            &self.compactor,
            || {
                if self
                    .used
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    anyhow::bail!("context policy admission already consumed");
                }
                admit()
            },
        )
    }
}

/// Semantic delivery intent, never identity inferred from an equal historical body.
/// SDK timestamps are not compared for preaccepted deliveries. The owner returns
/// the complete authoritative raw row, whose canonical timestamp stays untouched.
pub struct CanonicalLiveUserRequest {
    pub expected: Option<CanonicalCursor>,
    pub source: CanonicalSourceRequest,
    pub message: pa_types::ai::UserMessage,
}

/// The exact owner-delivered User identity in the same prepared operation.
pub struct CanonicalLiveUserPrepared {
    pub prepared: CanonicalPrepared,
    pub admitted_entry_id: String,
}

/// Explicit custom delivery, distinct from metadata append and User replay.
pub struct CanonicalLiveCustomRequest {
    pub expected: Option<CanonicalCursor>,
    pub source: CanonicalSourceRequest,
    pub message: pa_types::session::CustomMessage,
}
pub struct CanonicalLiveCustomPrepared {
    pub prepared: CanonicalPrepared,
    pub admitted_entry_id: String,
}

/// Owner publication proof at the matching raw-adoption ACK barrier.
/// The leaf is the exact safe selected-branch prefix before either pending
/// delivery ledger, not an ID inferred from matching message bodies.
pub struct CanonicalLiveAcknowledged {
    pub operation: CanonicalOperation,
    pub cursor: CanonicalCursor,
    pub visible_leaf: Option<String>,
}

/// Intrinsic identity of one actual AgentStart/AgentEnd run.
pub struct CanonicalLiveRun {
    _private: (),
}
/// Intrinsic identity of one actual main stream invocation.
pub struct CanonicalLiveProviderInvocation {
    _private: (),
}
/// Actual serving allocations, never desired-next model selectors.
pub struct CanonicalLiveRunRequest {
    pub expected: CanonicalCursor,
    pub run: Arc<CanonicalLiveRun>,
}

pub struct CanonicalLiveProviderStepRequest {
    pub expected: CanonicalCursor,
    pub run: Arc<CanonicalLiveRun>,
    pub model: Arc<pa_agent::types::Model>,
    pub invocation: Arc<CanonicalLiveProviderInvocation>,
}
/// The actual tool-start execution record retained through its terminal receipt.
pub struct CanonicalLiveToolExecution {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
}
pub struct CanonicalLiveToolRequest {
    pub run: Arc<CanonicalLiveRun>,
    pub tool: Arc<CanonicalLiveToolExecution>,
}

/// Actual file/lease lifetime owner. Implementations retain pending/uncertain state
/// through raw adoption and matching ACK, and refuse all writes while pending.
/// Each method must release host locks before returning its awaited result.
/// No default implementation may mint authority or report retained-write success.
pub trait CanonicalLiveOwner: Send + Sync {
    /// This SDK mirror is rebuilding under an existing actual owner. The owner
    /// captures this for each new adapter before bootstrap; a fresh owner must
    /// instead bind the local pending admissions with its initial Replace.
    fn bootstrap_preserves_owner_admission(&self) -> bool {
        false
    }

    /// Retire this completed actual runtime scope without deleting its journal.
    fn retire_completed_runtime(&self, _expected: CanonicalCursor) -> CanonicalFuture<()> {
        Box::pin(async {
            Err(failure(
                "owner does not support runtime retirement",
                CanonicalRequestedWrite::NotRequested,
            ))
        })
    }

    fn snapshot(&self) -> CanonicalFuture<CanonicalLiveSnapshot>;
    fn register_run(&self, request: CanonicalLiveRunRequest) -> CanonicalFuture<()>;
    fn prepare(&self, request: CanonicalLivePrepareRequest) -> CanonicalFuture<CanonicalPrepared>;
    fn append(
        &self,
        request: CanonicalLiveAppendRequest,
    ) -> CanonicalFuture<CanonicalCommitDisposition>;
    fn acknowledge(
        &self,
        request: CanonicalAcknowledgement,
    ) -> CanonicalFuture<CanonicalLiveAcknowledged>;
    fn admit_user(
        &self,
        request: CanonicalLiveUserRequest,
    ) -> CanonicalFuture<CanonicalLiveUserPrepared>;
    fn admit_custom(
        &self,
        request: CanonicalLiveCustomRequest,
    ) -> CanonicalFuture<CanonicalLiveCustomPrepared>;
    /// Admit one actual MAIN provider attempt, not auxiliary inference.
    fn begin_provider_step(&self, request: CanonicalLiveProviderStepRequest)
        -> CanonicalFuture<()>;
    /// Admit the actual correlated tool before execution, never after its result.
    fn admit_tool(&self, request: CanonicalLiveToolRequest) -> CanonicalFuture<()>;
}

struct CanonicalProjection {
    cursor: CanonicalCursor,
    local_revision: u64,
    run: Arc<CanonicalLiveRun>,
}

/// Work that may run only after a successful assistant receipt is acknowledged.
pub type CanonicalAfterAssistantCommitFn =
    Arc<dyn Fn() -> pa_agent::BoxFut<'static, anyhow::Result<()>> + Send + Sync>;

struct AfterAssistantCommit {
    callback: CanonicalAfterAssistantCommitFn,
}

/// Episode-owned callback registration. Dropping it cannot clear another episode.
pub struct CanonicalAfterAssistantCommitGuard {
    live: std::sync::Weak<CanonicalLiveSession>,
    registration: Arc<AfterAssistantCommit>,
}

impl Drop for CanonicalAfterAssistantCommitGuard {
    fn drop(&mut self) {
        let Some(live) = self.live.upgrade() else {
            return;
        };
        match live.after_assistant_commit.lock() {
            Ok(mut slot) => {
                if slot
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &self.registration))
                {
                    slot.take();
                }
            }
            Err(_) => live.block_projection(),
        };
    }
}

/// Core's live local adapter. No SDK guard survives an owner await.
pub struct CanonicalLiveSession {
    owner: Arc<dyn CanonicalLiveOwner>,
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    serving: Arc<()>,
    run: std::sync::Mutex<Option<Arc<CanonicalLiveRun>>>,
    projection: std::sync::Mutex<Option<CanonicalProjection>>,
    archive:
        std::sync::Mutex<Option<Arc<std::sync::RwLock<Option<pa_types::session::ContextArchive>>>>>,
    busy: AtomicBool,
    blocked: AtomicBool,
    after_assistant_commit: std::sync::Mutex<Option<Arc<AfterAssistantCommit>>>,
}

struct Flight<'a> {
    live: &'a CanonicalLiveSession,
    complete: bool,
}
impl Drop for Flight<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.live.blocked.store(true, Ordering::Release);
        }
        self.live.busy.store(false, Ordering::Release);
    }
}

/// Unique prepared context-derived mutation. Drop leaves the live run blocked.
pub struct CanonicalLiveMutation<'a> {
    flight: Flight<'a>,
    operation: CanonicalOperation,
    cursor: CanonicalCursor,
    caller: CanonicalCallerGuard,
    kind: CanonicalMutationKind,
    local_revision: u64,
}

fn failure(detail: impl Into<String>, requested: CanonicalRequestedWrite) -> CanonicalStoreError {
    let detail = detail.into();
    CanonicalStoreError {
        kind: CanonicalFailureKind::AdoptionConflict,
        requested,
        source: CanonicalSourceDisposition::NeedsReconcile,
        detail,
    }
}

impl CanonicalLiveSession {
    #[must_use]
    pub fn new(
        owner: Arc<dyn CanonicalLiveOwner>,
        session: Arc<tokio::sync::Mutex<SessionManager>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            owner,
            session,
            serving: Arc::new(()),
            run: std::sync::Mutex::new(None),
            projection: std::sync::Mutex::new(None),
            archive: std::sync::Mutex::new(None),
            busy: AtomicBool::new(false),
            blocked: AtomicBool::new(false),
            after_assistant_commit: std::sync::Mutex::new(None),
        })
    }

    /// Register one episode's work at the existing post-ACK, pre-Agent phase.
    ///
    /// # Errors
    /// Refuses an unresolved receipt, overlapping episode, or poisoned slot.
    pub fn register_after_assistant_commit(
        self: &Arc<Self>,
        callback: CanonicalAfterAssistantCommitFn,
    ) -> anyhow::Result<CanonicalAfterAssistantCommitGuard> {
        self.ensure_ready()?;
        let mut slot = self
            .after_assistant_commit
            .lock()
            .map_err(|_| anyhow::anyhow!("assistant commit callback slot poisoned"))?;
        anyhow::ensure!(
            slot.is_none(),
            "assistant commit callback already registered"
        );
        let registration = Arc::new(AfterAssistantCommit { callback });
        *slot = Some(Arc::clone(&registration));
        Ok(CanonicalAfterAssistantCommitGuard {
            live: Arc::downgrade(self),
            registration,
        })
    }

    async fn run_after_assistant_commit(&self) -> anyhow::Result<()> {
        self.ensure_ready()?;
        // Snapshot under the slot lock. No source/manager/authority or slot
        // guard survives into callback I/O.
        let registration = self
            .after_assistant_commit
            .lock()
            .map_err(|_| anyhow::anyhow!("assistant commit callback slot poisoned"))?
            .clone();
        if let Some(registration) = registration {
            if let Err(error) = (registration.callback)().await {
                self.block_projection();
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) async fn attach_metadata_writer(self: &Arc<Self>) {
        let writer: Arc<dyn CanonicalMetadataWriter> = self.clone();
        self.session
            .lock()
            .await
            .bind_canonical_metadata_writer(Arc::downgrade(&writer));
    }

    fn enter(&self) -> Result<Flight<'_>, CanonicalStoreError> {
        if self
            .projection
            .lock()
            .map_err(|_| {
                failure(
                    "canonical projection lock poisoned",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?
            .is_some()
        {
            return Err(failure(
                "canonical model projection is pending",
                CanonicalRequestedWrite::NotRequested,
            ));
        }
        if self.blocked.load(Ordering::Acquire)
            || self
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(failure(
                "canonical local run is blocked or has pending adoption",
                CanonicalRequestedWrite::NotRequested,
            ));
        }
        // Retirement may have closed admission between the first check and CAS.
        if self.blocked.load(Ordering::Acquire) {
            self.busy.store(false, Ordering::Release);
            return Err(failure(
                "canonical view is retired or blocked",
                CanonicalRequestedWrite::NotRequested,
            ));
        }
        Ok(Flight {
            live: self,
            complete: false,
        })
    }

    pub(crate) fn block_projection(&self) {
        self.blocked.store(true, Ordering::Release);
    }

    /// Refuse new projections after any failed, uncertain or lost receipt.
    ///
    /// # Errors
    /// Refuses changed authority, invalid source/adoption, or incomplete owner ACK.
    pub fn ensure_ready(&self) -> Result<(), CanonicalStoreError> {
        if self.blocked.load(Ordering::Acquire) {
            return Err(failure(
                "canonical receipt has not completed",
                CanonicalRequestedWrite::NotRequested,
            ));
        }
        if self.busy.load(Ordering::Acquire) {
            return Err(failure(
                "canonical receipt has not completed",
                CanonicalRequestedWrite::NotRequested,
            ));
        }
        if self
            .projection
            .lock()
            .map_err(|_| {
                failure(
                    "canonical projection lock poisoned",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?
            .is_some()
        {
            return Err(failure(
                "canonical receipt has not completed",
                CanonicalRequestedWrite::NotRequested,
            ));
        }
        Ok(())
    }

    /// Permanently close this view, then retire its completed physical owner.
    /// No local guard survives the owner call. Failures never reopen admission.
    pub(super) async fn retire_completed_runtime(&self) -> Result<(), CanonicalStoreError> {
        let mut flight = self.enter()?;
        self.blocked.store(true, Ordering::Release);
        if self
            .run
            .lock()
            .map_err(|_| {
                failure(
                    "canonical run lock poisoned",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?
            .is_some()
        {
            return Err(failure(
                "canonical run is active",
                CanonicalRequestedWrite::NotRequested,
            ));
        }
        let expected = {
            let session = self.session.lock().await;
            if !session.canonical_pending_admitted_users().is_empty() {
                return Err(failure(
                    "Core still has admitted execution work",
                    CanonicalRequestedWrite::NotRequested,
                ));
            }
            session.canonical_binding().cloned().ok_or_else(|| {
                failure(
                    "canonical view is unbound",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?
        };
        flight.complete = true;
        self.owner.retire_completed_runtime(expected).await
    }

    async fn install(
        &self,
        prepared: CanonicalPrepared,
        revision: u64,
        binding: Option<CanonicalCursor>,
        admission: CanonicalAdmissionPolicy,
    ) -> Result<
        (
            CanonicalCallerGuard,
            CanonicalOperation,
            CanonicalCursor,
            u64,
            CanonicalLiveAcknowledged,
        ),
        CanonicalStoreError,
    > {
        let CanonicalPrepared {
            caller,
            operation,
            checkpoint,
        } = prepared;
        let cursor = checkpoint.cursor.clone();
        let certificate = {
            let mut session = self.session.lock().await;
            session.install_canonical_live_checkpoint(
                &operation,
                revision,
                binding.as_ref(),
                checkpoint,
                admission,
                &self.serving,
            )
        };
        let certificate = match certificate {
            Ok(certificate) => certificate,
            Err(error) => {
                // Only the owner can prove this prepare wrote/admitted nothing.
                // Keep the caller alive through the exact abandon attempt. A lost
                // reply still leaves Flight blocked; it is not lifecycle recovery.
                if let Ok(acknowledged) = self
                    .owner
                    .acknowledge(CanonicalAcknowledgement {
                        operation: operation.clone(),
                        cursor: cursor.clone(),
                        phase: CanonicalAckPhase::AbandonPreparedOnly,
                        adoption: None,
                    })
                    .await
                {
                    if check_ack(&operation, &cursor, &acknowledged).is_ok() {
                        caller.complete();
                    }
                }
                return Err(failure(
                    error.to_string(),
                    CanonicalRequestedWrite::NotRequested,
                ));
            }
        };
        let local_revision = certificate.local_revision();
        let acknowledged = self
            .owner
            .acknowledge(CanonicalAcknowledgement {
                operation: operation.clone(),
                cursor: cursor.clone(),
                phase: CanonicalAckPhase::Prepared,
                adoption: Some(certificate),
            })
            .await?;
        self.session
            .lock()
            .await
            .check_canonical_live_revision(local_revision, &cursor)
            .map_err(|error| failure(error.to_string(), CanonicalRequestedWrite::NotRequested))?;
        check_ack(&operation, &cursor, &acknowledged)?;
        Ok((caller, operation, cursor, local_revision, acknowledged))
    }

    async fn publish_prepared(
        &self,
        operation: &CanonicalOperation,
        cursor: &CanonicalCursor,
        local_revision: u64,
        acknowledged: CanonicalLiveAcknowledged,
    ) -> Result<u64, CanonicalStoreError> {
        check_ack(operation, cursor, &acknowledged)?;
        let mut session = self.session.lock().await;
        session
            .publish_canonical_live_prepared(
                operation,
                cursor,
                local_revision,
                acknowledged.visible_leaf,
            )
            .map_err(|error| failure(error.to_string(), CanonicalRequestedWrite::NotRequested))?;
        Ok(session.canonical_local_revision())
    }

    /// Certify and ACK the raw source before projecting the next main call.
    ///
    /// # Errors
    /// Refuses changed authority, invalid source/adoption, or incomplete owner ACK.
    pub async fn observe(&self) -> Result<(), CanonicalStoreError> {
        let mut flight = self.enter()?;
        // Reads own only the busy slot. Cancellation/local rejection is not lost I/O.
        flight.complete = true;
        let (revision, binding, admission) = {
            let session = self.session.lock().await;
            (
                session.canonical_local_revision(),
                session.canonical_binding().cloned(),
                if session.canonical_binding().is_none()
                    && !self.owner.bootstrap_preserves_owner_admission()
                {
                    CanonicalAdmissionPolicy::Replace(session.canonical_pending_admitted_users())
                } else {
                    CanonicalAdmissionPolicy::Preserve
                },
            )
        };
        let bootstrap = binding.is_none()
            && matches!(&admission,
            CanonicalAdmissionPolicy::Replace(ids) if !ids.is_empty());
        if !bootstrap {
            let snapshot = self.owner.snapshot().await?;
            self.session
                .lock()
                .await
                .install_canonical_snapshot(revision, binding.as_ref(), snapshot)
                .map_err(|error| {
                    failure(error.to_string(), CanonicalRequestedWrite::NotRequested)
                })?;
            return Ok(());
        }
        // Nonempty bootstrap changes owner admission and retains lost-reply debt.
        flight.complete = false;
        let prepared = self
            .owner
            .prepare(CanonicalLivePrepareRequest {
                expected: binding.clone(),
                source: CanonicalSourceRequest::Full,
                mode: CanonicalPrepareMode::Observe,
                admission: admission.clone(),
            })
            .await?;
        let (caller, operation, cursor, local_revision, acknowledged) =
            self.install(prepared, revision, binding, admission).await?;
        self.publish_prepared(&operation, &cursor, local_revision, acknowledged)
            .await?;
        caller.complete();
        flight.complete = true;
        Ok(())
    }

    /// Prepare once before paid work. The returned ticket never retries callbacks.
    ///
    /// # Errors
    /// Refuses changed authority, invalid source/adoption, or incomplete owner ACK.
    pub async fn prepare_mutation(
        &self,
        kind: CanonicalMutationKind,
    ) -> Result<CanonicalLiveMutation<'_>, CanonicalStoreError> {
        self.observe().await?;
        let flight = self.enter()?;
        let (revision, binding, admission) = {
            let session = self.session.lock().await;
            (
                session.canonical_local_revision(),
                session.canonical_binding().cloned(),
                if session.canonical_binding().is_none()
                    && !self.owner.bootstrap_preserves_owner_admission()
                {
                    CanonicalAdmissionPolicy::Replace(session.canonical_pending_admitted_users())
                } else {
                    CanonicalAdmissionPolicy::Preserve
                },
            )
        };
        let prepared = self
            .owner
            .prepare(CanonicalLivePrepareRequest {
                expected: binding.clone(),
                source: CanonicalSourceRequest::Full,
                mode: CanonicalPrepareMode::Mutation(kind),
                admission: admission.clone(),
            })
            .await?;
        let (mut caller, operation, cursor, local_revision, acknowledged) =
            self.install(prepared, revision, binding, admission).await?;
        let local_revision = self
            .publish_prepared(&operation, &cursor, local_revision, acknowledged)
            .await?;
        caller.prepared_acknowledged()?;
        Ok(CanonicalLiveMutation {
            flight,
            operation,
            cursor,
            caller,
            kind,
            local_revision,
        })
    }

    /// Persist one terminal or metadata payload with no locally generated envelope.
    ///
    /// # Errors
    /// Refuses changed authority, invalid source/adoption, or incomplete owner ACK.
    pub async fn append(
        &self,
        kind: CanonicalMutationKind,
        entry: FileEntry,
    ) -> Result<String, CanonicalStoreError> {
        let draft = CanonicalEntryDraft::new(kind, entry)
            .map_err(|error| failure(error.to_string(), CanonicalRequestedWrite::NotWritten))?;
        self.prepare_mutation(kind).await?.commit(draft).await
    }

    /// Record an actual loop message; User delivery is a distinct owner operation.
    ///
    /// # Errors
    /// Refuses changed authority, invalid source/adoption, or incomplete owner ACK.
    pub async fn persist_message(&self, message: AgentMessage) -> Result<(), CanonicalStoreError> {
        self.persist_message_canonical(message).await.map(|_| ())
    }

    // The event hook returns this exact admitted row to Agent before reduction.
    // Identity comes only from the owner's named delivery result, never a body,
    // timestamp, or latest-visible-leaf search.
    pub(super) async fn persist_message_canonical(
        &self,
        message: AgentMessage,
    ) -> Result<AgentMessage, CanonicalStoreError> {
        let message = match message {
            AgentMessage::User(message) => {
                return self.admit_user(message).await.map(AgentMessage::User);
            }
            AgentMessage::Custom(message) => {
                return self.admit_custom(message).await.map(AgentMessage::Custom);
            }
            other => other,
        };
        let canonical_message = message.clone();
        let kind = match &message {
            AgentMessage::Assistant(_) | AgentMessage::ToolResult(_) => {
                CanonicalMutationKind::AdmittedTerminal
            }
            AgentMessage::User(_) => unreachable!("handled above"),
            AgentMessage::BashExecution(_)
            | AgentMessage::Custom(_)
            | AgentMessage::BranchSummary(_)
            | AgentMessage::CompactionSummary(_) => CanonicalMutationKind::OwnerMetadata,
        };
        let entry = match message {
            AgentMessage::Custom(custom) => FileEntry::CustomMessage {
                base: EntryBase {
                    id: None,
                    parent_id: None,
                    timestamp: None,
                    rest: serde_json::Map::default(),
                },
                payload: pa_types::session::CustomMessageEntry {
                    custom_type: custom.custom_type,
                    content: custom.content,
                    display: custom.display,
                    details: custom.details,
                    rest: serde_json::Map::default(),
                },
            },
            other => FileEntry::Message {
                base: EntryBase {
                    id: None,
                    parent_id: None,
                    timestamp: None,
                    rest: serde_json::Map::default(),
                },
                message: other,
            },
        };
        self.append(kind, entry).await?;
        Ok(canonical_message)
    }

    async fn admit_user(
        &self,
        message: pa_types::ai::UserMessage,
    ) -> Result<pa_types::ai::UserMessage, CanonicalStoreError> {
        self.observe().await?;
        let mut flight = self.enter()?;
        let (revision, binding) = {
            let session = self.session.lock().await;
            (
                session.canonical_local_revision(),
                session.canonical_binding().cloned(),
            )
        };
        let delivered = self
            .owner
            .admit_user(CanonicalLiveUserRequest {
                expected: binding.clone(),
                source: CanonicalSourceRequest::Full,
                message: message.clone(),
            })
            .await?;
        let id = delivered.admitted_entry_id;
        let rows = match &delivered.prepared.checkpoint.source {
            CanonicalSource::Full(rows) => rows,
            CanonicalSource::Window(window) => &window.rows,
        };
        let matching: Vec<_> = rows
            .iter()
            .filter(|row| row.entry().id() == Some(id.as_str()))
            .collect();
        let canonical = match matching.as_slice() {
            [row] => match row.entry() {
                FileEntry::Message {
                    message: AgentMessage::User(actual),
                    ..
                } if actual.content == message.content && actual.rest == message.rest => {
                    actual.clone()
                }
                _ => {
                    return Err(failure(
                        "owner-delivered User differs from exact requested intent",
                        CanonicalRequestedWrite::NotRequested,
                    ));
                }
            },
            _ => {
                return Err(failure(
                    "owner-delivered User identity is missing or duplicated",
                    CanonicalRequestedWrite::NotRequested,
                ));
            }
        };
        let admission = {
            let session = self.session.lock().await;
            if session.canonical_pending_admitted_users().first() == Some(&id) {
                CanonicalAdmissionPolicy::Preserve
            } else {
                CanonicalAdmissionPolicy::Append(vec![id.clone()])
            }
        };
        let (caller, operation, cursor, local_revision, acknowledged) = self
            .install(delivered.prepared, revision, binding, admission)
            .await?;
        {
            let mut session = self.session.lock().await;
            session
                .consume_canonical_live_user(
                    &operation,
                    &cursor,
                    local_revision,
                    &id,
                    acknowledged.visible_leaf,
                )
                .map_err(|error| {
                    failure(error.to_string(), CanonicalRequestedWrite::NotRequested)
                })?;
        }
        caller.complete();
        flight.complete = true;
        Ok(canonical)
    }

    async fn persist_context_notice(
        &self,
        message: pa_types::session::CustomMessage,
    ) -> Result<AgentMessage, CanonicalStoreError> {
        let payload = pa_types::session::CustomMessageEntry {
            custom_type: message.custom_type,
            content: message.content,
            display: message.display,
            details: message.details,
            rest: message.rest,
        };
        let id = self
            .append(
                CanonicalMutationKind::OwnerMetadata,
                FileEntry::CustomMessage {
                    base: draft_base(),
                    payload: payload.clone(),
                },
            )
            .await?;
        let canonical = {
            let session = self.session.lock().await;
            session
                .canonical_raw_entry(&id)
                .and_then(|row| match row.entry() {
                    FileEntry::CustomMessage {
                        payload: actual, ..
                    } if actual == &payload => {
                        Some(AgentMessage::Custom(pa_types::session::CustomMessage {
                            custom_type: actual.custom_type.clone(),
                            content: actual.content.clone(),
                            display: actual.display,
                            details: actual.details.clone(),
                            timestamp: crate::session::timestamp_to_millis(row.entry().timestamp()),
                            rest: actual.rest.clone(),
                        }))
                    }
                    _ => None,
                })
        };
        canonical.ok_or_else(|| {
            self.block_projection();
            failure(
                "committed context notice has no exact canonical row",
                CanonicalRequestedWrite::Committed,
            )
        })
    }

    async fn admit_custom(
        &self,
        message: pa_types::session::CustomMessage,
    ) -> Result<pa_types::session::CustomMessage, CanonicalStoreError> {
        self.observe().await?;
        let mut flight = self.enter()?;
        let (revision, binding) = {
            let session = self.session.lock().await;
            (
                session.canonical_local_revision(),
                session.canonical_binding().cloned(),
            )
        };
        let delivered = self
            .owner
            .admit_custom(CanonicalLiveCustomRequest {
                expected: binding.clone(),
                source: CanonicalSourceRequest::Full,
                message: message.clone(),
            })
            .await?;
        let id = delivered.admitted_entry_id;
        let rows = match &delivered.prepared.checkpoint.source {
            CanonicalSource::Full(rows) => rows,
            CanonicalSource::Window(window) => &window.rows,
        };
        let expected = FileEntry::CustomMessage {
            base: draft_base(),
            payload: pa_types::session::CustomMessageEntry {
                custom_type: message.custom_type,
                content: message.content,
                display: message.display,
                details: message.details,
                rest: message.rest,
            },
        };
        let matching: Vec<_> = rows
            .iter()
            .filter(|row| row.entry().id() == Some(id.as_str()))
            .collect();
        if matching.len() != 1 || payload_intent(matching[0].entry())? != payload_intent(&expected)?
        {
            return Err(failure(
                "owner-delivered CustomMessage differs from requested semantic intent",
                CanonicalRequestedWrite::NotRequested,
            ));
        }
        let canonical = match matching[0].entry() {
            FileEntry::CustomMessage { payload, .. } => pa_types::session::CustomMessage {
                custom_type: payload.custom_type.clone(),
                content: payload.content.clone(),
                display: payload.display,
                details: payload.details.clone(),
                timestamp: crate::session::timestamp_to_millis(matching[0].entry().timestamp()),
                rest: payload.rest.clone(),
            },
            _ => {
                return Err(failure(
                    "owner-delivered Custom identity has another kind",
                    CanonicalRequestedWrite::NotRequested,
                ));
            }
        };
        let (caller, operation, cursor, local_revision, acknowledged) = self
            .install(
                delivered.prepared,
                revision,
                binding,
                CanonicalAdmissionPolicy::Preserve,
            )
            .await?;
        {
            let mut session = self.session.lock().await;
            session
                .consume_canonical_live_custom(
                    &operation,
                    &cursor,
                    local_revision,
                    &id,
                    acknowledged.visible_leaf,
                )
                .map_err(|error| {
                    failure(error.to_string(), CanonicalRequestedWrite::NotRequested)
                })?;
        }
        caller.complete();
        flight.complete = true;
        Ok(canonical)
    }

    /// Adopt only the actual owner's exact raw-same one-step branch successor.
    ///
    /// # Errors
    /// Refuses a different owner/source, skipped revisions, local conflict or ACK failure.
    pub async fn adopt_navigation(
        &self,
        prepared: CanonicalPrepared,
        expected: CanonicalCursor,
    ) -> Result<(), CanonicalStoreError> {
        let mut flight = self.enter()?;
        let revision = {
            let session = self.session.lock().await;
            if session.canonical_binding() != Some(&expected) {
                return Err(failure(
                    "navigation local source changed",
                    CanonicalRequestedWrite::NotRequested,
                ));
            }
            session.canonical_local_revision()
        };
        let next = &prepared.checkpoint.cursor;
        if next.owner != expected.owner
            || next.session_id != expected.session_id
            || next.source_path != expected.source_path
            || next.header_certificate != expected.header_certificate
            || next.raw_source_generation != expected.raw_source_generation
            || next.frontier != expected.frontier
            || Some(next.canonical_revision) != expected.canonical_revision.checked_add(1)
            || Some(next.branch_revision) != expected.branch_revision.checked_add(1)
            || !matches!(prepared.checkpoint.source, CanonicalSource::Full(_))
        {
            return Err(failure(
                "navigation is not the exact raw-same owner branch successor",
                CanonicalRequestedWrite::NotRequested,
            ));
        }
        let (caller, operation, cursor, revision, acknowledged) = self
            .install(
                prepared,
                revision,
                Some(expected),
                CanonicalAdmissionPolicy::Replace(Vec::new()),
            )
            .await?;
        self.publish_prepared(&operation, &cursor, revision, acknowledged)
            .await?;
        caller.complete();
        flight.complete = true;
        Ok(())
    }

    /// Runs outside strategy projection and outside any SDK/Agent guard.
    ///
    /// # Errors
    /// Refuses changed authority, invalid source/adoption, or incomplete owner ACK.
    pub async fn before_model(
        &self,
        model: pa_agent::types::Model,
    ) -> Result<(), CanonicalStoreError> {
        let projection = self
            .projection
            .lock()
            .map_err(|_| {
                failure(
                    "canonical projection lock poisoned",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?
            .take()
            .ok_or_else(|| {
                failure(
                    "main stream has no certified transform snapshot",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?;
        let mut flight = self.enter()?;
        let run = self
            .run
            .lock()
            .map_err(|_| {
                failure(
                    "canonical run lock poisoned",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?
            .clone()
            .ok_or_else(|| {
                failure(
                    "main inference has no actual AgentStart",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?;
        if !Arc::ptr_eq(&run, &projection.run) {
            return Err(failure(
                "canonical projection belongs to another run",
                CanonicalRequestedWrite::NotWritten,
            ));
        }
        {
            let session = self.session.lock().await;
            session
                .check_canonical_live_revision(projection.local_revision, &projection.cursor)
                .map_err(|error| failure(error.to_string(), CanonicalRequestedWrite::NotWritten))?;
        }
        self.owner
            .begin_provider_step(CanonicalLiveProviderStepRequest {
                expected: projection.cursor.clone(),
                run,
                model: Arc::new(model),
                invocation: Arc::new(CanonicalLiveProviderInvocation { _private: () }),
            })
            .await?;
        {
            let session = self.session.lock().await;
            session
                .check_canonical_live_revision(projection.local_revision, &projection.cursor)
                .map_err(|error| failure(error.to_string(), CanonicalRequestedWrite::NotWritten))?;
        }
        flight.complete = true;
        Ok(())
    }

    pub(crate) fn bind_archive(
        &self,
        archive: Arc<std::sync::RwLock<Option<pa_types::session::ContextArchive>>>,
    ) -> Result<(), CanonicalStoreError> {
        *self.archive.lock().map_err(|_| {
            failure(
                "canonical archive lock poisoned",
                CanonicalRequestedWrite::NotRequested,
            )
        })? = Some(archive);
        Ok(())
    }

    /// Certify the canonical view before the loop converts it to provider messages.
    ///
    /// # Errors
    /// Refuses unresolved receipts, missing runs, source drift, or a lost prior projection.
    pub async fn transform_messages(
        &self,
    ) -> Result<Vec<pa_agent::types::AgentMessage>, CanonicalStoreError> {
        self.observe().await?;
        let mut flight = self.enter()?;
        let run = self
            .run
            .lock()
            .map_err(|_| {
                failure(
                    "canonical run lock poisoned",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?
            .clone()
            .ok_or_else(|| {
                failure(
                    "context transform has no actual AgentStart",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?;
        let (context, cursor, local_revision) = {
            let session = self.session.lock().await;
            (
                session.canonical_visible_context(),
                session.canonical_binding().cloned().ok_or_else(|| {
                    failure(
                        "context transform has no owner binding",
                        CanonicalRequestedWrite::NotRequested,
                    )
                })?,
                session.canonical_local_revision(),
            )
        };
        // Typed legacy harness exclusions are model-view policy, not conversion
        // failures. Keep the certified context and physical history untouched,
        // then retain strict conversion for every row left in the model view.
        let model_messages = super::refinement_gate::without_refinements(context.messages);
        let messages = model_messages
            .iter()
            .map(|message| {
                super::session_message_to_loop(message).ok_or_else(|| {
                    failure(
                        "canonical context row cannot enter the loop",
                        CanonicalRequestedWrite::NotRequested,
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let archive = self
            .archive
            .lock()
            .map_err(|_| {
                failure(
                    "canonical archive lock poisoned",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?
            .clone();
        if let Some(archive) = archive {
            *archive.write().map_err(|_| {
                failure(
                    "canonical archive state poisoned",
                    CanonicalRequestedWrite::NotRequested,
                )
            })? = context.archive;
        }
        let mut pending = self.projection.lock().map_err(|_| {
            failure(
                "canonical projection lock poisoned",
                CanonicalRequestedWrite::NotRequested,
            )
        })?;
        if pending.is_some() {
            return Err(failure(
                "canonical transform already has a pending invocation",
                CanonicalRequestedWrite::NotRequested,
            ));
        }
        *pending = Some(CanonicalProjection {
            cursor,
            local_revision,
            run,
        });
        flight.complete = true;
        Ok(messages)
    }

    pub(crate) async fn start_run(&self) -> Result<(), CanonicalStoreError> {
        self.observe().await?;
        let mut flight = self.enter()?;
        {
            let run = self.run.lock().map_err(|_| {
                failure(
                    "canonical run lock poisoned",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?;
            if run.is_some() {
                return Err(failure(
                    "actual canonical run already active",
                    CanonicalRequestedWrite::NotRequested,
                ));
            }
        }
        let expected = self
            .session
            .lock()
            .await
            .canonical_binding()
            .cloned()
            .ok_or_else(|| {
                failure(
                    "actual AgentStart has no canonical source",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?;
        let run = Arc::new(CanonicalLiveRun { _private: () });
        self.owner
            .register_run(CanonicalLiveRunRequest {
                expected,
                run: Arc::clone(&run),
            })
            .await?;
        *self.run.lock().map_err(|_| {
            failure(
                "canonical run lock poisoned",
                CanonicalRequestedWrite::NotRequested,
            )
        })? = Some(run);
        flight.complete = true;
        Ok(())
    }

    pub(crate) fn cancel_projection_at_run_end(&self) {
        if let Ok(mut projection) = self.projection.lock() {
            projection.take();
        } else {
            self.blocked.store(true, Ordering::Release);
        }
    }

    pub(crate) fn end_run(&self) {
        // Only the actual terminal run boundary may discard a snapshot that
        // never reached StreamFn. Such a snapshot admitted no provider work.
        if let Ok(mut projection) = self.projection.lock() {
            projection.take();
        } else {
            self.blocked.store(true, Ordering::Release);
        }
        if let Ok(mut run) = self.run.lock() {
            *run = None;
        } else {
            self.blocked.store(true, Ordering::Release);
        }
    }

    pub(crate) async fn start_tool(
        &self,
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    ) -> Result<(), CanonicalStoreError> {
        let mut flight = self.enter()?;
        let run = self
            .run
            .lock()
            .map_err(|_| {
                failure(
                    "canonical run lock poisoned",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?
            .clone()
            .ok_or_else(|| {
                failure(
                    "tool execution has no actual AgentStart",
                    CanonicalRequestedWrite::NotRequested,
                )
            })?;
        self.owner
            .admit_tool(CanonicalLiveToolRequest {
                run,
                tool: Arc::new(CanonicalLiveToolExecution {
                    tool_call_id,
                    tool_name,
                    args,
                }),
            })
            .await?;
        flight.complete = true;
        Ok(())
    }
}

impl CanonicalLiveMutation<'_> {
    #[must_use]
    pub fn cursor(&self) -> &CanonicalCursor {
        &self.cursor
    }
    #[must_use]
    pub fn source_cursor(&self) -> &CanonicalCursor {
        &self.cursor
    }

    /// One strict append, full suffix adoption, ACK, then visible publication.
    ///
    /// # Errors
    /// Refuses changed authority, invalid source/adoption, or incomplete owner ACK.
    pub async fn commit(self, draft: CanonicalEntryDraft) -> Result<String, CanonicalStoreError> {
        Ok(self.commit_receipt(draft).await?.entry_id)
    }

    ///
    /// # Errors
    /// Refuses changed authority, invalid source/adoption, or incomplete owner ACK.
    pub async fn commit_receipt(
        self,
        draft: CanonicalEntryDraft,
    ) -> Result<CanonicalMetadataReceipt, CanonicalStoreError> {
        self.commit_receipt_with_context(draft, None).await
    }

    pub(super) async fn commit_receipt_with_context(
        mut self,
        draft: CanonicalEntryDraft,
        context_admission: Option<Arc<CanonicalLiveContextAdmission>>,
    ) -> Result<CanonicalMetadataReceipt, CanonicalStoreError> {
        if draft.kind() != self.kind {
            return Err(failure(
                "mutation kind changed",
                CanonicalRequestedWrite::NotWritten,
            ));
        }
        let requested_entry = draft.entry().clone();
        let run = if self.kind == CanonicalMutationKind::AdmittedTerminal {
            Some(
                self.flight
                    .live
                    .run
                    .lock()
                    .map_err(|_| {
                        failure(
                            "canonical run lock poisoned",
                            CanonicalRequestedWrite::NotWritten,
                        )
                    })?
                    .clone()
                    .ok_or_else(|| {
                        failure(
                            "terminal has no actual registered run",
                            CanonicalRequestedWrite::NotWritten,
                        )
                    })?,
            )
        } else {
            None
        };
        let disposition = self
            .flight
            .live
            .owner
            .append(CanonicalLiveAppendRequest {
                run,
                context_admission,
                operation: self.operation.clone(),
                expected: self.cursor.clone(),
                source: CanonicalSourceRequest::Full,
                draft,
            })
            .await?;
        let (receipt, committed) = match disposition {
            CanonicalCommitDisposition::Committed(receipt) => (receipt, true),
            CanonicalCommitDisposition::NeedsCatchUp(receipt) => (receipt, false),
        };
        if receipt.operation != self.operation
            || receipt.from != self.cursor
            || receipt.committed_entry_id.is_some() != committed
        {
            return Err(failure(
                "owner receipt differs from prepared operation/source",
                CanonicalRequestedWrite::Unknown,
            ));
        }
        if committed {
            let Some(id) = receipt
                .committed_entry_id
                .as_deref()
                .filter(|id| !id.is_empty())
            else {
                return Err(failure(
                    "committed receipt lacks exact ID",
                    CanonicalRequestedWrite::Committed,
                ));
            };
            let matching: Vec<_> = receipt
                .rows
                .iter()
                .filter(|row| row.entry().id() == Some(id))
                .collect();
            if matching.len() != 1
                || payload_intent(matching[0].entry())? != payload_intent(&requested_entry)?
            {
                return Err(failure(
                    "committed row differs from exact draft payload",
                    CanonicalRequestedWrite::Committed,
                ));
            }
        } else if receipt.from == receipt.to {
            return Err(failure(
                "catchup did not advance certified source",
                CanonicalRequestedWrite::NotWritten,
            ));
        }
        let id = receipt.committed_entry_id.clone();
        let cursor = receipt.to.clone();
        let certificate = {
            let mut session = self.flight.live.session.lock().await;
            session
                .apply_canonical_suffix(
                    self.local_revision,
                    receipt,
                    CanonicalLocalAdoption::preserve(),
                )
                .map_err(|error| {
                    failure(
                        error.to_string(),
                        if committed {
                            CanonicalRequestedWrite::Committed
                        } else {
                            CanonicalRequestedWrite::NotWritten
                        },
                    )
                })?
        };
        let acknowledged = self
            .flight
            .live
            .owner
            .acknowledge(CanonicalAcknowledgement {
                operation: self.operation.clone(),
                cursor: cursor.clone(),
                phase: CanonicalAckPhase::AppliedReady,
                adoption: Some(certificate.clone()),
            })
            .await?;
        check_ack(&self.operation, &cursor, &acknowledged)?;
        {
            let mut session = self.flight.live.session.lock().await;
            session
                .publish_canonical_live_receipt(
                    crate::session::manager::CanonicalLivePublication::after_owner_ack(
                        certificate,
                        &self.flight.live.serving,
                        acknowledged.visible_leaf,
                    ),
                )
                .map_err(|error| failure(error.to_string(), CanonicalRequestedWrite::Committed))?;
        }
        self.caller.complete();
        self.flight.complete = true;
        if !committed {
            return Err(failure(
                "canonical source advanced; draft not written; paid work is not retried",
                CanonicalRequestedWrite::NotWritten,
            ));
        }
        let entry_id = id.ok_or_else(|| {
            failure(
                "committed receipt lacks identity",
                CanonicalRequestedWrite::Committed,
            )
        })?;
        Ok(CanonicalMetadataReceipt { entry_id, cursor })
    }

    /// Abandon only a prepared ticket with no outstanding write.
    ///
    /// # Errors
    /// Refuses changed authority, invalid source/adoption, or incomplete owner ACK.
    pub async fn abandon(mut self) -> Result<(), CanonicalStoreError> {
        self.flight
            .live
            .owner
            .acknowledge(CanonicalAcknowledgement {
                operation: self.operation.clone(),
                cursor: self.cursor.clone(),
                phase: CanonicalAckPhase::AbandonPreparedOnly,
                adoption: None,
            })
            .await?;
        self.caller.complete();
        self.flight.complete = true;
        Ok(())
    }
}

/// Main-call wrapper. Auxiliary inference deliberately does not use this wrapper.
pub(crate) fn wrap_stream_fn(
    live: Arc<CanonicalLiveSession>,
    stream: pa_agent::stream::StreamFn,
) -> pa_agent::stream::StreamFn {
    Arc::new(move |model, context, options| {
        let live = live.clone();
        let stream = stream.clone();
        Box::pin(async move {
            live.before_model(model.clone()).await?;
            stream(model, context, options).await
        })
    })
}

/// Noncanonical envelope intent. Only the actual owner fills these fields.
pub(crate) fn draft_base() -> EntryBase {
    EntryBase {
        id: None,
        parent_id: None,
        timestamp: None,
        rest: serde_json::Map::default(),
    }
}

fn payload_intent(entry: &FileEntry) -> Result<serde_json::Value, CanonicalStoreError> {
    let mut value = serde_json::to_value(entry)
        .map_err(|error| failure(error.to_string(), CanonicalRequestedWrite::Committed))?;
    if let Some(object) = value.as_object_mut() {
        for key in ["id", "parentId", "timestamp"] {
            object.remove(key);
        }
    }
    Ok(value)
}

impl CanonicalMetadataWriter for CanonicalLiveSession {
    fn observe_metadata_source(self: Arc<Self>) -> CanonicalFuture<()> {
        Box::pin(async move { self.observe().await })
    }

    fn append_metadata(self: Arc<Self>, entry: FileEntry) -> CanonicalFuture<String> {
        Box::pin(async move {
            self.append(CanonicalMutationKind::OwnerMetadata, entry)
                .await
        })
    }
    fn append_metadata_from(
        self: Arc<Self>,
        entry: FileEntry,
        expected: CanonicalCursor,
    ) -> CanonicalFuture<CanonicalMetadataReceipt> {
        Box::pin(async move {
            let draft = CanonicalEntryDraft::new(CanonicalMutationKind::OwnerMetadata, entry)
                .map_err(|error| failure(error.to_string(), CanonicalRequestedWrite::NotWritten))?;
            let ticket = self
                .prepare_mutation(CanonicalMutationKind::OwnerMetadata)
                .await?;
            if ticket.cursor() != &expected {
                ticket.abandon().await?;
                return Err(failure(
                    "metadata source advanced; precomputed policy intent not written",
                    CanonicalRequestedWrite::NotWritten,
                ));
            }
            ticket.commit_receipt(draft).await
        })
    }
}

fn check_ack(
    operation: &CanonicalOperation,
    cursor: &CanonicalCursor,
    acknowledged: &CanonicalLiveAcknowledged,
) -> Result<(), CanonicalStoreError> {
    if &acknowledged.operation != operation || &acknowledged.cursor != cursor {
        return Err(failure(
            "owner ACK publication proof differs from adopted operation/source",
            CanonicalRequestedWrite::Unknown,
        ));
    }
    Ok(())
}

/// The actual loop invokes this before `convert_to_llm` and before any stream wrapper.
pub(crate) fn wrap_transform_context(
    live: Arc<CanonicalLiveSession>,
    transform: pa_agent::agent_loop::TransformContextFn,
) -> pa_agent::agent_loop::TransformContextFn {
    Arc::new(move |_stale_messages, signal| {
        let live = Arc::clone(&live);
        let transform = Arc::clone(&transform);
        Box::pin(async move {
            let messages = live.transform_messages().await?;
            transform(messages, signal).await
        })
    })
}

/// Persist bound events before Agent takes its delivery or state guards.
pub(crate) fn before_event(
    live: Arc<CanonicalLiveSession>,
    session: Arc<tokio::sync::Mutex<SessionManager>>,
) -> pa_agent::agent::BeforeEventFn {
    Arc::new(move |event, _signal| {
        let live = Arc::clone(&live);
        let session = Arc::clone(&session);
        Box::pin(async move {
            // A rejected post-ACK phase quarantines publication, including the
            // Agent lifecycle's synthetic failure events. Durable rows stay intact.
            if live.blocked.load(Ordering::Acquire) {
                return Err(failure(
                    "canonical receipt has not completed",
                    CanonicalRequestedWrite::NotRequested,
                )
                .into());
            }
            if let pa_agent::types::AgentEvent::MessageEnd { message } = &event {
                if let Some(message @ (AgentMessage::User(_) | AgentMessage::Custom(_))) =
                    super::loop_message_to_session(message)
                {
                    let delivery = match &event {
                        pa_agent::types::AgentEvent::MessageEnd {
                            message: pa_agent::types::AgentMessage::Custom(custom),
                        } => custom.delivery,
                        _ => pa_agent::types::CustomMessageDelivery::Input,
                    };
                    let canonical = match message {
                        AgentMessage::Custom(custom)
                            if delivery == pa_agent::types::CustomMessageDelivery::Context =>
                        {
                            live.persist_context_notice(custom).await?
                        }
                        message => live.persist_message_canonical(message).await?,
                    };
                    let mut message =
                        super::session_message_to_loop(&canonical).ok_or_else(|| {
                            live.block_projection();
                            anyhow::anyhow!("canonical named delivery cannot enter Agent state")
                        })?;
                    if let pa_agent::types::AgentMessage::Custom(custom) = &mut message {
                        custom.delivery = delivery;
                    }
                    return Ok(pa_agent::types::AgentEvent::MessageEnd { message });
                }
            }
            // Preserve every other native event, its fields and allocations.
            // The owner durability hook runs on a clone; Agent reduces exactly
            // the original event after that receipt has ACKed.
            super::persist_event(&session, Some(&live), event.clone()).await?;
            if matches!(
                &event,
                pa_agent::types::AgentEvent::MessageEnd {
                    message: pa_agent::types::AgentMessage::Standard(
                        pa_agent::types::Message::Assistant(assistant)
                    )
                } if !matches!(
                    assistant.stop_reason,
                    pa_agent::types::StopReason::Error | pa_agent::types::StopReason::Aborted
                )
            ) {
                live.run_after_assistant_commit().await?;
            }
            Ok(event)
        })
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod visible_empty_tests {
    use super::*;
    use crate::session::canonical::{CanonicalFrontier, CanonicalJournalRow, CanonicalOwnerScope};
    use sha2::{Digest, Sha256};
    #[test]
    fn live_bootstrap_with_queued_user_keeps_none_visible_empty() {
        let header_raw: Arc<[u8]> = Arc::from(&b"{\"type\":\"session\",\"id\":\"queued\",\"version\":3,\"timestamp\":\"t\",\"cwd\":\"/private\"}\n"[..]);
        let header = CanonicalJournalRow::parse(header_raw.clone(), 0, 0).unwrap();
        let user_raw: Arc<[u8]> = Arc::from(&b"{\"type\":\"message\",\"id\":\"accepted\",\"parentId\":null,\"timestamp\":\"t\",\"message\":{\"role\":\"user\",\"content\":\"not yet delivered\",\"timestamp\":7}}\n"[..]);
        let row = CanonicalJournalRow::parse(user_raw.clone(), header_raw.len() as u64, 1).unwrap();
        let mut hash = Sha256::new();
        hash.update(&header_raw);
        hash.update(&user_raw);
        let prefix = hash.finalize().into();
        let cursor = CanonicalCursor {
            owner: CanonicalOwnerScope::new(),
            session_id: "queued".into(),
            source_path: "/private/queued.jsonl".into(),
            raw_source_generation: prefix,
            header_certificate: Sha256::digest(&header_raw).into(),
            canonical_revision: 1,
            branch_revision: 0,
            selected_leaf: Some("accepted".into()),
            frontier: CanonicalFrontier {
                bytes: (header_raw.len() + user_raw.len()) as u64,
                rows: 2,
                prefix_certificate: prefix,
            },
        };
        let FileEntry::Header { header: parsed } = header.entry() else {
            unreachable!()
        };
        let mut manager = SessionManager::in_memory_from_header(
            std::path::Path::new("/private"),
            std::path::Path::new("/private/sessions"),
            parsed.clone(),
        )
        .unwrap();
        let serving = Arc::new(());
        let operation = CanonicalOperation::new();
        manager
            .install_canonical_live_checkpoint(
                &operation,
                0,
                None,
                CanonicalCheckpoint {
                    cursor,
                    header,
                    source: CanonicalSource::Full(vec![row]),
                },
                CanonicalAdmissionPolicy::Replace(vec!["accepted".into()]),
                &serving,
            )
            .unwrap();
        assert_eq!(manager.canonical_visible_leaf(), None);
        assert!(manager.active_context().messages.is_empty());
        assert_eq!(manager.canonical_pending_admitted_users(), vec!["accepted"]);
        assert!(manager.canonical_raw_entry("accepted").is_some());
    }
}
