//! Native plugin discovery and dispatch run through the real headless client path.
//! /plugins owns Lua lifecycle management; /mcp owns external connections.
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

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use pa_tui::interactive::{
    run_interactive, HeadlessPlan, HeadlessStep, InteractiveOptions, ModelSelection,
    SessionSelection, UiMode,
};
use serde_json::{json, Value};

#[derive(Clone, Copy)]
enum WorkerMode {
    Native,
    Legacy,
    DelayedCatalog,
    StartupCatalog,
    CatalogFailure,
    HarnessDisabled,
    PartialReload,
}

struct MockSupervisor {
    listener: UnixListener,
    mode: WorkerMode,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
}

impl MockSupervisor {
    fn bind(
        socket: &std::path::Path,
        mode: WorkerMode,
        requests: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    ) -> Self {
        MockSupervisor {
            listener: UnixListener::bind(socket).expect("bind mock socket"),
            mode,
            requests,
        }
    }

    /// Serve one connection: the roster carries two catalog services.
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        let write_stream = stream.try_clone().expect("clone mock socket");
        let mut writer = write_stream;
        let mut reader = BufReader::new(stream);

        let hello = json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "serverCapabilities": if matches!(self.mode, WorkerMode::Legacy) { Vec::<&str>::new() } else { vec!["plugin_commands"] },
            "clientId": "mock",
            "schemaRevision": 32,
        });
        write_json(&mut writer, &hello);

        let mut line = String::new();
        let mut held_catalog = None;
        let mut catalog_requests = 0;
        let mut catalog_pruned = false;
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
            self.requests.lock().unwrap().push(command.clone());
            match command_type.as_str() {
                "get_commands" => {
                    catalog_requests += 1;
                    if matches!(self.mode, WorkerMode::CatalogFailure) {
                        write_json(
                            &mut writer,
                            &json!({"type":"response", "id":id,
                            "command":"get_commands", "success":false, "error":"catalog unavailable"}),
                        );
                        continue;
                    }
                    if (matches!(self.mode, WorkerMode::DelayedCatalog)
                        && command["activeSessionId"] == "s1")
                        || (matches!(self.mode, WorkerMode::StartupCatalog)
                            && catalog_requests == 1)
                    {
                        held_catalog = Some(id.to_string());
                        if matches!(self.mode, WorkerMode::StartupCatalog) {
                            write_json(
                                &mut writer,
                                &json!({"type":"session_event", "activeSessionId":"s1",
                                "event":{"type":"message_end", "message":{"role":"assistant", "content":[{"type":"text", "text":"Discovery pending"}]}}}),
                            );
                        }
                        continue;
                    }
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response", "id": id, "command": command_type, "success": true,
                            "data": {"commands": command_rows(self.mode, catalog_pruned)}
                        }),
                    );
                    if let Some(old_id) = held_catalog.take() {
                        // The previous session's reply arrives AFTER the current catalog, over the
                        // real client channel. Only the refresh epoch can reject this stale row.
                        write_json(
                            &mut writer,
                            &json!({
                                "type":"response", "id":old_id, "command":"get_commands", "success":true,
                                "data":{"commands":[{"name":"native-obsolete", "source":"plugin", "description":"STALE CATALOG MUST NOT RETURN", "target":target()}]}
                            }),
                        );
                    }
                }
                "plugin_command" => {
                    let text = command["text"].as_str().unwrap();
                    let (success, text) = match text {
                        "/reload" | "/plugins disable fixture"
                            if matches!(self.mode, WorkerMode::PartialReload) =>
                        {
                            catalog_pruned = true;
                            (false, "Disabled fixture; enabled replacement rejected")
                        }
                        "/plugins" => (true, "Native Lua plugins: harness (enabled)"),
                        "/plugins disable harness" => {
                            (true, "Disabled harness; saved data preserved")
                        }
                        "/reload" => (true, "Reloaded native Lua plugins"),
                        "/refine request" if !matches!(self.mode, WorkerMode::HarnessDisabled) => {
                            (true, "Harness command scheduled")
                        }
                        "/native-check status" if !catalog_pruned => {
                            (true, "Native invocation completed")
                        }
                        "/native-check stream" => {
                            (false, "Streaming plugin commands are not supported")
                        }
                        _ => (false, "Unknown plugin command"),
                    };
                    write_json(
                        &mut writer,
                        &json!({
                            "type":"response", "id":id, "command":command_type, "success":success,
                            "data":{"text":text}, "error":if success {Value::Null} else {json!(text)}
                        }),
                    );
                }

                "create" => {
                    write_json(
                        &mut writer,
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
                    let mut data = attach_data(id);
                    let session = command["activeSessionId"].clone();
                    data["data"]["activeSessionId"] = session.clone();
                    data["data"]["snapshot"]["activeSessionId"] = session.clone();
                    data["data"]["snapshot"]["state"]["activeSessionId"] = session;
                    write_json(&mut writer, &data);
                }
                "prompt" => {
                    write_json(
                        &mut writer,
                        &json!({"type":"response", "id":id, "command":"prompt", "success":true}),
                    );
                    write_json(
                        &mut writer,
                        &json!({"type":"session_event", "activeSessionId":"s1",
                        "event":{"type":"message_end", "message":{"role":"assistant", "content":[{"type":"text", "text":"Template task accepted"}]}}}),
                    );
                    write_json(
                        &mut writer,
                        &json!({"type":"session_event", "activeSessionId":"s1",
                        "event":{"type":"turn_end"}}),
                    );
                }
                "get_mcp_connections" => {
                    write_json(&mut writer, &roster_data(id));
                }
                "get_session_stats" => {
                    write_json(
                        &mut writer,
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
                "detach" => {
                    write_json(
                        &mut writer,
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
                        &mut writer,
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

/// The slim attach result: one empty session.
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
                    "sessionName": "plugins session",
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

/// The daemon roster: two catalog services (the resolved `services` cards
/// the catalog picker renders).
fn roster_data(id: &str) -> Value {
    json!({
        "type": "response",
        "id": id,
        "command": "get_mcp_connections",
        "success": true,
        "data": {
            "services": [
                {
                    "serviceId": "github",
                    "label": "GitHub",
                    "connectionStatus": "not_connected",
                    "connectable": true,
                    "connectionIds": [],
                },
                {
                    "serviceId": "linear",
                    "label": "Linear",
                    "connectionStatus": "not_connected",
                    "connectable": true,
                    "connectionIds": [],
                },
            ],
            "connections": [],
        },
    })
}

fn options(socket: PathBuf) -> InteractiveOptions {
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
        fullscreen_mouse: true,
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

fn command_rows(mode: WorkerMode, pruned: bool) -> Vec<Value> {
    let mut commands = vec![
        json!({"name":"native-check", "source":"plugin", "description":"Inspect native Lua state", "argumentHint":"request", "target": target()}),
        json!({"name":"saved-task", "source":"prompt"}),
    ];
    if !matches!(mode, WorkerMode::PartialReload) {
        commands.push(json!({"name":"native-check", "source":"prompt"}));
    }
    if pruned {
        commands.retain(|command| command["name"] != "native-check");
    }
    if !matches!(mode, WorkerMode::HarnessDisabled) {
        commands.push(json!({"name":"refine", "source":"plugin", "description":"Maintain local harness", "target": harness_target()}));
    }
    commands
}

fn harness_target() -> Value {
    json!({"plugin":"harness", "generation":"harness-generation", "kind":"command",
        "name":"refine", "description":"Maintain local harness", "mode":"unary"})
}

fn target() -> Value {
    json!({"plugin":"fixture", "generation":"frozen-generation", "kind":"command",
        "name":"native-check", "description":"Inspect native Lua state", "mode":"unary"})
}

fn run_plan(mode: WorkerMode, steps: Vec<HeadlessStep>) -> (Vec<String>, Vec<Value>) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let supervisor = MockSupervisor::bind(&socket, mode, requests.clone());
    let handle = std::thread::spawn(move || supervisor.serve());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps,
        width: 100,
        height: 30,
    };
    let outcome = runtime
        .block_on(run_interactive(options(socket), UiMode::Headless(plan)))
        .expect("interactive run");
    handle.join().expect("mock supervisor finished");
    let requests = requests.lock().unwrap().clone();
    (outcome.frames, requests)
}

fn wait(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 3000,
    }
}

#[test]
fn plugins_manages_native_lua_while_mcp_keeps_connections() {
    let (frames, requests) = run_plan(
        WorkerMode::Native,
        vec![
            HeadlessStep::Submit("/plugins".into()),
            wait("Native Lua plugins"),
            HeadlessStep::Submit("/plugins disable harness".into()),
            wait("saved data preserved"),
            HeadlessStep::Submit("/mcp".into()),
            wait("Search MCP connections"),
        ],
    );
    let all = frames.join("\n");
    assert!(all.contains("GitHub") && all.contains("Linear"), "{all}");
    let native: Vec<_> = requests
        .iter()
        .filter(|c| c["type"] == "plugin_command")
        .map(|c| c["text"].clone())
        .collect();
    assert_eq!(
        native,
        vec![json!("/plugins"), json!("/plugins disable harness")]
    );
    assert!(
        !requests.iter().any(|c| c["type"] == "prompt"),
        "{requests:?}"
    );
}

#[test]
fn unknown_and_streaming_plugin_commands_never_become_model_prompts() {
    let (frames, requests) = run_plan(
        WorkerMode::Native,
        vec![
            HeadlessStep::Submit("/not-a-plugin".into()),
            wait("Unknown plugin command"),
            HeadlessStep::Submit("/native-check stream".into()),
            wait("Streaming plugin commands are not supported"),
        ],
    );
    assert!(!frames.is_empty());
    assert!(
        !requests.iter().any(|c| c["type"] == "prompt"),
        "{requests:?}"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|c| c["type"] == "plugin_command")
            .count(),
        2
    );
}

#[test]
fn old_worker_explicitly_rejects_native_commands_without_sending() {
    let (frames, requests) = run_plan(
        WorkerMode::Legacy,
        vec![
            HeadlessStep::Submit("/plugins".into()),
            wait("not supported by this worker"),
            HeadlessStep::Submit("/reload".into()),
            wait("Native plugin reload is not supported"),
            HeadlessStep::Submit("/unknown-native".into()),
            wait("Unknown command: /unknown-native"),
        ],
    );
    assert!(!frames.is_empty());
    assert!(
        !requests.iter().any(|c| matches!(
            c["type"].as_str(),
            Some("prompt" | "plugin_command" | "reload")
        )),
        "{requests:?}"
    );
}

#[test]
fn reload_dispatches_to_the_worker_and_refreshes_discovery() {
    let (frames, requests) = run_plan(
        WorkerMode::Native,
        vec![
            HeadlessStep::Submit("/reload".into()),
            wait("Reloaded native Lua plugins"),
        ],
    );
    assert!(!frames.is_empty());
    assert!(
        requests
            .iter()
            .any(|c| c["type"] == "plugin_command" && c["text"] == "/reload"),
        "{requests:?}"
    );
    assert!(
        !requests
            .iter()
            .any(|c| c["type"] == "reload" || c["type"] == "prompt"),
        "{requests:?}"
    );
    assert!(
        requests
            .iter()
            .filter(|c| c["type"] == "get_commands")
            .count()
            >= 2,
        "{requests:?}"
    );
}

#[test]
fn plugin_completion_dispatches_with_its_generation_target() {
    let enter = || {
        HeadlessStep::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ))
    };
    let (frames, requests) = run_plan(
        WorkerMode::Native,
        vec![
            HeadlessStep::Submit("/plugins".into()),
            wait("Native Lua plugins"),
            HeadlessStep::Type("/native-ch".into()),
            HeadlessStep::SettleIdle,
            wait("Inspect native Lua state"),
            enter(),
            HeadlessStep::Type("status".into()),
            enter(),
            wait("Native invocation completed"),
        ],
    );
    let all = frames.join("\n");
    assert!(all.contains("#plugin"), "{all}");
    let command = requests
        .iter()
        .find(|c| c["type"] == "plugin_command" && c["text"] == "/native-check status")
        .expect("native invocation");
    assert_eq!(command["target"], target());
    assert!(
        !requests.iter().any(|c| c["type"] == "prompt"),
        "{requests:?}"
    );
}

