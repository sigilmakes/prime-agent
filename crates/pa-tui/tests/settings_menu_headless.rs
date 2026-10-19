//! Headless e2e for the `/settings` menu's UX pass (the operator's
//! 2026-09-28 directive set): the spacing between the header/tabs and
//! the settings list (and between the tabs themselves), the arrow
//! value-cycling with its persisted writes, the Tab/number tab keys
//! (the arrows' old tab job is rebinded away), the detail block's
//! separator rule, the description-matched hint padding, the two
//! open-into-a-setting regression pins (the top bar and the padding-x
//! stay), the search field's edit keys after a no-match query, and the
//! fullscreen setting's retirement (the always-fullscreen surface has no
//! toggle left to advertise).
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
use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{
    run_interactive, HeadlessPlan, HeadlessStep, InteractiveOptions, ModelSelection,
    SessionSelection, UiMode,
};
use serde_json::{json, Value};

#[path = "support/personalization_env.rs"]
mod personalization_env;

static RUN_LOCK: Mutex<()> = Mutex::new(());

/// The rule row's glyph: the settings page's bars (the search field's borders, the detail block's
/// separator).
const RULE: &str = "\u{2500}";

struct MockSupervisor {
    listener: UnixListener,
}

impl MockSupervisor {
    fn bind(socket: &std::path::Path) -> Self {
        MockSupervisor {
            listener: UnixListener::bind(socket).expect("bind mock socket"),
        }
    }

    /// Serve one connection: the connection state carries the session's thinking levels for the
    /// Models tab's submenu rows.
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        let write_stream = stream.try_clone().expect("clone mock socket");
        let mut writer = write_stream;
        let mut reader = BufReader::new(stream);

        let hello = json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "serverCapabilities": [],
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
                "get_commands" => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "get_commands",
                            "success": true,
                            "data": { "commands": [] }
                        }),
                    );
                }
                "attach" => {
                    write_json(&mut writer, &attach_data(id));
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
                "get_connection_state" => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "get_connection_state",
                            "success": true,
                            "data": {
                                "activeSessionId": "s1",
                                "sessionId": "sess-1",
                                "sessionName": "settings session",
                                "autoCompactionEnabled": true,
                                "steeringMode": "all",
                                "followUpMode": "one-at-a-time",
                                "thinkingLevel": "low",
                                "availableThinkingLevels": ["low", "high"],
                                "isStreaming": false,
                            },
                        }),
                    );
                }
                "detach" => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response", "id": id, "command": "detach", "success": true,
                        }),
                    );
                }
                _ => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response", "id": id, "command": command_type, "success": true, "data": {},
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
                    "sessionName": "settings session",
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

/// The recording settings seam: every setter appends `name=value` to the log the arrow-cycling test
/// reads.
struct RecordingSettings {
    writes: Mutex<Vec<String>>,
}

impl Default for RecordingSettings {
    fn default() -> Self {
        Self {
            writes: Mutex::new(Vec::new()),
        }
    }
}

impl RecordingSettings {
    fn record(&self, entry: &str) -> Result<()> {
        self.writes
            .lock()
            .expect("writes lock")
            .push(entry.to_string());
        Ok(())
    }

    fn log(&self) -> Vec<String> {
        self.writes.lock().expect("writes lock").clone()
    }
}

impl pa_tui::client_settings::ClientSettings for RecordingSettings {
    fn compaction_threshold_percent(&self) -> f64 {
        95.0
    }
    fn set_compaction_threshold_percent(&self, percent: f64) -> Result<()> {
        self.record(&format!("threshold={percent}"))
    }

