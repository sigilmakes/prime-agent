//! Durable session-log transport for each session's exact selected path.
//!
//! Row policy at this boundary (the spec: policy lives in callers; the core
//! never waits, retries, or inspects a peer): an append resolves exactly
//! once. A foreign `Busy` holder and a `NotWritten` verdict surface as
//! refusals through the store's existing failure paths; `Unknown` asks the
//! core's `Recover` to decide before any row is indexed. The daemon store
//! keeps its fold as the serving view; the log is the source of truth the
//! fold replays from (port-plan §3.2: open = acquire, repair, read-since
//! fold).
//!
//! Durability contract: the log holds every row the fold serves. The fold's
//! index gains a row only after the log committed it, and the unmaterialized
//! tail is tracked here, so `rewrite()` (the full-file atomic write the old
//! transport's create flows end with) becomes a tail flush.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use pa_core::session_log::{self, Outcome, SessionLog};

/// One open/repair's budget at this transport. The daemon's own append paths
/// run under the session's admission deadlines; the blocking open/repair here
/// takes this bound so a wedged storage device reports instead of stalling a
/// create. The core never waits; this is the caller's one clock.
const LOG_OPEN_DEADLINE: Duration = Duration::from_secs(10);

/// The session id a session file path names: its stem.
pub(crate) fn session_id_of(session_path: &Path) -> Option<String> {
    session_path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_owned)
}

/// The bounded instant for one open/append/repair decision at this transport.
pub(crate) fn deadline() -> Instant {
    Instant::now() + LOG_OPEN_DEADLINE
}

/// How long one wire operation rides out a busy lock before it reports it.
///
/// The core never waits: one `try_lock` refusal comes back as `Busy`. A holder
/// keeps the lock for exactly one operation, so the refusal is transient by
/// construction — but it is not imaginary: a *reader* that opens a live session
/// (usage attribution, the roster) scans under the same exclusive lock, and a
/// live worker's next row then lands inside that scan. Refusing the row there
/// aborts the writer's turn for a lock the reader is about to release. This
/// budget is the ceiling: a wedged holder is still reported, never waited on
/// forever.
const BUSY_RETRY_BUDGET: Duration = Duration::from_millis(250);

/// One retry's pause: long enough for a scan of a session file to finish.
const BUSY_RETRY_STEP: Duration = Duration::from_millis(2);

/// Why one wire step failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LogWireError {
    #[error("unsupported legacy session format; the file was not changed")]
    LegacyUnsupported,
    /// Another writer holds the log.
    #[error("session log is held by {holder}")]
    Busy {
        /// The record the foreign holder left, when there was one.
        holder: String,
    },
    /// Nothing durable was written, or a past write stayed undecided.
    #[error("session log: {0}")]
    Refused(#[source] anyhow::Error),
    /// The log could not be opened or (re)synced.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The lazy log handle behind a log-transported `SessionFile`.
///
/// The handle is clone-SHARED (a store's `Clone` copies the arc): every copy
/// flushes through one next-seq counter under one lock-held file, so copies
/// never interleave. Command surfaces that would turn a clone into a
/// divergent fold (branch/fork/import row rewrites) are refused at their
/// command seam while the session is log-transported, so exactly one serving
/// store flushes a session's log.
#[derive(Debug, Clone)]
pub(crate) struct LogWire {
    state: std::sync::Arc<std::sync::Mutex<WireState>>,
}

/// The wire's shared state. The seq cursor and the pending row live here
/// because a store clone carries the same wire: one log gets exactly one
/// tail cursor.
struct WireState {
    /// `None` until the first flush; a refused open retries at the next call.
    log: Option<SessionLog>,
    /// `None` in production (the open takes [`SessionLog::open`]); an
    /// injected disk drives the fault-injection tests through the very same
    /// code paths ([`Disk`]'s own contract).
    disk: Option<std::sync::Arc<dyn pa_core::session_log::Disk>>,
    directory: PathBuf,
    /// True only after this fresh intent committed its header, or when
    /// `held` explicitly reopened an existing validated log.
    header_written: bool,
    /// The next seq this wire pins: the log's last row's seq plus one, set
    /// at the open and moved by committed rows and tail adoptions only —
    /// never by a refused or undecided write.
    next_seq: u64,
    /// A write whose durability is not decided: the seq it pinned and the
    /// row it carried. The retry re-pins the same seq, and the core answers
    /// from the log's bytes — a write that landed is reported, never
    /// repeated. The pending survives every undecided read of its recovery;
    /// the log's own verdict on the seq clears it.
    pending: Option<PendingRow>,
}

/// One undecided write: the seq it pinned and the row it carried.
#[derive(Debug)]
struct PendingRow {
    seq: u64,
    row: Value,
    recover: session_log::Recover,
}

impl std::fmt::Debug for WireState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WireState")
            .field("has_log", &self.log.is_some())
            .field("has_disk", &self.disk.is_some())
            .field("directory", &self.directory)
            .field("header_written", &self.header_written)
            .field("next_seq", &self.next_seq)
            .field("pending", &self.pending)
            .finish()
    }
}

impl WireState {
    /// Open a fresh intent's file without adopting any existing row. Only
    /// `held` constructs a reopened wire; the first append atomically decides
    /// whether this fresh file is still empty.
    fn ensure_open(&mut self, session_id: &str) -> Result<(), LogWireError> {
        if self.log.is_some() {
            return Ok(());
        }
        std::fs::create_dir_all(self.directory.clone()).map_err(LogWireError::Io)?;
        let until = Instant::now() + BUSY_RETRY_BUDGET;
        let log = loop {
            let opened = match self.disk.clone() {
                Some(disk) => {
                    SessionLog::open_with(disk, &self.directory.clone(), session_id, deadline())
                }
                None => SessionLog::open(&self.directory.clone(), session_id, deadline()),
            };
            match opened {
                Ok(log) => break log,
                Err(session_log::Error::Busy { .. }) if Instant::now() < until => {
                    std::thread::sleep(BUSY_RETRY_STEP);
                }
                Err(error) => return Err(open_error(error)),
            }
        };
        self.log = Some(log);
        Ok(())
    }

    /// Adopt the log's tail ([`SessionLog::refresh`], the follower's read)
    /// before any writes: the seq cursor moves to the log's end, so a rival
    /// writer's rows since this wire's last look are pinned past, and an
    /// undecided write whose bytes landed — `Unknown` leaves neither the
    /// core's cursor nor the wire's advanced — is adopted here, its seq
    /// answered without a second write. The pending row itself stays: only
    /// the core's decided outcome clears it, because the caller may
    /// re-present the same bytes and must re-pin at the pending's seq for
    /// the core to answer from the landed row instead of repeating it.
    fn refresh_tail(&mut self) -> Result<(), LogWireError> {
        let until = Instant::now() + BUSY_RETRY_BUDGET;
        let rows = loop {
            let log = self.log.as_mut().expect("the caller ran ensure_open first");
            match log.refresh(deadline()) {
                Ok(rows) => break rows,
                Err(session_log::Error::Busy { .. }) if Instant::now() < until => {
                    std::thread::sleep(BUSY_RETRY_STEP);
                }
                Err(error) => return Err(open_error(error)),
            }
        };
        if let Some(last) = rows.last() {
            self.next_seq = last.seq + 1;
        }
        Ok(())
    }

