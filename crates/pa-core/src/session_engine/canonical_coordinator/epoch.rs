//! Production epoch handle: the one supported way a real writer takes authority.
//!
//! Division of ownership, restated because it is the whole point of this module:
//!
//! * Core owns the served epoch - admission, counters, close/gate linearization,
//!   quarantine, terminal slots and the serving identity.
//! * The writer owns the bytes and the classification. It asks this handle for a
//!   [`CanonicalAppendGrant`], does the I/O, then calls
//!   [`CanonicalAppendGrant::settle`] exactly once.
//!
//! The order is fixed by the types: a grant admits its flight and claims its
//! single-use recorder before the writer can touch a byte, and `settle` records
//! before it drops the flight. There is no public way to record after the drop.
//!
//! This module adds no authority of its own: every decision below is a call on
//! the private authority primitives in [`super::canonical_authority`].

use std::any::Any;
use std::sync::Arc;

use crate::session::canonical::{
    CanonicalCursor, CanonicalEntryDraft, CanonicalIoFlight, CanonicalIoStage, CanonicalOperation,
    CanonicalStoreError, CanonicalWritePermit,
};
use crate::session_engine::canonical_authority::{
    AuthorityOwner, CapturedRun, CloseReason, CloseTicket, DrainRefusal, DrainStatus, LateRunAnchor,
};

pub use crate::session_engine::canonical_authority::MetadataAction;

use super::{CanonicalWriteCertainty, CanonicalWriteRecorder, CanonicalWriteTicket};

/// The drain state of one epoch, as a production caller may observe it.
///
/// A view only exists after [`CanonicalEpoch::request_close`]; there is no
/// pre-close accessor, so `Blocked` never means "not closing yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalDrainView {
    /// Requested close, and nothing is outstanding.
    Drained,
    /// Requested close, but admitted work is still outstanding.
    Pending {
        /// Admitted I/O flights.
        io: usize,
        /// Admitted transaction leases.
        transactions: usize,
        /// Registered run anchors.
        anchors: usize,
    },
    /// The epoch cannot complete a drain: it is poisoned by an uncertain write,
    /// or its state is unreadable. This view only exists on a [`CanonicalClosing`],
    /// which `request_close` created, so it is never the "no close requested"
    /// state - that case has no accessor at all.
    Blocked,
    /// The caller still holds its own lease or anchor.
    WouldDrainSelf,
}

impl From<DrainStatus> for CanonicalDrainView {
    fn from(status: DrainStatus) -> Self {
        match status {
            DrainStatus::Drained => Self::Drained,
            DrainStatus::Pending {
                io,
                transactions,
                anchors,
            } => Self::Pending {
                io,
                transactions,
                anchors,
            },
            DrainStatus::Blocked => Self::Blocked,
            DrainStatus::WouldDrainSelf => Self::WouldDrainSelf,
        }
    }
}

impl From<DrainRefusal> for CanonicalDrainView {
    fn from(refusal: DrainRefusal) -> Self {
        match refusal {
            DrainRefusal::WouldDrainSelf => Self::WouldDrainSelf,
            DrainRefusal::Pending {
                io,
                transactions,
                anchors,
            } => Self::Pending {
                io,
                transactions,
                anchors,
            },
            DrainRefusal::Blocked => Self::Blocked,
        }
    }
}

/// One admitted I/O flight plus its single-use recorder.
///
/// The writer must call [`Self::settle`] exactly once. Dropping it un-settled
/// records `Uncertain` (the recorder's own `Drop`) and then releases the flight,
/// so a cancelled or panicking writer can never look `Known`.
#[must_use]
pub struct CanonicalAppendGrant {
    // Rust drops fields in declaration order. A cancelled writer classifies
    // Uncertain before its flight is released, just like explicit settlement.
    recorder: CanonicalWriteRecorder,
    flight: CanonicalIoFlight,
    permit: CanonicalWritePermit,
}

impl CanonicalAppendGrant {
    /// Record the writer's own observation, then release the flight.
    ///
    /// # Errors
    /// Typed refusal when the classification cannot be recorded; the flight is
    /// then released unclassified, which fails closed (the epoch will not drain).
    pub fn settle(self, certainty: CanonicalWriteCertainty) -> Result<(), CanonicalStoreError> {
        let CanonicalAppendGrant {
            permit,
            flight,
            recorder,
        } = self;
        recorder.record(certainty)?;
        drop(flight);
        drop(permit);
        Ok(())
    }
}

/// The two halves a coordinator-driven commit needs.
///
/// The store puts [`Self::into_request_parts`]' permit into
/// `CanonicalCommitRequest`, so the flight the store admits inside `commit` is the
/// caller's authority, not a second one. The recorder half stays with the store and
/// claims the single-use classification for the stage that same permit admitted.
pub struct CanonicalCommitGrant {
    permit: CanonicalWritePermit,
    control: crate::session_engine::canonical_authority::TransactionControl,
    /// The ONE stage this grant may classify. The permit alone cannot be narrowed
    /// (it is a frozen v3 type), so the narrowing lives here: the recorder refuses
    /// every other stage, and an unconsumed flight at any other stage therefore
    /// fails closed instead of being silently classifiable.
    stage: CanonicalIoStage,
}

impl CanonicalCommitGrant {
    /// Split into the permit the request carries and the recorder the writer keeps.
    ///
    /// This is the ONLY way to obtain either half. Dropping the recorder without
    /// claiming it leaves the flight that permit admits unclassified, which fails
    /// closed: the epoch is poisoned when the flight drops, and cannot drain. The
    /// split is safe only because dropping half of it is loud.
    #[must_use]
    pub fn into_request_parts(self) -> (CanonicalWritePermit, CanonicalCommitRecorder) {
        (
            self.permit,
            CanonicalCommitRecorder {
                control: self.control,
                stage: self.stage,
            },
        )
    }
}

/// The writer half of a coordinator-driven commit.
///
/// It can classify exactly one flight per stage, and only a stage this authority
/// actually admitted; both refusals are typed.
pub struct CanonicalCommitRecorder {
    control: crate::session_engine::canonical_authority::TransactionControl,
    stage: CanonicalIoStage,
}

impl CanonicalCommitRecorder {
    /// Claim the single-use recorder for the stage the store admitted.
    ///
    /// # Errors
    /// Typed refusal when this authority has no outstanding flight for the stage,
    /// or when the recorder was already claimed.
    pub fn claim(&self) -> Result<CanonicalWriteRecorder, CanonicalStoreError> {
        let inner = self.control.io_recorder(self.stage)?;
        Ok(CanonicalWriteRecorder::from_io(inner))
    }

