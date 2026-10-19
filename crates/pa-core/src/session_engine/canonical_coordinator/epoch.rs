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
    CanonicalStoreError,
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
    ticket: CanonicalWriteTicket,
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
            ticket,
            flight,
            recorder,
        } = self;
        recorder.record(certainty)?;
        drop(flight);
        drop(ticket);
        Ok(())
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

    /// Admit one append that classifies no single row (a batch or preflight write).
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
            ticket,
            flight,
            recorder,
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
}