    /// Extend one fold's cursor under the lock, riding out a live holder: the
    /// reader that keeps this writer's next row out is usually a scan of this
    /// same file (usage attribution, the roster) that is about to release it.
    fn read_tail_retrying(
        &mut self,
        cursor: &mut session_log::ReadCursor,
    ) -> Result<Vec<session_log::Row>, LogWireError> {
        let until = Instant::now() + BUSY_RETRY_BUDGET;
        loop {
            let log = self.log.as_mut().expect("ensure_open succeeded");
            match log.read_tail(cursor, deadline()) {
                Ok(rows) => return Ok(rows),
                Err(session_log::ReadError::Log(session_log::Error::Busy { .. }))
                    if Instant::now() < until =>
                {
                    std::thread::sleep(BUSY_RETRY_STEP);
                }
                Err(error) => {
                    return Err(LogWireError::Refused(anyhow::Error::from(error)));
                }
            }
        }
    }

    /// Reuse the original recovery capability until it decides durability.
    /// True means the pending row committed; false means it did not land.
    fn recover_pending(&mut self) -> Result<bool, LogWireError> {
        let pending = self.pending.as_ref().expect("a pending recovery exists");
        let seq = pending.seq;
        match pending.recover.recover(deadline()) {
            Ok(Outcome::Committed { .. }) => {
                self.next_seq = self.next_seq.max(seq + 1);
                self.pending = None;
                Ok(true)
            }
            Ok(Outcome::NotWritten { .. }) => {
                self.pending = None;
                self.refresh_tail()?;
                Ok(false)
            }
            Ok(Outcome::Busy { .. } | Outcome::Unknown { .. }) => Err(LogWireError::Refused(
                anyhow::anyhow!("the session log left its undecided write open; retry the flush"),
            )),
            Err(error) => Err(LogWireError::Refused(anyhow::Error::from(error).context(
                "the session log left its undecided write open; retry the flush",
            ))),
        }
    }

    /// Append `row`, pinned, and resolve its outcome against the log before
    /// anything the caller sees. The rule (the runtime's pending solution):
    /// the wire pins every row and only a committed row advances the
    /// cursor; an undecided write keeps its seq and bytes pending so any
    /// retry re-pins them and the core reports the landed row instead of
    /// repeating it; a seq that provably holds a foreign row re-syncs and
    /// re-pins the caller's row once.
    fn append_pinned(
        &mut self,
        row: &Value,
        cursor: &session_log::ReadCursor,
    ) -> Result<(), LogWireError> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.row != *row)
        {
            return Err(LogWireError::Refused(anyhow::anyhow!(
                "resolve the pending session row before another write"
            )));
        }
        if self.pending.is_some() && self.recover_pending()? {
            return Ok(());
        }
        // At most the first attempt plus one re-pin after a provable
        // conflict: beyond that the log keeps moving under the wire and a
        // reopen must settle it.
        for _attempt in 0..2 {
            let log = self.log.as_mut().expect("the caller ran ensure_open first");
            let seq = self
                .pending
                .as_ref()
                .filter(|pending| pending.row == *row)
                .map_or(self.next_seq, |pending| pending.seq);
            // The holder keeps the lock for one operation only; a reader
            // scanning this file is the holder this writer can meet most
            // easily. Ride that out instead of dropping the caller's row.
            let until = Instant::now() + BUSY_RETRY_BUDGET;
            let outcome = loop {
                let result = if !self.header_written
                    && row.get("type").and_then(Value::as_str) == Some("session")
                {
                    match log.append_first(row, deadline()) {
                        Outcome::Busy { holder, .. } => Err(LogWireError::Busy {
                            holder: holder.map_or_else(
                                || "another writer".to_owned(),
                                |holder| format!("pid {}", holder.pid),
                            ),
                        }),
                        outcome => Ok(outcome),
                    }
                } else {
                    log.append_at_checked(seq, row, cursor, deadline())
                        .map_err(|error| match error {
                            session_log::ReadError::Log(error) => open_error(error),
                            other => LogWireError::Refused(anyhow::Error::from(other)),
                        })
                };
                match result {
                    Ok(outcome) => break outcome,
                    Err(LogWireError::Busy { .. }) if Instant::now() < until => {
                        std::thread::sleep(BUSY_RETRY_STEP);
                    }
                    Err(error) => return Err(error),
                }
            };
            match outcome {
                Outcome::Committed { seq, .. } => {
                    // A committed row may be an earlier pending's landed
                    // bytes re-presented while younger rows already moved
                    // the cursor; the cursor only ever advances.
                    self.next_seq = self.next_seq.max(seq + 1);
                    // The caller indexes the entry it just saw committed.
                    self.pending = None;
                    return Ok(());
                }
                Outcome::Unknown { recover } => {
                    // Remember the seq before the decision: every retry of
                    // this row re-pins it, and the core answers from the
                    // log's bytes.
                    self.pending = Some(PendingRow {
                        seq: recover.seq(),
                        row: row.clone(),
                        recover,
                    });
                    if self.recover_pending()? {
                        return Ok(());
                    }
                }
                Outcome::NotWritten {
                    reason: pa_core::session_log::NotWritten::SeqConflict { .. },
                } => {
                    self.pending = None;
                    // The pinned seq holds someone else's row, or none at
                    // all. The core's own cursor is already current (every
                    // append adopts the tail first), so `refresh` may have
                    // nothing new to report; read the rows from this
                    // wire's stale point onward instead and take the
                    // cursor from their end. The pinned seq's pending
                    // stays; a re-present of its bytes still resolves
                    // through the core's body match.
                    {
                        let log = self.log.as_mut().expect("the caller ran ensure_open first");
                        let rows = log
                            .read_since(self.next_seq.saturating_sub(1))
                            .map_err(open_error)?;
                        if let Some(last) = rows.last() {
                            self.next_seq = self.next_seq.max(last.seq + 1);
                        }
                    }
                }
                outcome => return Err(wire_error(outcome)),
            }
        }
        Err(LogWireError::Refused(anyhow::anyhow!(
            "the session log's tail kept moving under the wire; reopen the session"
        )))
    }
}