    /// The stage this recorder may classify.
    #[must_use]
    pub fn stage(&self) -> CanonicalIoStage {
        self.stage
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "consumes the actual authority scope and retains its original exit drop order"
)]
fn append_scoped(
    scope: crate::session_engine::canonical_authority::CapturedScope,
    source: &CanonicalCursor,
    stage: CanonicalIoStage,
    draft: &CanonicalEntryDraft,
) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
    let (capture, prepare) = scope.begin_prepare()?;
    let operation = CanonicalOperation::new();
    // This prepare flight binds the operation but performs no I/O. Known means
    // no bytes were attempted here, never that the requested row was written.
    let prepare_ticket = CanonicalWriteTicket::from_admitted(prepare);
    let prepare_flight = prepare_ticket.permit.enter_io(
        CanonicalIoStage::PreparePreflight,
        source,
        &operation,
        None,
    )?;
    prepare_ticket
        .recorder(CanonicalIoStage::PreparePreflight)?
        .record(CanonicalWriteCertainty::Known)?;
    drop(prepare_flight);
    drop(prepare_ticket);
    let admitted = capture.admit_final(&operation, draft.clone())?;
    append_grant(admitted, source, stage, &operation, Some(draft))
}

fn append_grant(
    admitted: crate::session_engine::canonical_authority::AdmittedTransaction,
    source: &CanonicalCursor,
    stage: CanonicalIoStage,
    operation: &CanonicalOperation,
    draft: Option<&CanonicalEntryDraft>,
) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
    let ticket = CanonicalWriteTicket::from_admitted(admitted);
    let flight = ticket.permit.enter_io(stage, source, operation, draft)?;
    let recorder = ticket.recorder(stage)?;
    Ok(CanonicalAppendGrant {
        permit: ticket.into_permit(),
        flight,
        recorder,
    })
}

fn foreign_terminal() -> CanonicalStoreError {
    CanonicalStoreError {
        kind: crate::session::canonical::CanonicalFailureKind::OwnerChanged,
        requested: crate::session::canonical::CanonicalRequestedWrite::NotWritten,
        source: crate::session::canonical::CanonicalSourceDisposition::Unchanged,
        detail: "terminal capture belongs to a different serving epoch".to_owned(),
    }
}

/// A public, opaque handle to ONE admitted provider main step.
///
/// The authority's own step type is module-private. This handle exists because the
/// step is admitted where the provider main step actually starts - inside the run
/// that owns it - while the assistant row is written later by the session store.
/// Handing the handle out is the ONLY channel: the store never mints a step, and a
/// step admitted without being written is harmless (an unconsumed seal is not a
/// classification).
pub struct CanonicalProviderStep {
    inner: crate::session_engine::canonical_authority::ProviderStep,
}

impl CanonicalProviderStep {
    /// Admit the actual tool execution before it starts.
    ///
    /// The call ID must occur exactly once in this step's sealed assistant row.
    /// The native tool allocation remains pinned through its final result.
    ///
    /// # Errors
    /// Refuses an unsealed step, an absent/ambiguous/already admitted call ID,
    /// and a closed or poisoned serving capture.
    pub fn admit_tool<T: Any + Send + Sync>(
        &self,
        tool_call_id: &str,
        actual_tool: Arc<T>,
    ) -> Result<CanonicalAdmittedTool, CanonicalStoreError> {
        Ok(CanonicalAdmittedTool {
            inner: self.inner.admit_tool(tool_call_id, actual_tool)?,
        })
    }
}

/// One actual tool admitted from a sealed canonical assistant tool call.
/// It can seal one native result with that call's exact ID and name.
pub struct CanonicalAdmittedTool {
    inner: crate::session_engine::canonical_authority::AdmittedTool,
}

/// The actual `AgentStart` allocation registered in a source-only owner epoch.
/// Not Clone. Finish only after this epoch's run tails are adopted and acknowledged.
pub struct CanonicalOwnerRun {
    inner: crate::session_engine::canonical_authority::OwnerRun,
}
impl CanonicalOwnerRun {
    /// Consume the actual run's ordered boundary. Drop without it quarantines.
    pub fn finish_ordered_boundary(self) {
        self.inner.finish_ordered_boundary();
    }
}

/// Source-bound authority of the actual session-file owner.
///
/// This admits receipt-time preparation and exact lifecycle rows before a model
/// serves. It has no run/model allocation. Actual serving capture attaches to the
/// same gate, counters and source identity, never to a parallel authority.
pub struct CanonicalOwnerEpoch {
    owner: Arc<AuthorityOwner>,
    source: CanonicalCursor,
}

impl CanonicalOwnerEpoch {
    /// Attach the FIRST epoch to the actual session-file source.
    #[must_use]
    pub fn genesis(source: &CanonicalCursor) -> Self {
        Self {
            owner: Arc::new(AuthorityOwner::genesis(source.clone())),
            source: source.clone(),
        }
    }

    /// Attach a successor only after the previous epoch's ordered drain.
    ///
    /// # Errors
    /// Refuses a different owner, session, header, path or branch identity.
    pub fn successor(
        source: &CanonicalCursor,
        retired: CanonicalRetiredEpoch,
    ) -> Result<Self, CanonicalStoreError> {
        Ok(Self {
            owner: Arc::new(AuthorityOwner::successor(
                source.clone(),
                retired.into_token(),
            )?),
            source: source.clone(),
        })
    }

    /// Select a verified member of the same physical history after retirement.
    /// This consumes the predecessor's single-use token, not a fresh genesis.
    ///
    /// The actual session-file owner must verify selected-leaf membership.
    ///
    /// # Errors
    /// Refuses changed owner/header/path or raw generation/frontier. Both branch
    /// and canonical revisions must advance by exactly one.
    pub fn successor_after_navigation(
        source: &CanonicalCursor,
        retired: CanonicalRetiredEpoch,
    ) -> Result<Self, CanonicalStoreError> {
        Ok(Self {
            owner: Arc::new(AuthorityOwner::successor_after_navigation(
                source.clone(),
                retired.into_token(),
            )?),
            source: source.clone(),
        })
    }

    /// The epoch's source identity, not a current strict store-CAS cursor.
    #[must_use]
    pub fn cursor(&self) -> &CanonicalCursor {
        &self.source
    }

