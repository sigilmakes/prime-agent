// large_futures: stack futures on hot paths by design. too_many_lines:
// style gate only. Casts: 64-bit targets; narrowing sits at bounded
// OS/protocol boundaries.
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

//! Real-pty e2e for the external-editor handoff freeze (Willow's 2026-10-28
//! report: after closing the Ctrl+G editor the session view stops painting
//! turn events until a re-mount). The suspend flush measured the live view
//! through `layout_pass`, whose sparse-mode flag flip outlived the flush: the
//! fullscreen compose's tail re-anchor is gated on that flag, so every
//! streaming delta after the handoff painted the stale pre-edit anchor. The
//! real supervisor streams a long faux reply across a mid-stream handoff and
//! then a follow-up turn: the pty stream must carry both replies' tail after
//! the editor's marker returned into the input row.

#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Read as _, Write},
    os::fd::{AsRawFd, OwnedFd},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use nix::fcntl::{fcntl, FcntlArg::F_SETFL, OFlag};
use nix::pty::{openpty, Winsize};
use nix::unistd::Pid;
use serde_json::{json, Value};

use pa_tui::interactive::{
    run_interactive, InteractiveOptions, ModelSelection, SessionSelection, UiMode,
};
use pa_types::daemon::DaemonCommand;

/// The child-mode socket: set (with the socket path) only when this very binary is re-executed as
/// the product-under-test.
const CHILD_SOCKET_ENV: &str = "PA_EDIT_HANDOFF_CHILD_SOCKET";
/// The child-mode attach target: the live session id the parent created with the faux script.
const CHILD_SESSION_ENV: &str = "PA_EDIT_HANDOFF_CHILD_SESSION";
/// The child-mode scripted editor (`VISUAL`/`EDITOR`).
const CHILD_EDITOR_ENV: &str = "PA_EDIT_HANDOFF_CHILD_EDITOR";

/// Turn one's typed draft.
const TURN_ONE_TEXT: &str = "handoff freeze probe one";
/// Turn one's reply: an early paint marker and a late tail marker. Both are
/// single tokens so terminal wrapping can never split them. The tail is the
/// regression needle: it is delivered after the editor exit, so it must paint
/// after the editor's marker came back.
const REPLY_ONE_EARLY: &str = "handoff-early-x3fz";
const REPLY_ONE_TAIL: &str = "handoff-tail-x9qz";
/// Turn two: a plain submit after the handoff (Willow's secondary symptom:
/// later submits reached the daemon but never painted).
const TURN_TWO_TEXT: &str = "handoff freeze probe two";
const REPLY_TWO_TAIL: &str = "turn-two-tail-x7kz";
/// The scripted editor appends this token to the draft: the marker coming back
/// into the input row proves the round trip happened.
const EDITOR_MARKER: &str = "ENCLAVE-EDITOR-MARKER";

/// The `{"engine": "faux"}` script: one long streamed reply, then a short one.
fn handoff_engine_script(dir: &Path) -> PathBuf {
    // The filler paces the stream: ~60 filler tokens at 25 tok/s keep the
    // turn streaming for ~2.5s, so the early marker paints promptly and the
    // tail lands strictly after the editor's marker returns, even when the
    // handoff is fast.
    let _filler_tokens = String::from("alpha bravo charlie delta echo foxtrot golf hotel india ")
        + "juliet kilo lima mike november oscar papa quebec romeo sierra tango "
        + "uniform victor whiskey xray yankee zulu one two three four five six "
        + "seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen "
        + "seventeen eighteen nineteen twenty twentyone twentytwo twentythree "
        + "twentyfour twentyfive twentysix twentyseven twentyeight twentynine "
        + "thirty thirtyone thirtytwo thirtythree thirtyfour thirtyfive ";
    // Each marker rides its own short line: wrapped dialog rows split tokens
    // across line breaks mid-word, which would make the needle unmatchable in
    // the raw pty byte stream.
    let filler = String::from("alpha bravo charlie delta echo foxtrot golf hotel india ")
        + "juliet kilo lima mike november oscar papa quebec romeo sierra tango "
        + "uniform victor whiskey xray yankee zulu one two three four five six "
        + "seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen "
        + "seventeen eighteen nineteen twenty twentyone twentytwo twentythree "
        + "twentyfour twentyfive twentysix twentyseven twentyeight twentynine "
        + "thirty thirtyone thirtytwo thirtythree thirtyfour thirtyfive ";
    let script = dir.join("script.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "tokensPerSecond": 25, "responses": [
            {
                "text": format!("{REPLY_ONE_EARLY}\n{filler}\n{REPLY_ONE_TAIL}"),
            },
            { "text": format!("done.\n{REPLY_TWO_TAIL}") },
        ] })
        .to_string(),
    )
    .expect("write script");
    script
}

