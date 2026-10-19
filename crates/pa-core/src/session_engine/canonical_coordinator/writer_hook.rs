//! Internal integration seam: the writer's result channel for the canonical authority.
//!
//! This is NOT stable public API. The frozen v3 interface hands an opaque
//! `CanonicalIoFlight` to the holder of a `CanonicalWritePermit`, so a writer that
//! holds only those two values has no path to record `Known` or `Uncertain`; the
//! authority guard then poisons and quarantines every real write that is not
//! recorded. A [`CanonicalWriteTicket`] is the one supported way to obtain that
//! recording handle.
//!
//! Division of ownership:
//!
//! * Core owns the served epoch: admission, counters, close/gate linearization,
//!   quarantine, terminal slots and serving identity.
//! * The actual byte writer owns the bytes and the classification. It receives a
//!   ticket from Core, admits its flight with `permit.enter_io`, obtains the
//!   recorder with [`CanonicalWriteTicket::recorder`], and calls
//!   [`CanonicalWriteRecorder::record`] exactly once before the flight drops.
//!
//! Rules:
//!
//! * the recorder exists only for a stage this authority actually admitted, and
//!   only once per admitted flight; both refusals are typed errors;
//! * recording is writer-origin: [`CanonicalWriteRecorder::record`] consumes the
//!   recorder, so an awaiter that never held it cannot record;
//! * no registry and no statics: the ticket owns its weak transaction control, and
//!   nothing here calls back into any other subsystem;
//! * no lock is held across I/O and nothing awaits;
//! * an unrecorded flight is classified `Uncertain` before the authority guard
//!   observes the drop, so a cancelled or panicking writer can never look
//!   `Known`.
//!
//! This seam is not wired to a runtime writer yet. Remove this allowance at
//! integration.
#![allow(dead_code)]

use crate::session::canonical::{
    CanonicalFailureKind, CanonicalIoStage, CanonicalRequestedWrite, CanonicalSourceDisposition,
    CanonicalStoreError, CanonicalWritePermit,
};
use crate::session_engine::canonical_authority::{
    AdmittedTransaction, IoResultRecorder, RecordedIo, TransactionControl,
};

fn refused(detail: &str) -> CanonicalStoreError {
    CanonicalStoreError {
        kind: CanonicalFailureKind::InvalidRequest,
        requested: CanonicalRequestedWrite::NotWritten,
        source: CanonicalSourceDisposition::Unchanged,
        detail: detail.to_owned(),
    }
}

/// Classification the ACTUAL writer observed for one admitted I/O flight.
///
/// `Known` means the outcome is certain: either the write reached its exact
/// intended durable state, or nothing was written at all (a refusal before the
/// first byte). `Uncertain` means the row may be partially present, so the owner
/// needs a new epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalWriteCertainty {
    /// Certain outcome: durable success, or a refusal that wrote no byte.
    Known,
    /// Any doubt, including cancellation mid-write and a failed durability proof.
    Uncertain,
}

/// Single-use recording handle for one admitted flight.
///
/// It is neither `Clone` nor `Copy`, and [`Self::record`] consumes it, so exactly
/// one classification can ever be recorded for the flight it belongs to.
pub struct CanonicalWriteRecorder {
    inner: Option<IoResultRecorder>,
}

impl CanonicalWriteRecorder {
    /// Record the writer's own observation.
    ///
    /// Synchronous: no I/O, no await, and no foreign lock is taken here. `Known`
    /// is a single atomic compare-exchange; `Uncertain` additionally quarantines
    /// the owner epoch under its short entry mutex.
    ///
    /// # Errors
    /// Refuses when the classification was already recorded or the flight already
    /// closed. A refusal leaves the flight unclassified, which fails closed.
    pub fn record(mut self, certainty: CanonicalWriteCertainty) -> Result<(), CanonicalStoreError> {
        let inner = self
            .inner
            .take()
            .ok_or_else(|| refused("I/O result already classified"))?;
        inner.record(match certainty {
            CanonicalWriteCertainty::Known => RecordedIo::Known,
            CanonicalWriteCertainty::Uncertain => RecordedIo::Uncertain,
        })
    }
}

impl Drop for CanonicalWriteRecorder {
    fn drop(&mut self) {
        // A writer that drops an unconsumed recorder never classified its flight.
        // That is `Uncertain` by definition, and it must be stated explicitly: the
        // guard only poisons in memory, it records no classification.
        if let Some(inner) = self.inner.take() {
            let _ = inner.record(RecordedIo::Uncertain);
        }
    }
}

