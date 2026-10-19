//! Native opt-in working messages through real session events and rendered frames.
#![cfg(unix)]
// Casts: structurally bounded terminal-layout arithmetic; guarded conversions add panic paths.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Render routes are flat tables (one arm per route); splitting adds indirection.
#![allow(clippy::too_many_lines)]
// Widget state structs carry independent flag bits.
#![allow(clippy::struct_excessive_bools, clippy::fn_params_excessive_bools)]
// Futures are bounded by the surface's lifetime; boxing adds a steady-state allocation.
#![allow(clippy::large_futures)]
// The wrappers preserve a uniform Result-returning API surface.
#![allow(clippy::unnecessary_wraps)]

#[path = "support/personalization_env.rs"]
mod personalization_env;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// Headless runs share environment and terminal-mode state, so this verifier serializes them.
static RUN_LOCK: Mutex<()> = Mutex::new(());

fn run_lock() -> MutexGuard<'static, ()> {
    match RUN_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

use pa_tui::interactive::{
    run_interactive, HeadlessPlan, HeadlessStep, InteractiveOptions, ModelSelection,
    SessionSelection, UiMode,
};
use serde_json::{json, Value};

struct MockSupervisor {
    listener: UnixListener,
    events: std::sync::mpsc::Sender<std::sync::Arc<Mutex<UnixStream>>>,
}

impl MockSupervisor {
    fn bind(
        socket: &std::path::Path,
        events: std::sync::mpsc::Sender<std::sync::Arc<Mutex<UnixStream>>>,
    ) -> Self {
        MockSupervisor {
            listener: UnixListener::bind(socket).expect("bind mock socket"),
            events,
        }
    }

    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        let write_stream = stream.try_clone().expect("clone mock socket");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("read deadline");
        let shared = std::sync::Arc::new(Mutex::new(write_stream));
        self.events.send(shared.clone()).expect("event driver");
        let mut reader = BufReader::new(stream);

        let hello = json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "serverCapabilities": [],
            "clientId": "mock",
        });
        write_json(&mut shared.lock().expect("writer lock"), &hello);

        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(envelope) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let id = envelope.get("id").and_then(Value::as_str).unwrap_or("");
            let command = envelope.get("command").cloned().unwrap_or(Value::Null);
            let command_type = command
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match command_type.as_str() {
                "create" => {
                    write_json(
                        &mut shared.lock().expect("writer lock"),
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "create",
                            "success": true,
                            "data": {
                                "activeSessionId": "s1",
                                "id": "s1",
                                "sessionId": "sess-1",
                                "sessionFile": "/tmp/sess-1.jsonl",
                            },
                        }),
                    );
                }
                "attach" => {
                    write_json(&mut shared.lock().expect("writer lock"), &attach_data(id));
                }
                "get_session_stats" => {
                    write_json(
                        &mut shared.lock().expect("writer lock"),
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "get_session_stats",
                            "success": true,
                            "data": {
                                "contextUsage": { "tokens": 1200, "contextWindow": 200_000 },
                                "cost": 0.01,
                            },
                        }),
                    );
                }
                "prompt" => {
                    write_json(
                        &mut shared.lock().expect("writer lock"),
                        &json!({
                            "type": "session_event", "activeSessionId": "s1",
                            "event": { "type": "turn_start" }
                        }),
                    );
                    write_json(
                        &mut shared.lock().expect("writer lock"),
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "prompt",
                            "success": true,
                        }),
                    );
                }
                "detach" => {
                    write_json(
                        &mut shared.lock().expect("writer lock"),
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "detach",
                            "success": true,
                        }),
                    );
                }
                _ => {
                    write_json(
                        &mut shared.lock().expect("writer lock"),
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": command_type,
                            "success": true,
                            "data": {},
                        }),
                    );
                }
            }
        }
    }
}

fn write_json(writer: &mut UnixStream, value: &Value) {
    let mut line = serde_json::to_string(value).expect("serialize mock frame");
    line.push('\n');
    writer.write_all(line.as_bytes()).expect("write mock frame");
    writer.flush().expect("flush mock frame");
}

