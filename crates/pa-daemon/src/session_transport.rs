//! THE transport switch for the session-log wiring (`SESSIONS-LOG-SPEC.md`,
//! port-plan §3): the one point that decides whether a session is served by
//! the old canonical paths ([`SessionTransport::Canonical`], compiled until
//! the final deletion step) or by the session-log core
//! ([`SessionTransport::SessionLog`]).
//!
//! Resolution rule ([`resolve`]): a session that already has a log file is a
//! log session; an old-format session file stays canonical (the log core
//! cannot read that format, and the spec reports old files instead of
//! converting them); a fresh session takes the log only when the override
//! environment asks for it, so the default behavior of every existing flow is
//! unchanged. The override selects between the two transports until the
//! cutover flips the default; the final deletion step removes the canonical
//! half together with this switch.
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

/// The override that serves fresh sessions on the log transport. Absent, or
/// any other value, keeps the canonical default.
pub(crate) const TRANSPORT_OVERRIDE_ENV: &str = "PA_SESSION_TRANSPORT";
pub(crate) const TRANSPORT_OVERRIDE_VALUE: &str = "session-log";

/// One open/repair's budget at this transport. The daemon's own append paths
/// run under the session's admission deadlines; the blocking open/repair here
/// takes this bound so a wedged storage device reports instead of stalling a
/// create. The core never waits; this is the caller's one clock.
const LOG_OPEN_DEADLINE: Duration = Duration::from_secs(10);

/// Which transport serves a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionTransport {
    /// The pre-port canonical paths (the compiled default until cutover).
    Canonical,
    /// The session-log core: the log is the source of truth, the daemon
    /// store is its fold.
    SessionLog,
}

/// The log directory: one append-only log file per session id.
pub(crate) fn log_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("session-logs")
}

/// One session's log file path.
pub(crate) fn log_file(agent_dir: &Path, session_id: &str) -> PathBuf {
    log_dir(agent_dir).join(format!("{session_id}.jsonl"))
}

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

/// THE switch point. `session_path` is the session's file path (the daemon's
/// advertised identity).
pub(crate) fn resolve(agent_dir: &Path, session_path: &Path) -> SessionTransport {
    let Some(session_id) = session_id_of(session_path) else {
        return SessionTransport::Canonical;
    };
    if log_file(agent_dir, &session_id).exists() {
        return SessionTransport::SessionLog;
    }
    // An existing old-format session keeps serving through the canonical path.
    if session_path.exists() {
        return SessionTransport::Canonical;
    }
    if new_transport_requested() {
        return SessionTransport::SessionLog;
    }
    SessionTransport::Canonical
}

/// Whether the switch maps this session to the log.
pub(crate) fn transports_session_log(agent_dir: &Path, session_path: &Path) -> bool {
    resolve(agent_dir, session_path) == SessionTransport::SessionLog
}

/// The override's request, read once per resolve.
pub(crate) fn new_transport_requested() -> bool {
    match std::env::var(TRANSPORT_OVERRIDE_ENV) {
        Ok(override_value) => override_value == TRANSPORT_OVERRIDE_VALUE,
        Err(_) => false,
    }
}

/// Why one wire step failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LogWireError {
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
    agent_dir: PathBuf,
    /// Row one of the log is a session header. A fresh (deferred) wire writes
    /// it as row one; a wire whose open found a pre-existing log adopts the
    /// header it finds at row one instead of writing a second one.
    header_written: bool,
    /// How many of the fold's entry rows are durable in the log, in fold
    /// order. The store flushes only `entries.len() - flushed` at its tail;
    /// the funnel's per-row write commits then indexes, so this moves with
    /// the store's count, never ahead of it.
    flushed_entries: usize,
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
}

impl std::fmt::Debug for WireState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WireState")
            .field("has_log", &self.log.is_some())
            .field("has_disk", &self.disk.is_some())
            .field("header_written", &self.header_written)
            .field("flushed_entries", &self.flushed_entries)
            .field("next_seq", &self.next_seq)
            .field("pending", &self.pending)
            .finish()
    }
}

