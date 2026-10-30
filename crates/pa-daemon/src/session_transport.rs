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

    fn fresh_log_session(dir: &std::path::Path, name: &str) -> (SessionFile, std::path::PathBuf) {
        let session_path = dir.join(format!("{name}.jsonl"));
        let mut store = SessionFile::create("test", None, 0);
        store.set_path(session_path.clone());
        store.attach_log_wire(LogWire::deferred(&dir));
        store.rekey_log_session_id(&session_path).expect("re-key");
        (store, session_path)
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

    // ------------------------------------------------------------------
    // Verifier additions (verify-wiring @ 406e74539). These are the tests
    // verify-rip §5 lists as missing; see reports/verify-wiring.md for the
    // verdict each one supports.
    // ------------------------------------------------------------------

    /// T-C / W-3: a rival that created the log first must not cost the
    /// deferred wire's first flush a second header row.
    #[test]
    fn a_rival_created_log_keeps_exactly_one_header_row() {
        let dir = temp_dir("rival-header");
        let session_path = dir.join("rival-header.jsonl");
        // A rival writer creates the log, makes its own header durable, then
        // releases it. It names the SAME session: one session served twice,
        // not two sessions.
        let rival_header = {
            let rival = SessionFile::create("test", None, 0);
            let mut line = crate::session_store::session_header_line(&rival.header);
            line["id"] = json!("rival-header");
            line
        };
        {
            std::fs::create_dir_all(log_dir(&dir)).expect("log dir");
            let mut rival_log =
                SessionLog::open(&log_dir(&dir), "rival-header", deadline()).expect("rival open");
            assert!(matches!(
                rival_log.append(&rival_header, deadline()),
                Outcome::Committed { .. }
            ));
        }

        // The deferred wire's first flush opens the rival's existing, clean,
        // unlocked log and writes its own header before its rows.
        let (mut store, _) = fresh_log_session(&dir, "rival-header");
        store
            .persist_entry(
                "message",
                json!({ "message": { "role": "user", "content": "hi", "timestamp": 1 } }),
            )
            .expect("persist");

        let log = SessionLog::open(&log_dir(&dir), "rival-header", deadline()).expect("open");
        let rows = log.read_since(0).expect("read");
        let types: Vec<Value> = rows
            .iter()
            .map(|row| row.body.get("type").cloned().unwrap_or(Value::Null))
            .collect();
        let headers = types
            .iter()
            .filter(|type_| type_.as_str() == Some("session"))
            .count();
        assert_eq!(
            headers, 1,
            "the log must hold exactly one session header row; rows: {types:?}"
        );
        assert_eq!(rows[0].body.get("type").and_then(Value::as_str), Some("session"));
        drop(log);

        // The fold must not carry a second header as an entry either.
        let reopened = SessionFile::open_log(&session_path, &dir).expect("reopen");
        let folded = reopened
            .entries()
            .iter()
            .map(|entry| entry.type_.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            folded.iter().filter(|type_| *type_ == "session").count(),
            0,
            "a duplicate header joined the fold: {folded:?}"
        );
    }

    /// T-B / W-1, executed: a flush whose fsync fails leaves the row in the
    /// file and out of the cursor. The retry of that row must resolve to the
    /// row the file holds, not append a second one.
    ///
    /// Needs the fsync fault injector (LD_PRELOAD); skipped when it is absent:
    /// `PA_W1_ARM_FILE` names a file whose existence fails fsync for paths
    /// containing "w1fault". pa-daemon itself has no seam for this (see
    /// reports/verify-wiring.md section 5.3).
    #[test]
    fn an_undecided_flush_row_is_not_appended_twice() {
        let Ok(arm) = std::env::var("PA_W1_ARM_FILE") else {
            eprintln!("skipped: no fsync fault injector (PA_W1_ARM_FILE unset)");
            return;
        };
        let dir = temp_dir("w1fault");
        let session_id = format!(
            "w1fault-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        // The log path carries "w1fault", which is what the injector matches.
        let header = {
            let store = SessionFile::create("test", None, 0);
            let mut line = crate::session_store::session_header_line(&store.header);
            line["id"] = serde_json::json!(session_id);
            line
        };
        let row = |id: &str, text: &str| {
            serde_json::json!({
                "type": "message",
                "id": id,
                "parentId": serde_json::Value::Null,
                "timestamp": "2026-10-30T00:00:00.000Z",
                "message": { "role": "user", "content": text, "timestamp": 1 }
            })
        };
        let wire = LogWire::deferred(&dir);
        wire.flush(&session_id, &header, std::slice::from_ref(&row("row-a", "one")))
            .expect("the first flush commits");

        // Arm the injector: the write lands, its fsync fails, and the recovery
        // cannot decide either (its own fsync fails the same way).
        std::fs::write(&arm, b"armed").expect("arm");
        let undecided = wire.flush(&session_id, &header, std::slice::from_ref(&row("row-b", "two")));
        std::fs::remove_file(&arm).expect("disarm");
        assert!(
            undecided.is_err(),
            "an undecided append must be refused: {undecided:?}"
        );

        // The row is in the log even though the wire never decided it.
        let log = SessionLog::open(&log_dir(&dir), &session_id, deadline()).expect("open");
        let held = log
            .read_since(0)
            .expect("read")
            .into_iter()
            .filter(|held| held.body.get("id").and_then(Value::as_str) == Some("row-b"))
            .count();
        assert_eq!(held, 1, "the undecided row reached the file");
        drop(log);

        // The retry of the same row: ONE row, one seq.
        wire.flush(&session_id, &header, std::slice::from_ref(&row("row-b", "two")))
            .expect("the retry resolves");
        let log = SessionLog::open(&log_dir(&dir), &session_id, deadline()).expect("open");
        let rows = log.read_since(0).expect("read");
        let ids: Vec<Value> = rows
            .iter()
            .map(|held| held.body.get("id").cloned().unwrap_or(Value::Null))
            .collect();
        assert_eq!(
            ids.iter()
                .filter(|id| id.as_str() == Some("row-b"))
                .count(),
            1,
            "one caller intent, one row: {ids:?}"
        );
    }

    /// F3 (characterization): the daemon fold cannot see the runtime's
    /// `branch_selection` row, so a selection made through the runtime is
    /// invisible to it and its leaf is the last indexed entry.
    #[test]
    fn the_daemon_fold_skips_the_runtime_selection_row() {
        let dir = temp_dir("selection-gap");
        let session_path = dir.join("selection-gap.jsonl");
        let (mut store, _) = fresh_log_session(&dir, "selection-gap");
        store
            .persist_entry(
                "message",
                json!({ "message": { "role": "user", "content": "hi", "timestamp": 1 } }),
            )
            .expect("user row");
        let answer = store
            .persist_entry(
                "message",
                json!({ "message": { "role": "assistant", "content": [{ "type": "text", "text": "done" }], "timestamp": 2 } }),
            )
            .expect("assistant row");
        drop(store);

        // The row `SessionRuntime::select` writes (same shape, fixed id).
        let marker = json!({
            "type": "branch_selection",
            "id": "selection-1",
            "parentId": answer,
            "leafId": answer,
        });
        {
            let mut log =
                SessionLog::open(&log_dir(&dir), "selection-gap", deadline()).expect("open");
            assert!(matches!(
                log.append(&marker, deadline()),
                Outcome::Committed { .. }
            ));
        }

        let fold = SessionFile::open_log(&session_path, &dir).expect("daemon fold reopen");
        assert!(
            fold.entry("selection-1").is_none(),
            "the selection row is not a SessionEntry (it has no timestamp)"
        );
        assert_eq!(fold.skipped_lines, 1, "the fold counted it as unusable");
        assert_eq!(
            fold.leaf_id(),
            Some(answer.as_str()),
            "the fold's leaf is the last indexed entry, not the selected one"
        );
    }

    /// T-E isolation: the three rows WITHOUT the transport's header row project
    /// correctly, so the header row is what blanks the runtime's projection.
    #[test]
    fn the_header_row_is_what_blanks_the_runtime_projection() {
        use pa_core::session_runtime::SessionRuntime;

        let dir = temp_dir("header-slot");
        std::fs::create_dir_all(log_dir(&dir)).expect("log dir");
        {
            let mut log =
                SessionLog::open(&log_dir(&dir), "header-slot", deadline()).expect("open the log");
            for (id, parent, role, text) in [
                ("root", None, "user", "hi"),
                ("answer", Some("root"), "assistant", "done"),
            ] {
                let row = json!({
                    "type": "message",
                    "id": id,
                    "parentId": parent,
                    "timestamp": "2026-10-30T00:00:00.000Z",
                    "message": { "role": role, "content": [{ "type": "text", "text": text }] }
                });
                assert!(matches!(
                    log.append(&row, deadline()),
                    Outcome::Committed { .. }
                ));
            }
        }
        let opened = SessionRuntime::open(&log_dir(&dir), "header-slot", deadline())
            .expect("runtime open");
        assert_eq!(
            opened.runtime.projection().len(),
            2,
            "without a header row the fold selects the conversation tip"
        );
    }

    /// T-E (transport level): a flushed turn replays through the runtime fold
    /// with the same projection, and a branch selection made through that fold
    /// is a row in the same log.
    #[test]
    fn a_flushed_turn_replays_through_the_runtime_fold() {
        use pa_core::session_runtime::{ProviderMessage, SessionRuntime};

        let dir = temp_dir("runtime-replay");
        let session_path = dir.join("runtime-replay.jsonl");
        let (mut store, _) = fresh_log_session(&dir, "runtime-replay");
        let user = store
            .persist_entry(
                "message",
                json!({ "message": { "role": "user", "content": "hi", "timestamp": 1 } }),
            )
            .expect("user row");
        let answer = store
            .persist_entry(
                "message",
                json!({ "message": { "role": "assistant", "content": [{ "type": "text", "text": "done" }], "timestamp": 2 } }),
            )
            .expect("assistant row");
        let tool = store
            .persist_entry(
                "message",
                json!({ "message": { "role": "toolResult", "content": [{ "type": "text", "text": "listing" }], "timestamp": 3 } }),
            )
            .expect("tool row");
        // The serving store holds the log's flock; the runtime takes it next.
        drop(store);

        let opened = SessionRuntime::open(&log_dir(&dir), "runtime-replay", deadline())
            .expect("runtime open");
        let mut runtime = opened.runtime;
        let turn = vec![
            ProviderMessage {
                role: "user".to_owned(),
                content: "hi".to_owned(),
            },
            ProviderMessage {
                role: "assistant".to_owned(),
                content: "done".to_owned(),
            },
            ProviderMessage {
                role: "toolResult".to_owned(),
                content: "listing".to_owned(),
            },
        ];
        assert_eq!(
            runtime.projection(),
            turn,
            "the flushed turn must project in log order; the fold holds {} rows and selected leaf {:?}",
            runtime.rows().len(),
            runtime.leaf()
        );
        assert_eq!(runtime.leaf(), Some(tool.as_str()));

        // A second answer forking from the user row: the tip branch keeps the
        // first turn selected until a selection row says otherwise.
        let alternative = json!({
            "type": "message",
            "id": "alternative-answer",
            "parentId": user,
            "timestamp": "2026-10-30T00:00:04.000Z",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "other" }], "timestamp": 4 }
        });
        assert!(matches!(
            runtime.append(&alternative, deadline()).expect("append"),
            Outcome::Committed { .. }
        ));
        assert_eq!(
            runtime.projection(),
            turn,
            "a sibling row does not steal the selection"
        );

        // Selecting the fork's leaf switches the projection, and the selection
        // is a row: it must survive a reopen.
        runtime
            .select("alternative-answer", deadline())
            .expect("select the fork");
        assert_eq!(
            runtime.projection(),
            vec![
                ProviderMessage {
                    role: "user".to_owned(),
                    content: "hi".to_owned(),
                },
                ProviderMessage {
                    role: "assistant".to_owned(),
                    content: "other".to_owned(),
                },
            ]
        );
        runtime.select(&tool, deadline()).expect("select the turn");
        assert_eq!(runtime.projection(), turn);
        drop(runtime);

        let reopened = SessionRuntime::open(&log_dir(&dir), "runtime-replay", deadline())
            .expect("reopen");
        assert_eq!(
            reopened.runtime.projection(),
            turn,
            "the selection row replays: the same branch is selected after a reopen"
        );
        assert_eq!(reopened.runtime.leaf(), Some(tool.as_str()));
        drop(reopened);

        // The daemon's own fold opens the same log and keeps the turn.
        let fold = SessionFile::open_log(&session_path, &dir).expect("daemon fold reopen");
        assert!(fold.entry(&answer).is_some(), "the turn's rows are in the log");
        // Characterization, not an endorsement (port-order gap): the daemon
        // fold does not know the runtime's `branch_selection` row, so it skips
        // both selection rows and its leaf is the last indexed entry rather
        // than the selected one.
        assert_eq!(
            fold.leaf_id(),
            Some("alternative-answer"),
            "the daemon fold and the runtime fold disagree on the selected leaf"
        );
    }
}
