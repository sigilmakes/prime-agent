//! Child of `agent_engine::context_selection`; no process-global provider or env seams.
use super::*;
use crate::agent_engine::{AgentEngineConfig, AgentSessionEngine};
use crate::engine::{BranchSummaryOutcome, BranchSummaryRequest, SessionEngine};
use pa_core::session_engine::ContextStartupBoundary;
use pa_types::session::{AgentMessage, EntryBase, FileEntry, SessionInfoEntry};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const ACK_ERROR: &str = "context-selection-test-owned-ack-refused";
const PROVIDER: &str = "context-selection-owned-loopback";
const MODEL: &str = "ack-gate-model";

fn entry_base(id: &str, parent: Option<&str>) -> EntryBase {
    EntryBase {
        id: Some(id.to_owned()),
        parent_id: parent.map(str::to_owned),
        timestamp: Some("2026-01-01T00:00:00.000Z".to_owned()),
        rest: pa_types::JsonMap::new(),
    }
}

fn user_entry(id: &str, parent: Option<&str>, text: &str) -> FileEntry {
    FileEntry::Message {
        message: AgentMessage::User(pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text(text.to_owned()),
            timestamp: 0,
            rest: pa_types::JsonMap::new(),
        }),
        base: entry_base(id, parent),
    }
}

fn metadata_entry(id: &str, parent: Option<&str>) -> FileEntry {
    FileEntry::SessionInfo {
        payload: SessionInfoEntry { name: None },
        base: entry_base(id, parent),
    }
}

/// This is deliberately MOCK authority, not a canonical `SessionLease` owner.
/// The real mirror still reads and verifies the immutable JSONL fixture.
struct RefusingSelectionStore {
    source: ContextSelectionSource,
    captures: AtomicUsize,
    commits: Arc<AtomicUsize>,
    selections: Arc<Mutex<Vec<ContextSelection>>>,
}

impl InitialContextSelectionStore for RefusingSelectionStore {
    fn capture(&self, expected_file: &Path) -> anyhow::Result<ContextSelectionSource> {
        anyhow::ensure!(expected_file == self.source.path, "unexpected fixture path");
        self.captures.fetch_add(1, Ordering::SeqCst);
        Ok(self.source.clone())
    }

    fn commit(
        &self,
        source: ContextSelectionSource,
        selection: ContextSelection,
    ) -> ContextSelectionCommitFuture {
        assert_eq!(source.header, self.source.header);
        assert_eq!(source.path, self.source.path);
        assert!(Weak::ptr_eq(&source.lease, &self.source.lease));
        let commits = Arc::clone(&self.commits);
        let selections = Arc::clone(&self.selections);
        Box::pin(async move {
            commits.fetch_add(1, Ordering::SeqCst);
            selections.lock().unwrap().push(selection);
            anyhow::bail!(ACK_ERROR)
        })
    }
}

/// Owns both the listener thread and its stop/wakeup path. It answers every
/// accidental model request with a terminal SSE response, including when a
/// regression skips `ensure_core_session` entirely. No 30-second black hole.
struct OwnedModelServer {
    address: SocketAddr,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl OwnedModelServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while let Ok((mut socket, _)) = listener.accept() {
                if thread_stop.load(Ordering::SeqCst) {
                    break; // The owned wakeup is not a model request.
                }
                socket
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_millis(200)))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut request = Vec::new();
                let mut counted = false;
                let mut buffer = [0_u8; 8192];
                while !thread_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                    match socket.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            if !counted {
                                thread_requests.fetch_add(1, Ordering::SeqCst);
                                counted = true;
                            }
                            request.extend_from_slice(&buffer[..n]);
                            if request.len() > 1024 * 1024 {
                                break;
                            }
                            if let Some(head_end) =
                                request.windows(4).position(|w| w == b"\r\n\r\n")
                            {
                                let head = String::from_utf8_lossy(&request[..head_end]);
                                let content_length = head
                                    .lines()
                                    .find_map(|line| {
                                        let (name, value) = line.split_once(':')?;
                                        name.eq_ignore_ascii_case("content-length")
                                            .then(|| value.trim().parse::<usize>().ok())
                                            .flatten()
                                    })
                                    .unwrap_or(0);
                                if request.len() >= head_end + 4 + content_length {
                                    break;
                                }
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => break,
                    }
                }
                if counted {
                    let body = concat!(
                        "data: {\"id\":\"owned\",\"object\":\"chat.completion.chunk\",\"model\":\"ack-gate-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Owned regression response\"},\"finish_reason\":null}]}\n\n",
                        "data: {\"id\":\"owned\",\"object\":\"chat.completion.chunk\",\"model\":\"ack-gate-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: [DONE]\n\n"
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes());
                    let _ = socket.flush();
                }
            }
        });
        Self {
            address,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for OwnedModelServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // A real connection wakes the blocking accept; all socket I/O is bounded.
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
        if let Some(thread) = self.thread.take() {
            let joined = thread.join();
            if !std::thread::panicking() {
                joined.expect("owned model server thread");
            }
        }
    }
}