impl WireState {
    /// Resolve the log this wire writes through, exactly once per wire
    /// object: create the log when absent; for a log that already holds
    /// rows, adopt row one when it is a session header (no wire writes a
    /// second header) and take the seq cursor from the log's last row.
    fn ensure_open(&mut self, session_id: &str) -> Result<(), LogWireError> {
        if self.log.is_some() {
            return Ok(());
        }
        std::fs::create_dir_all(log_dir(&self.agent_dir)).map_err(LogWireError::Io)?;
        let log = match self.disk.clone() {
            Some(disk) => {
                SessionLog::open_with(disk, &log_dir(&self.agent_dir), session_id, deadline())
                    .map_err(open_error)?
            }
            None => SessionLog::open(&log_dir(&self.agent_dir), session_id, deadline())
                .map_err(open_error)?,
        };
        // One read decides the adoption and sets the seq cursor both, so the
        // wire never pins a seq the log already named.
        let rows = log.read_since(0).map_err(|error| {
            LogWireError::Refused(
                anyhow::Error::from(open_error(error))
                    .context("read the session log at its open"),
            )
        })?;
        if let Some(row) = rows.first() {
            if row.body.get("type").and_then(Value::as_str) == Some("session") {
                self.header_written = true;
                self.flushed_entries = rows.len().saturating_sub(1);
            }
        }
        if let Some(last) = rows.last() {
            self.next_seq = last.seq + 1;
        }
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
        let log = self
            .log
            .as_mut()
            .expect("the caller ran ensure_open first");
        let rows = log.refresh(deadline()).map_err(open_error)?;
        if let Some(last) = rows.last() {
            self.next_seq = last.seq + 1;
        }
        Ok(())
    }

    /// Append `row`, pinned, and resolve its outcome against the log before
    /// anything the caller sees. The rule (the runtime's pending solution):
    /// the wire pins every row and only a committed row advances the
    /// cursor; an undecided write keeps its seq and bytes pending so any
    /// retry re-pins them and the core reports the landed row instead of
    /// repeating it; a seq that provably holds a foreign row re-syncs and
    /// re-pins the caller's row once.
    fn append_pinned(&mut self, row: &Value, is_entry: bool) -> Result<(), LogWireError> {
        // At most the first attempt plus one re-pin after a provable
        // conflict: beyond that the log keeps moving under the wire and a
        // reopen must settle it.
        for _attempt in 0..2 {
            let log = self
                .log
                .as_mut()
                .expect("the caller ran ensure_open first");
            let seq = self
                .pending
                .as_ref()
                .filter(|pending| pending.row == *row)
                .map(|pending| pending.seq)
                .unwrap_or(self.next_seq);
            let outcome = log.append_at(seq, row, deadline());
            match outcome {
                Outcome::Committed { .. } => {
                    // A committed row may be an earlier pending's landed
                    // bytes re-presented while younger rows already moved
                    // the cursor; the cursor only ever advances.
                    self.next_seq = self.next_seq.max(seq + 1);
                    // The caller indexes the entry it just saw committed.
                    if is_entry {
                        self.flushed_entries += 1;
                    }
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
                    });
                    match recover.recover(deadline()) {
                        Ok(Outcome::Committed { .. }) => {
                            // The row is the file's row (the core matched the
                            // body); the caller indexes it.
                            self.next_seq = self.next_seq.max(seq + 1);
                            if is_entry {
                                self.flushed_entries += 1;
                            }
                            self.pending = None;
                            return Ok(());
                        }
                        // The log's verdict: this row's bytes never took the
                        // pinned seq. Sync the tail's cursor; the loop's next
                        // attempt re-pins.
                        Ok(Outcome::NotWritten { .. }) => {
                            self.pending = None;
                            self.refresh_tail()?;
                        }
                        // An undecided recovery (Busy/error): the seq stays
                        // pending; nothing else of this flush runs.
                        _ => {
                            return Err(LogWireError::Refused(anyhow::anyhow!(
                                "the session log left its undecided write open; retry the flush"
                            )))
                        }
                    }
                }
                Outcome::NotWritten {
                    reason: pa_core::session_log::NotWritten::SeqConflict { .. },
                } => {
                    // The pinned seq holds someone else's row, or none at
                    // all. The core's own cursor is already current (every
                    // append adopts the tail first), so `refresh` may have
                    // nothing new to report; read the rows from this
                    // wire's stale point onward instead and take the
                    // cursor from their end. The pinned seq's pending
                    // stays; a re-present of its bytes still resolves
                    // through the core's body match.
                    {
                        let log = self
                            .log
                            .as_mut()
                            .expect("the caller ran ensure_open first");
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
    /// A fresh wire: the log opens (creating file, lock, and row one) at the
    /// first flush.
    pub(crate) fn deferred(agent_dir: &Path) -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(WireState {
                log: None,
                disk: None,
                agent_dir: agent_dir.to_path_buf(),
                header_written: false,
                flushed_entries: 0,
                next_seq: 1,
                pending: None,
            })),
        }
    }

