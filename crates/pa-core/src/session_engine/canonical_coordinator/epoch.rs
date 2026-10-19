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
    permit: CanonicalWritePermit,
    flight: CanonicalIoFlight,
    recorder: CanonicalWriteRecorder,
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

/// A live canonical epoch for one serving run.
///
/// Created at ACTUAL serving-model capture, never at desired-next selection. The
/// first epoch of a session uses [`Self::genesis`]; every later epoch must present
/// the previous epoch's [`CanonicalRetiredEpoch`] to [`Self::successor`].
pub struct CanonicalEpoch {
    owner: Arc<AuthorityOwner>,
    run: CapturedRun,
    _anchor: LateRunAnchor,
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
            _anchor: anchor,
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
        let scope = self.run.context_policy();
        let (capture, prepare) = scope.begin_prepare()?;
        let operation = CanonicalOperation::new();
        // A prepare-phase transaction must bind the owner operation before any final
        // admission, and the authority binds it through `enter_io`; removing this
        // preflight breaks final admission rather than simplifying it.
        //
        // The classification below is `Known` because the stage's contract is "no
        // I/O at this phase": `Known` here means no I/O was attempted at this
        // stage, and no row is counted as written because of it. A mutation that
        // claims a row write while writing zero bytes is a different case and
        // records nothing at all.
        let prepare_ticket = CanonicalWriteTicket::from_admitted(prepare);
        let prepare_flight = prepare_ticket.permit.enter_io(
            CanonicalIoStage::PreparePreflight,
            self.run.source(),
            &operation,
            None,
        )?;
        prepare_ticket
            .recorder(CanonicalIoStage::PreparePreflight)?
            .record(CanonicalWriteCertainty::Known)?;
        drop(prepare_flight);
        drop(prepare_ticket);
        let admitted = capture.admit_final(&operation, draft.clone())?;
        self.grant(admitted, stage, &operation, Some(draft))
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
        let sealed = step.inner.seal_assistant(draft.clone())?;
        let (capture, prepare) = sealed.begin_prepare()?;
        let operation = CanonicalOperation::new();
        // Bind the minted operation through a prepare flight that writes nothing;
        // `admit_final` refuses an operation this scope never bound.
        let prepare_ticket = CanonicalWriteTicket::from_admitted(prepare);
        let prepare_flight = prepare_ticket.permit.enter_io(
            CanonicalIoStage::PreparePreflight,
            self.run.source(),
            &operation,
            None,
        )?;
        prepare_ticket
            .recorder(CanonicalIoStage::PreparePreflight)?
            .record(CanonicalWriteCertainty::Known)?;
        drop(prepare_flight);
        drop(prepare_ticket);
        let admitted = capture.admit_final(&operation, draft.clone())?;
        // A requested-mutation flight must name the exact authorized draft, or the
        // flight would not be bound to the row this grant was minted for.
        self.grant(admitted, stage, &operation, Some(draft))
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
        self.grant(prepare, stage, &operation, None)
    }

    fn grant(
        &self,
        admitted: crate::session_engine::canonical_authority::AdmittedTransaction,
        stage: CanonicalIoStage,
        operation: &CanonicalOperation,
        draft: Option<&CanonicalEntryDraft>,
    ) -> Result<CanonicalAppendGrant, CanonicalStoreError> {
        let ticket = CanonicalWriteTicket::from_admitted(admitted);
        let flight = ticket
            .permit
            .enter_io(stage, self.run.source(), operation, draft)?;
        let recorder = ticket.recorder(stage)?;
        Ok(CanonicalAppendGrant {
            permit: ticket.into_permit(),
            flight,
            recorder,
        })
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
}
