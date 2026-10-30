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
//! Durability of a created file needs two proofs, not one. `sync_all` on the file
//! descriptor proves the bytes of a file that already existed; for a file this
//! call has just created it does not prove the new directory entry, so a crash
//! could lose the name while `DurableOk` was already reported. The create path
//! therefore syncs the containing directory as well and only then reports
//! [`IoOutcome::DurableOk`]; a directory that cannot be opened or synced is
//! [`IoOutcome::FsyncUnproven`], which the authority treats as `Uncertain` and
//! which blocks the epoch instead of draining it.
//!
//! Wired so far: the session store's per-row append classifies its outcome here
//! ([`append_outcome`]) and settles the grant through [`settle`]. Still unwired:
//! the writer seam that takes a Core-issued `CanonicalWriteTicket`. The file-inner
//! allow is removed at that point and must not become permanent debt.
#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;

/// Consecutive `EINTR` retries allowed inside one write loop.
///
/// A signal storm must not be able to spin the loop forever, so the retries are
/// bounded and exhaustion is reported as a write that stopped where it stopped.
const MAX_INTERRUPTED_SPINS: u8 = 16;

/// One durability proof, in the order the kernel is asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncStage {
    /// The file's own bytes were synced.
    File,
    /// The directory entry was synced, which a created file needs.
    Directory,
}

/// What the actual I/O task observed at its decision points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IoOutcome {
    /// Every byte landed and durability was proved: the file was synced, and the
    /// containing directory was synced too when this call created the file.
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

/// Classify an append the shared window writer performed.
///
/// That writer (`window::append_cached`) opens append-only, so it never creates
/// the file and no directory entry is at stake: its success already includes the
/// file's `fsync`, which is [`IoOutcome::DurableOk`] only because the create path
/// renamed durably first. A failure is reported from outside its write loop, so
/// the daemon cannot tell a zero-byte refusal from a torn prefix; it must never be
/// reported as a refusal, and the authority reads it as an unproven tail.
#[must_use]
pub(super) fn append_outcome<E>(result: &Result<(), E>) -> IoOutcome {
    match result {
        Ok(()) => IoOutcome::DurableOk,
        Err(_) => IoOutcome::PartialPrefix,
    }
}

/// Append `bytes` and prove durability, reporting what the kernel let through.
///
/// Opens with append (creating the file when absent), counts accepted bytes in a
/// raw write loop so a zero-byte refusal is distinguishable from a torn prefix,
/// syncs the file, and then syncs the containing directory when this call created
/// the file. A completion is never reported without both proofs.
#[must_use]
pub(super) fn append_rows_durable(path: &Path, bytes: &[u8]) -> IoOutcome {
    append_rows_watched(path, bytes, &mut |_stage| {}, &sync_directory)
}