#[test]
fn branch_summary_ack_failure_reaches_no_model() {
    let dir = tempfile::tempdir().unwrap(); // TempDir is private and RAII-owned.
    let agent_dir = dir.path().join("agent");
    let cwd = dir.path().join("project");
    let session_dir = dir.path().join("sessions");
    for path in [&agent_dir, &cwd, &session_dir] {
        std::fs::create_dir_all(path).unwrap();
    }
    let server = OwnedModelServer::start();
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::json!({ "providers": { (PROVIDER): {
            "api": "openai-completions", "baseUrl": server.base_url(),
            // A literal dummy for the transport, not a credential or env edit.
            "apiKey": "none",
            "models": [{ "id": MODEL, "name": "Owned gate test model",
                "api": "openai-completions", "reasoning": false,
                "contextWindow": 128_000, "maxTokens": 4096 }]
        } } })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::json!({
            "harnessRefinements": false, "enableBuiltinSkills": false,
            "prewarmIpythonKernel": false, "contextStrategy": "default_context/rolling"
        })
        .to_string(),
    )
    .unwrap();
    let header = SessionHeader {
        version: Some(3),
        id: "owned-ack-fixture".to_owned(),
        timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
        parent_session: None,
        rlm_depth: Some(0),
        git: None,
        rest: pa_types::JsonMap::new(),
    };
    let entries = vec![user_entry("u1", None, "Summarize this fixture branch")];
    let session_file = session_dir.join("owned-ack-fixture.jsonl");
    let mut fixture = serde_json::to_string(&FileEntry::Header {
        header: header.clone(),
    })
    .unwrap();
    fixture.push('\n');
    for entry in &entries {
        fixture.push_str(&serde_json::to_string(entry).unwrap());
        fixture.push('\n');
    }
    std::fs::write(&session_file, fixture.as_bytes()).unwrap();
    let commits = Arc::new(AtomicUsize::new(0));
    let selections = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RefusingSelectionStore {
        source: ContextSelectionSource {
            header,
            path: session_file.clone(),
            lease: Weak::new(),
        },
        captures: AtomicUsize::new(0),
        commits: Arc::clone(&commits),
        selections: Arc::clone(&selections),
    });
    let engine = AgentSessionEngine::new(AgentEngineConfig {
        cwd,
        agent_dir,
        provider: Some(PROVIDER.to_owned()),
        model: Some(MODEL.to_owned()),
        api_key: None,
        thinking: None,
        session_dir: Some(session_dir),
        session_file: Some(session_file.clone()),
        faux_script: None,
        supervisor_link: None,
        telemetry_disabled: Some(true),
        cron_store: None,
        queued_steering_probe: None,
    })
    .unwrap();
    engine.set_initial_context_selection_store(store.clone());
    let abort = pa_agent::abort::AbortController::new();
    // Exercise the actual public daemon contract, not build_session_mirror directly.
    let outcome = engine.run_branch_summary(
        BranchSummaryRequest {
            entries,
            custom_instructions: None,
            replace_instructions: false,
        },
        &abort.signal(),
    );
    assert_eq!(
        outcome,
        BranchSummaryOutcome::Failed {
            error: ACK_ERROR.to_owned()
        }
    );
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(store.captures.load(Ordering::SeqCst), 2);
    assert_eq!(
        *selections.lock().unwrap(),
        vec![ContextSelection {
            version: 1,
            strategy: "default_context/rolling".to_owned(),
        }]
    );
    assert_eq!(
        server.request_count(),
        0,
        "ack failure reached the owned HTTP model"
    );
    assert!(
        engine.session.blocking_lock().is_none(),
        "failed build published a session"
    );
    assert_eq!(std::fs::read(&session_file).unwrap(), fixture.as_bytes());
    drop(engine);
    drop(server); // Always stops, wakes, and joins its owned server thread.
}