#[test]
fn delayed_previous_session_catalog_cannot_replace_the_rebound_worker_catalog() {
    let (frames, requests) = run_plan(
        WorkerMode::DelayedCatalog,
        vec![
            HeadlessStep::Submit("/switch s2".into()),
            wait("switched to session s2"),
            HeadlessStep::Type("/native-".into()),
            HeadlessStep::SettleIdle,
            wait("Inspect native Lua state"),
        ],
    );
    let last = frames.last().expect("completion frame");
    assert!(last.contains("native-check"), "{last}");
    assert!(
        !frames
            .iter()
            .any(|frame| frame.contains("STALE CATALOG MUST NOT RETURN")),
        "{frames:?}"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|c| c["type"] == "get_commands")
            .count(),
        2
    );
}

#[test]
fn early_template_input_waits_for_discovery_and_keeps_template_admission() {
    let (_, requests) = run_plan(
        WorkerMode::StartupCatalog,
        vec![
            wait("Discovery pending"),
            HeadlessStep::Submit("/saved-task details".into()),
            wait("Template task accepted"),
        ],
    );
    assert_eq!(
        requests.iter().filter(|c| c["type"] == "prompt").count(),
        1,
        "{requests:?}"
    );
    assert!(
        !requests.iter().any(|c| c["type"] == "plugin_command"),
        "{requests:?}"
    );
    assert!(
        requests
            .iter()
            .filter(|c| c["type"] == "get_commands")
            .count()
            >= 2,
        "{requests:?}"
    );
}