impl LogWire {
    /// Only a fresh header without an undecided original may be prepared.
    pub(crate) fn needs_initial_header(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.header_written && state.pending.is_none()
    }
    /// Exact durable bytes on the rare known-ID retry path, before view overlays.
    pub(crate) fn committed_row(&self, id: &str) -> Result<Value, LogWireError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rows = state
            .log
            .as_ref()
            .expect("a committed row has an open log")
            .read_since(0)
            .map_err(open_error)?;
        rows.into_iter()
            .skip(1)
            .find(|row| row.body.get("id").and_then(Value::as_str) == Some(id))
            .map(|row| row.body)
            .ok_or_else(|| {
                LogWireError::Refused(anyhow::anyhow!(
                    "committed row {id} is no longer present; reopen the session"
                ))
            })
    }
    /// The exact undecided envelope, before a public caller can mint a new ID.
    pub(crate) fn pending_row(&self) -> Option<Value> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .as_ref()
            .map(|pending| pending.row.clone())
    }

    /// Reconcile pending durability, then extend one fold's independent cursor.
    pub(crate) fn observe(
        &self,
        session_id: &str,
        cursor: &mut session_log::ReadCursor,
    ) -> Result<Vec<session_log::Row>, LogWireError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.ensure_open(session_id)?;
        if let Some(row) = state.pending.as_ref().map(|pending| pending.row.clone()) {
            state.append_pinned(&row, cursor)?;
            if row.get("type").and_then(Value::as_str) == Some("session") {
                state.header_written = true;
            }
        }
        if !state.header_written {
            return Ok(Vec::new());
        }
        state.read_tail_retrying(cursor)
    }
    /// A fresh intent for an exact durable path, including a custom directory.
    /// The initial header must win an atomic first append; existing bytes
    /// are never implicitly reopened or adopted.
    pub(crate) fn fresh(path: &Path) -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(WireState {
                log: None,
                disk: None,
                directory: path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf(),
                header_written: false,
                next_seq: 1,
                pending: None,
            })),
        }
    }

    pub(crate) fn flush_selection(
        &self,
        session_id: &str,
        header: &Value,
        leaf: Option<&str>,
        cursor: &session_log::ReadCursor,
    ) -> Result<(), LogWireError> {
        self.flush(session_id, header, &[], &mut 0, cursor)?;
        let mut state = self.state.lock().expect("log wire lock");
        state.append_pinned(
            &serde_json::json!({"type": "branch_selection", "selectedLeaf": leaf}),
            cursor,
        )
    }

    /// A reopened wire wrapping the log the fold was replayed from. The
    /// header is already durable (row one of the replayed log), and
    /// `last_seq` pins the cursor
    /// past the log's last row.
    pub(crate) fn held(log: SessionLog, directory: &Path, last_seq: u64) -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(WireState {
                log: Some(log),
                disk: None,
                directory: directory.to_owned(),
                header_written: true,
                next_seq: last_seq + 1,
                pending: None,
            })),
        }
    }

    /// THE one durable-write API: this intent's pending header first (an
    /// initial attempt refuses every pre-existing byte), then
    /// `rows`, in order, each pinned at the wire's seq cursor. Every write
    /// starts with the log's tail, adopted from the log's own cursor, so a
    /// landed-but-undecided row is answered instead of repeated, and a
    /// rival's rows are never overwritten.
    ///
    /// # Errors
    ///
    /// [`LogWireError`]: the boundary policy folded into one type. A returned
    /// error leaves the log and the cursor consistent: the committed prefix
    /// is durable, its rows indexed by the store, and an undecided write
    /// stays pending on the wire to resolve at the next flush.
    pub(crate) fn flush(
        &self,
        session_id: &str,
        header_row: &Value,
        rows: &[Value],
        flushed_entries: &mut usize,
        cursor: &session_log::ReadCursor,
    ) -> Result<(), LogWireError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.ensure_open(session_id)?;
        state.refresh_tail()?;
        if !state.header_written {
            // The header is row one: pinned at seq one of a log this wire
            // created.
            state.append_pinned(header_row, cursor)?;
            state.header_written = true;
        }
        for row in rows {
            state.append_pinned(row, cursor)?;
            *flushed_entries += 1;
        }
        Ok(())
    }

    /// Inject the disk the wire's flush opens its log through (the
    /// fault-injection seam: the tests drive the very same code paths).
    #[cfg(test)]
    pub(crate) fn defer_with_disk<D: pa_core::session_log::Disk + 'static>(
        path: &Path,
        disk: std::sync::Arc<D>,
    ) -> Self {
        let wire = Self::fresh(path);
        let state = std::sync::Arc::clone(&wire.state);
        let mut guard = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.disk = Some(disk);
        drop(guard);
        wire
    }
}

/// Fold a decided non-commit outcome into the wire error.
fn wire_error(outcome: Outcome) -> LogWireError {
    match outcome {
        Outcome::Busy { holder, .. } => LogWireError::Busy {
            holder: holder.map_or_else(
                || "another writer".to_owned(),
                |holder| format!("pid {}", holder.pid),
            ),
        },
        Outcome::NotWritten { reason } => {
            LogWireError::Refused(anyhow::anyhow!("session log refused the row: {reason:?}"))
        }
        Outcome::Unknown { .. } | Outcome::Committed { .. } => LogWireError::Refused(
            anyhow::anyhow!("the log decided an append this call did not write"),
        ),
    }
}

/// Open a session's log for the fold's replay: a torn or corrupt tail is
/// repaired first (the report returns so the caller can log it), then the log
/// opens for appends.
pub(crate) fn open_repaired_dir(
    directory: &Path,
    session_id: &str,
) -> Result<(SessionLog, Option<session_log::RepairReport>), LogWireError> {
    let path = directory.join(format!("{session_id}.jsonl"));
    if path.exists() {
        use std::io::BufRead as _;
        let file = std::fs::File::open(&path)?;
        let mut first = Vec::new();
        std::io::BufReader::new(file).read_until(b'\n', &mut first)?;
        if first.last() != Some(&b'\n') {
            return Err(LogWireError::LegacyUnsupported);
        }
        first.pop();
        let row =
            session_log::Row::decode(&first, 0).map_err(|_| LogWireError::LegacyUnsupported)?;
        if row.seq != 1
            || row.body.get("type").and_then(Value::as_str) != Some("session")
            || serde_json::from_value::<pa_types::session::FileEntry>(row.body).is_err()
        {
            return Err(LogWireError::LegacyUnsupported);
        }
    }
    match SessionLog::open(directory, session_id, deadline()) {
        Ok(log) => Ok((log, None)),
        Err(session_log::Error::Damaged { .. }) => {
            let report =
                SessionLog::repair(directory, session_id, deadline()).map_err(open_error)?;
            let log = SessionLog::open(directory, session_id, deadline()).map_err(open_error)?;
            Ok((log, Some(report)))
        }
        Err(error) => Err(open_error(error)),
    }
}

