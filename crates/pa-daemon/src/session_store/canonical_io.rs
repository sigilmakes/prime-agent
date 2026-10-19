//! Daemon-side durability classification for the canonical writer seam.
//!
//! Certainty authority stays in Core: one `CanonicalWriteRecorder` consumes the
//! single classification for the I/O flight Core admitted. This module only
//! touches the bytes and reports what actually happened, in two vocabularies:
//! the finer [`IoOutcome`] the daemon keeps for its own logs and tests, and the
//! two-valued [`IoCertainty`] the authority accepts.
//!
//! The distinction that matters is "no byte ever reached the kernel" versus
//! "a prefix may have landed". `write_all` cannot tell them apart, so the write
//! loop below counts bytes itself.
//!
//! Not wired yet: its consumer is the writer seam that takes a Core-issued
//! `CanonicalWriteTicket`. The file-inner allow is removed at that point and must
//! not become permanent debt.
#![allow(dead_code)]

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::Path;

/// What the actual I/O task observed at its decision points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IoOutcome {
    /// Every byte landed and `sync_all` proved durability.
    DurableOk,
    /// The write was refused before the kernel accepted a single byte.
    RefusedBeforeFirstByte,
    /// A prefix reached the kernel, then the write failed: the tail may be torn.
    PartialPrefix,
    /// Every byte landed but durability was never proven.
    FsyncUnproven,
}

/// The two certainty values the Core authority accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IoCertainty {
    /// The outcome is known: the bytes are durable, or nothing landed at all.
    Known,
    /// The outcome is unknown: the file may hold a partial or unproven tail.
    Uncertain,
}

impl IoOutcome {
    /// The single certainty value the authority accepts for this outcome.
    ///
    /// Only the two "the file is exactly as before" and "the bytes are durable"
    /// cases are [`IoCertainty::Known`]; anything that might have left a prefix
    /// behind is [`IoCertainty::Uncertain`].
    #[must_use]
    pub(super) fn certainty(self) -> IoCertainty {
        match self {
            Self::DurableOk | Self::RefusedBeforeFirstByte => IoCertainty::Known,
            Self::PartialPrefix | Self::FsyncUnproven => IoCertainty::Uncertain,
        }
    }

    /// A stable, greppable detail for the daemon's own log line.
    #[must_use]
    pub(super) fn detail(self) -> &'static str {
        match self {
            Self::DurableOk => "durable",
            Self::RefusedBeforeFirstByte => "refused-before-first-byte",
            Self::PartialPrefix => "partial-prefix",
            Self::FsyncUnproven => "fsync-unproven",
        }
    }
}

/// A sink that accepts exactly one classification, consuming itself to do it.
pub(super) trait CertaintySink {
    /// Error returned when the classification could not be recorded.
    type Error;

    /// Record the single classification for this flight.
    ///
    /// # Errors
    /// Implementation-defined. A sink that cannot record must return an error
    /// instead of reporting success.
    fn record(self, certainty: IoCertainty) -> Result<(), Self::Error>;
}

/// Settle `outcome` through `sink`, once.
///
/// # Errors
/// Propagates the sink's error. A classification that cannot be recorded is an
/// error, never silent success.
pub(super) fn settle<S: CertaintySink>(sink: S, outcome: IoOutcome) -> Result<(), S::Error> {
    sink.record(outcome.certainty())
}

/// Append `bytes` and prove durability, reporting what the kernel let through.
///
/// Opens with create+append, counts accepted bytes in a raw write loop so a
/// zero-byte refusal is distinguishable from a torn prefix, then calls
/// `sync_all` so a completion is never reported without proof.
#[must_use]
pub(super) fn append_rows_durable(path: &Path, bytes: &[u8]) -> IoOutcome {
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return IoOutcome::RefusedBeforeFirstByte;
    };
    let mut written = 0_usize;
    while written < bytes.len() {
        match file.write(&bytes[written..]) {
            Ok(0) => return after_write_error(written),
            Ok(count) => written += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return after_write_error(written),
        }
    }
    match file.sync_all() {
        Ok(()) => IoOutcome::DurableOk,
        Err(_) => IoOutcome::FsyncUnproven,
    }
}