#[test]
fn admitted_current_has_empty_proof_without_traversing_suffix() {
    // Current is intentionally not a graph validator; SDK owns prefix validation.
    let entries = vec![user_entry("cycle", Some("cycle"), "body")];
    assert!(
        admitted_user_ids(&entries, Some("cycle"), &ContextStartupBoundary::Current)
            .unwrap()
            .is_empty()
    );
    assert!(
        admitted_user_ids(&[], None, &ContextStartupBoundary::Current)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn admitted_empty_returns_exact_fifo_users_on_selected_branch() {
    let entries = vec![
        user_entry("u2", Some("m1"), "second"),
        user_entry("sibling", Some("u1"), "not selected"),
        metadata_entry("m1", Some("u1")),
        user_entry("u1", None, "first"),
        metadata_entry("tip", Some("u2")),
    ];
    assert_eq!(
        admitted_user_ids(&entries, Some("tip"), &ContextStartupBoundary::Empty).unwrap(),
        vec!["u1", "u2"]
    );
    assert!(admitted_user_ids(&[], None, &ContextStartupBoundary::Empty)
        .unwrap()
        .is_empty());
}

#[test]
fn admitted_leaf_excludes_boundary_and_unselected_sibling() {
    let entries = vec![
        user_entry("old", None, "older"),
        user_entry("boundary", Some("old"), "prefix"),
        user_entry("sibling", Some("boundary"), "other branch"),
        user_entry("new", Some("boundary"), "suffix"),
        metadata_entry("tip", Some("new")),
    ];
    let boundary = ContextStartupBoundary::Leaf("boundary".to_owned());
    assert_eq!(
        admitted_user_ids(&entries, Some("tip"), &boundary).unwrap(),
        vec!["new"]
    );
    assert!(admitted_user_ids(&entries, Some("boundary"), &boundary)
        .unwrap()
        .is_empty());
    let error = admitted_user_ids(
        &entries,
        Some("tip"),
        &ContextStartupBoundary::Leaf("sibling".to_owned()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("boundary is absent"), "{error}");
}

#[test]
fn admitted_missing_entry_fails_closed() {
    let entries = vec![user_entry("tip", Some("missing"), "body")];
    let error =
        admitted_user_ids(&entries, Some("tip"), &ContextStartupBoundary::Empty).unwrap_err();
    assert!(
        error.to_string().contains("missing entry missing"),
        "{error}"
    );
    assert!(admitted_user_ids(&[], Some("missing"), &ContextStartupBoundary::Empty).is_err());
}

#[test]
fn admitted_missing_leaf_boundary_fails_closed() {
    let entries = vec![user_entry("tip", None, "body")];
    let error = admitted_user_ids(
        &entries,
        Some("tip"),
        &ContextStartupBoundary::Leaf("absent".to_owned()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("boundary is absent"), "{error}");
    assert!(admitted_user_ids(
        &[],
        None,
        &ContextStartupBoundary::Leaf("absent".to_owned())
    )
    .is_err());
}

#[test]
fn admitted_cycle_fails_closed() {
    let entries = vec![
        user_entry("a", Some("b"), "one"),
        user_entry("b", Some("a"), "two"),
    ];
    let error = admitted_user_ids(&entries, Some("b"), &ContextStartupBoundary::Empty).unwrap_err();
    assert!(error.to_string().contains("contains a cycle"), "{error}");
}

#[test]
fn admitted_same_body_distinct_ids_are_preserved() {
    let entries = vec![
        user_entry("first", None, "same body"),
        user_entry("second", Some("first"), "same body"),
    ];
    assert_eq!(
        admitted_user_ids(&entries, Some("second"), &ContextStartupBoundary::Empty).unwrap(),
        vec!["first", "second"]
    );
}