/// Fold an open failure into the wire error.
pub(crate) fn open_error(error: session_log::Error) -> LogWireError {
    match error {
        session_log::Error::Busy { holder, .. } => LogWireError::Busy {
            holder: holder.map_or_else(
                || "another writer".to_owned(),
                |holder| format!("pid {}", holder.pid),
            ),
        },
        session_log::Error::Io(error) => error.into(),
        session_log::Error::Damaged { last_good_seq, .. }
        | session_log::Error::Corrupt {
            after_seq: last_good_seq,
            ..
        } => LogWireError::Refused(anyhow::anyhow!(
            "the session log is unusable after seq {last_good_seq}; repair is the recovery"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::SessionFile;
    use crate::worker::{Worker, WorkerConfig};
    use serde_json::json;
    use std::fs::{File, TryLockError};
    use std::io;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pa-transport-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn fresh_log_session_with(
        dir: &std::path::Path,
        name: &str,
        wire: LogWire,
    ) -> (SessionFile, std::path::PathBuf) {
        let session_path = dir.join(format!("{name}.jsonl"));
        let mut store = SessionFile::create("test", None, 0);
        store.set_path(session_path.clone());
        store.attach_log_wire(wire);
        store.rekey_log_session_id(&session_path).expect("re-key");
        (store, session_path)
    }

    fn fresh_log_session(dir: &std::path::Path, name: &str) -> (SessionFile, std::path::PathBuf) {
        let session_path = dir.join(format!("{name}.jsonl"));
        let mut store = SessionFile::create("test", None, 0);
        store.set_path(session_path.clone());
        store.rekey_log_session_id(&session_path).expect("re-key");
        (store, session_path)
    }

    /// A [`pa_core::session_log::Disk`] whose first few lock attempts are
    /// refused: what a reader scanning the same session file looks like to a
    /// live writer. The refusals are finite, which is the contract a live
    /// holder upholds — it publishes the lock only for one operation.
    struct BusyFirstLocks {
        real: pa_core::session_log::RealDisk,
        refusals: std::sync::atomic::AtomicU64,
    }

    impl pa_core::session_log::Disk for BusyFirstLocks {
        fn open_log(&self, path: &Path) -> io::Result<(File, bool)> {
            self.real.open_log(path)
        }

        fn open_lock(&self, path: &Path) -> io::Result<File> {
            self.real.open_lock(path)
        }

        fn open_rw(&self, path: &Path) -> io::Result<File> {
            self.real.open_rw(path)
        }

        fn open_read(&self, path: &Path) -> io::Result<File> {
            self.real.open_read(path)
        }

        fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
            self.real.write(file, bytes)
        }

        fn fsync(&self, file: &File) -> io::Result<()> {
            self.real.fsync(file)
        }

        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            self.real.sync_dir(dir)
        }

        fn len(&self, file: &File) -> io::Result<u64> {
            self.real.len(file)
        }

        fn truncate(&self, file: &File, len: u64) -> io::Result<()> {
            self.real.truncate(file, len)
        }

        fn try_lock(&self, file: &File) -> io::Result<bool> {
            if self.refusals.fetch_add(1, Ordering::SeqCst) < 3 {
                return Ok(false);
            }
            self.real.try_lock(file)
        }

        fn unlock(&self, file: &File) -> io::Result<()> {
            self.real.unlock(file)
        }
    }

    /// A row written while a reader scans the same session file: the lock is
    /// refused three times and then free, so the row must land instead of the
    /// turn being refused for a holder that was about to release.
    #[test]
    fn a_transient_lock_holder_is_ridden_out_instead_of_refusing_the_row() {
        let dir = temp_dir("busy-first");
        let session_path = dir.join("busy.jsonl");
        let disk = std::sync::Arc::new(BusyFirstLocks {
            real: pa_core::session_log::RealDisk,
            refusals: std::sync::atomic::AtomicU64::new(0),
        });
        let wire = LogWire::defer_with_disk(&session_path, disk);
        let (mut store, _) = fresh_log_session_with(&dir, "busy", wire);
        store
            .persist_entry("custom", json!({ "customType": "probe", "data": {} }))
            .expect("a brief holder must not refuse the row");
    }

    /// A [`pa_core::session_log::Disk`] wrapper that fails exactly one fsync —
    /// a write's durability barrier — while its bytes still land. The failed
    /// barrier's unlock arms the rival: a plain [`File`] takes the log's flock
    /// the moment the append releases it, so the write's own recovery then
    /// finds the lock taken and answers busy. fsync lost, bytes landed, lock
    /// lost — the incident's exact shape, through the production paths.
    struct FailNextFsync {
        real: pa_core::session_log::RealDisk,
        /// The ordinal of the fsync call that must fail; 0 = every call passes.
        fail_on_ordinal: std::sync::atomic::AtomicU64,
        calls: std::sync::atomic::AtomicU64,
        /// Set by the failing fsync; consumed by the next `unlock`, which is
        /// that append's own lock release.
        armed: std::sync::atomic::AtomicBool,
        /// The rival's handles, held so their flocks outlive the append.
        /// [`Self::release_rival`] drops them.
        rivals: std::sync::Mutex<Vec<std::fs::File>>,
        /// The log's lock file, whose fresh open is the rival's file
        /// description.
        lock_path: std::path::PathBuf,
    }

    impl FailNextFsync {
        fn new(lock_path: std::path::PathBuf) -> Self {
            Self {
                real: pa_core::session_log::RealDisk,
                fail_on_ordinal: std::sync::atomic::AtomicU64::new(0),
                calls: std::sync::atomic::AtomicU64::new(0),
                armed: std::sync::atomic::AtomicBool::new(false),
                rivals: std::sync::Mutex::new(Vec::new()),
                lock_path,
            }
        }

        /// The ordinal of the fsync call that must fail.
        fn arm_fsync_failure(&self, ordinal: u64) {
            self.fail_on_ordinal.store(ordinal, Ordering::SeqCst);
        }

        /// Drop the rival's handles, releasing its flocks.
        fn release_rival(&self) {
            self.rivals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
    }

    impl pa_core::session_log::Disk for FailNextFsync {
        fn open_log(&self, path: &Path) -> io::Result<(File, bool)> {
            self.real.open_log(path)
        }

        fn open_lock(&self, path: &Path) -> io::Result<File> {
            self.real.open_lock(path)
        }

        fn open_rw(&self, path: &Path) -> io::Result<File> {
            self.real.open_rw(path)
        }

        fn open_read(&self, path: &Path) -> io::Result<File> {
            self.real.open_read(path)
        }

        fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
            self.real.write(file, bytes)
        }

        fn fsync(&self, file: &File) -> io::Result<()> {
            let ordinal = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let target = self.fail_on_ordinal.load(Ordering::SeqCst);
            if target != 0 && ordinal == target {
                // The bytes land; durability does not.
                self.armed.store(true, Ordering::SeqCst);
                return Err(io::Error::other("fsync lost"));
            }
            self.real.fsync(file)
        }

        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            self.real.sync_dir(dir)
        }

        fn len(&self, file: &File) -> io::Result<u64> {
            self.real.len(file)
        }

        fn truncate(&self, file: &File, len: u64) -> io::Result<()> {
            self.real.truncate(file, len)
        }

        fn try_lock(&self, file: &File) -> io::Result<bool> {
            self.real.try_lock(file)
        }

        fn unlock(&self, file: &File) -> io::Result<()> {
            let result = self.real.unlock(file);
            if self.armed.swap(false, Ordering::SeqCst) {
                // The rival's window: the append just released the lock and
                // its recovery has not looked yet. A fresh open of the same
                // lock file is a fresh file description, so its flock
                // competes.
                let rival = match File::open(&self.lock_path) {
                    Ok(rival) => rival,
                    Err(error) => {
                        panic!("the rival could not open the lock file to take it: {error}")
                    }
                };
                match rival.try_lock() {
                    Ok(()) => self
                        .rivals
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(rival),
                    Err(TryLockError::WouldBlock) => {
                        panic!("the rival could not take the log's lock")
                    }
                    Err(TryLockError::Error(error)) => {
                        panic!("the rival's lock attempt failed: {error}")
                    }
                }
            }
            result
        }
    }

    fn init_git_repo(cwd: &Path) {
        assert!(std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .arg(cwd)
            .output()
            .unwrap()
            .status
            .success());
    }

    #[test]
    fn fresh_publication_captures_git_without_changing_reopened_absent_git() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        let mut fresh = SessionFile::create(cwd.to_str().unwrap(), None, 0);
        let path = dir.path().join("fresh.jsonl");
        fresh.set_path(path.clone());
        assert!(!path.exists());
        assert!(fresh.header.git.is_none());
        // Capture belongs to publication, not the no-I/O staging constructor.
        init_git_repo(&cwd);
        fresh.rewrite().unwrap();
        assert_eq!(
            fresh.header.git.as_ref().unwrap().branch.as_deref(),
            Some("main")
        );
        let reopened = SessionFile::open(&path).unwrap();
        assert_eq!(reopened.header.git, fresh.header.git);
        drop(reopened);

        let mut absent = fresh.header.clone();
        absent.id = "absent".into();
        absent.git = None;
        let mut log = SessionLog::open(dir.path(), "absent", deadline()).unwrap();
        assert!(matches!(
            log.append_first(
                &crate::session_store::session_header_line(&absent),
                deadline()
            ),
            Outcome::Committed { .. }
        ));
        let absent_path = dir.path().join("absent.jsonl");
        let bytes = std::fs::read(&absent_path).unwrap();
        let mut reopened = SessionFile::open(&absent_path).unwrap();
        reopened.rewrite().unwrap();
        assert!(reopened.header.git.is_none());
        assert_eq!(std::fs::read(&absent_path).unwrap(), bytes);
    }

    #[test]
    fn a_fresh_wire_refuses_identical_and_different_rival_headers() {
        for same_header in [true, false] {
            let dir = temp_dir("fresh-rival");
            let (mut store, session_path) = fresh_log_session(&dir, "fresh-rival");
            let own_header = crate::session_store::session_header_line(&store.header);
            let mut rival_header = own_header;
            if !same_header {
                rival_header["cwd"] = json!("different rival cwd");
            }
            std::fs::create_dir_all(&dir).unwrap();
            let mut rival = SessionLog::open(&dir, "fresh-rival", deadline()).unwrap();
            assert!(matches!(
                rival.append_first(&rival_header, deadline()),
                Outcome::Committed { .. }
            ));
            let bytes = std::fs::read(dir.join("fresh-rival.jsonl")).unwrap();
            store
                .persist_entry(
                    "message",
                    json!({"message":{"role":"user","content":"new intent"}}),
                )
                .expect_err("a fresh intent cannot adopt a rival header");
            assert!(store.entries().is_empty());
            assert_eq!(std::fs::read(dir.join("fresh-rival.jsonl")).unwrap(), bytes);
            // Explicit reopen is separate and may continue the existing log.
            let mut reopened = SessionFile::open(&session_path).unwrap();
            reopened
                .persist_entry(
                    "message",
                    json!({"message":{"role":"user","content":"explicit reopen"}}),
                )
                .unwrap();
            assert_eq!(rival.read_since(0).unwrap().len(), 2);
        }
    }

    #[test]
    fn a_fresh_wire_refuses_torn_bytes_without_repairing_them() {
        let dir = temp_dir("fresh-torn");
        let (mut store, _) = fresh_log_session(&dir, "fresh-torn");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fresh-torn.jsonl");
        let bytes = b"{\"seq\":1";
        std::fs::write(&path, bytes).unwrap();
        store
            .rewrite()
            .expect_err("a new intent cannot repair existing bytes");
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn an_unknown_first_header_keeps_its_recovery_and_retries_once() {
        let dir = temp_dir("unknown-first");
        let fake = std::sync::Arc::new(FailNextFsync::new(dir.join("unknown-first.lock")));
        // File creation sync is first; the header barrier is second.
        fake.arm_fsync_failure(2);
        let (mut store, path) = fresh_log_session_with(
            &dir,
            "unknown-first",
            LogWire::defer_with_disk(
                &dir.join("unknown-first.jsonl"),
                std::sync::Arc::clone(&fake),
            ),
        );
        let cwd = dir.join("repo");
        std::fs::create_dir(&cwd).unwrap();
        init_git_repo(&cwd);
        store.header.cwd = cwd.to_str().unwrap().into();
        store
            .rewrite()
            .expect_err("header barrier lost, recovery busy");
        assert_eq!(
            store.header.git.as_ref().unwrap().branch.as_deref(),
            Some("main")
        );
        assert!(std::process::Command::new("git")
            .current_dir(&cwd)
            .args(["symbolic-ref", "HEAD", "refs/heads/changed"])
            .output()
            .unwrap()
            .status
            .success());
        let bytes = std::fs::read(dir.join("unknown-first.jsonl")).unwrap();
        let header =
            session_log::Row::decode(bytes.strip_suffix(b"\n").expect("complete header row"), 0)
                .expect("exactly one validated header envelope");
        assert_eq!(header.seq, 1);
        assert_eq!(header.body["type"], "session");
        assert_eq!(header.body["git"]["branch"], "main");
        fake.release_rival();
        store
            .rewrite()
            .expect("the original header recovery resolves");
        assert_eq!(
            std::fs::read(dir.join("unknown-first.jsonl")).unwrap(),
            bytes
        );
        store
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"after recovery"}}),
            )
            .unwrap();
        let reopened = SessionFile::open(&path).unwrap();
        assert_eq!(reopened.entries().len(), 1);
        let log = SessionLog::open(&dir, "unknown-first", deadline()).unwrap();
        assert_eq!(log.read_since(0).unwrap().len(), 2);
    }

    #[test]
    fn an_undecided_row_retries_into_exactly_one_file_row() {
        let dir = temp_dir("undecided");
        // Ordinal 3: the created log's entry-sync is the first fsync, the
        // header's append the second, this entry row's barrier the third —
        // the one whose failure leaves the write undecided. Its unlock arms
        // the rival, so the recovery answers busy and the wire returns with
        // the write pending; the re-presented row re-pins the same seq and
        // the core answers from the landed bytes.
        let fake = std::sync::Arc::new(FailNextFsync::new(dir.join("undecided.lock")));
        fake.arm_fsync_failure(3);
        let session_path = dir.join("undecided.jsonl");
        let mut store = SessionFile::create("test", None, 0);
        store.set_path(session_path.clone());
        store.attach_log_wire(LogWire::defer_with_disk(
            &session_path,
            std::sync::Arc::clone(&fake),
        ));
        store.rekey_log_session_id(&session_path).expect("re-key");
        let entry = crate::session_store::SessionEntry {
            type_: "message".to_owned(),
            id: "entry-1".to_owned(),
            parent_id: None,
            timestamp: "2026-10-30T00:00:00Z".to_owned(),
            fields: json!({ "message": { "role": "user", "content": "undecided", "timestamp": 1 } }),
        };
        let refused = store
            .persist_built_log_entry(entry.clone())
            .expect_err("undecided");
        assert!(
            format!("{refused:#}").contains("undecided write open"),
            "{refused:#}"
        );
        // The row is not in the fold, but its bytes are in the file: the
        // barrier was lost, not the write.
        assert!(store.entries().is_empty());
        let raw = std::fs::read_to_string(dir.join("undecided.jsonl")).expect("log text");
        assert_eq!(
            raw.lines().count(),
            2,
            "header plus the landed-but-undecided row: {raw}"
        );
        // Re-present the same row after the rival releases: the pending
        // re-pins at its own seq, the core answers from the landed bytes,
        // and no second copy of the row is written anywhere.
        fake.release_rival();
        store
            .persist_built_log_entry(entry.clone())
            .expect("the retry");
        assert_eq!(store.entries().len(), 1, "one index entry");
        assert_eq!(store.entries()[0].id, entry.id);
        let raw = std::fs::read_to_string(dir.join("undecided.jsonl")).expect("log text");
        assert_eq!(raw.lines().count(), 2, "no second copy of the row: {raw}");
        let log = SessionLog::open(&dir, "undecided", deadline()).expect("open");
        let rows = log.read_since(0).expect("read");
        let body = serde_json::to_value(&entry).expect("encode");
        assert_eq!(rows[1].seq, 2);
        assert_eq!(rows[1].body, body);
    }

    #[test]
    fn an_exact_path_refuses_legacy_bytes_and_creates_fresh_log_rows() {
        let dir = temp_dir("exact-path");
        let legacy = dir.join("legacy.jsonl");
        let bytes = b"[]\n";
        std::fs::write(&legacy, bytes).expect("old file");
        assert!(SessionFile::open(&legacy).is_err());
        assert!(SessionFile::open_owned_log(&legacy).is_err());
        assert_eq!(std::fs::read(&legacy).unwrap(), bytes);

        let (mut store, path) = fresh_log_session(&dir, "fresh");
        assert!(!path.exists());
        store.rewrite().expect("fresh header");
        let replay = SessionFile::open(&path).expect("exact selected path");
        assert_eq!(replay.session_id(), "fresh");
        assert!(replay.entries().is_empty());
    }

    #[test]
    fn a_fresh_wires_first_flush_writes_the_header_then_the_rows() {
        let dir = temp_dir("fresh-wire");
        let (mut store, session_path) = fresh_log_session(&dir, "fresh-wire");
        // The log does not exist before the store's first flush.
        assert!(!dir.join("fresh-wire.jsonl").exists());

        let user = store
            .persist_entry(
                "message",
                json!({ "message": { "role": "user", "content": "hi", "timestamp": 1 } }),
            )
            .expect("persist");
        let assistant = store
            .persist_entry("message", json!({ "message": { "role": "assistant", "content": [{ "type": "text", "text": "one" }], "timestamp": 2 } }))
            .expect("persist");

        // The selected session path is the durable log.
        assert!(session_path.exists());
        let log = SessionLog::open(&dir, "fresh-wire", deadline()).expect("reopen the written log");
        let rows = log.read_since(0).expect("read back");
        assert_eq!(rows.len(), 3, "header row plus two entry rows");
        assert_eq!(
            rows[0].body.get("type").and_then(Value::as_str),
            Some("session"),
            "row one is the header"
        );
        assert_eq!(store.entries().len(), 2);
        assert_eq!(store.session_id(), "fresh-wire");
        let _ = (user, assistant);
    }

    #[test]
    fn a_payload_pathed_fresh_session_rekeys_to_its_path_id() {
        let dir = temp_dir("rekey");
        let session_path = dir.join("payload-stem.jsonl");
        // The payload-pathed fresh arm's steps: the exact-path wire, then the
        // re-key to the path's id, then the materialization rewrite.
        let mut created = SessionFile::create("test", None, 0);
        created.set_path(session_path.clone());
        created.rekey_log_session_id(&session_path).expect("re-key");
        created.append_session_state("active");
        created
            .rewrite()
            .expect("the header and state rows are durable");

        let reopened = SessionFile::open(&session_path).expect("reopen");
        assert_eq!(
            reopened.header.id, "payload-stem",
            "the path names the session"
        );
        let types = vec!["session_state".to_owned()];
        assert_eq!(
            reopened
                .entries()
                .iter()
                .map(|e| e.type_.clone())
                .collect::<Vec<_>>(),
            types
        );
    }

    #[test]
    fn a_reopen_replays_the_log_and_flows_new_appends_past_the_cursor() {
        let dir = temp_dir("reopen");
        let (mut store, session_path) = fresh_log_session(&dir, "reopen");
        store
            .persist_entry(
                "message",
                json!({ "message": { "role": "user", "content": "hi", "timestamp": 1 } }),
            )
            .expect("persist");
        store.rewrite().expect("flush");

        drop(store);
        let mut reloaded = SessionFile::open(&session_path).expect("reopen from the log");
        assert_eq!(reloaded.entries().len(), 1);
        assert_eq!(reloaded.header.id, "reopen");
        assert!(reloaded.is_log_served());
        // The reopened store appends past the replayed cursor: only the new
        // row reaches the log.
        let start = reloaded.entries().len();
        reloaded
            .persist_entry(
                "message",
                json!({ "message": { "role": "user", "content": "next", "timestamp": 2 } }),
            )
            .expect("persist");
        reloaded.persist_appended(start).expect("flush the tail");
        let log = SessionLog::open(&dir, "reopen", deadline()).expect("open");
        let rows = log.read_since(0).expect("read");
        assert_eq!(
            rows.len(),
            3,
            "header, replayed user row, appended user row"
        );
        assert_eq!(
            rows[2].body["message"]["content"].as_str(),
            Some("next"),
            "the appended tail lands in order"
        );
    }

    #[test]
    fn a_refused_row_is_durable_nowhere_never_indexed() {
        let dir = temp_dir("refused");
        let blocked = dir.join("blocked");
        let session_path = blocked.join("refused.jsonl");
        // A file where the log directory belongs: the wire's open cannot run,
        // the write refuses, and the entry stays out of the fold.
        std::fs::write(&blocked, "not a directory").expect("blocker file");
        let mut store = SessionFile::create("test", None, 0);
        store.set_path(session_path.clone());
        store.rekey_log_session_id(&session_path).expect("re-key");
        let refused = store.persist_entry(
            "message",
            json!({ "message": { "role": "user", "content": "lost", "timestamp": 1 } }),
        );
        assert!(
            refused.is_err(),
            "the blocked log refuses the write: {refused:?}"
        );
        assert_eq!(
            store.entries().len(),
            0,
            "the refused row never joined the fold"
        );
        assert_eq!(store.leaf_id(), None);
    }

    #[tokio::test]
    async fn the_minimum_serving_path_runs_a_log_session_end_to_end() {
        let dir = temp_dir("e2e");
        let agent_dir = dir.join("agent");
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let session_path = sessions_dir.join("log-e2e.jsonl");
        // Seed the exact selected path before its worker reopens it.
        {
            let (mut store, path) = fresh_log_session(&sessions_dir, "log-e2e");
            assert_eq!(path, session_path);
            store.rewrite().expect("durable header before worker");
            let log = SessionLog::open(&sessions_dir, "log-e2e", deadline()).expect("log");
            assert_eq!(log.read_since(0).expect("read").len(), 1, "header only");
        }
        std::fs::create_dir_all(&agent_dir).expect("agent dir");

        let config = WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "log-e2e".to_string(),
            agent_dir: agent_dir.clone(),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({
                "responses": [
                    { "text": "run the listing", "toolCalls": [ {
                        "toolCallId": "call-1",
                        "toolName": "bash",
                        "args": { "command": "ls" },
                        "result": "listing",
                        "isError": false,
                    } ] },
                    { "text": "steered through" }
                ],
            })),
        };
        let worker = std::sync::Arc::new(Worker::new(config, None));
        let created = worker
            .dispatch(
                "create",
                &json!({
                    "cwd": dir.to_string_lossy(),
                    "sessionDir": sessions_dir.to_string_lossy(),
                    "sessionPath": session_path.to_string_lossy(),
                    "name": "log-e2e",
                }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        {
            let core = worker.core.lock().unwrap();
            let store = core.store.as_ref().expect("created store");
            assert!(store.is_log_served(), "the create reopened the log session");
            assert_eq!(store.header.id, "log-e2e");
        }

        let prompted = worker
            .dispatch(
                "prompt_and_wait",
                &json!({ "activeSessionId": "log-e2e", "message": "run the tool" }),
            )
            .await;
        assert!(prompted.success, "prompt failed: {prompted:?}");

        // The turn's rows: the accepted user row, the assistant row, the
        // tool result row — durable in the log, served from the fold.
        {
            let core = worker.core.lock().unwrap();
            let store = core.store.as_ref().expect("store");
            let types = store
                .entries()
                .iter()
                .map(|e| e.type_.clone())
                .collect::<Vec<_>>();
            assert_eq!(
                store.entries()[4].fields["message"]["content"],
                json!("run the tool"),
                "the turn's accepted user row: {types:?}"
            );
            assert_eq!(
                store.entries()[5].fields["message"]["role"],
                json!("assistant")
            );
            assert_eq!(
                store.entries()[6].fields["message"]["role"],
                json!("toolResult")
            );
            let assistant_id = store.entries()[5].id.clone();
            drop(core);
            // Child usage appends through the same funnel and folds live.
            let mut core = worker.core.lock().unwrap();
            let store = core.store.as_mut().expect("store");
            store
                .append_child_usage_once(
                    "attrib-row",
                    &assistant_id,
                    pa_types::ai::Usage {
                        input: 3,
                        ..Default::default()
                    },
                    None,
                )
                .expect("child usage row");
            assert_eq!(
                store.entries().last().map(|entry| entry.type_.as_str()),
                Some("child_usage_attributed")
            );
        }

        // The attach path serves the same fold (the mirror seed rides the
        // store's entries); the snapshot carries the turn's messages.
        let _ = messages_of(&worker).await;

        // Restart: the fold replays from the log with the same rows, and the
        // old-format session file stays absent (the log is the session).
        drop(worker);
        let reloaded = SessionFile::open(&session_path).expect("restart from the actual log");
        let replayed = reloaded.entries();
        assert_eq!(
            replayed.len(),
            8,
            "prefix, user, assistant, tool result, usage: {replayed:#?}"
        );
        assert_eq!(
            replayed[4].fields["message"]["content"],
            json!("run the tool")
        );
        assert_eq!(replayed[6].fields["message"]["role"], json!("toolResult"));
        assert_eq!(
            replayed[5]
                .fields
                .get("message")
                .and_then(|message| message.get("usage"))
                .and_then(|usage| usage.get("input").and_then(Value::as_u64)),
            Some(123),
            "the replayed assistant folds the attribution's aggregate (the scripted 120 plus the child's 3)"
        );
        assert!(
            session_path.is_file(),
            "the selected path is the durable log"
        );
        assert!(
            !agent_dir.join("session-logs/log-e2e.jsonl").exists(),
            "no parallel default log"
        );
    }

    async fn messages_of(worker: &Worker) -> Vec<Value> {
        let response = worker
            .dispatch("get_messages", &json!({ "activeSessionId": "log-e2e" }))
            .await;
        assert!(response.success, "get_messages failed: {response:?}");
        response.data.expect("messages")["messages"]
            .as_array()
            .expect("messages array")
            .clone()
    }
    #[test]
    fn an_attribution_aggregates_the_same_way_through_the_log_funnel() {
        let dir = temp_dir("probe");
        // The canonical store's arithmetic, then the same arithmetic through
        // the log funnel: the folds must agree (one serving semantic).
        let mut plain = SessionFile::create("test", None, 0);
        plain.set_path(dir.join("plain.jsonl"));
        let assistant = plain
            .persist_entry(
                "message",
                json!({ "message": { "role": "assistant", "content": [], "usage": serde_json::to_value(pa_types::ai::Usage { input: 123, output: 8, ..Default::default() }).expect("usage"), "timestamp": 1 } }),
            )
            .expect("assistant row");
        plain
            .append_child_usage_once(
                "attrib-a",
                &assistant,
                pa_types::ai::Usage {
                    input: 3,
                    ..Default::default()
                },
                None,
            )
            .expect("attribution");
        let folded = plain
            .entry(&assistant)
            .expect("assistant")
            .fields
            .get("message")
            .and_then(|message| message.get("usage"))
            .cloned()
            .expect("folded usage");
        drop(plain);

        let (mut logged, _) = fresh_log_session(&dir, "probe");
        let logged_assistant = logged
            .persist_entry(
                "message",
                json!({ "message": { "role": "assistant", "content": [], "usage": serde_json::to_value(pa_types::ai::Usage { input: 123, output: 8, ..Default::default() }).expect("usage"), "timestamp": 2 } }),
            )
            .expect("assistant row");
        logged
            .append_child_usage_once(
                "attrib-b",
                &logged_assistant,
                pa_types::ai::Usage {
                    input: 3,
                    ..Default::default()
                },
                None,
            )
            .expect("attribution");
        let folded2 = logged
            .entry(&logged_assistant)
            .expect("assistant")
            .fields
            .get("message")
            .and_then(|message| message.get("usage"))
            .cloned()
            .expect("folded usage");
        assert_eq!(folded2, folded);
        let attribution = logged
            .entries()
            .iter()
            .find(|entry| entry.type_ == "child_usage_attributed")
            .expect("attribution row");
        assert_eq!(
            attribution.fields.get("aggregateUsage"),
            Some(&folded2),
            "the row's aggregate matches the served fold"
        );
    }

    #[test]
    fn review_foreign_rows_join_the_serving_fold_before_append() {
        let dir = temp_dir("review-follow");
        let (mut first, path) = fresh_log_session(&dir, "review-follow");
        first
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"seed"}}),
            )
            .unwrap();
        let mut second = SessionFile::open(&path).unwrap();
        let foreign = first
            .persist_entry(
                "message",
                json!({"message":{"role":"assistant","content":"foreign"}}),
            )
            .unwrap();
        second
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"after foreign"}}),
            )
            .unwrap();
        let durable = SessionFile::open(&path).unwrap();
        assert_eq!(
            serde_json::to_value(second.entries()).unwrap(),
            serde_json::to_value(durable.entries()).unwrap(),
            "a successful append must serve the full durable fold, including {foreign}"
        );
    }

    #[test]
    fn review_public_retry_does_not_duplicate_an_unknown_row() {
        let dir = temp_dir("review-public-retry");
        let fake = std::sync::Arc::new(FailNextFsync::new(dir.join("review-public-retry.lock")));
        fake.arm_fsync_failure(3);
        let (mut store, path) = fresh_log_session_with(
            &dir,
            "review-public-retry",
            LogWire::defer_with_disk(
                &dir.join("review-public-retry.jsonl"),
                std::sync::Arc::clone(&fake),
            ),
        );
        let fields = json!({"message":{"role":"user","content":"same user intent"}});
        store
            .persist_entry("message", fields.clone())
            .expect_err("unknown barrier");
        fake.release_rival();
        store.persist_entry("message", fields).expect("retry");
        let replay = SessionFile::open(&path).unwrap();
        assert_eq!(
            replay.entries().len(),
            1,
            "public retry must not mint another durable entry"
        );
        assert_eq!(
            serde_json::to_value(store.entries()).unwrap(),
            serde_json::to_value(replay.entries()).unwrap()
        );
    }

    #[test]
    fn review_external_repair_cannot_leave_removed_rows_in_serving_fold() {
        let dir = temp_dir("review-repair");
        let (mut store, path) = fresh_log_session(&dir, "review-repair");
        store
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"survivor"}}),
            )
            .unwrap();
        store
            .persist_entry(
                "message",
                json!({"message":{"role":"assistant","content":"lost in repair"}}),
            )
            .unwrap();
        let log_path = dir.join("review-repair.jsonl");
        let mut bytes = std::fs::read(&log_path).unwrap();
        let last_line = bytes
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'\n')
            .nth(1)
            .unwrap()
            .0
            + 1;
        bytes[last_line] = b'!';
        std::fs::write(&log_path, bytes).unwrap();
        let report = SessionLog::repair(&dir, "review-repair", deadline()).unwrap();
        assert_eq!(report.last_good_seq, 2);
        let refusal = store
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"after repair"}}),
            )
            .expect_err("stale live fold requires reopen");
        assert!(format!("{refusal:#}").contains("reopen"));
        let mut store = SessionFile::open(&path).unwrap();
        store
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"after repair"}}),
            )
            .unwrap();
        let replay = SessionFile::open(&path).unwrap();
        assert_eq!(
            serde_json::to_value(store.entries()).unwrap(),
            serde_json::to_value(replay.entries()).unwrap(),
            "repair must reject or rebuild a stale live fold"
        );
    }

    #[test]
    fn review_selected_branch_survives_sessionfile_reopen() {
        let dir = temp_dir("review-selection");
        let (mut store, path) = fresh_log_session(&dir, "review-selection");
        let root = store
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"root"}}),
            )
            .unwrap();
        store
            .persist_entry(
                "message",
                json!({"message":{"role":"assistant","content":"tip"}}),
            )
            .unwrap();
        store.branch_to(Some(&root)).unwrap();
        let replay = SessionFile::open(&path).unwrap();
        assert_eq!(
            store.leaf_id(),
            replay.leaf_id(),
            "successful navigation must be durable without a following message"
        );
    }

    #[test]
    fn review_new_intent_resolves_the_previous_unknown_before_minting() {
        let dir = temp_dir("review-next-intent");
        let fake = std::sync::Arc::new(FailNextFsync::new(dir.join("review-next-intent.lock")));
        fake.arm_fsync_failure(3);
        let (mut store, path) = fresh_log_session_with(
            &dir,
            "review-next-intent",
            LogWire::defer_with_disk(
                &dir.join("review-next-intent.jsonl"),
                std::sync::Arc::clone(&fake),
            ),
        );
        store
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"unknown"}}),
            )
            .expect_err("unknown barrier");
        fake.release_rival();
        store
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"new intent"}}),
            )
            .unwrap();
        let replay = SessionFile::open(&path).unwrap();
        assert_eq!(
            serde_json::to_value(store.entries()).unwrap(),
            serde_json::to_value(replay.entries()).unwrap()
        );
        assert_eq!(store.entries().len(), 2);
        assert_eq!(
            store.entries()[1].parent_id.as_deref(),
            Some(store.entries()[0].id.as_str())
        );
    }

    #[test]
    fn review_cloned_folds_observe_independent_positions() {
        let dir = temp_dir("review-clone");
        let (mut first, path) = fresh_log_session(&dir, "review-clone");
        first
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"root"}}),
            )
            .unwrap();
        let mut second = first.clone();
        first
            .persist_entry(
                "message",
                json!({"message":{"role":"assistant","content":"first"}}),
            )
            .unwrap();
        second.observe_log_tail().unwrap();
        second
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"second"}}),
            )
            .unwrap();
        first.observe_log_tail().unwrap();
        let replay = SessionFile::open(&path).unwrap();
        assert_eq!(
            serde_json::to_value(first.entries()).unwrap(),
            serde_json::to_value(replay.entries()).unwrap()
        );
        assert_eq!(
            serde_json::to_value(second.entries()).unwrap(),
            serde_json::to_value(replay.entries()).unwrap()
        );
    }
}