    /// Admit owner preparation only. No final requested draft can use this scope.
    /// The actual owner must validate the accepted user content before writing.
    ///
    /// # Errors
    /// Refuses closed/poisoned authority or a stage other than prepare preflight.
    pub fn begin_prepare_append(
        &self,
        stage: CanonicalIoStage,
    ) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
        let scope = self.owner.prepare_owner_append()?;
        let (_capture, prepare) = scope.begin_prepare()?;
        let operation = CanonicalOperation::new();
        append_grant(prepare, &self.source, stage, &operation, None)
    }

    /// Admit one exact named lifecycle action, without a serving allocation.
    ///
    /// # Errors
    /// Refuses an action/payload mismatch and closed or poisoned authority.
    pub fn begin_metadata_append(
        &self,
        action: MetadataAction,
        draft: &CanonicalEntryDraft,
        stage: CanonicalIoStage,
    ) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
        let scope = self.owner.owner_metadata(action, draft.clone())?;
        append_scoped(scope, &self.source, stage, draft)
    }

    /// Admit one exact owner context-policy row without a serving capture.
    ///
    /// # Errors
    /// Refuses anything other than a `ContextPolicy` compaction/branch summary,
    /// changed payloads, and closed or poisoned source authority.
    pub fn begin_context_append(
        &self,
        draft: &CanonicalEntryDraft,
        stage: CanonicalIoStage,
    ) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
        let scope = self.owner.owner_context(draft.clone())?;
        append_scoped(scope, &self.source, stage, draft)
    }

    /// Register the actual `AgentStart` run before any serving/provider capture.
    ///
    /// # Errors
    /// Refuses duplicate registration, existing serving work, unsettled owner I/O,
    /// and closed/poisoned authority. The run pins one ordered-boundary anchor.
    pub fn register_run<R: Any + Send + Sync>(
        &self,
        actual_run: Arc<R>,
    ) -> Result<CanonicalOwnerRun, CanonicalStoreError> {
        Ok(CanonicalOwnerRun {
            inner: self.owner.register_run(actual_run)?,
        })
    }

    /// Seal the actual zero-usage failed or aborted assistant before any provider served.
    /// The registered run, not a model capture or metadata action, owns this terminal.
    ///
    /// # Errors
    /// Refuses foreign/retired/replayed runs, any actual serving capture, nonzero
    /// usage, successful outcomes, errors without details, tool calls, and normal admission failures.
    pub fn begin_no_provider_terminal(
        &self,
        run: &CanonicalOwnerRun,
        draft: &CanonicalEntryDraft,
        stage: CanonicalIoStage,
    ) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
        let scope = run
            .inner
            .seal_no_provider_terminal(&self.owner, draft.clone())?;
        append_scoped(scope, &self.source, stage, draft)
    }

    /// Capture the actual serving run/model exactly once in this source epoch.
    /// Call only where inference really starts, not for desired configuration.
    ///
    /// # Errors
    /// Refuses a second serving capture and closed or poisoned authority.
    pub fn capture_serving_run<R: Any + Send + Sync, M: Any + Send + Sync>(
        &self,
        actual_run: Arc<R>,
        actual_model: Arc<M>,
    ) -> Result<CanonicalEpoch, CanonicalStoreError> {
        CanonicalEpoch::capture(self.owner.clone(), &self.source, actual_run, actual_model)
    }

    /// Close using the actual owner's strictly validated current source.
    ///
    /// The owner must first drain pending receipt ACKs and tool outcomes and
    /// verify the exact raw prefix under its existing lease fence. This refresh
    /// occurs only at the ordered close, not from arbitrary ACKs or projections.
    ///
    /// # Errors
    /// Refuses unsettled I/O/transactions, rewound or replaced physical history,
    /// changed source identity/branch, and closed or poisoned authority.
    pub fn request_close_at_current_source(
        &self,
        current: &CanonicalCursor,
    ) -> Result<CanonicalClosing, CanonicalStoreError> {
        Ok(CanonicalClosing {
            ticket: self.owner.request_close_at_current_source(current)?,
        })
    }

    /// Stage a synchronous close. No source-bound scope can admit after it.
    #[must_use]
    pub fn request_close(&self) -> CanonicalClosing {
        CanonicalClosing {
            ticket: self.owner.request_close(CloseReason::Retirement),
        }
    }

    /// Read poison only; this is not readiness or publication authority.
    #[must_use]
    pub fn poisoned(&self) -> bool {
        self.owner.is_poisoned()
    }
}

/// A live canonical epoch for one serving run.
///
/// Created at ACTUAL serving-model capture, never at desired-next selection. The
/// first epoch of a session uses [`Self::genesis`]; every later epoch must present
/// the previous epoch's [`CanonicalRetiredEpoch`] to [`Self::successor`].
pub struct CanonicalEpoch {
    owner: Arc<AuthorityOwner>,
    run: CapturedRun,
    anchor: LateRunAnchor,
}

impl CanonicalEpoch {
    /// The FIRST epoch of a session.
    ///
    /// `run` and `model` are the actual serving allocations, kept strongly pinned
    /// by the capture. A successor epoch must not use this constructor.
    ///
    /// # Errors
    /// Refuses a poisoned or already-captured authority, and a source that does
    /// not match the epoch's own binding.
    pub fn genesis<R: Any + Send + Sync, M: Any + Send + Sync>(
        source: &CanonicalCursor,
        run: Arc<R>,
        model: Arc<M>,
    ) -> Result<Self, CanonicalStoreError> {
        let owner = Arc::new(AuthorityOwner::genesis(source.clone()));
        Self::capture(owner, source, run, model)
    }

    /// A SUCCESSOR epoch, consuming the previous epoch's retired token.
    ///
    /// # Errors
    /// Refuses a token from a different owner/session/header/path/branch identity,
    /// and any capture refusal from the new epoch.
    pub fn successor<R: Any + Send + Sync, M: Any + Send + Sync>(
        source: &CanonicalCursor,
        retired: CanonicalRetiredEpoch,
        run: Arc<R>,
        model: Arc<M>,
    ) -> Result<Self, CanonicalStoreError> {
        let owner = Arc::new(AuthorityOwner::successor(
            source.clone(),
            retired.into_token(),
        )?);
        Self::capture(owner, source, run, model)
    }

    fn capture<R: Any + Send + Sync, M: Any + Send + Sync>(
        owner: Arc<AuthorityOwner>,
        source: &CanonicalCursor,
        run: Arc<R>,
        model: Arc<M>,
    ) -> Result<Self, CanonicalStoreError> {
        let (captured, anchor) = owner.capture_serving_run(source, run, model, Vec::new())?;
        Ok(Self {
            owner,
            run: captured,
            anchor,
        })
    }

    /// The exact source identity this epoch is bound to.
    #[must_use]
    pub fn cursor(&self) -> &CanonicalCursor {
        self.run.source()
    }

