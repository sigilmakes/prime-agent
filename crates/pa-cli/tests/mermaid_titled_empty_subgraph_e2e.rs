// large_futures: stack-resident futures on hot paths by design.
// too_many_lines: style gate, not correctness. Casts: 64-bit targets;
// narrowing sits at bounded OS/protocol boundaries.
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! End-to-end verifier for the titled-empty-subgraph mermaid poison: an
//! assistant flowchart carrying `subgraph X["label"] end` (no member nodes)
//! used to kill the whole TUI at the first paint before the fix (mmdflux
//! 2.6.1, `nesting.rs:269` `HashMap` index: "no entry found for key"). The
//! real supervisor answers with the poisoned fence through the scripted
//! faux engine and the headless TUI attaches afterwards: the populated
//! clusters draw, the empty one titles nowhere, and the process keeps
//! taking turns.
#![cfg(unix)]

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use pa_types::daemon::DaemonCommand;
use serde_json::{json, Value};

/// The assistant fence for the poisoning turn: one populated titled
/// subgraph, an edge into it (the layered engine runs), and one titled
/// subgraph with no member nodes. The fixed `LayoutGraph::from_digraph` lays
/// the empty compound out with no member ranks, so the drawn art omits it;
/// the unfixed source panicked the paint.
fn poison_fence() -> String {
    [
        "Cluster status for the deployment:",
        "",
        "```mermaid",
        "flowchart LR",
        "    subgraph OSC[\"on the cluster\"]",
        "        K3S[k3s] --> W1[worker a]",
        "    end",
        "    subgraph ONWK[\"on the workers\"]",
        "    end",
        "    NG[nix go] --> K3S",
        "```",
        "",
    ]
    .join("\n")
}

const FOLLOW_UP_REPLY: &str = "the process is still alive after the poisoned paint";

/// The `{"engine": "faux"}` script: the poisoning first turn, then a plain
/// second turn.
fn poison_engine(dir: &Path) -> PathBuf {
    let script = dir.join("script.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "responses": [
            { "text": poison_fence(), "delayMs": 20 },
            { "text": FOLLOW_UP_REPLY },
        ] })
        .to_string(),
    )
    .expect("write script");
    script
}