/// The scripted `$VISUAL`: a brief pause inside the cooked-mode handoff, then
/// the marker append and a zero exit.
fn editor_script(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("ed.sh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nsleep 0.25\nprintf ' {EDITOR_MARKER}\\n' >> \"$1\"\nexit 0\n"),
    )
    .expect("write editor script");
    let mut perms = std::fs::metadata(&path)
        .expect("editor script stat")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("editor script chmod");
    path
}

struct Supervisor {
    child: Child,
    socket: PathBuf,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the real supervisor over a hermetic agent dir, no ambient credentials
/// (the transcript-freeze repro must run whatever the shell's provider env).
#[allow(clippy::zombie_processes)]
fn spawn_supervisor(dir: &Path) -> Supervisor {
    let socket = dir.join("daemon.sock");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).expect("agent dir");
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
    command.env(
        pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
        "15000",
    );
    // A full workspace run can starve a session worker's boot far past the
    // 30s default connect budget; this keeps session creates deterministic.
    command.env("PA_DAEMON_WORKER_CONNECT_TIMEOUT_MS", "90000");
    // Ambient provider credentials must not leak into the daemon's catalog:
    // this supervisor serves the fixture script only.
    for provider in pa_ai::models_generated::get_providers() {
        if let Some(vars) = pa_ai::env_api_keys::get_api_key_env_vars(provider) {
            for var in vars {
                command.env_remove(var);
            }
        }
    }
    command.env_remove("PRIME_TEAM_ID");
    command.env_remove("PRIME_API_KEY");
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

/// Create a live session with the faux engine through the daemon protocol.
async fn create_session(socket: &Path, dir: &Path, script: &Path) -> String {
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

/// Send a raw pty shutdown over the supervisor socket (the headless e2e shape).
fn graceful_shutdown(socket: &Path) {
    let Ok(stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return;
    };
    let write_half = stream.try_clone().ok();
    let mut reader = BufReader::new(stream);
    let Some(mut writer) = write_half else { return };
    let _ = reader.read_line(&mut String::new());
    let command = json!({
        "type": "command",
        "id": "test-shutdown",
        "protocol": { "name": "prime-agent.daemon", "version": 7 },
        "command": { "type": "shutdown" },
    });
    let Ok(mut line) = serde_json::to_string(&command) else {
        return;
    };
    line.push('\n');
    if writer.write_all(line.as_bytes()).is_err() {
        return;
    }
    let _ = writer.flush();
}

/// The child half: runs the real interactive loop against the real supervisor; a
/// plain `cargo test` run passes trivially.
#[test]
fn editor_handoff_paint_child_mode() {
    let (Ok(socket), Ok(session), Ok(editor)) = (
        std::env::var(CHILD_SOCKET_ENV),
        std::env::var(CHILD_SESSION_ENV),
        std::env::var(CHILD_EDITOR_ENV),
    ) else {
        return;
    };
    std::env::set_var("VISUAL", &editor);
    std::env::set_var("EDITOR", &editor);
    let options = editor_child_options(PathBuf::from(socket), session);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _ = runtime.block_on(run_interactive(options, UiMode::Terminal));
}

fn editor_child_options(socket: PathBuf, session: String) -> InteractiveOptions {
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
        // A direct open into the parent's live session: the attach path the
        // handoff runs on.
        session: SessionSelection::Attach(session),
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

/// The pty harnesses serialize: each drives a raw pty; concurrent byte-level waits flake on the
/// shared sandbox CPUs.
static HARNESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The regression: a mid-stream external-editor handoff must not stop the
/// transcript from painting the settled reply, and a follow-up plain submit
/// must still paint.
#[test]
fn editor_handoff_keeps_painting_the_streaming_transcript() {
    if !session_runner() {
        return;
    }
    match nix::unistd::setsid() {
        Ok(_) => {}
        Err(error) => panic!("the harness could not start a fresh session: {error}"),
    }
    let _lock = match HARNESS_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let dir_temp = tempfile::TempDir::new().expect("temp dir");
    let dir = dir_temp.path().to_path_buf();
    let script = handoff_engine_script(&dir);
    let editor = editor_script(&dir);
    let supervisor = spawn_supervisor(&dir);
    // Leak the temp dir on purpose: the children need its paths for the test's
    // lifetime, and the whole tree dies with the test process.
    std::mem::forget(dir_temp);
    let session = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        runtime.block_on(create_session(&supervisor.socket, &dir, &script))
    };

    let mut harness = EditorHandoffHarness::start(&dir, &supervisor.socket, &session, &editor);
    type_prompt(&mut harness, TURN_ONE_TEXT);
    harness.wait_for(REPLY_ONE_EARLY, "turn one's early segment painted");
    // The stream is running: hand the terminal to the editor and take it back.
    harness.send_raw(b"\x07"); // ctrl+g
    harness.wait_for(EDITOR_MARKER, "the edited draft returned to the input row");
    // THE REGRESSION: the reply's tail was delivered after the editor exit; it must paint.
    harness.wait_for(REPLY_ONE_TAIL, "turn one's tail painted after the handoff");
    // The tail's delivery was ordered after the marker: the painted bytes too.
    let marker_index = harness.find_from(0, EDITOR_MARKER);
    let tail_index = harness.find_from(0, REPLY_ONE_TAIL);
    assert!(
        marker_index < tail_index,
        "the tail's paint preceded the editor marker: the handoff ran after the turn settled"
    );

    // Plain submit after the handoff: still paints its reply (Willow's secondary symptom).
    type_prompt(&mut harness, TURN_TWO_TEXT);
    harness.wait_for(REPLY_TWO_TAIL, "turn two painted after the handoff");

    harness.finish();
    graceful_shutdown(&supervisor.socket);
    drop(supervisor);
}

fn type_prompt(harness: &mut EditorHandoffHarness, text: &str) {
    // Key-by-key: the TUI coalesces fast bursts into one paste, and a paste's
    // trailing carriage return is draft text, so pacing keeps every byte a
    // discrete key event.
    for byte in text.bytes() {
        harness.send_raw(&[byte]);
        std::thread::sleep(Duration::from_millis(25));
    }
    std::thread::sleep(Duration::from_millis(80));
    harness.send_raw(b"\r");
    std::thread::sleep(Duration::from_millis(80));
}

/// Whether this runner is attached to a controlling-terminal session (the
/// child re-exec needs a session it can leave and re-enter safely).
fn session_runner() -> bool {
    // SAFETY: tcgetpgrp only queries the fd's foreground process group.
    let foreground = unsafe { libc::tcgetpgrp(0) };
    if foreground < 0 {
        eprintln!(
            "no controlling-terminal session on the runner (tcgetpgrp(fd 0) \
             failed); skipping the editor-handoff pty e2e — it needs a \
             controlling-terminal session to drive the pty child"
        );
        return false;
    }
    true
}

/// One pty-backed product child plus the real supervisor it attaches to.
struct EditorHandoffHarness {
    child: Child,
    master: PtyReader,
}

impl EditorHandoffHarness {
    fn start(dir: &Path, socket: &Path, session: &str, editor: &Path) -> EditorHandoffHarness {
        let pty = openpty(
            Some(&Winsize {
                ws_row: 30,
                ws_col: 100,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .expect("open pty");
        let child = spawn_child(dir, socket, session, editor, &pty.slave);
        EditorHandoffHarness {
            child,
            master: PtyReader::new(pty.master),
        }
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        if let Err(error) = self.master.file.write_all(bytes) {
            panic!("the pty hung up: {error}");
        }
        let _ = self.master.file.flush();
    }

    fn wait_for(&mut self, needle: &str, what: &str) {
        self.master.wait_from(0, needle, what);
    }

    fn find_from(&mut self, mark: usize, needle: &str) -> usize {
        find_subsequence(&self.master.output[mark..], needle.as_bytes())
            .map_or(usize::MAX, |at| at + mark)
    }

    fn finish(mut self) {
        let _ = self.child.kill();
        // Reap the child so no zombie is left behind.
        let _ = self.child.wait();
    }
}

/// Non-blocking reader over the pty master, collecting the raw byte stream the child writes.
struct PtyReader {
    file: std::fs::File,
    output: Vec<u8>,
}

impl PtyReader {
    fn new(master: OwnedFd) -> PtyReader {
        let fd = master.as_raw_fd();
        fcntl(fd, F_SETFL(OFlag::O_NONBLOCK)).expect("pty master non-blocking");
        PtyReader {
            file: master.into(),
            output: Vec::new(),
        }
    }

    /// Drain the master until the needle appears in the output, bounded by a
    /// generous harness deadline.
    fn wait_from(&mut self, mark: usize, needle: &str, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if find_subsequence(&self.output[mark..], needle.as_bytes()).is_some() {
                return;
            }
            let mut buffer = [0u8; 8192];
            let result = self.file.read(&mut buffer);
            match result {
                Ok(0) | Err(_) => {}
                Ok(n) => self.output.extend_from_slice(&buffer[..n]),
            }
            if Instant::now() > deadline {
                let text = String::from_utf8_lossy(&self.output[mark..]);
                panic!(
                    "timeout waiting for {what} (needle {needle:?}); pty tail since mark:\n{text}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A child process group of this very binary, re-executed in child mode with the
/// pty slave as its terminal and no tmux.
fn spawn_child(dir: &Path, socket: &Path, session: &str, editor: &Path, slave: &OwnedFd) -> Child {
    // Runs between fork and exec in the child: setpgid moves it into its own process group, inside
    // the runner's session.
    fn make_process_group() -> std::io::Result<()> {
        nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0))?;
        Ok(())
    }
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command
        .arg("--exact")
        .arg("editor_handoff_paint_child_mode")
        .env(CHILD_SOCKET_ENV, socket)
        .env(CHILD_SESSION_ENV, session)
        .env(CHILD_EDITOR_ENV, editor)
        .env("VISUAL", editor)
        .env("EDITOR", editor)
        .env("TERM", "xterm-ghostty")
        .env("TERM_PROGRAM", "ghostty")
        .env_remove("TMUX")
        .env_remove("STY")
        .env_remove("ZELLIJ")
        .env_remove("SSH_CONNECTION")
        .env_remove("SSH_TTY")
        .env_remove("KITTY_WINDOW_ID")
        .env_remove("KITTY_PID")
        .env_remove("KITTY_PUBLIC_KEY")
        .env_remove("KITTY_INSTALLATION_DIR")
        .stdin(slave_as_stdio(slave))
        .stdout(slave_as_stdio(slave))
        .stderr(slave_as_stdio(slave))
        .current_dir(dir);
    // SAFETY: the pre_exec hook is the supported std seam for
    // process-group setup; it runs post-fork pre-exec in the child only
    // and cannot disturb this process.
    unsafe { command.pre_exec(make_process_group) };
    command.spawn().expect("spawn child")
}

fn slave_as_stdio(slave: &OwnedFd) -> Stdio {
    slave.try_clone().expect("clone pty slave").into()
}