    /// Admit one append of `draft` and return its grant.
    ///
    /// # Errors
    /// Refuses a closed/poisoned epoch, a draft whose kind differs from the
    /// transaction purpose, and any admission failure.
    pub fn begin_append(
        &self,
        stage: CanonicalIoStage,
        draft: &CanonicalEntryDraft,
    ) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
        append_scoped(self.run.context_policy(), self.run.source(), stage, draft)
    }

    /// Admit ONE actual provider main step.
    ///
    /// Call this where the provider step actually starts, not when its output is
    /// about to be written: [`Self::begin_terminal_append`] refuses a step that was
    /// never admitted, and the seal is bound to the actual step allocation.
    ///
    /// # Errors
    /// Refuses a closed capture and a doubly admitted step allocation.
    pub fn admit_provider_step<R: Any + Send + Sync>(
        &self,
        actual_step: Arc<R>,
    ) -> Result<CanonicalProviderStep, CanonicalStoreError> {
        Ok(CanonicalProviderStep {
            inner: self.run.admit_provider_step(actual_step)?,
        })
    }

    /// Admit the assistant row of an ADMITTED provider step and return its grant.
    ///
    /// This is the native final-output path. It is not a requested mutation and it
    /// never travels through a coordinator `commit`: the seal is the admission, the
    /// grant carries the recorder, and the store performs the write.
    ///
    /// # Errors
    /// Refuses a draft that is not an actual assistant row, a step already sealed,
    /// a late capture that is no longer eligible, and any admission failure.
    pub fn begin_terminal_append(
        &self,
        step: &CanonicalProviderStep,
        draft: &CanonicalEntryDraft,
        stage: CanonicalIoStage,
    ) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
        if !step.inner.belongs_to(&self.run) {
            return Err(foreign_terminal());
        }
        let sealed = step.inner.seal_assistant(draft.clone())?;
        append_scoped(sealed, self.run.source(), stage, draft)
    }

    /// Admit the exact final result of a tool admitted before execution.
    ///
    /// # Errors
    /// Refuses a foreign epoch, a result with a different call ID/name, a second
    /// result for the same execution, or a closed/poisoned capture.
    pub fn begin_tool_append(
        &self,
        tool: &CanonicalAdmittedTool,
        draft: &CanonicalEntryDraft,
        stage: CanonicalIoStage,
    ) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
        if !tool.inner.belongs_to(&self.run) {
            return Err(foreign_terminal());
        }
        let sealed = tool.inner.seal_final_outcome(draft.clone())?;
        append_scoped(sealed, self.run.source(), stage, draft)
    }

    /// Admit one exact, named owner lifecycle action. No generic row fallback.
    ///
    /// # Errors
    /// Refuses a draft of another action/kind and closed or poisoned authority.
    pub fn begin_metadata_append(
        &self,
        action: MetadataAction,
        draft: &CanonicalEntryDraft,
        stage: CanonicalIoStage,
    ) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
        let scope = self.run.owner_metadata(action, draft.clone())?;
        append_scoped(scope, self.run.source(), stage, draft)
    }

    /// Admit one append that binds no payload: a batch, or a row the owner admitted
    /// at receipt time instead of through a draft.
    ///
    /// This is the ONLY cover for a user row. `CanonicalEntryDraft::new` refuses
    /// `AgentMessage::User(_)` ("user rows require exact owner admission/replay"),
    /// and every FINAL admission requires a draft, so a user row can never take a
    /// final flight. It can take this one: the prepare phase requires `draft: None`
    /// and `canonical.rs:289` grants that "Preparation may itself write rows".
    ///
    /// LIMITATION, stated rather than implied: with no draft there is no payload
    /// binding, so the epoch vouches for the write's COMPLETION (the flight exists,
    /// cannot be drained across, and is classified exactly once) and NOT for its
    /// content. That is acceptable only because the session owner admitted the
    /// content at receipt time, before this grant existed.
    ///
    /// # Errors
    /// Refuses a closed/poisoned epoch and any admission failure.
    pub fn begin_prepare_append(
        &self,
        stage: CanonicalIoStage,
    ) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
        let scope = self.run.context_policy();
        let (_capture, prepare) = scope.begin_prepare()?;
        let operation = CanonicalOperation::new();
        append_grant(prepare, self.run.source(), stage, &operation, None)
    }

    /// Admit the COMMIT of a coordinator-prepared operation.
    ///
    /// The commit path must classify a flight it did not create, so Core issues the
    /// permit for it here and keeps the transaction control that alone can mint the
    /// single-use recorder. A bare [`CanonicalWritePermit`] cannot do this: it holds
    /// only a mutation kind and the admission trait object, while the recorder lives
    /// on the transaction control.
    ///
    /// The operation is the CALLER's prepared operation, never one minted here, so
    /// no second admission exists over the write. The draft is consumed into the
    /// final admission, which is what binds `enter_io(RequestedMutation, ...)` to
    /// exactly this payload.
    ///
    /// # Errors
    /// `OwnerChanged`/`SourceChanged`/`BranchChanged` when `expected` is not this
    /// epoch's source, plus any admission refusal.
    pub fn begin_commit(
        &self,
        operation: &CanonicalOperation,
        expected: &CanonicalCursor,
        draft: CanonicalEntryDraft,
        stage: CanonicalIoStage,
    ) -> Result<CanonicalCommitGrant, CanonicalStoreError> {
        self.owner.validate_source_against(expected)?;
        let scope = self.run.context_policy();
        let (capture, prepare) = scope.begin_prepare()?;
        // Bind the caller's operation through a prepare-phase admission that writes
        // nothing; an unprepared operation is refused by `admit_final`.
        let prepare_ticket = CanonicalWriteTicket::from_admitted(prepare);
        let prepare_flight = prepare_ticket.permit.enter_io(
            CanonicalIoStage::PreparePreflight,
            expected,
            operation,
            None,
        )?;
        prepare_ticket
            .recorder(CanonicalIoStage::PreparePreflight)?
            .record(CanonicalWriteCertainty::Known)?;
        drop(prepare_flight);
        drop(prepare_ticket);
        let admitted = capture.admit_final(operation, draft)?;
        let (permit, control) = CanonicalWriteTicket::from_admitted(admitted).into_commit_parts();
        Ok(CanonicalCommitGrant {
            permit,
            control,
            stage,
        })
    }

    /// Stage the closing transition. Synchronous and nonblocking; nothing waits.
    #[must_use]
    pub fn request_close(&self) -> CanonicalClosing {
        CanonicalClosing {
            ticket: self.owner.request_close(CloseReason::Retirement),
        }
    }

    /// Consume the epoch at its ordered external-idle boundary.
    ///
    /// Dropping an epoch is NOT this, and the difference is deliberate: an
    /// abandoned `LateRunAnchor` poisons the shared state and quarantines
    /// projection, because an epoch that ends without an ordered boundary has a
    /// tail nobody classified. Only this call releases the anchor's count with the
    /// boundary claim set, which is why it CONSUMES the epoch: after it the epoch
    /// admits nothing, and the caller's remaining step is
    /// [`CanonicalClosing::finish`], which can now find `anchors == 0`.
    ///
    /// Infallible by construction: the authority's own boundary call returns no
    /// refusal, so a `Result` here would advertise a variant nothing can produce.
    pub fn finish_ordered_boundary(self) {
        self.anchor.finish_ordered_boundary();
    }

    /// Whether an uncertain write poisoned this epoch.
    ///
    /// True only when a write admitted by this epoch was classified `Uncertain`
    /// (or dropped unclassified). It answers nothing about readiness, drain or
    /// projection: an unpoisoned epoch may still be closing, retired, or blocked
    /// by ordinary work, and a poisoned epoch stays poisoned after retirement.
    #[must_use]
    pub fn poisoned(&self) -> bool {
        self.owner.is_poisoned()
    }
}

/// The staged close of one epoch.
pub struct CanonicalClosing {
    ticket: CloseTicket,
}

impl CanonicalClosing {
    /// The current drain view.
    #[must_use]
    pub fn view(&self) -> CanonicalDrainView {
        self.ticket.status().into()
    }

    /// Wait until the drain is no longer `Pending`.
    ///
    /// Notification-driven, never a poll loop. The caller supplies the deadline:
    /// this waits indefinitely so a policy timeout stays the caller's decision.
    pub async fn wait_drained(&self) -> CanonicalDrainView {
        self.ticket.wait_drained().await.into()
    }

    /// Consume the ordered boundary and mint the successor token.
    ///
    /// # Errors
    /// The typed refusal, never a `Drained` view: `WouldDrainSelf`, `Pending{..}`
    /// or `Blocked`. A refusal is not a drain and must not be treated as success.
    pub fn finish(self) -> Result<CanonicalRetiredEpoch, CanonicalDrainView> {
        self.ticket
            .finish_at_external_idle()
            .map(CanonicalRetiredEpoch)
            .map_err(CanonicalDrainView::from)
    }
}

