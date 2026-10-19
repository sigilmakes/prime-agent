//! The daemon half of the canonical writer joint.
//!
//! Core mints one single-use recorder per admitted I/O flight. This module is
//! the only place the daemon may consume it, and it fixes the order the joint
//! depends on: admit the flight, obtain the recorder for that exact operation and
//! stage, do the bytes, classify, then drop the flight, then drop the ticket.
//!
//! Nothing here constructs authority. The ticket always arrives from Core; a
//! recorder refusal means the flight was already claimed or has no owned record,
//! so the flight is dropped unclassified and drain fails closed instead of a
//! classification being fabricated for a write that never had a recorder.
#![allow(dead_code)]

use std::path::Path;

use pa_core::session::canonical::{
    CanonicalCursor, CanonicalEntryDraft, CanonicalIoStage, CanonicalOperation, CanonicalStoreError,
};
use pa_core::session_engine::canonical_coordinator::{
    CanonicalWriteCertainty, CanonicalWriteRecorder, CanonicalWriteTicket,
};

use super::canonical_io::{append_rows_durable, settle, CertaintySink, IoCertainty, IoOutcome};

impl From<IoCertainty> for CanonicalWriteCertainty {
    fn from(certainty: IoCertainty) -> Self {
        match certainty {
            IoCertainty::Known => Self::Known,
            IoCertainty::Uncertain => Self::Uncertain,
        }
    }
}

impl CertaintySink for CanonicalWriteRecorder {
    type Error = CanonicalStoreError;

    fn record(self, certainty: IoCertainty) -> Result<(), Self::Error> {
        CanonicalWriteRecorder::record(self, CanonicalWriteCertainty::from(certainty))
    }
}

/// Write `bytes` to `path` through the real syscalls and record one classification.
///
/// # Errors
/// Propagates a flight-admission refusal, a recorder refusal (the flight is then
/// dropped unclassified, which poisons drain), or a refusal to record.
pub(super) fn commit_canonical_rows(
    ticket: CanonicalWriteTicket,
    stage: CanonicalIoStage,
    scope: &CanonicalCursor,
    operation: &CanonicalOperation,
    draft: &CanonicalEntryDraft,
    path: &Path,
    bytes: &[u8],
) -> Result<IoOutcome, CanonicalStoreError> {
    let flight = ticket
        .permit
        .enter_io(stage, scope, operation, Some(draft))?;
    match ticket.recorder(stage) {
        Ok(recorder) => {
            let outcome = append_rows_durable(path, bytes);
            settle(recorder, outcome)?;
            drop(flight);
            drop(ticket);
            Ok(outcome)
        }
        Err(error) => {
            drop(flight);
            Err(error)
        }
    }
}
