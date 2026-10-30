//! THE transport switch for the session-log cutover (`SESSIONS-LOG-SPEC.md`):
//! the one point that decides whether a session rides the session-log core
//! ([`SessionTransport::SessionLog`]) or is refused as a pre-port old-format
//! file ([`SessionTransport::LegacyUnsupported`]).
//!
//! Resolution rule ([`resolve`]): a session that already has a log file is a
//! log session; a fresh session starts one; an old-format session file is
//! kept on disk but never read and never migrated (the spec: report old
//! files instead of converting them), so the caller surfaces the typed
//! refusal. There is no transport override: every served session uses the
//! log.
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

/// Which transport opens a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionTransport {
    /// A pre-port old-format session file: kept on disk, never read and
    /// never migrated. The caller surfaces this as the typed refusal.
    LegacyUnsupported,
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
        return SessionTransport::LegacyUnsupported;
    };
    if log_file(agent_dir, &session_id).exists() {
        return SessionTransport::SessionLog;
    }
    // A pre-port old-format file is not read and not migrated: the caller
    // surfaces the verdict as the typed refusal.
    if session_path.exists() {
        return SessionTransport::LegacyUnsupported;
    }
    SessionTransport::SessionLog
}

/// Does this session's log file exist yet? The create arms' reopen decision:
/// an existing log reopens, the absence (with no old-format file either)
/// means the session is fresh.
pub(crate) fn log_exists(agent_dir: &Path, session_path: &Path) -> bool {
    session_id_of(session_path).is_some_and(|id| log_file(agent_dir, &id).exists())
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

/// The wire's shared state. The unmaterialized-row cursor lives here because
/// a store clone carries the same wire: one log gets exactly one tail cursor.
#[derive(Debug)]
struct WireState {
    /// `None` until the first flush; a refused open retries at the next call.
    log: Option<SessionLog>,
    agent_dir: PathBuf,
    /// The header row is durable in the log. A fresh (deferred) wire writes
    /// it as row one; a held (reopened) wire replays it from the fold's open.
    header_written: bool,
    /// How many of the fold's entry rows are durable in the log, in fold
    /// order. The store flushes only `entries.len() - flushed` at its tail.
    flushed_entries: usize,
}

/// Resolve one decided append outcome; `flushed` grows only with committed
/// rows, and an `Unknown` verdict is asked of the core's recovery before the
/// caller sees anything.
fn commit_one(flushed: &mut usize, outcome: Outcome) -> Result<(), LogWireError> {
    match outcome {
        Outcome::Committed { .. } => {
            *flushed += 1;
            Ok(())
        }
        Outcome::Unknown { recover } => match recover.recover(deadline()) {
            Ok(Outcome::Committed { .. }) => {
                *flushed += 1;
                Ok(())
            }
            Ok(Outcome::NotWritten { reason }) => Err(LogWireError::Refused(anyhow::anyhow!(
                "the log resolved the append as not written: {reason:?}"
            ))),
            Ok(Outcome::Busy { .. } | Outcome::Unknown { .. }) | Err(_) => {
                Err(LogWireError::Refused(anyhow::anyhow!(
                    "the log could not decide its undecided write"
                )))
            }
        },
        outcome => Err(wire_error(outcome)),
    }
}

impl LogWire {
    /// A fresh wire: the log opens (creating file, lock, and row one) at the
    /// first flush.
    pub(crate) fn deferred(agent_dir: &Path) -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(WireState {
                log: None,
                agent_dir: agent_dir.to_path_buf(),
                header_written: false,
                flushed_entries: 0,
            })),
        }
    }

    /// A reopened wire wrapping the log the fold was replayed from. The
    /// header is already durable (row one of the replayed log), and the
    /// replayed entry count is the wire's tail cursor.
    pub(crate) fn held(log: SessionLog, agent_dir: &Path, replayed_entries: usize) -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(WireState {
                log: Some(log),
                agent_dir: agent_dir.to_path_buf(),
                header_written: true,
                flushed_entries: replayed_entries,
            })),
        }
    }

    /// THE one durable-write API: the pending header row first (exactly once
    /// per wire), then `rows`, in order.
    ///
    /// # Errors
    ///
    /// [`LogWireError`]: the boundary policy folded into one type. A returned
    /// error leaves the log and the cursor consistent: the committed prefix
    /// is durable, the rest was never written.
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
        // The open resolves first, then appends run one at a time, so the
        // state's borrow never overlaps itself.
        if state.log.is_none() {
            std::fs::create_dir_all(log_dir(&state.agent_dir)).map_err(LogWireError::Io)?;
            let log = SessionLog::open(&log_dir(&state.agent_dir), session_id, deadline())
                .map_err(open_error)?;
            state.log = Some(log);
        }
        if !state.header_written {
            let outcome = state
                .log
                .as_mut()
                .expect("the open above filled the slot")
                .append(header_row, deadline());
            commit_one(&mut state.flushed_entries, outcome)?;
            state.header_written = true;
        }
        for row in rows {
            let outcome = state
                .log
                .as_mut()
                .expect("the open above filled the slot")
                .append(row, deadline());
            commit_one(&mut state.flushed_entries, outcome)?;
        }
        Ok(())
    }

    /// How many entry rows the log already holds. The store's unmaterialized
    /// tail is `entries.len() - flushed`.
    pub(crate) fn flushed_entries(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flushed_entries
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

/// The fixture-facing log read: every row's body except the header's.
///
/// The seeding side must DROP its `SessionLog` before a serving open: the
/// log's flock is the writer's mutual exclusion, so a seeded file left open
/// refuses the worker.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A session's log row bodies (without the header row), read back
    /// through the core.
    #[must_use]
    pub(crate) fn log_row_bodies(agent_dir: &Path, session_id: &str) -> Vec<Value> {
        let log =
            SessionLog::open(&log_dir(agent_dir), session_id, deadline()).expect("open the log");
        log.read_since(0)
            .expect("read the log")
            .into_iter()
            .skip(1)
            .map(|row| row.body)
            .collect()
    }

    /// Seed a session's log from fixture text: one row per non-blank line.
    /// A line that parses as JSON lands as its value (the fold replays it as
    /// an entry, or skips a malformed one, the way the old loader did); a
    /// line that does not parse lands as a string row the fold skips.
    pub(crate) fn seed_log_from_text(agent_dir: &Path, session_id: &str, content: &str) {
        std::fs::create_dir_all(log_dir(agent_dir)).expect("log dir");
        let mut log = SessionLog::open(&log_dir(agent_dir), session_id, deadline())
            .expect("open the log for seeding");
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            let row = serde_json::from_str::<Value>(line)
                .unwrap_or(Value::String(line.to_owned()));
            let outcome = log.append(&row, deadline());
            assert!(
                matches!(outcome, Outcome::Committed { .. }),
                "seeding refused a row: {outcome:?}"
            );
        }
    }

    /// Whether a serving open of the log is refused (a worker holds it).
    #[must_use]
    pub(crate) fn log_open_refused(agent_dir: &Path, session_id: &str) -> bool {
        std::fs::create_dir_all(log_dir(agent_dir)).expect("log dir");
        SessionLog::open(&log_dir(agent_dir), session_id, deadline()).is_err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::SessionFile;
    use crate::worker::{Worker, WorkerConfig};
    use serde_json::json;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pa-transport-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn fresh_log_session(dir: &std::path::Path, name: &str) -> (SessionFile, std::path::PathBuf) {
        let session_path = dir.join(format!("{name}.jsonl"));
        let mut store = SessionFile::create("test", None, 0);
        store.set_path(session_path.clone());
        store.attach_log_wire(LogWire::deferred(&dir));
        store.rekey_log_session_id(&session_path).expect("re-key");
        (store, session_path)
    }

    #[test]
    fn the_switch_serves_every_fresh_session_on_the_log_and_refuses_legacy_files() {
        let dir = temp_dir("switch");
        // A fresh session with no files: the log transport, no override.
        let path = dir.join("fresh.jsonl");
        assert_eq!(resolve(&dir, &path), SessionTransport::SessionLog);

        // An old-format session file: not read and not migrated, typed.
        std::fs::write(&path, "[]\n").expect("old file");
        assert_eq!(
            resolve(&dir, &path),
            SessionTransport::LegacyUnsupported
        );
        std::fs::remove_file(&path).expect("clean");

        // An existing log: the log transport.
        let log_session = dir.join("logged.jsonl");
        let mut store = SessionFile::create("test", None, 0);
        store.set_path(log_session.clone());
        store.attach_log_wire(LogWire::deferred(&dir));
        store.rekey_log_session_id(&log_session).expect("re-key");
        store.rewrite().expect("flush");
        assert!(log_file(&dir, "logged").exists());
        assert_eq!(resolve(&dir, &log_session), SessionTransport::SessionLog);
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
        // The log session pre-exists its worker: the create below resolves
        // to the log transport through the log itself, so the serving run
        // exercises the reopen.
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