/// The single-use token a successor epoch must present.
///
/// Not `Clone`: consuming it is what makes "the previous boundary completed"
/// a type property rather than a convention.
pub struct CanonicalRetiredEpoch(crate::session_engine::canonical_authority::RetiredEpoch);

impl CanonicalRetiredEpoch {
    fn into_token(self) -> crate::session_engine::canonical_authority::RetiredEpoch {
        self.0
    }
}

#[cfg(test)]
mod tests {
    //! The epoch handle's production invariants: poison is poison only, and the
    //! grant fixes the enter/record/drop order.
    use super::*;
    use crate::session::canonical::{
        CanonicalFrontier, CanonicalMutationKind, CanonicalOwnerScope,
    };
    use std::path::PathBuf;

    fn cursor() -> CanonicalCursor {
        CanonicalCursor {
            owner: CanonicalOwnerScope::new(),
            session_id: "epoch-session".into(),
            source_path: PathBuf::from("/private/epoch.jsonl"),
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

    fn epoch() -> CanonicalEpoch {
        CanonicalEpoch::genesis(
            &cursor(),
            Arc::new(String::from("actual run")),
            Arc::new(String::from("actual model")),
        )
        .expect("genesis epoch")
    }

    fn compaction() -> CanonicalEntryDraft {
        CanonicalEntryDraft::new(
            CanonicalMutationKind::ContextPolicy,
            serde_json::from_value(serde_json::json!({"type":"compaction",
                "summary":"epoch","firstKeptEntryId":"a","tokensBefore":1,
                "details":{"bytes":"AA=="}}))
            .expect("native entry"),
        )
        .expect("draft")
    }

    fn label_draft(value: &str) -> CanonicalEntryDraft {
        CanonicalEntryDraft::new(
            CanonicalMutationKind::OwnerMetadata,
            serde_json::from_value(serde_json::json!({
                "type":"label", "targetId":"a", "label":value
            }))
            .expect("label entry"),
        )
        .expect("label draft")
    }

    fn assistant_call(id: &str) -> CanonicalEntryDraft {
        let mut value = serde_json::to_value(assistant_draft().entry()).expect("assistant value");
        value["message"]["content"] = serde_json::json!([
            {"type":"toolCall","id":id,"name":"native","arguments":{"x":1}}
        ]);
        value["message"]["stopReason"] = serde_json::json!("toolUse");
        CanonicalEntryDraft::new(
            CanonicalMutationKind::AdmittedTerminal,
            serde_json::from_value(value).expect("assistant entry"),
        )
        .expect("assistant draft")
    }

    fn aborted_draft() -> CanonicalEntryDraft {
        let mut value = serde_json::to_value(assistant_draft().entry()).expect("assistant value");
        value["message"]["usage"] =
            serde_json::to_value(pa_types::ai::Usage::default()).expect("zero usage");
        value["message"]["stopReason"] = serde_json::json!("aborted");
        CanonicalEntryDraft::new(
            CanonicalMutationKind::AdmittedTerminal,
            serde_json::from_value(value).expect("aborted assistant"),
        )
        .expect("aborted draft")
    }

    fn tool_draft(id: &str, name: &str) -> CanonicalEntryDraft {
        CanonicalEntryDraft::new(
            CanonicalMutationKind::AdmittedTerminal,
            serde_json::from_value(serde_json::json!({"type":"message", "message":{
                "role":"toolResult", "toolCallId":id, "toolName":name,
                "content":[{"type":"text","text":"native result"}],
                "details":{"native":[1,null,true]}, "isError":false, "timestamp":2
            }}))
            .expect("tool result entry"),
        )
        .expect("tool draft")
    }

    /// The anti-oracle test: gate state must never leak into `poisoned()`.
    ///
    /// The middle assertion is the one that dies the moment somebody ANDs
    /// `gate == Open` (or any closing/readiness state) into the query.
    #[test]
    fn poisoned_ignores_gate_state() {
        let open_epoch = epoch();
        assert!(
            !open_epoch.poisoned(),
            "an open, unclassified epoch is not poisoned"
        );

        // Closing is a lifecycle transition, not a poison.
        let closing = open_epoch.request_close();
        assert!(
            matches!(
                closing.view(),
                CanonicalDrainView::Pending { .. } | CanonicalDrainView::Blocked
            ),
            "a closing epoch is not drained"
        );
        assert!(
            !open_epoch.poisoned(),
            "request_close() must not make the epoch read as poisoned"
        );
        drop(closing);

        // A real uncertain classification is the ONLY thing that poisons.
        let poisoned_epoch = epoch();
        let grant = poisoned_epoch
            .begin_prepare_append(CanonicalIoStage::PreparePreflight)
            .expect("flight");
        drop(grant);
        assert!(
            poisoned_epoch.poisoned(),
            "an unrecorded flight drop poisons the epoch"
        );
    }

    /// A settled `Known` append leaves the epoch unpoisoned and drainable.
    #[test]
    fn settled_known_append_keeps_the_epoch_clean() {
        let epoch = epoch();
        let grant = epoch
            .begin_append(CanonicalIoStage::RequestedMutation, &compaction())
            .expect("grant");
        grant
            .settle(CanonicalWriteCertainty::Known)
            .expect("classify known");
        assert!(!epoch.poisoned());
        let closing = epoch.request_close();
        assert_eq!(
            closing.view(),
            CanonicalDrainView::Pending {
                io: 0,
                transactions: 0,
                anchors: 1
            }
        );
        assert_eq!(
            closing.finish().err(),
            Some(CanonicalDrainView::Pending {
                io: 0,
                transactions: 0,
                anchors: 1
            }),
            "the epoch still holds its own serving anchor, so nothing drains yet"
        );
    }

    /// The grant's recorder is single use: a second classification is refused.
    #[test]
    fn a_grant_classifies_its_flight_exactly_once() {
        let epoch = epoch();
        let grant = epoch
            .begin_prepare_append(CanonicalIoStage::PreparePreflight)
            .expect("flight");
        assert!(grant.settle(CanonicalWriteCertainty::Known).is_ok());
        assert!(!epoch.poisoned());
    }

    /// A successor needs the retired token, and the token is owner-bound.
    #[test]
    fn successor_requires_the_retired_token() {
        let epoch = epoch();
        let anchor_epoch = epoch;
        let closing = anchor_epoch.request_close();
        // The epoch holds its own anchor, so a clean drain needs the boundary to
        // be finished by its owner. Only the refusal path is asserted here: a
        // refusal mints no token, so no successor can be built from it.
        assert_eq!(
            closing.finish().err(),
            Some(CanonicalDrainView::Pending {
                io: 0,
                transactions: 0,
                anchors: 1
            })
        );
    }

    /// A preflight that never starts records nothing: no `Known`, no quarantine.
    ///
    /// A refused `enter_io` means there was no flight, so there is nothing to
    /// classify and nothing to poison. This asserts the property instead of
    /// assuming it, because a "helpful" classifier here would fabricate a
    /// classification for a stage that never ran.
    #[test]
    fn a_preflight_that_never_starts_records_nothing() {
        let epoch = epoch();
        let closing = epoch.request_close();
        // The gate is Closing, so the prepare-preflight admission is refused.
        let refused = epoch.begin_append(CanonicalIoStage::RequestedMutation, &compaction());
        assert!(
            matches!(
                refused,
                Err(CanonicalStoreError {
                    kind: crate::session::canonical::CanonicalFailureKind::AuthorityClosed,
                    ..
                })
            ),
            "a closed epoch refuses the append before any flight exists"
        );
        assert!(
            !epoch.poisoned(),
            "a refusal that admitted no flight must not quarantine the epoch"
        );
        assert_eq!(
            closing.view(),
            CanonicalDrainView::Pending {
                io: 0,
                transactions: 0,
                anchors: 1
            },
            "no flight was admitted, so no io count exists to classify"
        );
    }

    /// A coordinator-driven commit takes the CALLER's operation and admits no
    /// second one; its permit classifies the flight the store admits.
    #[test]
    fn begin_commit_uses_the_callers_operation_and_classifies_once() {
        let epoch = epoch();
        let operation = CanonicalOperation::new();
        let cursor = epoch.cursor().clone();
        let grant = epoch
            .begin_commit(
                &operation,
                &cursor,
                compaction(),
                CanonicalIoStage::RequestedMutation,
            )
            .expect("commit grant");
        let (permit, recorder) = grant.into_request_parts();
        let flight = permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                &cursor,
                &operation,
                Some(&compaction()),
            )
            .expect("flight");
        recorder
            .claim()
            .expect("recorder")
            .record(CanonicalWriteCertainty::Known)
            .expect("classify");
        drop(flight);
        drop(permit);
        assert!(!epoch.poisoned());
    }

    /// A cursor from a DIFFERENT epoch is refused before any flight exists.
    #[test]
    fn begin_commit_refuses_a_foreign_epoch_cursor() {
        let mine = epoch();
        let theirs = epoch();
        let operation = CanonicalOperation::new();
        let refused = mine.begin_commit(
            &operation,
            theirs.cursor(),
            compaction(),
            CanonicalIoStage::RequestedMutation,
        );
        assert!(
            matches!(
                refused,
                Err(CanonicalStoreError {
                    kind: crate::session::canonical::CanonicalFailureKind::OwnerChanged,
                    ..
                })
            ),
            "a foreign owner allocation must be refused"
        );
        assert!(!mine.poisoned(), "a refusal admits no flight to poison");
    }

    /// A permit from a foreign epoch cannot classify this epoch's scope either:
    /// the refusal comes from that permit's own admission, not from a second check.
    #[test]
    fn a_foreign_epoch_permit_cannot_admit_this_scope() {
        let mine = epoch();
        let theirs = epoch();
        let operation = CanonicalOperation::new();
        let foreign = theirs
            .begin_commit(
                &operation,
                theirs.cursor(),
                compaction(),
                CanonicalIoStage::RequestedMutation,
            )
            .expect("their commit grant");
        let (permit, _recorder) = foreign.into_request_parts();
        let refused = permit.enter_io(
            CanonicalIoStage::RequestedMutation,
            mine.cursor(),
            &operation,
            Some(&compaction()),
        );
        assert!(
            matches!(
                refused,
                Err(CanonicalStoreError {
                    kind: crate::session::canonical::CanonicalFailureKind::OwnerChanged,
                    ..
                })
            ),
            "the foreign permit must refuse our cursor"
        );
    }

    /// Dropping the recorder half without claiming it leaves the flight
    /// unclassified, which poisons the epoch. The split is safe only because
    /// dropping half of it is loud, so this asserts the loudness.
    #[test]
    fn dropping_the_commit_recorder_without_claiming_fails_closed() {
        let epoch = epoch();
        let operation = CanonicalOperation::new();
        let cursor = epoch.cursor().clone();
        let grant = epoch
            .begin_commit(
                &operation,
                &cursor,
                compaction(),
                CanonicalIoStage::RequestedMutation,
            )
            .expect("commit grant");
        let (permit, recorder) = grant.into_request_parts();
        drop(recorder);
        let flight = permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                &cursor,
                &operation,
                Some(&compaction()),
            )
            .expect("flight");
        drop(flight);
        assert!(
            epoch.poisoned(),
            "an unclaimed commit flight must poison the epoch, not pass as Known"
        );
        drop(permit);
    }