/// Core-issued ticket for one admitted transaction lease.
///
/// Core builds it where the final admitted transaction is produced, BEFORE the
/// permit leaves Core, and hands it to the actual writer. The writer admits its
/// flight with `ticket.permit.enter_io(...)`, then asks
/// [`Self::recorder`] for the single-use recorder of that exact stage.
///
/// It is an owned value, never a registry entry, so no session, operation, or
/// global map can reach another session's flights.
#[must_use]
pub struct CanonicalWriteTicket {
    /// The frozen v3 permit. Admit the flight through it, in this order, first.
    pub permit: CanonicalWritePermit,
    control: TransactionControl,
}

impl CanonicalWriteTicket {
    /// Core-side construction from a freshly admitted final transaction.
    ///
    /// The daemon never constructs authority; it only receives this value.
    pub(in crate::session_engine) fn from_admitted(admitted: AdmittedTransaction) -> Self {
        Self {
            permit: admitted.permit,
            control: admitted.control,
        }
    }

    /// The single-use recorder for a stage this authority admitted.
    ///
    /// Call it only after `permit.enter_io(stage, ...)` returned `Ok`. It is
    /// single use per admitted flight and refuses a stage with no owned flight.
    ///
    /// # Errors
    /// Typed refusal when this authority has no outstanding flight for the stage,
    /// or when the recorder was already claimed.
    pub fn recorder(
        &self,
        stage: CanonicalIoStage,
    ) -> Result<CanonicalWriteRecorder, CanonicalStoreError> {
        let inner = self.control.io_recorder(stage)?;
        Ok(CanonicalWriteRecorder { inner: Some(inner) })
    }
}

#[cfg(test)]
mod tests {
    //! Real private authority primitives plus the real durable append primitive
    //! (`append_cached`: open, torn-tail probe, `write_all`, `flush`, `fsync`).
    //! Not claimed: a real runtime session lease, the daemon's
    //! `write.rs`/`lease.rs` wrapper, an installed backend journal, or any wiring.
    use super::*;
    use crate::session::canonical::{
        CanonicalCursor, CanonicalEntryDraft, CanonicalFrontier, CanonicalMutationKind,
        CanonicalOperation, CanonicalOwnerScope,
    };
    use crate::session::window::{append_cached, AppendOwnership};
    use crate::session_engine::canonical_authority::{
        AuthorityOwner, CapturedRun, CloseReason, DrainStatus, LateRunAnchor,
    };
    use std::path::PathBuf;
    use std::sync::{mpsc, Arc};
    use std::thread;

    struct Fixture {
        owner: AuthorityOwner,
        run: CapturedRun,
        anchor: Option<LateRunAnchor>,
    }

    fn cursor() -> CanonicalCursor {
        CanonicalCursor {
            owner: CanonicalOwnerScope::new(),
            session_id: "s".into(),
            source_path: PathBuf::from("/private/s.jsonl"),
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

    fn fixture() -> Fixture {
        let source = cursor();
        let owner = AuthorityOwner::genesis(source.clone());
        let (run, anchor) = owner
            .capture_serving_run(
                &source,
                Arc::new(String::from("same run")),
                Arc::new(String::from("same model")),
                Vec::new(),
            )
            .expect("actual capture");
        Fixture {
            owner,
            run,
            anchor: Some(anchor),
        }
    }

    fn compaction() -> CanonicalEntryDraft {
        CanonicalEntryDraft::new(
            CanonicalMutationKind::ContextPolicy,
            serde_json::from_value(serde_json::json!({"type":"compaction",
                "summary":"exact","firstKeptEntryId":"a","tokensBefore":1,
                "details":{"bytes":"AA=="}}))
            .expect("native entry"),
        )
        .expect("draft")
    }

    /// Prepare preflight through a public ticket, then a fresh final transaction.
    fn final_ticket(fixture: &mut Fixture) -> (CanonicalOperation, CanonicalWriteTicket) {
        let scope = fixture.run.context_policy();
        let (capture, prepare) = scope.begin_prepare().expect("prepare lease");
        let operation = CanonicalOperation::new();
        let prepare_ticket = CanonicalWriteTicket::from_admitted(prepare);
        let flight = prepare_ticket
            .permit
            .enter_io(
                CanonicalIoStage::PreparePreflight,
                fixture.run.source(),
                &operation,
                None,
            )
            .expect("prepare flight");
        prepare_ticket
            .recorder(CanonicalIoStage::PreparePreflight)
            .expect("prepare recorder")
            .record(CanonicalWriteCertainty::Known)
            .expect("classify prepare");
        drop(flight);
        drop(prepare_ticket);
        let transaction = capture
            .admit_final(&operation, compaction())
            .expect("fresh final transaction");
        (operation, CanonicalWriteTicket::from_admitted(transaction))
    }

    fn drain(fixture: &mut Fixture) -> DrainStatus {
        fixture
            .anchor
            .take()
            .expect("registered anchor")
            .finish_ordered_boundary();
        fixture
            .owner
            .request_close(CloseReason::Retirement)
            .status()
    }

    #[test]
    fn classified_known_flight_lets_the_owner_drain() {
        let mut fixture = fixture();
        let (operation, ticket) = final_ticket(&mut fixture);
        let draft = compaction();
        let flight = ticket
            .permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                fixture.run.source(),
                &operation,
                Some(&draft),
            )
            .expect("actual flight");
        ticket
            .recorder(CanonicalIoStage::RequestedMutation)
            .expect("unique recorder")
            .record(CanonicalWriteCertainty::Known)
            .expect("classify known");
        drop(flight);
        drop(ticket);
        assert_eq!(drain(&mut fixture), DrainStatus::Drained);
    }

    #[test]
    fn unrecorded_flight_drop_is_uncertain_and_blocks_drain() {
        let mut fixture = fixture();
        let (operation, ticket) = final_ticket(&mut fixture);
        let draft = compaction();
        let flight = ticket
            .permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                fixture.run.source(),
                &operation,
                Some(&draft),
            )
            .expect("actual flight");
        drop(flight);
        drop(ticket);
        assert_eq!(drain(&mut fixture), DrainStatus::Blocked);
    }

