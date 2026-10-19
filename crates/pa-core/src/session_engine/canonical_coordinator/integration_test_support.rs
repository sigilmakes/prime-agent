//! Integration-test support: real owner capture and real close ticket for a
//! cross-crate writer test.
//!
//! This module is compiled ONLY with the non-default `canonical-integration-test-support`
//! feature. It exists because the seam's failure mode is a cross-crate joint: the
//! daemon write site records and Core drains. A pa-daemon test cannot name the
//! private authority types, so this module exposes the minimum constructors it
//! needs to build a real [`CanonicalWriteTicket`] against a real owner capture and
//! to observe the real close ticket.
//!
//! It is integration-test support, NOT public API: no production module references
//! it, it is never enabled by default, and it adds no capability to
//! [`super::CanonicalWriteCertainty`] or to the recording rules.
#![allow(dead_code)]

use std::any::Any;
use std::sync::Arc;

use super::{CanonicalWriteCertainty, CanonicalWriteTicket};
use crate::session::canonical::{
    CanonicalCursor, CanonicalEntryDraft, CanonicalFailureKind, CanonicalIoStage,
    CanonicalOperation, CanonicalRequestedWrite, CanonicalSourceDisposition, CanonicalStoreError,
};
use crate::session_engine::canonical_authority::{
    AuthorityOwner, CapturedRun, CloseReason, CloseTicket, DrainStatus, LateRunAnchor,
};

fn refused(detail: &str) -> CanonicalStoreError {
    CanonicalStoreError {
        kind: CanonicalFailureKind::InvalidRequest,
        requested: CanonicalRequestedWrite::NotWritten,
        source: CanonicalSourceDisposition::Unchanged,
        detail: detail.to_owned(),
    }
}

/// Drain view of a real close ticket, mirrored for cross-crate assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalTestDrain {
    /// Outstanding work: admitted flights, transaction leases or registered anchors.
    Pending {
        io: usize,
        transactions: usize,
        anchors: usize,
    },
    /// Nothing outstanding and not poisoned: the ordered boundary may be consumed.
    Drained,
    /// Poisoned or unavailable: never treated as ready.
    Blocked,
    /// The caller itself still holds the transaction lease or the anchor.
    WouldDrainSelf,
}

impl From<DrainStatus> for CanonicalTestDrain {
    fn from(status: DrainStatus) -> Self {
        match status {
            DrainStatus::Pending {
                io,
                transactions,
                anchors,
            } => Self::Pending {
                io,
                transactions,
                anchors,
            },
            DrainStatus::Drained => Self::Drained,
            DrainStatus::Blocked => Self::Blocked,
            DrainStatus::WouldDrainSelf => Self::WouldDrainSelf,
        }
    }
}

/// One real owner epoch for a cross-crate writer test.
pub struct CanonicalTestEpoch {
    owner: AuthorityOwner,
    cursor: CanonicalCursor,
    run: Option<CapturedRun>,
    anchor: Option<LateRunAnchor>,
    close: Option<CloseTicket>,
}

impl CanonicalTestEpoch {
    /// Start a real owner epoch on the given source cursor.
    ///
    /// # Errors
    /// Refuses only when the source cursor cannot be mirrored; construction itself
    /// admits nothing.
    pub fn new(source: CanonicalCursor) -> Result<Self, CanonicalStoreError> {
        let owner = AuthorityOwner::new(source.clone());
        Ok(Self {
            owner,
            cursor: source,
            run: None,
            anchor: None,
            close: None,
        })
    }

    /// The exact source cursor this epoch was started on.
    #[must_use]
    pub fn cursor(&self) -> CanonicalCursor {
        self.cursor.clone()
    }

    /// Capture the actual serving run and model allocations (real capture, once).
    ///
    /// # Errors
    /// Refuses a second capture, a retired contribution, or a closed epoch.
    pub fn capture_serving<R: Any + Send + Sync, M: Any + Send + Sync>(
        &mut self,
        actual_run: Arc<R>,
        actual_model: Arc<M>,
    ) -> Result<(), CanonicalStoreError> {
        let (run, anchor) =
            self.owner
                .capture_serving_run(&self.cursor, actual_run, actual_model, Vec::new())?;
        self.run = Some(run);
        self.anchor = Some(anchor);
        Ok(())
    }

