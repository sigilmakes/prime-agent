//! Explicit /refresh: capability/busy guards, credential-free reports, and nonblocking input.
#![cfg(unix)]

use pa_tui::interactive::{
    run_interactive, HeadlessPlan, HeadlessStep, InteractiveOptions, ModelSelection,
    SessionSelection, UiMode,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

struct MockSupervisor {
    listener: UnixListener,
    revision: u64,
    supported: bool,
    busy: bool,
    hang: bool,
    commands: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
}

impl MockSupervisor {
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        let write_stream = stream.try_clone().expect("clone mock socket");
        let mut writer = write_stream;
        let mut reader = BufReader::new(stream);

        let hello = json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "schemaRevision": self.revision,
            "serverCapabilities": if self.supported { vec!["model_refresh"] } else { Vec::<&str>::new() },
            "clientId": "mock",
        });
        write_json(&mut writer, &hello);

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
            self.commands.lock().unwrap().push(command.clone());
            match command_type.as_str() {
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
                    data["data"]["snapshot"]["state"]["isStreaming"] = json!(self.busy);
                    write_json(&mut writer, &data);
                }
                "refresh_models" if self.hang => {}
                "refresh_models" => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response", "id": id, "command": command_type, "success": true,
                            "data": { "startedAt": 1, "finishedAt": 2, "sources": [
                                { "source": "openrouter", "status": "updated", "modelCount": 42 },
                                { "source": "https://private.invalid/token", "status": "skipped", "reason": "unconfigured" }
                            ] }
                        }),
                    );
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
                    "sessionName": "nightly session",
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

fn options(socket: PathBuf) -> InteractiveOptions {
    InteractiveOptions {
        models: None,
        cwd: socket.parent().unwrap().to_path_buf(),
        socket_path: socket,
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

fn run_plan(
    revision: u64,
    supported: bool,
    busy: bool,
    hang: bool,
    steps: Vec<HeadlessStep>,
) -> (Vec<String>, Vec<Value>) {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("tui.sock");
    let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let supervisor = MockSupervisor {
        listener: UnixListener::bind(&socket).unwrap(),
        revision,
        supported,
        busy,
        hang,
        commands: commands.clone(),
    };
    let handle = std::thread::spawn(move || supervisor.serve());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let outcome = runtime
        .block_on(run_interactive(
            options(socket),
            UiMode::Headless(HeadlessPlan {
                steps,
                width: 110,
                height: 30,
            }),
        ))
        .unwrap();
    drop(runtime);
    handle.join().unwrap();
    let sent = commands.lock().unwrap().clone();
    (outcome.frames, sent)
}

fn wait(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 3_000,
    }
}

#[test]
fn refresh_reports_fixed_fields_without_changing_the_selected_model() {
    let (frames, commands) = run_plan(
        32,
        /*supported*/ true,
        /*busy*/ false,
        /*hang*/ false,
        vec![
            HeadlessStep::Submit("/refresh".to_string()),
            wait("unknown-source: skipped (unconfigured)"),
        ],
    );
    let all = frames.join("\n");
    // TS showStatus rewrites consecutive report rows, leaving the last source visible.
    assert!(
        all.contains("unknown-source: skipped (unconfigured)"),
        "{all}"
    );
    assert!(!all.contains("private.invalid"), "{all}");
    let refresh: Vec<_> = commands
        .iter()
        .filter(|command| command["type"] == "refresh_models")
        .collect();
    assert_eq!(refresh.len(), 1);
    assert_eq!(refresh[0]["activeSessionId"], "s1");
    assert!(!commands
        .iter()
        .any(|command| command["type"] == "set_model"));
}

#[test]
fn old_or_unadvertised_daemons_are_not_sent_refresh_commands() {
    for (revision, supported) in [(31, true), (32, false)] {
        let (frames, commands) = run_plan(
            revision,
            supported,
            /*busy*/ false,
            /*hang*/ false,
            vec![
                HeadlessStep::Submit("/refresh".to_string()),
                wait("cannot refresh models"),
            ],
        );
        assert!(frames
            .join("\n")
            .contains("restart or update it and try again."));
        assert!(!commands
            .iter()
            .any(|command| command["type"] == "refresh_models"));
    }
}

#[test]
fn busy_refresh_is_rejected_without_a_request() {
    let (_, commands) = run_plan(
        32,
        /*supported*/ true,
        /*busy*/ true,
        /*hang*/ false,
        vec![
            HeadlessStep::Submit("/refresh".to_string()),
            wait("Wait for the current response to finish before refreshing models."),
            HeadlessStep::Submit("/exit".to_string()),
        ],
    );
    assert!(!commands
        .iter()
        .any(|command| command["type"] == "refresh_models"));
}

#[test]
fn a_hanging_refresh_does_not_block_input_or_paint() {
    let (tx, rx) = std::sync::mpsc::channel();
    let start = std::time::Instant::now();
    let (frames, commands) = run_plan(
        32,
        /*supported*/ true,
        /*busy*/ false,
        /*hang*/ true,
        vec![
            HeadlessStep::Submit("/refresh".to_string()),
            HeadlessStep::Type("visible-draft".to_string()),
            wait("visible-draft"),
            HeadlessStep::Timestamp(tx),
            HeadlessStep::Submit("/exit".to_string()),
        ],
    );
    assert!(commands
        .iter()
        .any(|command| command["type"] == "refresh_models"));
    assert!(frames.join("\n").contains("visible-draft"));
    let (painted, _) = rx.recv().unwrap();
    assert!(painted.duration_since(start) < std::time::Duration::from_secs(3));
}