    /// A reopened wire wrapping the log the fold was replayed from. The
    /// header is already durable (row one of the replayed log), `replayed`
    /// counts the fold's durable entry rows, and `last_seq` pins the cursor
    /// past the log's last row.
    pub(crate) fn held(log: SessionLog, agent_dir: &Path, replayed: usize, last_seq: u64) -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(WireState {
                log: Some(log),
                disk: None,
                agent_dir: agent_dir.to_path_buf(),
                header_written: true,
                flushed_entries: replayed,
                next_seq: last_seq + 1,
                pending: None,
            })),
        }
    }

    /// THE one durable-write API: the pending header row first (exactly once
    /// per wire; a pre-existing log's header is adopted at the open), then
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
            state.append_pinned(header_row, false)?;
            state.header_written = true;
        }
        for row in rows {
            state.append_pinned(row, true)?;
        }
        Ok(())
    }

    /// How many entry rows the log already holds, per the wire's cursor.
    pub(crate) fn flushed_entries(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flushed_entries
    }

    /// Inject the disk the wire's flush opens its log through (the
    /// fault-injection seam: the tests drive the very same code paths).
    #[cfg(test)]
    pub(crate) fn defer_with_disk<D: pa_core::session_log::Disk + 'static>(
        agent_dir: &Path,
        disk: std::sync::Arc<D>,
    ) -> Self {
        let mut wire = Self::deferred(agent_dir);
        let state = std::sync::Arc::clone(&wire.state);
        let mut guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.disk = Some(disk);
        let _ = guard;
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
pub(crate) fn open_repaired(
    agent_dir: &Path,
    session_id: &str,
) -> Result<(SessionLog, Option<session_log::RepairReport>), LogWireError> {
    match SessionLog::open(&log_dir(agent_dir), session_id, deadline()) {
        Ok(log) => Ok((log, None)),
        Err(session_log::Error::Damaged { .. }) => {
            let report = SessionLog::repair(&log_dir(agent_dir), session_id, deadline())
                .map_err(open_error)?;
            let log = SessionLog::open(&log_dir(agent_dir), session_id, deadline())
                .map_err(open_error)?;
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
    use std::io::{self, Write};
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

    /// The env tests mutate process state; they serialize on this lock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_override<T>(run: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = std::env::var(TRANSPORT_OVERRIDE_ENV).ok();
        std::env::set_var(TRANSPORT_OVERRIDE_ENV, TRANSPORT_OVERRIDE_VALUE);
        let result = run();
        match saved {
            Some(saved) => std::env::set_var(TRANSPORT_OVERRIDE_ENV, saved),
            None => std::env::remove_var(TRANSPORT_OVERRIDE_ENV),
        }
        result
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
        fresh_log_session_with(dir, name, LogWire::deferred(dir))
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
                    Err(std::fs::TryLockError::WouldBlock) => {
                        panic!("the rival could not take the log's lock")
                    }
                    Err(std::fs::TryLockError::Error(error)) => {
                        panic!("the rival's lock attempt failed: {error}")
                    }
                }
            }
            result
        }
    }

    #[test]
    fn a_rival_created_log_is_adopted_not_re_headed() {
        let dir = temp_dir("rival-header");
        // The rival creates the log, writes row one (a session header this
        // wire did not write), and releases its handle.
        {
            let mut log =
                SessionLog::open(&log_dir(&dir), "rival-header", deadline()).expect("rival open");
            assert!(
                matches!(
                    log.append(&json!({ "type": "session", "id": "rival-header" }), deadline()),
                    Outcome::Committed { .. }
                ),
                "the rival's header row commits"
            );
        }
        let (mut store, session_path) = fresh_log_session(&dir, "rival-header");
        // The wire's flush adopts row one and writes no header of its own.
        store
            .persist_entry(
                "message",
                json!({ "message": { "role": "user", "content": "hi", "timestamp": 1 } }),
            )
            .expect("persist");
        let log = SessionLog::open(&log_dir(&dir), "rival-header", deadline()).expect("reopen");
        let rows = log.read_since(0).expect("read");
        assert_eq!(rows.len(), 2, "the rival's header plus the entry row only");
        assert_eq!(rows[0].body["type"].as_str(), Some("session"));
        // The header is metadata, never a conversation root: the served fold
        // is unaffected. One entry live; one entry on the replay; and if the
        // header ever leaked into the fold, both counts would be two.
        assert_eq!(store.entries().len(), 1);
        drop(store);
        let replay = SessionFile::open_log(&session_path, &dir).expect("replay");
        assert_eq!(replay.entries().len(), 1);
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
        let fake = std::sync::Arc::new(FailNextFsync::new(
            log_dir(&dir).join("undecided.lock"),
        ));
        fake.arm_fsync_failure(3);
        let session_path = dir.join("undecided.jsonl");
        let mut store = SessionFile::create("test", None, 0);
        store.set_path(session_path.clone());
        store.attach_log_wire(LogWire::defer_with_disk(
            &dir,
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
        assert!(refused.to_string().contains("undecided write open"), "{refused:#}");
        // The row is not in the fold, but its bytes are in the file: the
        // barrier was lost, not the write.
        assert!(store.entries().is_empty());
        let raw = std::fs::read_to_string(log_file(&dir, "undecided")).expect("log text");
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
        let raw = std::fs::read_to_string(log_file(&dir, "undecided")).expect("log text");
        assert_eq!(raw.lines().count(), 2, "no second copy of the row: {raw}");
        let log = SessionLog::open(&log_dir(&dir), "undecided", deadline()).expect("open");
        let rows = log.read_since(0).expect("read");
        let body = serde_json::to_value(&entry).expect("encode");
        assert_eq!(rows[1].seq, 2);
        assert_eq!(rows[1].body, body);
    }

    #[test]
    fn the_switch_serves_a_log_session_and_refuses_to_map_an_old_file() {
        let dir = temp_dir("switch");
        // A fresh session with no files and no override: the canonical default.
        let path = dir.join("fresh.jsonl");
        assert!(!transports_session_log(&dir, &path));

        // An old-format session file: the log core cannot read that format,
        // so it keeps serving canonically, override or not.
        std::fs::write(&path, "[]\n").expect("old file");
        assert!(!transports_session_log(&dir, &path));
        assert_eq!(with_override(|| transports_session_log(&dir, &path)), false);
        std::fs::remove_file(&path).expect("clean");

        // An existing log: the log transport, override or not.
        let log_session = dir.join("logged.jsonl");
        let mut store = SessionFile::create("test", None, 0);
        store.set_path(log_session.clone());
        store.attach_log_wire(LogWire::deferred(&dir));
        store.rekey_log_session_id(&log_session).expect("re-key");
        store.rewrite().expect("flush");
        assert!(log_file(&dir, "logged").exists());
        assert_eq!(
            with_override(|| transports_session_log(&dir, &log_session)),
            true
        );
        assert!(transports_session_log(&dir, &log_session));
    }

    #[test]
    fn a_fresh_wires_first_flush_writes_the_header_then_the_rows() {
        let dir = temp_dir("fresh-wire");
        let (mut store, session_path) = fresh_log_session(&dir, "fresh-wire");
        // The log does not exist before the store's first flush.
        assert!(!log_file(&dir, "fresh-wire").exists());

        let user = store
            .persist_entry(
                "message",
                json!({ "message": { "role": "user", "content": "hi", "timestamp": 1 } }),
            )
            .expect("persist");
        let assistant = store
            .persist_entry("message", json!({ "message": { "role": "assistant", "content": [{ "type": "text", "text": "one" }], "timestamp": 2 } }))
            .expect("persist");

        // The rows are durable in the log, and the old file was never written.
        assert!(log_file(&dir, "fresh-wire").exists());
        assert!(!session_path.exists());
        let log = SessionLog::open(&log_dir(&dir), "fresh-wire", deadline())
            .expect("reopen the written log");
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
        // The payload-pathed fresh arm's steps: the deferred wire, then the
        // re-key to the path's id, then the materialization rewrite.
        let mut created = SessionFile::create("test", None, 0);
        created.set_path(session_path.clone());
        created.attach_log_wire(LogWire::deferred(&dir));
        created.rekey_log_session_id(&session_path).expect("re-key");
        created.append_session_state("active");
        created
            .rewrite()
            .expect("the header and state rows are durable");

        let reopened = SessionFile::open_log(&session_path, &dir).expect("reopen");
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
        let mut reloaded = SessionFile::open_log(&session_path, &dir).expect("reopen from the log");
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
        let log = SessionLog::open(&log_dir(&dir), "reopen", deadline()).expect("open");
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
        let session_path = dir.join("refused.jsonl");
        // A file where the log directory belongs: the wire's open cannot run,
        // the write refuses, and the entry stays out of the fold.
        std::fs::write(log_dir(&dir), "not a directory").expect("blocker file");
        let mut store = SessionFile::create("test", None, 0);
        store.set_path(session_path.clone());
        store.attach_log_wire(LogWire::deferred(&dir));
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
        // The log session pre-exists its worker: the create below resolves to
        // the log transport through the log itself (the resolve() test covers
        // the override's fresh path), so the serving run exercises the reopen.
        {
            let (store, _) = fresh_log_session(&dir, "log-e2e");
            store
                .rewrite()
                .expect("the header row is durable before the worker");
            let log = SessionLog::open(&log_dir(&dir), "log-e2e", deadline()).expect("log");
            assert_eq!(log.read_since(0).expect("read").len(), 1, "header only");
        }
        std::fs::rename(log_file(&dir, "log-e2e"), dir.join("staged-log.jsonl"))
            .expect("move the seeded log");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::create_dir_all(log_dir(&agent_dir)).expect("log dir");
        std::fs::rename(
            dir.join("staged-log.jsonl"),
            log_file(&agent_dir, "log-e2e"),
        )
        .expect("seed the log at the worker's agent dir");

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
            assert!(
                store.entries().last().map(|e| e.type_.clone())
                    == Some("child_usage_attributed".to_string())
            );
        }

        // The attach path serves the same fold (the mirror seed rides the
        // store's entries); the snapshot carries the turn's messages.
        let _ = messages_of(&worker).await;

        // Restart: the fold replays from the log with the same rows, and the
        // old-format session file stays absent (the log is the session).
        drop(worker);
        let reloaded =
            SessionFile::open_log(&session_path, &agent_dir).expect("restart from the log");
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
                .map(|usage| usage.get("input").and_then(Value::as_u64))
                .flatten(),
            Some(123),
            "the replayed assistant folds the attribution's aggregate (the scripted 120 plus the child's 3)"
        );
        assert!(!session_path.exists(), "the canonical write path never ran");
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
}