/// An idle snapshot; working messages originate in live session events, not attach data.
fn attach_data(id: &str) -> Value {
    json!({
        "type": "response",
        "id": id,
        "command": "attach",
        "success": true,
        "data": {
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "activeSessionId": "s1",
            "snapshot": {
                "activeSessionId": "s1",
                "summary": { "id": "s1", "cwd": "/tmp" },
                "state": {
                    "activeSessionId": "s1",
                    "cwd": "/tmp",
                    "sessionId": "sess-1",
                    "sessionName": "mouse session",
                    "model": null,
                    "isStreaming": false,
                    "isCompacting": false,
                    "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                },
                "messages": [],
                "lastEventSequence": 0,
                "lastEventCursor": null,
            },
            "client": { "id": "mock", "capabilities": [] },
            "lastEventSequence": 0,
            "lastEventCursor": null,
        },
    })
}

fn options(socket: PathBuf, fullscreen_mouse: bool) -> InteractiveOptions {
    InteractiveOptions {
        models: None,
        socket_path: socket,
        cwd: PathBuf::from("/tmp"),
        session_dir: None,
        script_path: None,
        model_selection: ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: SessionSelection::New,
        initial_message: None,
        show_images: true,
        fullscreen_mouse,
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
        client_settings: None,
    }
}

fn event(value: &Value) -> Value {
    json!({"type": "session_event", "activeSessionId": "s1", "event": value})
}

fn run_case(resource: Option<&str>, expected: &str, final_event: Value) -> Vec<String> {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let _environment = personalization_env::IsolatedEnv::new(dir.path());
    let agent_dir = dir.path().join("agent");
    if let Some(resource) = resource {
        std::fs::write(agent_dir.join("working-messages.json"), resource).expect("resource");
    }
    let socket = dir.path().join("tui.sock");
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let (ack_tx, ack_rx) = std::sync::mpsc::channel();
    let supervisor = MockSupervisor::bind(&socket, events_tx);
    let server = std::thread::spawn(move || supervisor.serve());
    let failed = matches!(
        final_event["message"]["stopReason"].as_str(),
        Some("error" | "aborted")
    );
    let final_marker = match final_event["message"]["stopReason"].as_str() {
        Some("aborted") => "Operation aborted",
        Some("error") => "test error",
        _ => "complete",
    };
    let custom = expected != "Waiting";
    let driver = std::thread::spawn(move || {
        let writer = events_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("connected");
        for _ in 0..2 {
            ack_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("label painted");
            if custom {
                {
                    let mut writer = writer.lock().expect("event writer");
                    write_json(
                        &mut writer,
                        &event(
                            &json!({"type":"tool_execution_start", "toolCallId":"kernel", "toolName":"python_repl", "args":{}}),
                        ),
                    );
                    write_json(
                        &mut writer,
                        &event(
                            &json!({"type":"tool_execution_update", "toolCallId":"kernel", "partialResult":{"details":{"status":"starting"}, "content":[{"type":"text", "text":"Starting isolated kernel..."}]}}),
                        ),
                    );
                }
                ack_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("tool label painted");
                write_json(
                    &mut writer.lock().expect("event writer"),
                    &event(
                        &json!({"type":"tool_execution_end", "toolCallId":"kernel", "result":{"content":[]}, "isError":false}),
                    ),
                );
                ack_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("custom label restored");
            }
            write_json(
                &mut writer.lock().expect("event writer"),
                &event(&final_event),
            );
            ack_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("message completion painted");
            write_json(
                &mut writer.lock().expect("event writer"),
                &event(&json!({"type":"turn_end"})),
            );
        }
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut steps = Vec::new();
    for _ in 0..2 {
        steps.push(HeadlessStep::Submit("run".into()));
        steps.push(HeadlessStep::WaitRender {
            needle: expected.into(),
            timeout_ms: 3000,
        });
        steps.push(HeadlessStep::Timestamp(ack_tx.clone()));
        if custom {
            steps.push(HeadlessStep::WaitRender {
                needle: "Starting isolated kernel...".into(),
                timeout_ms: 3000,
            });
            steps.push(HeadlessStep::Timestamp(ack_tx.clone()));
            steps.push(HeadlessStep::WaitGone {
                needle: "Starting isolated kernel...".into(),
                timeout_ms: 3000,
            });
            steps.push(HeadlessStep::WaitRender {
                needle: expected.into(),
                timeout_ms: 3000,
            });
            steps.push(HeadlessStep::Timestamp(ack_tx.clone()));
        }
        steps.push(HeadlessStep::WaitRender {
            needle: final_marker.into(),
            timeout_ms: 3000,
        });
        if custom && failed {
            steps.push(HeadlessStep::WaitGone {
                needle: expected.into(),
                timeout_ms: 3000,
            });
        }
        steps.push(HeadlessStep::Timestamp(ack_tx.clone()));
        steps.push(HeadlessStep::WaitIdle { timeout_ms: 3000 });
        steps.push(HeadlessStep::WaitGone {
            needle: expected.into(),
            timeout_ms: 3000,
        });
    }
    let result = runtime.block_on(run_interactive(
        options(socket, false),
        UiMode::Headless(HeadlessPlan {
            steps,
            width: 100,
            height: 30,
        }),
    ));
    driver.join().expect("driver finished");
    server.join().expect("server finished");
    let frames = result.expect("interactive run").frames;
    assert!(
        !frames.join("\n").contains("timed out waiting"),
        "render barrier failed: {frames:#?}"
    );
    frames
}

#[test]
fn working_message_resource_is_opt_in_and_resets_after_success_error_and_abort() {
    let _guard = run_lock();
    let success = json!({"type":"message_end", "message":{"role":"assistant","content":[{"type":"text","text":"complete"}],"stopReason":"stop"}});
    for stop in ["stop", "error", "aborted"] {
        let frames = run_case(
            Some(r#"["Scrying the native verifier..."]"#),
            "Scrying the native verifier...",
            json!({
                "type":"message_end", "message":{"role":"assistant", "content":[{"type":"text", "text":"complete"}], "stopReason":stop, "errorMessage":"test error"}
            }),
        );
        assert!(
            frames
                .iter()
                .filter(|frame| frame.contains("Scrying the native verifier..."))
                .count()
                >= 2
        );
        assert!(!frames
            .last()
            .expect("final frame")
            .contains("Scrying the native verifier..."));
    }
    for resource in [
        None,
        Some("[]"),
        Some("not JSON"),
        Some(r#"[""]"#),
        Some(r#"["bad\u001b[31m"]"#),
    ] {
        let frames = run_case(resource, "Waiting", success.clone());
        let all = frames.join("\n");
        assert!(!all.contains("Scrying the native verifier..."));
        if resource.is_some_and(|raw| raw != "[]") {
            assert!(
                all.contains("Custom working messages disabled:"),
                "bad resource diagnostic: {all}"
            );
        }
    }
}