/// [`append_rows_durable`] with the durability proofs observable and the
/// directory sync injectable.
///
/// The observer sees each proof only after it succeeded, so a test can assert the
/// order rather than trusting a return value. `sync_dir` exists so the failure
/// path can be exercised without a broken filesystem.
#[must_use]
pub(super) fn append_rows_watched<O, D>(
    path: &Path,
    bytes: &[u8],
    observe: &mut O,
    sync_dir: &D,
) -> IoOutcome
where
    O: FnMut(SyncStage),
    D: Fn(&Path) -> std::io::Result<()>,
{
    let Ok((mut file, created)) = open_rows(path) else {
        return IoOutcome::RefusedBeforeFirstByte;
    };
    let mut written = 0_usize;
    let mut spins = 0_u8;
    while written < bytes.len() {
        match file.write(&bytes[written..]) {
            Ok(0) => return after_write_error(written),
            Ok(count) => {
                written += count;
                spins = 0;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {
                spins = spins.saturating_add(1);
                if spins > MAX_INTERRUPTED_SPINS {
                    return after_write_error(written);
                }
            }
            Err(_) => return after_write_error(written),
        }
    }
    if file.sync_all().is_err() {
        return IoOutcome::FsyncUnproven;
    }
    observe(SyncStage::File);
    if !created {
        return IoOutcome::DurableOk;
    }
    if sync_dir(parent_directory(path)).is_err() {
        return IoOutcome::FsyncUnproven;
    }
    observe(SyncStage::Directory);
    IoOutcome::DurableOk
}

/// Open for append, reporting whether this call created the file.
///
/// `create_new` makes the answer atomic instead of a check-then-open race.
fn open_rows(path: &Path) -> std::io::Result<(File, bool)> {
    match OpenOptions::new().create_new(true).append(true).open(path) {
        Ok(file) => Ok((file, true)),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => OpenOptions::new()
            .append(true)
            .open(path)
            .map(|file| (file, false)),
        Err(error) => Err(error),
    }
}

/// Prove that the directory entry itself is durable.
fn sync_directory(parent: &Path) -> std::io::Result<()> {
    File::open(parent)?.sync_all()
}

/// The directory a created file must sync.
///
/// `Path::parent` reports `Some("")` for a bare relative name such as
/// `rows.jsonl`, and `File::open("")` fails, which would classify a legitimate
/// relative-path write as unproven and block the epoch for no reason. The current
/// directory is the right answer there.
fn parent_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
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
    fn created_file_syncs_the_file_and_then_the_directory() {
        let path = scratch_path("create-sync");
        let mut stages = Vec::new();
        let outcome = append_rows_watched(
            &path,
            b"row\n",
            &mut |stage| stages.push(stage),
            &sync_directory,
        );
        assert_eq!(outcome, IoOutcome::DurableOk);
        // A created file needs both proofs, and the order is file then directory.
        assert_eq!(stages, vec![SyncStage::File, SyncStage::Directory]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn existing_file_syncs_only_the_file() {
        let path = scratch_path("existing-sync");
        assert_eq!(append_rows_durable(&path, b"one\n"), IoOutcome::DurableOk);
        let mut stages = Vec::new();
        let outcome = append_rows_watched(
            &path,
            b"two\n",
            &mut |stage| stages.push(stage),
            &sync_directory,
        );
        assert_eq!(outcome, IoOutcome::DurableOk);
        assert_eq!(stages, vec![SyncStage::File]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn directory_sync_failure_is_unproven_and_never_durable() {
        let path = scratch_path("dir-sync-fails");
        let mut stages = Vec::new();
        let outcome = append_rows_watched(
            &path,
            b"row\n",
            &mut |stage| stages.push(stage),
            &|_parent| Err(std::io::Error::from(ErrorKind::PermissionDenied)),
        );
        // The bytes did land, so this is not a refusal: it is unproven durability.
        assert_eq!(outcome, IoOutcome::FsyncUnproven);
        assert_eq!(outcome.certainty(), IoCertainty::Uncertain);
        assert_eq!(stages, vec![SyncStage::File]);
        assert_eq!(std::fs::read(&path).unwrap(), b"row\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn bare_relative_name_syncs_the_current_directory() {
        // `Path::new("rows.jsonl").parent()` is `Some("")`, and `File::open("")`
        // fails with ENOENT. A relative write must not turn that into an
        // unproven durability that blocks the epoch.
        assert_eq!(parent_directory(Path::new("rows.jsonl")), Path::new("."));
        assert_eq!(
            parent_directory(Path::new("/tmp/dir/rows.jsonl")),
            Path::new("/tmp/dir")
        );
        assert_eq!(parent_directory(Path::new("/rows.jsonl")), Path::new("/"));
    }

    #[test]
    fn a_failed_append_is_never_reported_as_a_refusal() {
        // The shared writer reports one error for "the open refused" and "a prefix
        // landed". Outside its write loop the two cannot be told apart, so the
        // classification must stay unproven; only success carries a durability proof.
        let failed: Result<(), std::io::Error> =
            Err(std::io::Error::from(ErrorKind::PermissionDenied));
        assert_eq!(append_outcome(&failed), IoOutcome::PartialPrefix);
        assert_eq!(
            append_outcome(&failed).certainty(),
            IoCertainty::Uncertain,
            "a failed append must never be Known"
        );
        let appended: Result<(), std::io::Error> = Ok(());
        assert_eq!(append_outcome(&appended), IoOutcome::DurableOk);
        assert_eq!(append_outcome(&appended).certainty(), IoCertainty::Known);
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