    /// Run the owner prepare preflight and admit one fresh final transaction,
    /// returning its operation identity and the public ticket.
    ///
    /// The prepare flight is classified `Known` here because no daemon write is
    /// involved in preflight; that is exactly the owner's own path.
    ///
    /// # Errors
    /// Refuses when no serving capture exists or the owner refuses admission.
    pub fn final_ticket(
        &self,
        draft: CanonicalEntryDraft,
    ) -> Result<(CanonicalOperation, CanonicalWriteTicket), CanonicalStoreError> {
        let run = self
            .run
            .as_ref()
            .ok_or_else(|| refused("no serving capture"))?;
        let scope = run.context_policy();
        let (capture, prepare) = scope.begin_prepare()?;
        let operation = CanonicalOperation::new();
        let prepare_ticket = CanonicalWriteTicket::from_admitted(prepare);
        let flight = prepare_ticket.permit.enter_io(
            CanonicalIoStage::PreparePreflight,
            run.source(),
            &operation,
            None,
        )?;
        prepare_ticket
            .recorder(CanonicalIoStage::PreparePreflight)?
            .record(CanonicalWriteCertainty::Known)?;
        drop(flight);
        drop(prepare_ticket);
        let admitted = capture.admit_final(&operation, draft)?;
        Ok((operation, CanonicalWriteTicket::from_admitted(admitted)))
    }

    /// Stage the real closing transition (nonblocking; nothing waits here).
    ///
    /// # Errors
    /// Refuses only when the epoch state cannot be read.
    pub fn request_close(&mut self) -> Result<(), CanonicalStoreError> {
        self.close = Some(self.owner.request_close(CloseReason::Retirement));
        Ok(())
    }

    /// The current drain view, or `Blocked` when no close was requested.
    #[must_use]
    pub fn close_status(&self) -> CanonicalTestDrain {
        self.close
            .as_ref()
            .map_or(CanonicalTestDrain::Blocked, |ticket| {
                CanonicalTestDrain::from(ticket.status())
            })
    }

    /// Consume the ordered boundary after the drain is complete.
    ///
    /// # Errors
    /// Refuses anything but a real `Drained` ticket.
    pub fn finish_at_external_idle(self) -> Result<(), CanonicalStoreError> {
        let ticket = self
            .close
            .ok_or_else(|| refused("no staged close ticket"))?;
        ticket.finish_at_external_idle()
    }

    /// Whether this epoch still permits projection. NOT a readiness predicate.
    #[must_use]
    pub fn projection_allowed(&self) -> bool {
        self.owner.projection_allowed()
    }

    /// Finish the registered serving anchor at the ordered boundary.
    pub fn finish_anchor(&mut self) {
        if let Some(anchor) = self.anchor.take() {
            anchor.finish_ordered_boundary();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::canonical::{
        CanonicalFrontier, CanonicalMutationKind, CanonicalOwnerScope,
    };
    use std::path::PathBuf;

    fn source() -> CanonicalCursor {
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

    fn draft() -> CanonicalEntryDraft {
        CanonicalEntryDraft::new(
            CanonicalMutationKind::ContextPolicy,
            serde_json::from_value(serde_json::json!({"type":"compaction",
                "summary":"exact","firstKeptEntryId":"a","tokensBefore":1,
                "details":{"bytes":"AA=="}}))
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

    #[test]
    fn integration_ticket_classifies_known_and_drains() {
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
            .expect("actual flight");
        ticket
            .recorder(CanonicalIoStage::RequestedMutation)
            .expect("recorder")
            .record(CanonicalWriteCertainty::Known)
            .expect("classify known");
        epoch.request_close().expect("close");
        assert!(matches!(
            epoch.close_status(),
            CanonicalTestDrain::Pending { io: 1, .. }
        ));
        drop(flight);
        drop(ticket);
        epoch.finish_anchor();
        assert_eq!(epoch.close_status(), CanonicalTestDrain::Drained);
        epoch.finish_at_external_idle().expect("ordered boundary");
    }

    #[test]
    fn integration_ticket_unrecorded_flight_blocks_the_epoch() {
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
            .expect("actual flight");
        epoch.request_close().expect("close");
        drop(flight);
        drop(ticket);
        epoch.finish_anchor();
        assert_eq!(epoch.close_status(), CanonicalTestDrain::Blocked);
        assert!(!epoch.projection_allowed());
    }
}
