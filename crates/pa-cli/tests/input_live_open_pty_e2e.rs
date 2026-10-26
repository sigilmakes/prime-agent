// The Tier-C/D ruling (fleet-uniform, 2026-09-28): stack-resident futures
// by design on hot paths; the fn-length threshold is a style gate, not
// correctness; the harness fns are intentionally linear; 64-bit targets -
// the narrowing sits at OS/protocol boundaries where the values are
// bounded (pid syscalls, epoch/elapsed milliseconds).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! Real-pty e2e for the input-latency contract (the operator's
//! "first page fast, time-to-type slow" report + AGENTS.md
//! "Performance and the critical path"): a NEW session's interactive
//! surface must accept keystrokes the moment its startup chrome is
//! mounted — the session open (create, attach, the dock folds, every
//! daemon readiness behind them) runs in the background, so the echo
//! never waits on it (TS `init()`: `ui.start()` is live before
//! `rebindCurrentSession()` awaits). The mock supervisor stalls the
//! `attach` response far past the echo deadline, so an open-gated input
//! path cannot fake a pass.
//!
//! The typed-ahead contract rides the same run: text typed during the
//! stall plus an Enter queued behind the open must submit once the
//! session lands — never lost, never errored.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::fcntl::{fcntl, FcntlArg::F_SETFL, OFlag};
use nix::pty::{openpty, Winsize};
use serde_json::{json, Value};

use pa_tui::interactive::{
    run_interactive, InteractiveOptions, ModelSelection, SessionSelection, UiMode,
};

/// The child-mode socket: set (with the socket path) only when this very
/// binary is re-executed as the product-under-test.
const CHILD_SOCKET_ENV: &str = "PA_INPUT_LIVE_CHILD_SOCKET";

/// How long the mock supervisor holds the `attach` response: far past the
/// echo deadline below, so the open is provably still in flight while the
/// keystrokes are expected to echo.
const OPEN_STALL_MS: u64 = 2_500;

/// The echo deadline: the keystroke echo must land while the attach is
/// still stalled, well inside the stall window.
const ECHO_DEADLINE_MS: u64 = 1_500;

/// The pty window the child renders into (a 0x0 pty renders nothing; the
/// grid the echo oracle reads matches the window the child lays out).
const PTY_ROWS: usize = 24;
const PTY_COLS: usize = 80;