    /// The recorder classifies ONLY the stage the grant bound: a flight admitted at
    /// another stage cannot be classified through it, so it fails closed.
    #[test]
    fn the_commit_recorder_refuses_a_stage_it_did_not_bind() {
        let epoch = epoch();
        let operation = CanonicalOperation::new();
        let cursor = epoch.cursor().clone();
        let grant = epoch
            .begin_commit(
                &operation,
                &cursor,
                compaction(),
                CanonicalIoStage::RequestedMutation,
            )
            .expect("commit grant");
        let (permit, recorder) = grant.into_request_parts();
        assert_eq!(recorder.stage(), CanonicalIoStage::RequestedMutation);
        let flight = permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                &cursor,
                &operation,
                Some(&compaction()),
            )
            .expect("flight");
        // The bound stage classifies once; a second claim is refused.
        recorder.claim().expect("first claim");
        assert!(
            recorder.claim().is_err(),
            "one flight admits exactly one recorder"
        );
        drop(flight);
        drop(permit);
    }

    fn assistant_draft() -> CanonicalEntryDraft {
        CanonicalEntryDraft::new(
            CanonicalMutationKind::AdmittedTerminal,
            serde_json::from_value(serde_json::json!({"type":"message","message":{
                "role":"assistant","content":[],"api":"fake","provider":"fake",
                "model":"actual model",
                "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,
                    "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
                "stopReason":"stop","timestamp":1
            }}))
            .expect("native entry"),
        )
        .expect("draft")
    }

    /// The native final-output path: one admitted step, one sealed assistant row,
    /// one classification, and the epoch stays clean.
    #[test]
    fn begin_terminal_append_classifies_the_sealed_assistant_row() {
        let epoch = epoch();
        let step = epoch
            .admit_provider_step(Arc::new(String::from("actual step")))
            .expect("admitted step");
        let grant = epoch
            .begin_terminal_append(
                &step,
                &assistant_draft(),
                CanonicalIoStage::RequestedMutation,
            )
            .expect("terminal grant");
        grant
            .settle(CanonicalWriteCertainty::Known)
            .expect("classified");
        assert!(
            !epoch.poisoned(),
            "a settled terminal row leaves a clean epoch"
        );
    }

    /// The seal is bound to an actual assistant row: a draft of another kind is
    /// refused before any flight exists.
    #[test]
    fn begin_terminal_append_refuses_a_non_assistant_draft() {
        let epoch = epoch();
        let step = epoch
            .admit_provider_step(Arc::new(String::from("actual step")))
            .expect("admitted step");
        // A lawful draft of another kind is the honest refusal case: the seal is
        // bound to an actual assistant row, not merely to a well-formed draft.
        let wrong = compaction();
        assert!(
            epoch
                .begin_terminal_append(&step, &wrong, CanonicalIoStage::RequestedMutation)
                .is_err(),
            "a compaction is not a provider final outcome"
        );
        assert!(!epoch.poisoned(), "a refusal admits no flight to poison");
    }

    /// One step seals exactly one assistant row.
    #[test]
    fn a_provider_step_seals_one_assistant_row() {
        let epoch = epoch();
        let step = epoch
            .admit_provider_step(Arc::new(String::from("actual step")))
            .expect("admitted step");
        epoch
            .begin_terminal_append(
                &step,
                &assistant_draft(),
                CanonicalIoStage::RequestedMutation,
            )
            .expect("first seal")
            .settle(CanonicalWriteCertainty::Known)
            .expect("classified");
        assert!(
            epoch
                .begin_terminal_append(
                    &step,
                    &assistant_draft(),
                    CanonicalIoStage::RequestedMutation
                )
                .is_err(),
            "a second assistant row for one step is refused"
        );
    }

    /// A held terminal grant is visible to a close: the drain reports the open
    /// flight, and only the settle lets it drain. This is B's claim, asserted.
    #[test]
    fn a_terminal_grant_is_not_drained_across() {
        let epoch = epoch();
        let step = epoch
            .admit_provider_step(Arc::new(String::from("actual step")))
            .expect("admitted step");
        let grant = epoch
            .begin_terminal_append(
                &step,
                &assistant_draft(),
                CanonicalIoStage::RequestedMutation,
            )
            .expect("terminal grant");
        let closing = epoch.request_close();
        assert!(
            matches!(closing.view(), CanonicalDrainView::Pending { io: 1, .. }),
            "the open terminal flight must block the drain, got {:?}",
            std::mem::discriminant(&closing.view())
        );
        grant
            .settle(CanonicalWriteCertainty::Known)
            .expect("classified");
        assert_eq!(
            closing.view(),
            CanonicalDrainView::Pending {
                io: 0,
                transactions: 0,
                anchors: 1
            },
            "the settle releases the flight; only the epoch's own serving anchor remains"
        );
    }

    #[test]
    fn owner_source_prepares_and_writes_exact_metadata_before_actual_capture() {
        let source = cursor();
        let owner = CanonicalOwnerEpoch::genesis(&source);
        owner
            .begin_prepare_append(CanonicalIoStage::PreparePreflight)
            .expect("owner preparation without serving model")
            .settle(CanonicalWriteCertainty::Known)
            .expect("actual owner classified preparation");
        assert!(owner
            .begin_prepare_append(CanonicalIoStage::RequestedMutation)
            .is_err());
        assert!(owner
            .begin_metadata_append(
                MetadataAction::SessionInfo,
                &label_draft("exact"),
                CanonicalIoStage::RequestedMutation,
            )
            .is_err());
        owner
            .begin_metadata_append(
                MetadataAction::Label,
                &label_draft("exact"),
                CanonicalIoStage::RequestedMutation,
            )
            .expect("exact owner lifecycle row")
            .settle(CanonicalWriteCertainty::Known)
            .expect("classified lifecycle write");
        let serving = owner
            .capture_serving_run(Arc::new(String::from("actual run")), Arc::new(7_u64))
            .expect("first actual serving capture");
        assert!(owner
            .capture_serving_run(Arc::new(()), Arc::new(()))
            .is_err());
        let closing = owner.request_close();
        assert_eq!(
            closing.view(),
            CanonicalDrainView::Pending {
                io: 0,
                transactions: 0,
                anchors: 1,
            }
        );
        assert!(owner
            .begin_prepare_append(CanonicalIoStage::PreparePreflight)
            .is_err());
        assert!(serving
            .admit_provider_step(Arc::new("late invocation"))
            .is_err());
        serving.finish_ordered_boundary();
        assert_eq!(closing.view(), CanonicalDrainView::Drained);
        closing.finish().expect("ordered retired source");
        assert!(!owner.poisoned());
    }

    #[test]
    fn owner_and_serving_share_uncertain_write_quarantine() {
        let owner = CanonicalOwnerEpoch::genesis(&cursor());
        let serving = owner
            .capture_serving_run(Arc::new(()), Arc::new(()))
            .expect("serving capture");
        let pending = owner
            .begin_prepare_append(CanonicalIoStage::PreparePreflight)
            .expect("owner flight");
        assert!(!serving.poisoned());
        drop(pending);
        assert!(owner.poisoned());
        assert!(serving.poisoned());
        assert_eq!(owner.request_close().view(), CanonicalDrainView::Blocked);
    }

    #[test]
    fn source_successor_needs_boundary_but_allows_own_frontier_advance() {
        let mut source = cursor();
        let owner = CanonicalOwnerEpoch::genesis(&source);
        let serving = owner
            .capture_serving_run(Arc::new(()), Arc::new(()))
            .expect("serving capture");
        let closing = owner.request_close();
        serving.finish_ordered_boundary();
        let retired = closing.finish().expect("retired token");
        source.canonical_revision += 1;
        source.raw_source_generation = [9; 32];
        source.selected_leaf = Some("new".to_owned());
        source.frontier.bytes += 70;
        source.frontier.rows += 1;
        source.frontier.prefix_certificate = [8; 32];
        let successor = CanonicalOwnerEpoch::successor(&source, retired).expect("own advance");
        assert_eq!(successor.cursor(), &source);
        assert!(owner
            .begin_metadata_append(
                MetadataAction::Label,
                &label_draft("retired"),
                CanonicalIoStage::RequestedMutation,
            )
            .is_err());
        successor
            .begin_metadata_append(
                MetadataAction::Label,
                &label_draft("successor"),
                CanonicalIoStage::RequestedMutation,
            )
            .expect("new source authority")
            .settle(CanonicalWriteCertainty::Known)
            .expect("classified");
    }

    #[test]
    fn foreign_provider_refusal_does_not_consume_the_actual_assistant_seal() {
        let mine = epoch();
        let theirs = epoch();
        let step = mine.admit_provider_step(Arc::new(())).expect("actual step");
        let draft = assistant_draft();
        assert!(theirs
            .begin_terminal_append(&step, &draft, CanonicalIoStage::RequestedMutation)
            .is_err());
        mine.begin_terminal_append(&step, &draft, CanonicalIoStage::RequestedMutation)
            .expect("foreign refusal must not seal actual output")
            .settle(CanonicalWriteCertainty::Known)
            .expect("classified");
        assert!(!mine.poisoned());
        assert!(!theirs.poisoned());
    }

    #[test]
    fn tool_grant_requires_the_actual_call_and_exact_result_and_is_single_use() {
        let mine = epoch();
        let theirs = epoch();
        let step = mine.admit_provider_step(Arc::new(())).expect("actual step");
        assert!(step.admit_tool("real", Arc::new(())).is_err());
        mine.begin_terminal_append(
            &step,
            &assistant_call("real"),
            CanonicalIoStage::RequestedMutation,
        )
        .expect("sealed canonical tool call")
        .settle(CanonicalWriteCertainty::Known)
        .expect("assistant classification");
        assert!(step.admit_tool("absent", Arc::new(())).is_err());
        let tool = step.admit_tool("real", Arc::new(())).expect("actual tool");
        assert!(step.admit_tool("real", Arc::new(())).is_err());
        for invalid in [tool_draft("foreign", "native"), tool_draft("real", "wrong")] {
            assert!(mine
                .begin_tool_append(&tool, &invalid, CanonicalIoStage::RequestedMutation)
                .is_err());
        }
        let result = tool_draft("real", "native");
        assert!(theirs
            .begin_tool_append(&tool, &result, CanonicalIoStage::RequestedMutation)
            .is_err());
        let grant = mine
            .begin_tool_append(&tool, &result, CanonicalIoStage::RequestedMutation)
            .expect("exact result remains sealable after foreign/malformed refusals");
        assert!(mine
            .begin_tool_append(&tool, &result, CanonicalIoStage::RequestedMutation)
            .is_err());
        let closing = mine.request_close();
        assert_eq!(
            closing.view(),
            CanonicalDrainView::Pending {
                io: 1,
                transactions: 1,
                anchors: 1,
            }
        );
        grant
            .settle(CanonicalWriteCertainty::Known)
            .expect("tool classification");
        assert_eq!(
            closing.view(),
            CanonicalDrainView::Pending {
                io: 0,
                transactions: 0,
                anchors: 1,
            }
        );
        assert!(mine
            .begin_tool_append(&tool, &result, CanonicalIoStage::RequestedMutation)
            .is_err());
        assert!(!mine.poisoned());
        assert!(!theirs.poisoned());
    }

    #[test]
    fn serving_metadata_is_action_specific_and_cannot_write_native_terminals() {
        let epoch = epoch();
        assert!(epoch
            .begin_metadata_append(
                MetadataAction::SessionInfo,
                &label_draft("label"),
                CanonicalIoStage::RequestedMutation,
            )
            .is_err());
        assert!(epoch
            .begin_metadata_append(
                MetadataAction::Label,
                &assistant_draft(),
                CanonicalIoStage::RequestedMutation,
            )
            .is_err());
        epoch
            .begin_metadata_append(
                MetadataAction::Label,
                &label_draft("label"),
                CanonicalIoStage::RequestedMutation,
            )
            .expect("exact named lifecycle action")
            .settle(CanonicalWriteCertainty::Known)
            .expect("classified");
        assert!(!epoch.poisoned());
    }

    #[test]
    fn registered_source_run_seals_one_exact_no_provider_terminal_and_drains_in_order() {
        for stop_reason in ["aborted", "error"] {
            let owner = CanonicalOwnerEpoch::genesis(&cursor());
            let actual = Arc::new(String::from("actual `AgentStart` allocation"));
            let run = owner
                .register_run(actual.clone())
                .expect("actual run registration");
            assert!(owner.register_run(actual).is_err());
            let foreign = CanonicalOwnerEpoch::genesis(&cursor());
            let mut value = serde_json::to_value(aborted_draft().entry()).unwrap();
            value["message"]["stopReason"] = serde_json::json!(stop_reason);
            value["message"]["errorMessage"] =
                serde_json::json!("failed before provider admission");
            let draft = CanonicalEntryDraft::new(
                CanonicalMutationKind::AdmittedTerminal,
                serde_json::from_value(value).unwrap(),
            )
            .unwrap();
            assert!(foreign
                .begin_no_provider_terminal(&run, &draft, CanonicalIoStage::RequestedMutation)
                .is_err());
            let grant = owner
                .begin_no_provider_terminal(&run, &draft, CanonicalIoStage::RequestedMutation)
                .expect("actual zero-usage aborted outcome");
            assert!(owner
                .begin_no_provider_terminal(&run, &draft, CanonicalIoStage::RequestedMutation)
                .is_err());
            let closing = owner.request_close();
            assert_eq!(
                closing.view(),
                CanonicalDrainView::Pending {
                    io: 1,
                    transactions: 1,
                    anchors: 1
                }
            );
            grant
                .settle(CanonicalWriteCertainty::Known)
                .expect("writer classified terminal");
            assert_eq!(
                closing.view(),
                CanonicalDrainView::Pending {
                    io: 0,
                    transactions: 0,
                    anchors: 1
                }
            );
            run.finish_ordered_boundary();
            assert_eq!(closing.view(), CanonicalDrainView::Drained);
            closing.finish().expect("one source successor token");
            assert!(!owner.poisoned());
            assert!(!foreign.poisoned());
        }
    }

    #[test]
    fn no_provider_abort_refuses_ordinary_errors_nonzero_usage_and_tool_calls() {
        let owner = CanonicalOwnerEpoch::genesis(&cursor());
        let run = owner.register_run(Arc::new(())).expect("actual run");
        for invalid in [
            assistant_draft(),
            compaction(),
            label_draft("metadata"),
            tool_draft("id", "native"),
        ] {
            assert!(owner
                .begin_no_provider_terminal(&run, &invalid, CanonicalIoStage::RequestedMutation)
                .is_err());
        }
        for change in 0..3 {
            let mut value =
                serde_json::to_value(aborted_draft().entry()).expect("actual abort value");
            match change {
                0 => value["message"]["stopReason"] = serde_json::json!("error"),
                1 => value["message"]["usage"]["input"] = serde_json::json!(1),
                2 => {
                    value["message"]["content"] = serde_json::json!([
                        {"type":"toolCall","id":"forged","name":"native","arguments":{}}
                    ]);
                }
                _ => unreachable!(),
            }
            let invalid = CanonicalEntryDraft::new(
                CanonicalMutationKind::AdmittedTerminal,
                serde_json::from_value(value).expect("native invalid abort"),
            )
            .expect("native draft");
            assert!(owner
                .begin_no_provider_terminal(&run, &invalid, CanonicalIoStage::RequestedMutation)
                .is_err());
        }
        owner
            .begin_no_provider_terminal(&run, &aborted_draft(), CanonicalIoStage::RequestedMutation)
            .expect("refusals did not consume the actual abort slot")
            .settle(CanonicalWriteCertainty::Known)
            .expect("known actual write");
        run.finish_ordered_boundary();
        assert!(!owner.poisoned());
    }

    #[test]
    fn real_serving_capture_permanently_disables_registered_no_provider_abort() {
        let owner = CanonicalOwnerEpoch::genesis(&cursor());
        let actual = Arc::new(String::from("actual outer run"));
        let run = owner
            .register_run(actual.clone())
            .expect("actual source run");
        assert!(owner
            .capture_serving_run(
                Arc::new(String::from("same text foreign run")),
                Arc::new(())
            )
            .is_err());
        let serving = owner
            .capture_serving_run(actual, Arc::new(String::from("actual model")))
            .expect("matched actual serving capture");
        assert!(owner
            .begin_no_provider_terminal(&run, &aborted_draft(), CanonicalIoStage::RequestedMutation)
            .is_err());
        run.finish_ordered_boundary();
        assert!(owner.register_run(Arc::new(())).is_err());
        let closing = owner.request_close();
        serving.finish_ordered_boundary();
        assert_eq!(closing.view(), CanonicalDrainView::Drained);
        closing.finish().expect("real serving ordered retirement");
        assert!(!owner.poisoned());
    }

    #[test]
    fn abandoned_registered_run_quarantines_the_source_epoch() {
        let owner = CanonicalOwnerEpoch::genesis(&cursor());
        let run = owner.register_run(Arc::new(())).expect("actual run");
        drop(run);
        assert!(owner.poisoned());
        assert_eq!(owner.request_close().view(), CanonicalDrainView::Blocked);
    }
}