#[test]
fn failed_catalog_does_not_fall_back_to_a_paid_prompt() {
    let (_, requests) = run_plan(
        WorkerMode::CatalogFailure,
        vec![
            HeadlessStep::Submit("/saved-task details".into()),
            wait("Command discovery failed"),
        ],
    );
    assert!(
        !requests
            .iter()
            .any(|c| matches!(c["type"].as_str(), Some("prompt" | "plugin_command"))),
        "{requests:?}"
    );
}

#[test]
fn refine_completion_uses_the_optional_plugin_target_not_the_legacy_builtin() {
    let enter = || {
        HeadlessStep::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ))
    };
    let (frames, requests) = run_plan(
        WorkerMode::Native,
        vec![
            HeadlessStep::Submit("/plugins".into()),
            wait("Native Lua plugins"),
            HeadlessStep::Type("/refin".into()),
            HeadlessStep::SettleIdle,
            wait("Maintain local harness"),
            enter(),
            HeadlessStep::Type(" request".into()),
            enter(),
            wait("Harness command scheduled"),
        ],
    );
    assert!(frames.iter().any(|frame| frame.contains("#plugin")));
    let command = requests
        .iter()
        .find(|command| command["type"] == "plugin_command" && command["text"] == "/refine request")
        .unwrap_or_else(|| panic!("generic plugin invocation missing: {requests:?}"));
    assert_eq!(command["target"], harness_target());
    assert!(!requests
        .iter()
        .any(|command| matches!(command["type"].as_str(), Some("prompt" | "refine"))));
}