/// The child half of the e2e: runs the real interactive loop in terminal
/// mode against the parent's mock supervisor. A plain `cargo test` run
/// (no `CHILD_SOCKET_ENV`) passes trivially — only the parent test drives
/// the real path.
#[test]
fn input_live_open_child_mode() {
    let Ok(socket) = std::env::var(CHILD_SOCKET_ENV) else {
        return;
    };
    let mut options = child_options(PathBuf::from(socket));
    if let Ok(bindings) = std::env::var("PA_INPUT_LIVE_BINDINGS") {
        options.keybindings = pa_tui::keybindings::KeybindingsManager::with_user_bindings(
            serde_json::from_str(&bindings).expect("child test bindings"),
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _ = runtime.block_on(run_interactive(options, UiMode::Terminal));
}

/// The pty harnesses serialize: each drives a raw pty; concurrent
/// byte-level waits flake on the shared sandbox CPUs.
static HARNESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A keystroke echoes into the rendered surface while the session open is
/// still in flight (the attach is stalled for `OPEN_STALL_MS`), and the
/// typed-ahead text plus the queued Enter submit once the open lands.
#[test]
fn typing_echoes_while_the_session_open_is_stalled() {
    let _lock = match HARNESS_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut harness = InputLiveHarness::start(Arc::clone(&commands));

    // The startup chrome (the new chat's brand splash) paints first: the
    // surface is mounted and in raw mode by then.
    harness.wait_from_start("prime agent", "the startup chrome painted");

    // Type while the open is still stalled. The echo must land inside the
    // echo deadline — far inside the attach's stall window.
    harness.type_text("zzq");
    let echo_at = harness.wait_from_start("zzq", "the keystroke echoed while the open was stalled");

    // The queued submit: the typed-ahead text plus Enter ride the opening
    // phase's queue and dispatch once the session lands.
    harness.type_text("\r");

    // Let the stalled attach land and the fold run.
    let content_at = harness.wait_from_start(
        "openprobe settled answer",
        "the attach's content frame painted",
    );

    // The red-first oracle, ordering-based (load-robust where a wall
    // clock is not): the keystroke echo must have painted while the
    // attach was still stalled, so it became visible strictly BEFORE the
    // attach's content frame in the byte stream. An open-gated input path
    // cannot fake this — its first echo frame can only follow the
    // stalled open, the same event that paints the content frame.
    assert!(
        echo_at < content_at,
        "the keystroke echo (byte {echo_at}) painted only after the          attach's content frame (byte {content_at}): the input path was          open-gated"
    );

    // The typed-ahead submit was delivered: the mock supervisor saw the
    // prompt carrying the typed text.
    let deadline = Instant::now() + Duration::from_secs(10);
    let submitted = loop {
        if let Some(found) = find_prompt_with(&commands, "zzq") {
            break found;
        }
        if Instant::now() > deadline {
            let seen = commands.lock().expect("commands").clone();
            panic!("the typed-ahead submit never reached the daemon; commands seen: {seen:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = submitted;

    // The harness drops here: the child dies with the test on every path.
}

/// Drive terminal bytes through the real reader, bindings, editor, and wire.
/// Negotiation matters: sending CSI-u alone does not prove the app enabled it.
#[test]
fn terminal_enter_bindings_preserve_newlines_and_follow_ups() {
    let _lock = HARNESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for newline in ["\x1b[13;2u", "\x1b[27;2;13~"] {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let mut harness = InputLiveHarness::start(Arc::clone(&commands));
        harness.negotiate_keyboard();
        harness.wait_from_start("openprobe settled answer", "the session attached");
        harness.type_text("shiftprobe first");
        harness.type_text(newline);
        harness.type_text("shiftprobe second");
        harness.wait_from_start("shiftprobe second", "the second draft line painted");
        assert!(
            find_prompt_with(&commands, "shiftprobe").is_none(),
            "newline encoding {newline:?} submitted the draft"
        );
        harness.type_text("\r");
        let payload = wait_for_prompt(&commands, "shiftprobe");
        assert_eq!(payload["message"], "shiftprobe first\nshiftprobe second");
        assert_ne!(payload["streamingBehavior"], "followUp");

        // ESC CR is real Alt+Enter, not an alternative Shift+Enter encoding.
        for follow_up in ["\x1b\r", "\x1b[13;3u"] {
            harness.type_text("altprobe");
            harness.type_text(follow_up);
            let payload = wait_for_prompt(&commands, "altprobe");
            assert_eq!(payload["message"], "altprobe");
            assert_eq!(payload["streamingBehavior"], "followUp");
            commands.lock().expect("commands").clear();
        }
    }

    // Reassign all three actions. No hardcoded Enter shortcut may survive.
    let commands = Arc::new(Mutex::new(Vec::new()));
    let bindings = serde_json::from_value(json!({
        "tui.input.newLine": ["alt+enter"],
        "tui.input.submit": ["shift+enter"],
        "app.message.followUp": ["f6"],
    }))
    .expect("test bindings");
    let mut harness = InputLiveHarness::start_with_bindings(Arc::clone(&commands), &bindings);
    harness.negotiate_keyboard();
    harness.wait_from_start("openprobe settled answer", "the session attached");
    harness.type_text("rebound first");
    harness.type_text("\r"); // Plain Enter is now unbound.
    harness.type_text("\x1b\r"); // Alt+Enter now inserts a newline.
    harness.type_text("rebound second");
    harness.wait_from_start("rebound second", "the rebound draft painted");
    assert!(find_prompt_with(&commands, "rebound").is_none());
    harness.type_text("\x1b[13;2u");
    let payload = wait_for_prompt(&commands, "rebound");
    assert_eq!(payload["message"], "rebound first\nrebound second");
    assert_ne!(payload["streamingBehavior"], "followUp");
    harness.type_text("rebound next");
    harness.wait_from_start("rebound next", "the rebound follow-up draft painted");
    harness.type_text("\x1b[17~");
    let payload = wait_for_prompt(&commands, "rebound next");
    assert_eq!(payload["streamingBehavior"], "followUp");
}

fn wait_for_prompt(commands: &Arc<Mutex<Vec<String>>>, text: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(command) = find_prompt_with(commands, text) {
            let (_, payload) = command.split_once(": ").expect("recorded command");
            return serde_json::from_str(payload).expect("prompt command JSON");
        }
        assert!(
            Instant::now() < deadline,
            "prompt {text:?} did not submit; commands: {:?}",
            commands.lock().expect("commands")
        );
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

/// One pty-backed product child plus the mock supervisor it opens against.
struct InputLiveHarness {
    child: Child,
    _dir: tempfile::TempDir,
    server: Option<std::thread::JoinHandle<()>>,
    /// Whether the mock's `accept` landed: `Drop` joins the serve thread
    /// only then, so a child that dies before connecting cannot hang the
    /// teardown on the still-accepting listener.
    connected: Arc<std::sync::atomic::AtomicBool>,
    master: PtyReader,
}

impl InputLiveHarness {
    fn start(commands: Arc<Mutex<Vec<String>>>) -> InputLiveHarness {
        Self::start_with_bindings(commands, &pa_tui::keybindings::KeybindingsConfig::default())
    }

    fn start_with_bindings(
        commands: Arc<Mutex<Vec<String>>>,
        bindings: &pa_tui::keybindings::KeybindingsConfig,
    ) -> InputLiveHarness {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let socket = dir.path().join("tui.sock");
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let supervisor = MockSupervisor::bind(&socket);
        let server = {
            let connected = Arc::clone(&connected);
            Some(std::thread::spawn(move || {
                supervisor.serve(&commands, &connected);
            }))
        };
        let pty = openpty(
            Some(&Winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .expect("open pty");
        let child = spawn_child(&socket, &pty.slave, bindings);
        InputLiveHarness {
            child,
            _dir: dir,
            server,
            connected,
            master: PtyReader::new(pty.master),
        }
    }

    fn negotiate_keyboard(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut answered = false;
        loop {
            let mut buffer = [0u8; 8192];
            if let Ok(n) = self.master.file.read(&mut buffer) {
                self.master.output.extend_from_slice(&buffer[..n]);
                self.master.feed(&buffer[..n]);
            }
            if !answered && self.master.output.windows(4).any(|w| w == b"\x1b[?u") {
                self.type_text("\x1b[?0u\x1b[?1;2c");
                answered = true;
            }
            if self.master.output.windows(5).any(|w| w == b"\x1b[>7u") {
                assert!(answered, "kitty was enabled without querying the terminal");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "kitty negotiation did not finish: {}",
                String::from_utf8_lossy(&self.master.output)
            );
            std::thread::park_timeout(Duration::from_millis(1));
        }
    }

    fn type_text(&mut self, text: &str) {
        // The typed bytes must land on the child's stdin (the pty slave):
        // write to the master.
        self.master.write_typed(text);
    }

    /// Wait for a needle in the rendered grid; returns the raw byte
    /// offset at which it first became visible (the ordering oracle).
    fn wait_from_start(&mut self, needle: &str, what: &str) -> usize {
        self.master
            .wait_for(needle, what, ECHO_DEADLINE_MS * 3 + OPEN_STALL_MS * 2)
    }
}

impl Drop for InputLiveHarness {
    fn drop(&mut self) {
        // The teardown runs on every exit path — including the panic
        // unwinds the oracles below produce — so no product child ever
        // outlives the test that spawned it. The child's death ends the
        // mock's read loop (the serve thread joins only when its accept
        // landed; a child that died pre-connect leaves the listener
        // thread to the process exit instead of hanging the drop).
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.connected.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(server) = self.server.take() {
                let _ = server.join();
            }
        }
    }
}

/// The rendered screen (a minimal ANSI grid renderer, the probe doc's
/// detection method): the TUI draws char-by-char with cursor moves, so a
/// needle must be matched against the RENDERED grid — a raw byte search
/// false-negatives whenever the wordmark, a name, or an editor echo is
/// painted glyph-by-glyph with cursor hops between them.
struct PtyReader {
    file: std::fs::File,
    /// The raw byte stream (kept for the panic dump).
    output: Vec<u8>,
    rows: usize,
    cols: usize,
    grid: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

impl PtyReader {
    fn new(master: OwnedFd) -> PtyReader {
        let fd = master.as_raw_fd();
        fcntl(fd, F_SETFL(OFlag::O_NONBLOCK)).expect("pty master non-blocking");
        PtyReader {
            file: master.into(),
            output: Vec::new(),
            rows: PTY_ROWS,
            cols: PTY_COLS,
            grid: vec![vec![' '; PTY_COLS]; PTY_ROWS],
            row: 0,
            col: 0,
        }
    }

    fn write_typed(&mut self, text: &str) {
        self.file.write_all(text.as_bytes()).expect("type text");
    }

    fn put(&mut self, ch: char) {
        if self.row < self.rows && self.col < self.cols {
            self.grid[self.row][self.col] = ch;
        }
        self.col = (self.col + 1).min(self.cols - 1);
    }

    /// Feed the raw bytes through the grid: text lands at the cursor, CSI
    /// cursor/erase sequences move or clear it, and every other escape
    /// (SGR, private modes, OSC) is consumed.
    fn feed(&mut self, data: &[u8]) {
        let bytes = data;
        let mut i = 0;
        while i < bytes.len() {
            let byte = bytes[i];
            if byte == 0x1b {
                match bytes.get(i + 1) {
                    Some(b'[') => {
                        // CSI: consume the parameter bytes, then the final.
                        let mut j = i + 2;
                        let start = j;
                        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                            j += 1;
                        }
                        if j >= bytes.len() {
                            break;
                        }
                        self.csi(&String::from_utf8_lossy(&bytes[start..j]), bytes[j]);
                        i = j + 1;
                    }
                    Some(b']') => {
                        // OSC: consumed through its BEL or ST terminator
                        // (a fresh ESC starts a new sequence instead).
                        let mut j = i + 2;
                        let mut consumed = false;
                        while j < bytes.len() {
                            if bytes[j] == 0x07 {
                                i = j + 1;
                                consumed = true;
                                break;
                            }
                            if bytes[j] == 0x1b {
                                let terminator = bytes.get(j + 1) == Some(&b'\\');
                                i = if terminator { j + 2 } else { j };
                                consumed = true;
                                break;
                            }
                            j += 1;
                        }
                        if !consumed {
                            i = j;
                        }
                    }
                    // A two-byte escape the grid does not model (ESC (, ESC =,
                    // ...): skip the pair.
                    Some(_) => i += 2,
                    None => break,
                }
                continue;
            }
            match byte {
                b'\n' => {
                    self.row = (self.row + 1).min(self.rows - 1);
                    i += 1;
                }
                b'\r' => {
                    self.col = 0;
                    i += 1;
                }
                byte if byte < 0x20 => i += 1,
                byte if byte < 0x80 => {
                    self.put(byte as char);
                    i += 1;
                }
                _ => {
                    // Multi-byte UTF-8: decode the full code point.
                    let len = if byte < 0xe0 {
                        2
                    } else if byte < 0xf0 {
                        3
                    } else {
                        4
                    };
                    let end = (i + len).min(bytes.len());
                    if let Ok(text) = std::str::from_utf8(&bytes[i..end]) {
                        if let Some(ch) = text.chars().next() {
                            self.put(ch);
                        }
                    }
                    i = end;
                }
            }
        }
    }

    fn csi(&mut self, params: &str, final_byte: u8) {
        let params = params.trim_start_matches(['?', '>', '<', '=']);
        let number = |index: usize, default: usize| -> usize {
            params
                .split(';')
                .nth(index)
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(default)
        };
        match final_byte {
            b'H' | b'f' => {
                self.row = number(0, 1).saturating_sub(1).min(self.rows - 1);
                self.col = number(1, 1).saturating_sub(1).min(self.cols - 1);
            }
            b'A' => self.row = self.row.saturating_sub(number(0, 1)),
            b'B' => self.row = (self.row + number(0, 1)).min(self.rows - 1),
            b'C' => self.col = (self.col + number(0, 1)).min(self.cols - 1),
            b'D' => self.col = self.col.saturating_sub(number(0, 1)),
            b'G' => self.col = number(0, 1).saturating_sub(1).min(self.cols - 1),
            b'J' => {
                let mode = number(0, 0);
                if mode == 2 || mode == 3 {
                    self.grid = vec![vec![' '; self.cols]; self.rows];
                }
            }
            _ => {}
        }
    }

    /// The rendered grid as text (trailing blanks trimmed per row).
    fn rendered(&self) -> String {
        self.grid
            .iter()
            .map(|row| row.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Drain the master until the needle appears in the RENDERED grid,
    /// bounded by the given window; returns the raw byte offset at which
    /// the needle first became visible (the ordering oracle's evidence).
    fn wait_for(&mut self, needle: &str, what: &str, window_ms: u64) -> usize {
        let deadline = Instant::now() + Duration::from_millis(window_ms);
        let mut seen_at = None;
        loop {
            if seen_at.is_none() && self.rendered().contains(needle) {
                seen_at = Some(self.output.len());
            }
            if let Some(at) = seen_at {
                return at;
            }
            let mut buffer = [0u8; 8192];
            match self.file.read(&mut buffer) {
                Ok(0) | Err(_) => {}
                Ok(n) => {
                    self.output.extend_from_slice(&buffer[..n]);
                    self.feed(&buffer[..n]);
                }
            }
            if Instant::now() > deadline {
                let screen = self.rendered();
                let stream = String::from_utf8_lossy(&self.output);
                panic!("timeout waiting for {what} (needle {needle:?}); rendered screen:\n{screen}\npty stream:\n{stream}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The mock's recorded commands: one line per received command envelope.
fn find_prompt_with(commands: &Arc<Mutex<Vec<String>>>, text: &str) -> Option<String> {
    commands
        .lock()
        .expect("commands")
        .iter()
        .find(|line| line.contains("\"prompt\"") && line.contains(text))
        .cloned()
}

/// A private terminal session of this very binary, re-executed in child mode
/// with the pty slave as its controlling terminal and no inherited relay hints.
fn spawn_child(
    socket: &std::path::Path,
    slave: &OwnedFd,
    bindings: &pa_tui::keybindings::KeybindingsConfig,
) -> Child {
    fn make_terminal_session() -> std::io::Result<()> {
        nix::unistd::setsid()?;
        // SAFETY: stdin is the owned pty slave installed by Command before pre_exec.
        // The child has just created its own session, so it cannot steal a user tty.
        if unsafe { libc::ioctl(0, libc::TIOCSCTTY, 0) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command
        .arg("--exact")
        .arg("input_live_open_child_mode")
        .env(CHILD_SOCKET_ENV, socket)
        .env(
            "PA_INPUT_LIVE_BINDINGS",
            serde_json::to_string(bindings).expect("bindings"),
        )
        .env("TERM", "xterm-256color")
        .env_remove("TERM_PROGRAM")
        .env_remove("KITTY_WINDOW_ID")
        .env_remove("GHOSTTY_RESOURCES_DIR")
        .env_remove("WEZTERM_PANE")
        .env_remove("TMUX")
        .env_remove("STY")
        .env_remove("ZELLIJ")
        .env_remove("SSH_CONNECTION")
        .env_remove("SSH_TTY")
        .stdin(slave_as_stdio(slave))
        .stdout(slave_as_stdio(slave))
        .stderr(slave_as_stdio(slave));
    // SAFETY: the pre_exec hook is the supported std seam for
    // process-group setup; it runs post-fork pre-exec in the child only
    // and cannot disturb this process.
    unsafe { command.pre_exec(make_terminal_session) };
    command.spawn().expect("spawn child")
}

fn slave_as_stdio(slave: &OwnedFd) -> Stdio {
    slave.try_clone().expect("clone pty slave").into()
}

fn child_options(socket: PathBuf) -> InteractiveOptions {
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
        // The fresh-launch flow: a NEW session, so the startup chrome
        // mounts before the open (the exact surface the report measured).
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

/// The supervisor stand-in: the `create` answers immediately, the
/// `attach` stalls past the echo deadline, everything else answers
/// generically. Every received command is recorded for the typed-ahead
/// submit assertion.
struct MockSupervisor {
    listener: std::os::unix::net::UnixListener,
    socket_path: std::path::PathBuf,
}

impl MockSupervisor {
    fn bind(socket: &std::path::Path) -> MockSupervisor {
        MockSupervisor {
            listener: std::os::unix::net::UnixListener::bind(socket).expect("bind mock socket"),
            socket_path: socket.to_path_buf(),
        }
    }

    fn serve(self, commands: &Arc<Mutex<Vec<String>>>, connected: &std::sync::atomic::AtomicBool) {
        let (stream, _) = self.listener.accept().expect("accept");
        connected.store(true, std::sync::atomic::Ordering::SeqCst);
        let write_stream = stream.try_clone().expect("clone mock socket");
        let mut writer = write_stream;
        let mut reader = BufReader::new(stream);

        let hello = json!({
            "type": "daemon_hello",
            "socketPath": self.socket_path.display().to_string(),
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "serverCapabilities": ["kernel_bash_activity"],
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
            commands
                .lock()
                .expect("commands")
                .push(format!("{command_type}: {command}"));
            match command_type.as_str() {
                "create" => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "create",
                            "success": true,
                            "data": { "activeSessionId": "s1", "sessionId": "sess-1" },
                        }),
                    );
                }
                "attach" => {
                    // The stall: the open is provably in flight while the
                    // keystrokes are expected to echo.
                    std::thread::sleep(Duration::from_millis(OPEN_STALL_MS));
                    write_json(&mut writer, &attach_data(id));
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

fn write_json(writer: &mut std::os::unix::net::UnixStream, value: &Value) {
    let mut line = serde_json::to_string(value).expect("serialize mock frame");
    line.push('\n');
    writer.write_all(line.as_bytes()).expect("write mock frame");
    writer.flush().expect("flush mock frame");
}

/// The attach result: one live session whose transcript carries a marker
/// row — the content frame the fold paints once the stall ends.
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
                    "sessionName": "input live probe",
                    "model": "faux-1",
                    "isStreaming": false,
                    "isCompacting": false,
                    "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                },
                "messages": [
                    { "role": "user", "content": "openprobe marker question", "timestamp": 1 },
                    { "role": "assistant", "content": "openprobe settled answer", "provider": "scripted", "model": "faux-1", "timestamp": 2 },
                ],
                "lastEventSequence": 0,
                "lastEventCursor": null,
            },
            "client": { "id": "mock", "capabilities": [] },
            "lastEventSequence": 0,
            "lastEventCursor": null,
        },
    })
}