struct Supervisor {
    child: Child,
    socket: PathBuf,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        graceful_shutdown(&self.socket);
        let worker_pids = child_pids_of(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
        for pid in worker_pids {
            kill_worker(pid);
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn kill_worker(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "worker {pid} survived the teardown kill"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn child_pids_of(ppid: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    let entries = std::fs::read_dir("/proc").expect("read /proc");
    for entry in entries.flatten() {
        let Ok(entry_pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{entry_pid}/stat")) else {
            continue;
        };
        // `comm` can contain spaces and parens, so parse after the last ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        fields.next();
        let Ok(parent) = fields.next().unwrap_or_default().parse::<u32>() else {
            continue;
        };
        if parent == ppid {
            pids.push(entry_pid);
        }
    }
    pids
}

/// Liveness that ignores zombies (reparented children nobody reaps).
fn process_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    let state = rest.split_whitespace().next().unwrap_or_default();
    !state.starts_with('Z') && !state.starts_with('X')
}

fn graceful_shutdown(socket: &Path) {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;

    let Ok(stream) = UnixStream::connect(socket) else {
        return;
    };
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut writer = write_half;
    let mut hello = String::new();
    let _ = reader.read_line(&mut hello);
    let command = json!({
        "type": "command",
        "id": "test-shutdown",
        "protocol": { "name": "prime-agent.daemon", "version": 7 },
        "command": { "type": "shutdown" },
    });
    let mut line = serde_json::to_string(&command).expect("serialize");
    line.push('\n');
    if writer.write_all(line.as_bytes()).is_err() {
        return;
    }
    let _ = writer.flush();
    let _ = reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(5)));
    let mut response = String::new();
    let _ = reader.read_line(&mut response);
}

/// Spawn the real supervisor over a hermetic agent dir, with no ambient
/// credentials.
#[allow(clippy::zombie_processes)]
fn spawn_supervisor(dir: &Path) -> Supervisor {
    let socket = dir.join("daemon.sock");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(&socket)
        .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
        .env("PI_OFFLINE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The launcher strips inherited worker role env vars before spawning the
    // supervisor; a CLI running inside a daemon worker must not leak them.
    for var in [
        pa_daemon::worker::WORKER_ROLE_ENV,
        pa_daemon::worker::WORKER_TOKEN_ENV,
        pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV,
        pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV,
        pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV,
        pa_daemon::worker::WORKER_SOCKET_ENV,
        pa_daemon::worker::WORKER_INSTANCE_ID_ENV,
        pa_daemon::worker::WORKER_SCRIPT_ENV,
    ] {
        command.env_remove(var);
    }
    // Ambient provider credentials must not leak into the daemon's catalog:
    // every spawned supervisor here serves models.json fixtures only.
    for provider in pa_ai::models_generated::get_providers() {
        if let Some(vars) = pa_ai::env_api_keys::get_api_key_env_vars(provider) {
            for var in vars {
                command.env_remove(var);
            }
        }
    }
    command.env_remove("PRIME_TEAM_ID");
    command.env_remove("PRIME_API_KEY");
    // A supervisor killed by a failing test must not leak its session
    // workers: the supervisor-lost exit runs on this short window.
    command.env(
        pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
        "15000",
    );
    // A full workspace run can starve a session worker's boot far past the
    // 30s default connect budget; this keeps session creates deterministic.
    command.env("PA_DAEMON_WORKER_CONNECT_TIMEOUT_MS", "90000");
    let child = command.spawn().expect("spawn prime-agent --mode daemon");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if socket.exists() {
            return Supervisor { child, socket };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("supervisor socket never appeared");
}

/// The empty `PromptInput` (the struct carries no `Default`).
fn empty_prompt_input() -> pa_types::daemon::PromptInput {
    pa_types::daemon::PromptInput {
        content: None,
        images: None,
        streaming_behavior: None,
        queue_if_busy: None,
        expand_prompt_templates: None,
        source: None,
        agent_message_id: None,
        custom_message: None,
        queue_key: None,
        prefix_messages: None,
        admission_id: None,
        rlm_notice_nonce: None,
    }
}

/// Create a live session through the daemon protocol over `script`.
async fn create_session_with(socket: &Path, dir: &Path, script: &Path) -> String {
    let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(socket)
        .await
        .expect("connect supervisor");
    let data = client
        .request_ok(DaemonCommand::Create {
            id: None,
            session_path: None,
            continue_recent: None,
            no_session: None,
            name: None,
            config: Some(json!({
                "cwd": dir.display().to_string(),
                "sessionDir": dir.join("agent").join("sessions").display().to_string(),
                "script": script.display().to_string(),
            })),
            telemetry_disabled: Some(true),
            runtime_metadata: None,
            lifecycle: None,
            env: None,
            launch_env: None,
            rest: serde_json::Map::default(),
        })
        .await
        .expect("create session");
    client.close();
    data.get("activeSessionId")
        .or_else(|| data.get("id"))
        .and_then(Value::as_str)
        .expect("session id")
        .to_string()
}

/// The headless run's interactive options (the verifier seam: no TTY, the
/// same attach/submit/stream/render path).
fn headless_options(
    socket: &Path,
    dir: &Path,
    script: &Path,
) -> pa_tui::interactive::InteractiveOptions {
    pa_tui::interactive::InteractiveOptions {
        models: None,
        socket_path: socket.to_path_buf(),
        cwd: dir.to_path_buf(),
        session_dir: Some(dir.join("agent").join("sessions")),
        script_path: Some(script.to_path_buf()),
        model_selection: pa_tui::interactive::ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: pa_tui::interactive::SessionSelection::New,
        show_images: true,
        client_settings: None,
        fullscreen_mouse: true,
        initial_message: None,
        theme: "prime".to_string(),
        code_block_indent: "  ".to_string(),
        tree_filter_mode: String::new(),
        branch_summary_skip_prompt: false,
        version: "0.0.0".to_string(),
        onboarding: None,
        client_auth: None,
        provider_auth: None,
        update_commands: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        session_rlm_depth: None,
        prompt_stash: std::sync::Arc::default(),
        session_has_children: false,
        restore_dock_focus: false,
    }
}

/// Drive the poisoning turn through the daemon, then attach the headless TUI
/// to the settled session. The pre-fix renderer panicked the TUI process on
/// the attach paint; the fixed one paints the surviving clusters, omits the
/// empty one titled `on the workers`, and keeps taking turns.
#[tokio::test]
async fn attach_survives_a_titled_empty_subgraph_diagram() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    std::fs::create_dir_all(dir.path().join("agent").join("sessions")).expect("session dir");
    let supervisor = spawn_supervisor(dir.path());
    let script = poison_engine(dir.path());
    let session = create_session_with(&supervisor.socket, dir.path(), &script).await;

    // Run the poisoning turn out-of-band: the faux engine settles the
    // flowchart response, the transcript persists, and only then does the
    // TUI attach (the report's in-session e2e shape).
    {
        let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(&supervisor.socket)
            .await
            .expect("connect supervisor");
        client
            .request_ok(DaemonCommand::Prompt {
                id: None,
                active_session_id: session.clone(),
                message: "show the cluster status".to_string(),
                input: empty_prompt_input(),
                rest: serde_json::Map::default(),
            })
            .await
            .expect("start the poisoning turn");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let data = client
                .request_ok(DaemonCommand::GetLastAssistantText {
                    id: None,
                    active_session_id: session.clone(),
                    rest: serde_json::Map::default(),
                })
                .await
                .expect("get_last_assistant_text");
            let text = data["text"].as_str().unwrap_or_default();
            if text.contains("flowchart LR") {
                assert_eq!(text, poison_fence(), "the poisoning turn settled verbatim");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the poisoning turn never settled; last text: {text:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        client.close();
    }

    // Attach the poisoned session. The draw fits this plan canvas (the art
    // is 5 rows over ~65 columns), so the block draws instead of staying on
    // the fenced-source fallback.
    let plan = pa_tui::interactive::HeadlessPlan {
        steps: vec![
            pa_tui::interactive::HeadlessStep::WaitRender {
                needle: "on the cluster".to_string(),
                timeout_ms: 30_000,
            },
            // The turn AFTER the poisoned paint: the process must keep
            // serving turns through the drawn diagram.
            pa_tui::interactive::HeadlessStep::SubmitAndSettle {
                text: "still there".to_string(),
                timeout_ms: 30_000,
            },
        ],
        width: 120,
        height: 30,
    };
    let options = pa_tui::interactive::InteractiveOptions {
        session: pa_tui::interactive::SessionSelection::Attach(session.clone()),
        ..headless_options(&supervisor.socket, dir.path(), &script)
    };
    let outcome =
        pa_tui::interactive::run_interactive(options, pa_tui::interactive::UiMode::Headless(plan))
            .await
            .expect("the headless attach survived the poisoned transcript");

    assert!(!outcome.frames.is_empty(), "frames were captured");
    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains("on the cluster"),
        "the populated cluster drew:\n{rendered}"
    );
    assert!(
        rendered.contains("k3s") && rendered.contains("worker a"),
        "the cluster members drew inside it:\n{rendered}"
    );
    // The block drew as art, not as its fenced source: a source fallback
    // (which WOULD contain this line) means the draw stopped working.
    assert!(
        !rendered.contains("flowchart LR"),
        "the block drew as a diagram, not the fenced source:\n{rendered}"
    );
    // The fixed layout gives the empty compound no member ranks: its draw
    // contributes no bounds, so the cluster titled `on the workers` is
    // omitted from the art entirely.
    assert!(
        !rendered.contains("on the workers"),
        "the empty cluster is omitted from the draw:\n{rendered}"
    );
    assert!(
        rendered.contains("still there"),
        "the follow-up prompt echoed after the poisoned paint:\n{rendered}"
    );
    assert!(
        rendered.contains(FOLLOW_UP_REPLY),
        "the turn after the poisoned paint rendered:\n{rendered}"
    );
}