#[test]
fn disabled_harness_is_not_a_builtin_and_never_falls_back_to_a_model_prompt() {
    let (frames, requests) = run_plan(
        WorkerMode::HarnessDisabled,
        vec![
            HeadlessStep::Type("/ref".into()),
            HeadlessStep::SettleIdle,
            HeadlessStep::Submit("/refine request".into()),
            wait("Unknown plugin command"),
        ],
    );
    assert!(!frames
        .iter()
        .any(|frame| frame.contains("Maintain local harness")));
    let command = requests
        .iter()
        .find(|command| command["type"] == "plugin_command" && command["text"] == "/refine request")
        .expect("safe optional command refusal");
    assert!(command.get("target").is_none());
    assert!(!requests
        .iter()
        .any(|command| matches!(command["type"].as_str(), Some("prompt" | "refine"))));
}

#[test]
fn failed_management_reconcile_refreshes_commands_after_applied_disable() {
    for management in ["/reload", "/plugins disable fixture"] {
        let (frames, requests) = run_plan(
            WorkerMode::PartialReload,
            vec![
                HeadlessStep::Submit("/plugins".into()),
                wait("Native Lua plugins"),
                HeadlessStep::Submit(management.into()),
                wait("enabled replacement rejected"),
                HeadlessStep::SettleIdle,
                HeadlessStep::Type("/native-".into()),
                HeadlessStep::SettleIdle,
                HeadlessStep::Submit("/native-check status".into()),
                wait("Unknown plugin command"),
            ],
        );
        assert!(!frames.is_empty());
        let command = requests
            .iter()
            .find(|command| {
                command["type"] == "plugin_command" && command["text"] == "/native-check status"
            })
            .expect("safe refusal after partial reconcile");
        assert!(
            command.get("target").is_none(),
            "{management}: stale disabled target: {requests:?}"
        );
        assert!(
            requests
                .iter()
                .filter(|command| command["type"] == "get_commands")
                .count()
                >= 3,
            "{management}: management error must refresh discovery: {requests:?}"
        );
        assert!(
            !requests.iter().any(|command| command["type"] == "prompt"),
            "{requests:?}"
        );
    }
}