/// A write that stopped after `written` accepted bytes: nothing landed, or a prefix did.
fn after_write_error(written: usize) -> IoOutcome {
    if written == 0 {
        IoOutcome::RefusedBeforeFirstByte
    } else {
        IoOutcome::PartialPrefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn scratch_path(tag: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "pa-canonical-io-{tag}-{}-{unique}",
            std::process::id()
        ))
    }

    struct RecordingSink {
        seen: Rc<Cell<Option<IoCertainty>>>,
    }

    impl CertaintySink for RecordingSink {
        type Error = ();

        fn record(self, certainty: IoCertainty) -> Result<(), Self::Error> {
            self.seen.set(Some(certainty));
            Ok(())
        }
    }

    struct FailingSink;

    impl CertaintySink for FailingSink {
        type Error = &'static str;

        fn record(self, _certainty: IoCertainty) -> Result<(), Self::Error> {
            Err("recorder refused")
        }
    }

    #[test]
    fn durable_append_reports_durable_and_writes_exact_bytes() {
        let path = scratch_path("durable");
        let outcome = append_rows_durable(&path, b"{\"row\":1}\n");
        assert_eq!(outcome, IoOutcome::DurableOk);
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"row\":1}\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn second_append_is_append_only_and_never_rewrites_the_tail() {
        let path = scratch_path("append");
        assert_eq!(append_rows_durable(&path, b"one\n"), IoOutcome::DurableOk);
        assert_eq!(append_rows_durable(&path, b"two\n"), IoOutcome::DurableOk);
        assert_eq!(std::fs::read(&path).unwrap(), b"one\ntwo\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn missing_parent_directory_refuses_before_first_byte() {
        let path = scratch_path("no-parent").join("nested").join("rows.jsonl");
        assert_eq!(
            append_rows_durable(&path, b"row\n"),
            IoOutcome::RefusedBeforeFirstByte
        );
        assert!(!path.exists());
    }

    #[test]
    fn zero_byte_write_failure_is_refused_not_partial() {
        // /dev/full accepts open() and every write returns ENOSPC, so the kernel
        // accepts zero bytes: that must classify as refused, never as a prefix.
        let full = Path::new("/dev/full");
        if !full.exists() {
            return;
        }
        let outcome = append_rows_durable(full, b"row\n");
        assert_eq!(outcome, IoOutcome::RefusedBeforeFirstByte);
        assert_eq!(outcome.certainty(), IoCertainty::Known);
    }

    #[test]
    fn every_outcome_maps_into_the_authority_vocabulary() {
        assert_eq!(IoOutcome::DurableOk.certainty(), IoCertainty::Known);
        assert_eq!(
            IoOutcome::RefusedBeforeFirstByte.certainty(),
            IoCertainty::Known
        );
        assert_eq!(IoOutcome::PartialPrefix.certainty(), IoCertainty::Uncertain);
        assert_eq!(IoOutcome::FsyncUnproven.certainty(), IoCertainty::Uncertain);
        // No outcome may map to silence: every arm above is exhaustive, and the
        // four details are distinct so a log line never has to guess.
        let details: Vec<&str> = [
            IoOutcome::DurableOk,
            IoOutcome::RefusedBeforeFirstByte,
            IoOutcome::PartialPrefix,
            IoOutcome::FsyncUnproven,
        ]
        .iter()
        .map(|outcome| outcome.detail())
        .collect();
        let mut unique = details.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), details.len());
    }

    #[test]
    fn settle_records_exactly_once() {
        let seen = Rc::new(Cell::new(None));
        settle(
            RecordingSink {
                seen: Rc::clone(&seen),
            },
            IoOutcome::PartialPrefix,
        )
        .unwrap();
        assert_eq!(seen.get(), Some(IoCertainty::Uncertain));
    }

    #[test]
    fn a_sink_that_cannot_record_is_an_error_not_silent_success() {
        assert_eq!(
            settle(FailingSink, IoOutcome::DurableOk),
            Err("recorder refused")
        );
    }
}