    #[test]
    fn uncertain_classification_quarantines_projection() {
        let mut fixture = fixture();
        let (operation, ticket) = final_ticket(&mut fixture);
        let draft = compaction();
        let flight = ticket
            .permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                fixture.run.source(),
                &operation,
                Some(&draft),
            )
            .expect("actual flight");
        ticket
            .recorder(CanonicalIoStage::RequestedMutation)
            .expect("unique recorder")
            .record(CanonicalWriteCertainty::Uncertain)
            .expect("classify uncertain");
        drop(flight);
        drop(ticket);
        assert_eq!(drain(&mut fixture), DrainStatus::Blocked);
    }

    #[test]
    fn a_second_recorder_for_the_same_stage_is_refused() {
        let mut fixture = fixture();
        let (operation, ticket) = final_ticket(&mut fixture);
        let draft = compaction();
        let flight = ticket
            .permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                fixture.run.source(),
                &operation,
                Some(&draft),
            )
            .expect("actual flight");
        let recorder = ticket
            .recorder(CanonicalIoStage::RequestedMutation)
            .expect("unique recorder");
        assert!(
            ticket
                .recorder(CanonicalIoStage::RequestedMutation)
                .is_err(),
            "a second recorder must not be minted"
        );
        assert!(
            ticket.recorder(CanonicalIoStage::CommitPreflight).is_err(),
            "a stage with no owned flight must refuse"
        );
        recorder
            .record(CanonicalWriteCertainty::Known)
            .expect("classify known");
        drop(flight);
        drop(ticket);
        assert_eq!(drain(&mut fixture), DrainStatus::Drained);
    }

    #[test]
    fn refused_admission_leaves_no_recorder() {
        let mut fixture = fixture();
        let (operation, ticket) = final_ticket(&mut fixture);
        let draft = compaction();
        let refused_flight = ticket.permit.enter_io(
            CanonicalIoStage::PreparePreflight,
            fixture.run.source(),
            &operation,
            None,
        );
        assert!(refused_flight.is_err(), "phase mismatch must refuse");
        let flight = ticket
            .permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                fixture.run.source(),
                &operation,
                Some(&draft),
            )
            .expect("actual flight after refusal");
        ticket
            .recorder(CanonicalIoStage::RequestedMutation)
            .expect("recorder after refusal")
            .record(CanonicalWriteCertainty::Known)
            .expect("classify known");
        drop(flight);
        drop(ticket);
        assert_eq!(drain(&mut fixture), DrainStatus::Drained);
    }

    #[test]
    fn each_stage_gets_exactly_one_recorder() {
        let mut fixture = fixture();
        let (operation, ticket) = final_ticket(&mut fixture);
        let draft = compaction();
        for stage in [
            CanonicalIoStage::CommitPreflight,
            CanonicalIoStage::RequestedMutation,
        ] {
            let flight = ticket
                .permit
                .enter_io(stage, fixture.run.source(), &operation, Some(&draft))
                .expect("stage flight");
            ticket
                .recorder(stage)
                .expect("stage recorder")
                .record(CanonicalWriteCertainty::Known)
                .expect("classify stage");
            drop(flight);
        }
        drop(ticket);
        assert_eq!(drain(&mut fixture), DrainStatus::Drained);
    }

    #[test]
    fn a_second_writer_sharing_the_ticket_cannot_take_the_same_stage() {
        let mut fixture = fixture();
        let (operation, ticket) = final_ticket(&mut fixture);
        let draft = compaction();
        let shared = Arc::new(ticket);
        let flight = shared
            .permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                fixture.run.source(),
                &operation,
                Some(&draft),
            )
            .expect("actual flight");
        shared
            .recorder(CanonicalIoStage::RequestedMutation)
            .expect("recorder")
            .record(CanonicalWriteCertainty::Known)
            .expect("classify known");
        let other = Arc::clone(&shared);
        let joined =
            thread::spawn(move || other.recorder(CanonicalIoStage::RequestedMutation).is_err())
                .join()
                .expect("joined");
        assert!(joined, "the recorder is unique across threads");
        drop(flight);
        drop(shared);
        assert_eq!(drain(&mut fixture), DrainStatus::Drained);
    }

    /// The recorder's own Drop must classify `Uncertain`, never `Known`: a writer
    /// that panics or is cancelled while holding an unminted... unminted is not the
    /// case here - it holds a recorder and never classifies it.
    #[test]
    fn recorder_dropped_without_classification_blocks_the_epoch() {
        let mut fixture = fixture();
        let (operation, ticket) = final_ticket(&mut fixture);
        let draft = compaction();
        let flight = ticket
            .permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                fixture.run.source(),
                &operation,
                Some(&draft),
            )
            .expect("actual flight");
        let recorder = ticket
            .recorder(CanonicalIoStage::RequestedMutation)
            .expect("unique recorder");
        drop(recorder);
        drop(flight);
        drop(ticket);
        assert_eq!(drain(&mut fixture), DrainStatus::Blocked);
    }

    /// Real durable append through `append_cached` (open, `write_all`, `flush`,
    /// `fsync`), and the close ticket must show Pending until the flight drops.
    #[test]
    fn real_durable_append_classifies_known_and_drains_only_after_the_drop() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, b"{\"type\":\"session\",\"version\":3}\n").expect("seed row");
        let mut fixture = fixture();
        let (operation, ticket) = final_ticket(&mut fixture);
        let draft = compaction();
        let flight = ticket
            .permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                fixture.run.source(),
                &operation,
                Some(&draft),
            )
            .expect("actual flight");
        let recorder = ticket
            .recorder(CanonicalIoStage::RequestedMutation)
            .expect("unique recorder");
        let close = fixture.owner.request_close(CloseReason::Retirement);
        match close.status() {
            DrainStatus::Pending {
                io, transactions, ..
            } => {
                assert_eq!(io, 1, "one actual flight is outstanding");
                assert_eq!(transactions, 1, "the transaction lease is outstanding");
            }
            other => panic!("expected Pending before the drop, got {other:?}"),
        }
        append_cached(
            &path,
            b"{\"type\":\"compaction\",\"summary\":\"exact\"}\n",
            AppendOwnership::Unleased,
        )
        .expect("real durable append");
        recorder
            .record(CanonicalWriteCertainty::Known)
            .expect("classify known");
        assert!(
            matches!(close.status(), DrainStatus::Pending { .. }),
            "still Pending while the flight is alive"
        );
        drop(flight);
        drop(ticket);
        fixture
            .anchor
            .take()
            .expect("registered anchor")
            .finish_ordered_boundary();
        assert_eq!(close.status(), DrainStatus::Drained);
        close.finish_at_external_idle().expect("retired boundary");
        let bytes = std::fs::read(&path).expect("read back");
        assert!(bytes.ends_with(b"{\"type\":\"compaction\",\"summary\":\"exact\"}\n"));
    }

    /// A real writer that hangs after its durable append: the scope stays Pending,
    /// and once that flight drops unrecorded (panic/cancel path) the scope is
    /// Blocked, never Drained and never projecting.
    #[test]
    fn hung_real_writer_keeps_the_scope_pending_and_never_drains() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, b"{\"type\":\"session\",\"version\":3}\n").expect("seed row");
        let mut fixture = fixture();
        let (operation, ticket) = final_ticket(&mut fixture);
        let draft = compaction();
        let flight = ticket
            .permit
            .enter_io(
                CanonicalIoStage::RequestedMutation,
                fixture.run.source(),
                &operation,
                Some(&draft),
            )
            .expect("actual flight");
        let close = fixture.owner.request_close(CloseReason::Retirement);
        let (written, wrote) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let writer = thread::spawn(move || {
            append_cached(
                &path,
                b"{\"type\":\"compaction\",\"summary\":\"hung\"}\n",
                AppendOwnership::Unleased,
            )
            .expect("real durable append");
            written.send(()).expect("signal");
            released.recv().expect("release");
            // Cancellation path: the flight drops without any classification.
            drop(flight);
        });
        wrote.recv().expect("writer appended");
        assert!(
            matches!(close.status(), DrainStatus::Pending { .. }),
            "hung flight keeps the scope Pending"
        );
        release.send(()).expect("release writer");
        writer.join().expect("writer joined");
        assert_eq!(close.status(), DrainStatus::Blocked);
        drop(ticket);
    }
}