    fn theme(&self) -> Option<String> {
        None
    }
    fn set_theme(&self, theme: &str) -> Result<()> {
        self.record(&format!("theme={theme}"))
    }
    fn show_images(&self) -> bool {
        true
    }
    fn set_show_images(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn clear_on_shrink(&self) -> bool {
        false
    }
    fn set_clear_on_shrink(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn show_terminal_progress(&self) -> bool {
        false
    }
    fn set_show_terminal_progress(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn image_auto_resize(&self) -> bool {
        true
    }
    fn set_image_auto_resize(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn block_images(&self) -> bool {
        false
    }
    fn set_block_images(&self, _blocked: bool) -> Result<()> {
        Ok(())
    }
    fn image_model(&self) -> Option<String> {
        None
    }
    fn enable_skill_commands(&self) -> bool {
        true
    }
    fn set_enable_skill_commands(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn enable_builtin_skills(&self) -> bool {
        true
    }
    fn set_enable_builtin_skills(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn show_hardware_cursor(&self) -> bool {
        false
    }
    fn set_show_hardware_cursor(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn editor_padding_x(&self) -> u64 {
        0
    }
    fn set_editor_padding_x(&self, _padding: u64) -> Result<()> {
        Ok(())
    }
    fn autocomplete_max_visible(&self) -> u64 {
        5
    }
    fn set_autocomplete_max_visible(&self, _max_visible: u64) -> Result<()> {
        Ok(())
    }
    fn quiet_startup(&self) -> bool {
        false
    }
    fn set_quiet_startup(&self, quiet: bool) -> Result<()> {
        self.record(&format!("quiet-startup={quiet}"))
    }
    fn idle_eviction_minutes(&self) -> String {
        "90".to_string()
    }
    fn set_idle_eviction_minutes(&self, value: &str) -> Result<()> {
        self.record(&format!("idle-eviction-minutes={value}"))
    }
    fn mermaid_rendering_mode(&self) -> String {
        "streaming".to_string()
    }
    fn set_mermaid_rendering_mode(&self, _mode: &str) -> Result<()> {
        Ok(())
    }
    fn tree_filter_mode(&self) -> String {
        "user-only".to_string()
    }
    fn set_tree_filter_mode(&self, _mode: &str) -> Result<()> {
        Ok(())
    }
    fn default_service_tier(&self) -> String {
        "default".to_string()
    }
    fn set_default_service_tier(&self, _tier: &str) -> Result<()> {
        Ok(())
    }
    fn chat_detail(&self) -> String {
        "details".to_string()
    }
    fn set_chat_detail(&self, _detail: &str) -> Result<()> {
        Ok(())
    }
    fn factory_enabled(&self) -> bool {
        false
    }
    fn set_factory_enabled(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn warnings_anthropic_extra_usage(&self) -> bool {
        true
    }
    fn set_warnings_anthropic_extra_usage(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn update_channel(&self) -> Option<String> {
        None
    }
    fn set_update_channel(&self, _channel: &str) -> Result<()> {
        Ok(())
    }
    fn effective_update_channel(&self, version: &str) -> String {
        if version.contains("-beta") {
            "nightly".to_string()
        } else {
            "stable".to_string()
        }
    }
}

fn options(socket: PathBuf, settings: Arc<RecordingSettings>) -> InteractiveOptions {
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
        prompt_stash: Arc::default(),
        session_has_children: false,
        restore_dock_focus: false,
        client_settings: Some(settings),
    }
}

/// Run a plan against the mock daemon, with the recording settings seam.
fn run_plan(steps: Vec<HeadlessStep>) -> (Vec<String>, Arc<RecordingSettings>) {
    run_plan_with_themes(steps, &[], "prime")
}

fn run_plan_with_themes(
    steps: Vec<HeadlessStep>,
    themes: &[(&str, Value)],
    initial_theme: &str,
) -> (Vec<String>, Arc<RecordingSettings>) {
    run_plan_with_theme_setup(steps, themes, initial_theme, |_| {})
}

fn run_plan_with_theme_setup(
    steps: Vec<HeadlessStep>,
    themes: &[(&str, Value)],
    initial_theme: &str,
    setup: impl FnOnce(&std::path::Path),
) -> (Vec<String>, Arc<RecordingSettings>) {
    let _lock = RUN_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::TempDir::new().expect("temp dir");
    let _environment = personalization_env::IsolatedEnv::new(dir.path());
    let themes_dir = dir.path().join("agent/themes");
    std::fs::create_dir_all(&themes_dir).expect("themes directory");
    for (name, value) in themes {
        std::fs::write(
            themes_dir.join(format!("{name}.json")),
            serde_json::to_vec(value).unwrap(),
        )
        .expect("write isolated theme");
    }
    setup(&themes_dir);
    let socket = dir.path().join("tui.sock");
    let supervisor = MockSupervisor::bind(&socket);
    let handle = std::thread::spawn(move || supervisor.serve());

    let settings = Arc::new(RecordingSettings::default());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps,
        width: 100,
        height: 30,
    };
    let mut options = options(socket, settings.clone());
    options.theme = initial_theme.to_string();
    let outcome = runtime
        .block_on(run_interactive(options, UiMode::Headless(plan)))
        .expect("interactive run");
    handle.join().expect("mock supervisor finished");
    (outcome.frames, settings)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The frame rows as text, one String per row, with the frame's full-width padding trimmed (the
/// asserts read the content).
fn frame_rows(frame: &str) -> Vec<String> {
    frame
        .split('\n')
        .map(|row| row.trim_end().to_string())
        .collect()
}

/// The last frame that contains `needle` (the plan's waits guarantee it exists by the time the run
/// ends).
fn frame_with(frames: &[String], needle: &str) -> Vec<String> {
    frames
        .iter()
        .rev()
        .find(|frame| frame.contains(needle))
        .map_or_else(
            || panic!("no frame contains {needle:?}"),
            |frame| frame_rows(frame),
        )
}

fn open_settings() -> Vec<HeadlessStep> {
    vec![
        HeadlessStep::WaitMs(300),
        HeadlessStep::Submit("/settings".to_string()),
        HeadlessStep::WaitRender {
            needle: "General".to_string(),
            timeout_ms: 5000,
        },
    ]
}

/// The spacing pass: a blank row above and below the tab strip, four spaces between the tabs,
/// the separator rule below the description, the hint carrying the description's padding.
#[test]
fn the_settings_page_renders_the_spacing_and_the_new_keys() {
    let (frames, _) = run_plan(open_settings());
    let rows = frame_with(&frames, "Type to search");
    let strip_index = rows
        .iter()
        .position(|row| row.starts_with("  1 General"))
        .expect("the tab strip renders");
    assert_eq!(
        rows[strip_index], "  1 General    2 Models    3 Display    4 Editor    5 Agents",
        "the tabs sit four spaces apart (the spacing pass)"
    );
    // A blank row rides between the search field's bottom rule and the strip, and between the strip
    // and the settings list.
    assert_eq!(rows[strip_index - 1], "", "the blank above the strip");
    assert_eq!(
        rows[strip_index - 2],
        RULE.repeat(100),
        "the search field's bottom rule rides above the blank"
    );
    assert_eq!(rows[strip_index + 1], "", "the blank below the strip");
    assert!(
        rows[strip_index + 2].starts_with("\u{203a} Auto-compact"),
        "the settings list begins below the blank"
    );
    // The detail block: the description's two-space padding, the separator rule below it, and the
    // hint row's own two-space padding.
    let hint_index = rows
        .iter()
        .position(|row| row.starts_with("  Type to search"))
        .expect("the hint row renders");
    assert_eq!(
        rows[hint_index - 1],
        RULE.repeat(100),
        "the separator rule rides below the description, above the hint"
    );
    assert!(
        rows[hint_index - 2].starts_with("  Automatically compact"),
        "the description rides above the rule with its padding"
    );
    assert!(
        rows[hint_index].starts_with(
            "  Type to search · Tab/1-5 tabs · \u{2190}/\u{2192}/Enter/Space change · Esc close"
        ),
        "the hint names the Tab/number tab keys and the arrow value keys: {:?}",
        rows[hint_index]
    );
    assert!(
        !frames.join("\n").contains("Fullscreen rendering"),
        "the fullscreen setting no longer lists"
    );
}

/// Right flips the Quiet startup toggle, left flips it back, Enter keeps its cycle; the
/// multi-option Idle eviction row walks its list the same way.
#[test]
fn the_arrows_cycle_values_and_the_writes_persist() {
    let mut steps = open_settings();
    // Down x3 lands the selection on Quiet startup (General's fourth row).
    for _ in 0..3 {
        steps.push(HeadlessStep::Key(key(KeyCode::Down)));
    }
    steps.push(HeadlessStep::WaitMs(100));
    steps.push(HeadlessStep::Key(key(KeyCode::Right)));
    steps.push(HeadlessStep::WaitMs(100));
    steps.push(HeadlessStep::Key(key(KeyCode::Left)));
    steps.push(HeadlessStep::WaitMs(100));
    steps.push(HeadlessStep::Key(key(KeyCode::Enter)));
    steps.push(HeadlessStep::WaitMs(100));
    // 5 jumps to the Agents tab; down x2 lands on Idle worker eviction (a multi-option row:
    // off/30/60/90/180/360).
    steps.push(HeadlessStep::Key(key(KeyCode::Char('5'))));
    steps.push(HeadlessStep::WaitMs(100));
    steps.push(HeadlessStep::Key(key(KeyCode::Down)));
    steps.push(HeadlessStep::Key(key(KeyCode::Down)));
    steps.push(HeadlessStep::Key(key(KeyCode::Right)));
    steps.push(HeadlessStep::WaitMs(100));
    steps.push(HeadlessStep::Key(key(KeyCode::Left)));
    steps.push(HeadlessStep::WaitMs(150));
    let (frames, settings) = run_plan(steps);
    // The row shows true (Enter's cycle landed last), and the seam recorded the exact write
    // sequence the arrows drove.
    let rows = frame_with(&frames, "Quiet startup");
    let row = rows
        .iter()
        .find(|row| row.contains("Quiet startup"))
        .expect("the quiet-startup row renders");
    assert!(row.contains("true"), "the cycled value shows: {row}");
    assert_eq!(
        settings.log(),
        vec![
            "quiet-startup=true",
            "quiet-startup=false",
            "quiet-startup=true",
            "idle-eviction-minutes=180",
            "idle-eviction-minutes=90",
        ],
        "every arrow cycle persisted through the settings seam"
    );
}

#[test]
fn tab_and_the_number_keys_move_the_tabs_not_the_arrows() {
    let mut steps = open_settings();
    // Right cycles Auto-compact's value; the General rows still own the frame (no tab switch).
    steps.push(HeadlessStep::Key(key(KeyCode::Right)));
    steps.push(HeadlessStep::WaitMs(100));
    steps.push(HeadlessStep::Key(key(KeyCode::Tab)));
    steps.push(HeadlessStep::WaitMs(100));
    steps.push(HeadlessStep::Key(key(KeyCode::Char('1'))));
    steps.push(HeadlessStep::WaitMs(100));
    steps.push(HeadlessStep::Key(key(KeyCode::Char('3'))));
    steps.push(HeadlessStep::WaitMs(150));
    let (frames, _) = run_plan(steps);
    // Right after the menu opened: the value cycled, not the tab.
    let after_right = frame_with(&frames, "Auto-compact");
    assert!(after_right.iter().any(|row| row.contains("Steering mode")));
    assert!(!after_right.iter().any(|row| row.contains("Transport")));
    let after_tab = frame_with(&frames, "Transport");
    assert!(after_tab.iter().any(|row| row.contains("Thinking level")));
    assert!(frame_with(&frames, "Warnings")
        .iter()
        .any(|row| row.contains("Quiet startup")));
    // The retired fullscreen row is not there.
    let display = frame_with(&frames, "Mermaid diagrams");
    assert!(display.iter().any(|row| row.contains("Theme")));
    assert!(
        !display
            .iter()
            .any(|row| row.contains("Fullscreen rendering")),
        "the retired setting does not render"
    );
}

/// The two regression pins: the full-width rule stays over the submenu, and the setting's rows
/// keep the list rows' two-space padding — the hint too.
#[test]
fn opening_into_a_setting_keeps_the_bar_and_the_padding() {
    let mut steps = open_settings();
    steps.push(HeadlessStep::Key(key(KeyCode::Char('2'))));
    steps.push(HeadlessStep::WaitMs(100));
    steps.push(HeadlessStep::Key(key(KeyCode::Enter)));
    steps.push(HeadlessStep::WaitRender {
        needle: "Thinking Level".to_string(),
        timeout_ms: 5000,
    });
    steps.push(HeadlessStep::WaitMs(150));
    let (frames, _) = run_plan(steps);
    let rows = frame_with(&frames, "Thinking Level");
    let title_index = rows
        .iter()
        .position(|row| row == "  Thinking Level")
        .expect("the submenu title renders with its padding");
    assert_eq!(
        rows[title_index - 1],
        RULE.repeat(100),
        "the menu's top bar stays over the submenu"
    );
    let description = rows
        .iter()
        .find(|row| row.starts_with("  Select reasoning depth"))
        .expect("the padded description renders");
    assert!(description.contains("Select reasoning depth for thinking-capable models"));
    // The session's levels (from the connection state) render through the shared menu-row grammar.
    assert!(rows.iter().any(|row| row.contains("\u{203a} low")));
    assert!(rows.iter().any(|row| row.contains("high")));
    assert!(rows
        .iter()
        .any(|row| row.starts_with("  Enter select · Esc back")));
}

/// A toggle for an unsupported mode is worse than none.
#[test]
fn the_fullscreen_setting_and_command_are_retired() {
    let mut steps = open_settings();
    for digit in ['1', '2', '3', '4', '5'] {
        steps.push(HeadlessStep::Key(key(KeyCode::Char(digit))));
        steps.push(HeadlessStep::WaitMs(100));
    }
    steps.push(HeadlessStep::WaitMs(150));
    let (frames, _) = run_plan(steps);
    let all = frames.join("\n");
    assert!(
        !all.contains("Fullscreen rendering"),
        "no tab lists the retired setting: {all}"
    );
    assert!(
        !all.contains("Alternate-screen UI"),
        "the retired setting's description is gone: {all}"
    );

    let (frames, _) = run_plan(vec![
        HeadlessStep::WaitMs(300),
        HeadlessStep::Type("/full".to_string()),
        HeadlessStep::SettleIdle,
        HeadlessStep::WaitMs(200),
    ]);
    let all = frames.join("\n");
    assert!(
        !all.contains("Toggle fullscreen"),
        "the retired command no longer completes: {all}"
    );
    assert!(
        !all.contains("Fullscreen (alternate screen)"),
        "the retired command's description is gone: {all}"
    );
}

/// The search field keeps its edit keys after a no-match query: the
/// garbage backspaces away, the rows return, and Space still cycles
/// the selected row (TS `SettingsList.handleInput`).
#[test]
fn a_no_match_query_backspaces_away_and_space_still_cycles() {
    let mut steps = open_settings();
    for c in ['z', 'q', 'x'] {
        steps.push(HeadlessStep::Key(key(KeyCode::Char(c))));
    }
    steps.push(HeadlessStep::WaitRender {
        needle: "No matching settings".to_string(),
        timeout_ms: 5000,
    });
    for _ in 0..3 {
        steps.push(HeadlessStep::Key(key(KeyCode::Backspace)));
    }
    steps.push(HeadlessStep::WaitRender {
        needle: "Auto-compact".to_string(),
        timeout_ms: 5000,
    });
    steps.push(HeadlessStep::Key(key(KeyCode::Char(' '))));
    steps.push(HeadlessStep::WaitMs(300));
    let (frames, _) = run_plan(steps);
    let rows = frame_rows(frames.last().expect("the run captured frames"));
    let row = rows
        .iter()
        .find(|row| row.contains("Auto-compact"))
        .expect("the settings rows return once the query is backspaced away");
    assert!(
        row.contains("false"),
        "Space still cycles the selected row: {row}"
    );
}

#[test]
fn threshold_percentage_cycles_through_the_persistence_seam() {
    let mut steps = open_settings();
    for character in "threshold".chars() {
        steps.push(HeadlessStep::Key(key(KeyCode::Char(character))));
    }
    steps.push(HeadlessStep::Key(key(KeyCode::Right)));
    steps.push(HeadlessStep::WaitRender {
        needle: "› Auto-compact threshold (%)".into(),
        timeout_ms: 5000,
    });
    let (frames, settings) = run_plan(steps);
    assert_eq!(settings.log(), vec!["threshold=100"]);
    let rows = frame_with(&frames, "› Auto-compact threshold (%)");
    assert!(rows
        .iter()
        .any(|row| row.contains("Auto-compact threshold (%)") && row.contains("100")));
}

/// The real `SessionUi` must supply discovered themes to the menu, then
/// validate and persist the chosen filename through its settings seam.
#[test]
fn a_discovered_custom_theme_is_rendered_previewed_and_persisted() {
    let mut theme: Value = serde_json::from_str(pa_tui::theme::PRIME_JSON).unwrap();
    theme["name"] = json!("terminal-native");
    theme["vars"] = json!({"native": "", "blue": 4, "alias": "blue"});
    for color in theme["colors"].as_object_mut().unwrap().values_mut() {
        *color = json!("native");
    }
    theme["colors"]["accent"] = json!("alias");
    theme["colors"]["selectedBg"] = json!(4);
    let mut steps = open_settings();
    steps.extend([
        HeadlessStep::Key(key(KeyCode::Char('3'))),
        HeadlessStep::WaitRender {
            needle: "Mermaid diagrams".into(),
            timeout_ms: 5000,
        },
        HeadlessStep::Key(key(KeyCode::Enter)),
        HeadlessStep::WaitRender {
            needle: "terminal-native".into(),
            timeout_ms: 5000,
        },
        HeadlessStep::Key(key(KeyCode::Down)),
        HeadlessStep::Key(key(KeyCode::Down)),
        HeadlessStep::Key(key(KeyCode::Down)),
        HeadlessStep::WaitRender {
            needle: "› terminal-native".into(),
            timeout_ms: 5000,
        },
        HeadlessStep::Key(key(KeyCode::Enter)),
        HeadlessStep::WaitRender {
            needle: "› Theme".into(),
            timeout_ms: 5000,
        },
    ]);
    let (frames, settings) = run_plan_with_themes(steps, &[("terminal-native", theme)], "prime");
    assert!(frames
        .iter()
        .any(|frame| frame.contains("› terminal-native")));
    let rows = frame_with(&frames, "› Theme");
    assert!(rows
        .iter()
        .any(|row| row.contains("› Theme") && row.contains("terminal-native")));
    assert_eq!(settings.log(), vec!["theme=terminal-native"]);
}

#[test]
fn an_invalid_discovered_theme_shows_a_diagnostic_and_is_not_persisted() {
    let mut steps = open_settings();
    steps.extend([
        HeadlessStep::Key(key(KeyCode::Char('3'))),
        HeadlessStep::WaitRender {
            needle: "Mermaid diagrams".into(),
            timeout_ms: 5000,
        },
        HeadlessStep::Key(key(KeyCode::Enter)),
        HeadlessStep::WaitRender {
            needle: "› broken".into(),
            timeout_ms: 5000,
        },
        HeadlessStep::Key(key(KeyCode::Enter)),
        HeadlessStep::WaitRender {
            needle: "› Theme".into(),
            timeout_ms: 5000,
        },
        HeadlessStep::Key(key(KeyCode::Esc)),
        HeadlessStep::WaitRender {
            needle: "using prime".into(),
            timeout_ms: 5000,
        },
    ]);
    let (frames, settings) = run_plan_with_themes(
        steps,
        &[("broken", json!({"name":"broken", "colors":{"text":999}}))],
        "prime",
    );
    let all = frames.join("\n");
    assert!(all.contains("invalid colors token"), "{all}");
    assert!(all.contains("using prime"), "{all}");
    assert!(settings.log().is_empty(), "a rejected theme is never saved");
}

#[test]
fn bad_startup_themes_keep_a_visible_diagnostic_after_the_initial_paint() {
    for initial_theme in ["missing-theme", "broken"] {
        let (frames, settings) = run_plan_with_themes(
            vec![
                HeadlessStep::WaitRender {
                    needle: "using prime".into(),
                    timeout_ms: 3000,
                },
                HeadlessStep::Type("native draft".into()),
                HeadlessStep::WaitRender {
                    needle: "native draft".into(),
                    timeout_ms: 3000,
                },
            ],
            &[("broken", json!({"name":"broken", "colors":{"text":999}}))],
            initial_theme,
        );
        let final_frame = frames.last().expect("a rendered draft frame");
        assert!(final_frame.contains("native draft"));
        assert!(
            final_frame.contains(initial_theme),
            "theme name remains visible: {final_frame}"
        );
        assert!(
            final_frame.contains("using prime"),
            "fallback remains visible: {final_frame}"
        );
        assert!(!frames.join("\n").contains("timed out waiting"));
        assert!(
            settings.log().is_empty(),
            "startup fallback never changes saved preference"
        );
    }
}

#[test]
fn valid_startup_themes_are_silent_and_do_not_rewrite_preferences() {
    let custom: Value = serde_json::from_str(pa_tui::theme::PRIME_JSON).unwrap();
    for initial_theme in ["", "prime", "dark", "light", "terminal-native"] {
        let (frames, settings) = run_plan_with_themes(
            vec![HeadlessStep::Type("native draft".into())],
            &[("terminal-native", custom.clone())],
            initial_theme,
        );
        let all = frames.join("\n");
        assert!(all.contains("native draft"), "{all}");
        assert!(!all.contains("using prime"), "valid theme warned: {all}");
        assert!(!all.contains("⚠"), "valid theme warned: {all}");
        assert!(settings.log().is_empty(), "startup never saves a theme");
    }
}

/// A FIFO holds the actual production file read until a rendered-input
/// timestamp arrives. Its timeout releases a synchronous-loader mutant too,
/// so a failure never leaves the runtime waiting on a blocked file reader.
#[cfg(target_os = "linux")]
#[test]
fn a_stalled_startup_theme_does_not_hold_first_paint_or_typing() {
    let (paint_tx, paint_rx) = std::sync::mpsc::channel();
    let mut writer = None;
    let (frames, settings) = run_plan_with_theme_setup(
        vec![
            HeadlessStep::Type("native draft".into()),
            HeadlessStep::WaitRender {
                needle: "native draft".into(),
                timeout_ms: 3000,
            },
            HeadlessStep::Timestamp(paint_tx),
            HeadlessStep::WaitRender {
                needle: "using prime".into(),
                timeout_ms: 3000,
            },
        ],
        &[],
        "stalled",
        |themes_dir| {
            let path = themes_dir.join("stalled.json");
            assert!(std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success());
            writer = Some(std::thread::spawn(move || {
                let painted = paint_rx.recv_timeout(std::time::Duration::from_secs(3));
                let mut fifo = std::fs::OpenOptions::new().write(true).open(path).unwrap();
                fifo.write_all(b"{malformed").unwrap();
                drop(fifo);
                if painted.is_err() {
                    let _ = paint_rx.recv_timeout(std::time::Duration::from_secs(10));
                }
                painted
            }));
        },
    );
    let (_, renders) = writer
        .unwrap()
        .join()
        .unwrap()
        .expect("input painted before theme read completed");
    assert!(renders > 0, "timestamp follows a real rendered frame");
    let final_frame = frames.last().unwrap();
    assert!(final_frame.contains("native draft"), "{final_frame}");
    assert!(final_frame.contains("using prime"), "{final_frame}");
    assert!(!frames.join("\n").contains("timed out waiting"));
    assert!(settings.log().is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn a_late_startup_theme_failure_cannot_replace_an_intentional_theme_setting() {
    let (chosen_tx, chosen_rx) = std::sync::mpsc::channel();
    let mut writer = None;
    let mut steps = vec![HeadlessStep::Submit("/settings".into())];
    steps.extend([
        HeadlessStep::WaitRender {
            needle: "General".into(),
            timeout_ms: 3000,
        },
        HeadlessStep::Key(key(KeyCode::Char('3'))),
        HeadlessStep::Key(key(KeyCode::Enter)),
        HeadlessStep::WaitRender {
            needle: "› dark".into(),
            timeout_ms: 3000,
        },
        HeadlessStep::Key(key(KeyCode::Down)),
        HeadlessStep::WaitRender {
            needle: "› light".into(),
            timeout_ms: 3000,
        },
        HeadlessStep::Key(key(KeyCode::Enter)),
        HeadlessStep::WaitRender {
            needle: "› Theme".into(),
            timeout_ms: 3000,
        },
        HeadlessStep::Timestamp(chosen_tx),
        HeadlessStep::Key(key(KeyCode::Esc)),
        HeadlessStep::WaitRender {
            needle: "keeping light theme".into(),
            timeout_ms: 3000,
        },
        HeadlessStep::Submit("/settings".into()),
        HeadlessStep::WaitRender {
            needle: "General".into(),
            timeout_ms: 3000,
        },
        HeadlessStep::Key(key(KeyCode::Char('3'))),
        HeadlessStep::WaitRender {
            needle: "› Theme".into(),
            timeout_ms: 3000,
        },
    ]);
    let (frames, settings) = run_plan_with_theme_setup(steps, &[], "stalled", |themes_dir| {
        let path = themes_dir.join("stalled.json");
        assert!(std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success());
        writer = Some(std::thread::spawn(move || {
            let chosen = chosen_rx.recv_timeout(std::time::Duration::from_secs(3));
            let mut fifo = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            fifo.write_all(b"{malformed").unwrap();
            drop(fifo);
            if chosen.is_err() {
                let _ = chosen_rx.recv_timeout(std::time::Duration::from_secs(10));
            }
            chosen
        }));
    });
    let ready = writer.unwrap().join().unwrap();
    assert!(
        ready.is_ok(),
        "theme setting blocked: {ready:?}\n{}",
        frames.join("\n---\n")
    );
    assert_eq!(settings.log(), vec!["theme=light"]);
    let final_frame = frames.last().unwrap();
    assert!(
        final_frame
            .lines()
            .any(|row| row.contains("› Theme") && row.contains("light")),
        "{final_frame}"
    );
    assert!(!frames.join("\n").contains("timed out waiting"));
}

/// Run the real public headless entry in its own process. A held theme
/// read must not keep Tokio runtime teardown (and therefore the CLI) alive.
#[cfg(target_os = "linux")]
#[test]
fn quitting_while_a_startup_theme_read_is_held_does_not_hold_process_exit() {
    let _lock = RUN_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        use tokio::io::AsyncReadExt;
        for mode in ["exit", "finish"] {
            let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "held_startup_theme_quit_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("PA_TUI_HELD_THEME_CHILD", mode)
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("start isolated verifier process");
            let mut stdout = child.stdout.take().unwrap();
            let captured = tokio::spawn(async move {
                let mut output = String::new();
                stdout.read_to_string(&mut output).await.unwrap();
                output
            });
            let exit = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
            if exit.is_err() {
                child.kill().await.expect("stop held verifier process");
            }
            let output = captured.await.unwrap();
            assert!(
                output.contains("PA_HELD_THEME_PAINTED"),
                "the child painted before quitting: {output}"
            );
            assert!(
                exit.is_ok(),
                "quitting held on the startup theme read: {output}"
            );
            assert!(exit.unwrap().unwrap().success(), "{mode}: {output}");
        }
    });
}
#[cfg(target_os = "linux")]
#[test]
#[ignore = "subprocess helper for the bounded whole-process quit verifier"]
fn held_startup_theme_quit_child() {
    let mode = std::env::var("PA_TUI_HELD_THEME_CHILD").expect("subprocess mode");
    let (paint_tx, paint_rx) = std::sync::mpsc::channel();
    let observer = std::thread::spawn(move || {
        let (_, renders) = paint_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        assert!(renders > 0);
        println!("PA_HELD_THEME_PAINTED");
    });
    let mut steps = vec![
        HeadlessStep::Type("quit draft".into()),
        HeadlessStep::WaitRender {
            needle: "quit draft".into(),
            timeout_ms: 3000,
        },
        HeadlessStep::Timestamp(paint_tx),
    ];
    if mode == "exit" {
        steps.push(HeadlessStep::Submit("/exit".into()));
    }
    let (frames, _) = run_plan_with_theme_setup(steps, &[], "stalled", |themes_dir| {
        assert!(std::process::Command::new("mkfifo")
            .arg(themes_dir.join("stalled.json"))
            .status()
            .unwrap()
            .success());
    });
    observer.join().unwrap();
    assert!(frames.iter().any(|frame| frame.contains("quit draft")));
}
