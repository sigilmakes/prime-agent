// large_futures: stack futures on hot paths by design. too_many_lines: style gate
// only. Casts: 64-bit targets; narrowing sits at bounded OS/protocol boundaries.
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! End-to-end daemon-command tests: spawn the real `pa-daemon` supervisor on a
//! temp socket, drive `prime-agent` list/stop/rename against it, and diff the
//! output against goldens captured from the installed TS `prime-agent` binary.
//! A second test gives each CLI a private TS counterpart at its own product default.
//!
//! Goldens (captured from the live TS binary, protocol 7):
//!
//! ```text
//! $ prime-agent list            # one live session named "research"
//! name      id            status  age  model                         messages  clients
//! research  b72ad7009b11  idle    3s   prime-inference/z-ai/glm-5.3  0         0
//!
//! $ prime-agent list           # no live sessions
//! No active agents.
//!
//! $ prime-agent list --all     # no saved sessions
//! No agents.
//!
//! $ prime-agent stop <id>
//! ok
//!
//! $ prime-agent rename <id> gamma
//! Renamed 0f04b31cb2b6 to gamma
//!
//! $ prime-agent schedule add gamma "every 5 minutes" -- "do the thing"
//! Scheduled 0451e5a9-951c-457b-ae76-ea3142ea5d25 next=2026-09-16T18:36:59.287Z
//!
//! $ prime-agent schedule list
//! 0451e5a9-951c-457b-ae76-ea3142ea5d25 active next=9/16/2026, 6:36:59 PM last=- runs=0 schedule="every 5 minutes" prompt="do the thing"
//! ```
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Env keys this box's own worker sets; they must not leak into spawned
/// daemons. `PI_PACKAGE_DIR` too: a Rust-checkout value kills the TS CLI.
const SCRUB_ENV: [&str; 11] = [
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_TOKEN",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_ACTIVE_SESSION_ID",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_INSTANCE_ID",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_RECOVERY_JOURNAL",
    "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
    "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL",
    "PRIME_AGENT_INTERNAL_SESSION_LEASES",
    "PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID",
    "PI_PACKAGE_DIR",
    // The workspace gate pins its own endpoint. Counterpart CLIs must resolve
    // their selected private product defaults from this fixture's child TMPDIR.
    "PRIME_AGENT_DAEMON_SOCKET",
];

/// Locate the built `pa-daemon` binary next to this crate's `prime-agent`
/// binary (cargo builds the tested crate's bins only).
fn daemon_binary() -> PathBuf {
    let profile_dir = Path::new(env!("CARGO_BIN_EXE_prime-agent"))
        .parent()
        .expect("profile directory of the prime-agent binary")
        .to_path_buf();
    let daemon = profile_dir.join("pa-daemon");
    assert!(
        daemon.exists(),
        "pa-daemon binary not found at {}; run `cargo build -p pa-daemon` \
         (or the workspace gate `cargo test --workspace`) first",
        daemon.display()
    );
    daemon
}

/// The installed TS binary, when present (`PA_TS_BINARY` or `prime-agent` on PATH).
fn ts_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PA_TS_BINARY") {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }
    let probe = Command::new("prime-agent")
        .arg("--version")
        .output()
        .ok()?
        .status
        .success()
        .then(|| PathBuf::from("prime-agent"));
    probe
}

struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(clippy::zombie_processes)]
fn spawn_daemon(binary: &Path, socket: &Path, agent_dir: &Path) -> Daemon {
    let mut command = Command::new(binary);
    command
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for key in SCRUB_ENV {
        command.env_remove(key);
    }
    // A supervisor killed at teardown must not leak its session workers: the
    // supervisor-lost exit runs on this short window, not the 5-minute default.
    command.env(
        pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
        "15000",
    );
    let child = command.spawn().expect("spawn daemon supervisor");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if socket.exists() {
            return Daemon {
                child,
                socket: socket.to_path_buf(),
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("supervisor socket never appeared");
}

/// A raw JSONL protocol client, mirroring the `supervisor_e2e` harness.
struct Wire {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Wire {
    fn connect(socket: &Path) -> (Self, Value) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("connect daemon: {error}"),
            }
        };
        let writer = stream.try_clone().expect("clone socket");
        let mut wire = Wire {
            reader: BufReader::new(stream),
            writer,
        };
        let hello = wire.read_line();
        (wire, hello)
    }

    fn send_command(&mut self, id: &str, command: &Value) {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        let mut line = serde_json::to_string(&envelope).expect("serialize command");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_mins(1);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set timeout");
        loop {
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("daemon closed the connection"),
                Ok(_) if line.trim().is_empty() => line.clear(),
                Ok(_) => return serde_json::from_str(line.trim()).expect("parse daemon line"),
                Err(_) if Instant::now() < deadline => {}
                Err(error) => panic!("timed out waiting for daemon line: {error}"),
            }
        }
    }

    fn request(&mut self, id: &str, command: &Value) -> Value {
        self.send_command(id, command);
        loop {
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }
}

/// Create one session (the scripted engine for the Rust supervisor, a real worker for the TS one).
fn create_session(
    wire: &mut Wire,
    id: &str,
    name: &str,
    cwd: &Path,
    session_dir: &Path,
    script: Option<&Path>,
) -> String {
    let mut config = json!({
        "cwd": cwd.to_string_lossy(),
        "sessionDir": session_dir.to_string_lossy(),
    });
    if let Some(script) = script {
        config["script"] = json!(script.to_string_lossy());
    } else {
        // TS runtime config resolves these strings through its real registry.
        config["provider"] = json!("daemon-cli-fixture");
        config["model"] = json!("scripted");
    }
    let response = wire.request(
        id,
        &json!({ "type": "create", "name": name, "config": config }),
    );
    assert_eq!(response["success"], true, "create failed: {response}");
    response["data"]["activeSessionId"]
        .as_str()
        .or_else(|| response["data"]["id"].as_str())
        .expect("active session id")
        .to_string()
}

/// Run a CLI in an isolated env (agent dir + TMPDIR point into the temp dir).
fn run_cli(binary: &Path, dir: &Path, agent_dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(dir)
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("TMPDIR", dir)
        // C is available even in minimal CI images; requesting an uninstalled
        // locale makes the installed TS CLI's shell wrapper pollute stderr.
        // Node's ICU default for C is US English, matching the native formatter.
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("LC_TIME", "C")
        .env("TZ", "UTC");
    for key in SCRUB_ENV {
        command.env_remove(key);
    }
    command.output().expect("spawn CLI binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Replace volatile tokens (ages, hex ids, uuids, timestamps) so outputs compare equal.
fn normalize(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            line.split_whitespace()
                .map(normalize_token)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_token(token: &str) -> String {
    if let Some((key, value)) = token.split_once('=') {
        return format!("{key}={}", normalize_token(value));
    }
    if let Some(stripped) = token.strip_suffix(',') {
        return format!("{},", normalize_token(stripped));
    }
    // The strict, fixed-length id shapes are matched first: a 12-char hex
    // display id can read as an age (eleven digits plus a `d` suffix), and
    // the age matcher would otherwise fold it into `<time>` and break the
    // golden on a legal id draw.
    if is_hex_id(token) || is_uuid(token) || is_iso_timestamp(token) || is_local_date(token) {
        return "<timestamp>".to_string();
    }
    if is_age(token) || is_clock(token) || token == "AM" || token == "PM" {
        return "<time>".to_string();
    }
    token.to_string()
}

fn is_age(token: &str) -> bool {
    let digits = token
        .strip_suffix(['s', 'm', 'h', 'd', 'w', 'y'])
        .unwrap_or("");
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

fn is_hex_id(token: &str) -> bool {
    token.len() == 12 && token.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_uuid(token: &str) -> bool {
    token.len() == 36
        && token.split('-').count() == 5
        && token
            .split('-')
            .map(str::len)
            .zip([8, 4, 4, 4, 12])
            .all(|(actual, expected)| actual == expected)
}

fn is_iso_timestamp(token: &str) -> bool {
    token.len() >= 20 && token.starts_with(|c: char| c.is_ascii_digit()) && token.contains('T')
}

fn is_local_date(token: &str) -> bool {
    let Some((month_day_year, _)) = token.split_once('/') else {
        return false;
    };
    month_day_year.chars().all(|c| c.is_ascii_digit())
}

fn is_clock(token: &str) -> bool {
    let parts: Vec<&str> = token.split(':').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

#[test]
fn hex_display_id_normalizes_as_a_timestamp_even_when_it_reads_as_an_age() {
    // The display id is the last 12 hex chars of a random uuid, so one
    // draw in ~2800 is eleven digits plus `d` — the exact shape of the
    // age matcher. It is an id, and must fold into `<timestamp>` (the
    // CI flake this regression test pins; the real ages keep `<time>`).
    assert_eq!(normalize_token("12345678901d"), "<timestamp>");
    assert_eq!(normalize_token("b72ad7009b11"), "<timestamp>");
    assert_eq!(normalize_token("3s"), "<time>");
    assert_eq!(normalize_token("2h"), "<time>");
    assert_eq!(normalize_token("10:34:59"), "<time>");
    assert_eq!(normalize_token("PM"), "<time>");
}

/// Recursively sort object keys so TS (insertion order) and Rust (sorted) JSON
/// key orders compare equal.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonicalize(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

/// CLI args with the daemon socket flag prepended.
fn list_args<'a>(socket: &'a str, rest: &[&'a str]) -> Vec<&'a str> {
    let mut args: Vec<&'a str> = vec!["--daemon-socket", socket];
    args.extend(rest.iter().copied());
    args
}

/// The two CLIs under comparison and their shared sandbox.
struct CliPair {
    ts: PathBuf,
    rust: PathBuf,
    dir: PathBuf,
    ts_agent_dir: PathBuf,
    rust_agent_dir: PathBuf,
}

impl CliPair {
    /// Run one command through both CLIs and record any output mismatch.
    fn compare(&self, failures: &mut Vec<String>, ts_args: &[&str], rs_args: &[&str], label: &str) {
        let ts_out = run_cli(&self.ts, &self.dir, &self.ts_agent_dir, ts_args);
        let rs_out = run_cli(&self.rust, &self.dir, &self.rust_agent_dir, rs_args);
        let ok = ts_out.status.code() == rs_out.status.code()
            && normalize(&stdout(&ts_out)) == normalize(&stdout(&rs_out))
            && normalize(&stderr(&ts_out)) == normalize(&stderr(&rs_out));
        if !ok {
            failures.push(format!(
            "{label}:\n  ts exit {:?} stdout {:?} stderr {:?}\n  rs exit {:?} stdout {:?} stderr {:?}",
            ts_out.status.code(),
            stdout(&ts_out),
            stderr(&ts_out),
            rs_out.status.code(),
            stdout(&rs_out),
            stderr(&rs_out)
        ));
        }
    }

    /// JSON contains actual owner IDs and paths. Each CLI must preserve its own
    /// server's full payload rather than inventing cross-owner identities.
    fn compare_rpc_json(
        &self,
        failures: &mut Vec<String>,
        wires: &mut (&mut Wire, &mut Wire),
        args: &[&str],
        request: &Value,
        label: &str,
    ) {
        let ts_out = run_cli(&self.ts, &self.dir, &self.ts_agent_dir, args);
        let rs_out = run_cli(&self.rust, &self.dir, &self.rust_agent_dir, args);
        assert_eq!(
            ts_out.status.code(),
            Some(0),
            "TS {label} must succeed: {}",
            stderr(&ts_out)
        );
        assert_eq!(
            rs_out.status.code(),
            Some(0),
            "Rust {label} must succeed: {}",
            stderr(&rs_out)
        );
        let ts_json: Value = serde_json::from_str(&stdout(&ts_out)).expect("TS JSON");
        let rs_json: Value = serde_json::from_str(&stdout(&rs_out)).expect("Rust JSON");
        let ts_rpc = wires.0.request("json-oracle", request);
        let rs_rpc = wires.1.request("json-oracle", request);
        assert_eq!(ts_rpc["success"], true, "TS owner oracle");
        assert_eq!(rs_rpc["success"], true, "Rust counterpart owner oracle");
        if canonicalize(&ts_json) != canonicalize(&ts_rpc["data"]) {
            failures.push(format!(
                "{label}: TS CLI changed its actual owner RPC payload"
            ));
        }
        if canonicalize(&rs_json) != canonicalize(&rs_rpc["data"]) {
            failures.push(format!(
                "{label}: Rust CLI changed its actual owner RPC payload"
            ));
        }
    }
}

// Rust end-to-end: pa-daemon + prime-agent

#[test]
fn rust_daemon_cli_commands_end_to_end() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let socket = dir.path().join("daemon.sock");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let daemon = spawn_daemon(&daemon_binary(), &socket, &agent_dir);
    let socket_str = socket.to_string_lossy().to_string();

    let list = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list"]),
    );
    assert_eq!(list.status.code(), Some(0));
    assert_eq!(stdout(&list), "No active agents.\n");

    let list_all = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list", "--all"]),
    );
    assert_eq!(stdout(&list_all), "No agents.\n");

    let bogus = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list", "--bogus"]),
    );
    assert_eq!(bogus.status.code(), Some(1));
    assert_eq!(stderr(&bogus), "Error: Unknown list option: --bogus\n");

    let sessions_empty = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["sessions"]),
    );
    assert_eq!(sessions_empty.status.code(), Some(0));
    assert_eq!(stdout(&sessions_empty), "No active agents.\n");

    let sessions_all = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["sessions", "--all"]),
    );
    assert_eq!(stdout(&sessions_all), "No agents.\n");

    let sessions_bogus = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["sessions", "--bogus"]),
    );
    assert_eq!(sessions_bogus.status.code(), Some(1));
    assert_eq!(
        stderr(&sessions_bogus),
        "Error: Unknown sessions option: --bogus\n"
    );

    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        json!({ "responses": [{ "text": "hello", "delayMs": 0 }] }).to_string(),
    )
    .expect("write script");
    let (mut wire, _hello) = Wire::connect(&daemon.socket);
    let session = create_session(
        &mut wire,
        "c1",
        "parity",
        dir.path(),
        &sessions,
        Some(&script),
    );
    assert_eq!(session.len(), 12, "short active id: {session}");

    let list = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list"]),
    );
    assert_eq!(list.status.code(), Some(0), "{}", stderr(&list));
    assert_eq!(
        normalize(&stdout(&list)),
        "name id status age model messages clients\n\
         parity <timestamp> idle <time> 0 0\n",
        "table shape must match the TS golden"
    );

    // The --json output carries the daemon's summary rows unmodified.
    let list_json = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list", "--json"]),
    );
    let parsed: Value = serde_json::from_str(&stdout(&list_json)).expect("valid json list");
    let row = &parsed["sessions"][0];
    for key in [
        "id",
        "sessionId",
        "cwd",
        "lifecycle",
        "activity",
        "isSessionActive",
        "isStreaming",
        "isCompacting",
        "attachedClients",
        "messageCount",
        "sessionActions",
    ] {
        assert!(row.get(key).is_some(), "summary row must carry {key}");
    }

    // Golden: the sessions operator table over the live session (TS binary
    // shape): the last-heard, activity, error, and usage cells stay empty for
    // a healthy idle session.
    let sessions_table = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["sessions"]),
    );
    assert_eq!(
        sessions_table.status.code(),
        Some(0),
        "{}",
        stderr(&sessions_table)
    );
    assert_eq!(
        normalize(&stdout(&sessions_table)),
        "name status activity last heard error usage\nparity idle\n",
        "sessions table shape must match the TS golden"
    );

    // The sessions --json dump is the same list RPC data (the sessions
    // command reuses it).
    let sessions_json = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["sessions", "--json"]),
    );
    let parsed: Value = serde_json::from_str(&stdout(&sessions_json)).expect("valid json sessions");
    assert_eq!(parsed["sessions"][0]["sessionName"], "parity");

    let rename = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["rename", session.as_str(), "renamed"]),
    );
    assert_eq!(rename.status.code(), Some(0), "{}", stderr(&rename));
    assert_eq!(stdout(&rename), format!("Renamed {session} to renamed\n"));

    let stop = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["stop", session.as_str()]),
    );
    assert_eq!(stop.status.code(), Some(0), "{}", stderr(&stop));
    assert_eq!(stdout(&stop), "ok\n");

    let list = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list"]),
    );
    assert_eq!(stdout(&list), "No active agents.\n");

    let stop_missing = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["stop", "missing-selector"]),
    );
    assert_eq!(stop_missing.status.code(), Some(1));
    assert_eq!(
        stderr(&stop_missing),
        "Error: Unknown active session: missing-selector\n"
    );

    // Saved sessions still appear in list --all after the stop.
    let list_all = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["list", "--all"]),
    );
    let normalized = normalize(&stdout(&list_all));
    assert_eq!(normalized.lines().count(), 2, "saved row after kill");
    assert!(normalized.contains("<timestamp>"), "{normalized}");

    // send_message by the stopped worker's active id answers the unknown-session
    // error: active ids are not durable; the catalog keys saved sessions by id and name.
    let send = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["send", session.as_str(), "hello"]),
    );
    assert_eq!(send.status.code(), Some(1));
    assert_eq!(
        stderr(&send),
        format!("Error: Unknown active session: {session}\n")
    );

    // The saved-session wake (messaging-7): sending by the NAME wakes it — the
    // catalog-resolves the selector, spawns a worker, and delivers.
    let send_wake = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &list_args(&socket_str, &["send", "renamed", "hello again"]),
    );
    assert_eq!(send_wake.status.code(), Some(0), "{}", stderr(&send_wake));
    assert_eq!(stdout(&send_wake), "Sent to renamed\n");
}

#[test]
fn cli_connect_error_matches_ts_golden() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let missing = dir.path().join("missing.sock");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let output = run_cli(
        &cli,
        dir.path(),
        &agent_dir,
        &[
            "--daemon-socket",
            missing.to_string_lossy().as_ref(),
            "list",
        ],
    );
    assert_eq!(output.status.code(), Some(1));

    let text = stderr(&output).trim_end().to_string();
    let expected_prefix = format!(
        "Error: Failed to connect to the Prime Agent daemon: connect ENOENT {missing}. Socket: {missing}. Daemon log: ",
        missing = missing.display()
    );
    assert!(
        text.starts_with(&expected_prefix),
        "connect error text: {text}"
    );
    let log_detail = text
        .strip_prefix(&expected_prefix)
        .expect("daemon log detail");
    let log_name = log_detail.rsplit('/').next().expect("daemon log file name");
    let hash_end = log_name
        .strip_prefix(&format!(
            "{}.",
            missing.file_name().unwrap().to_string_lossy()
        ))
        .and_then(|rest| rest.strip_suffix(".log."))
        .expect("daemon log basename and tail");
    assert!(
        log_detail.starts_with(agent_dir.to_string_lossy().as_ref()),
        "log under agent dir: {text}"
    );
    assert_eq!(text, format!("{expected_prefix}{log_detail}"));
    assert_eq!(hash_end.len(), 8, "8-hex log suffix: {text}");
    assert!(
        hash_end.chars().all(|c| c.is_ascii_hexdigit()),
        "hex log suffix: {text}"
    );
}

// Schedule CLI usage surface: no daemon, validation fires first

/// The schedule commands' usage errors fire in the router's validation, before
/// any daemon request (`--daemon-socket` before the command rotates into the
/// operands; the TS CLI rejects the same shape).
#[test]
fn schedule_usage_errors_and_valid_invocations() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).expect("sessions dir");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let rotated_socket = dir.path().join("bogus.sock");
    let rotated_socket = rotated_socket.to_str().expect("socket path");

    let usage_shapes = [
        (
            vec!["schedule", "cancel"],
            "Error: Usage: prime-agent schedule cancel <job-id>\n",
        ),
        (
            vec!["schedule", "cancel", "a", "b"],
            "Error: Usage: prime-agent schedule cancel <job-id>\n",
        ),
        (
            vec!["schedule", "list", "--bogus-flag"],
            "Error: Usage: prime-agent schedule list [--all] [agent] [--json]\n",
        ),
        (
            vec!["--daemon-socket", rotated_socket, "schedule", "list"],
            "Error: Usage: prime-agent schedule list [--all] [agent] [--json]\n",
        ),
        (
            vec![
                "--daemon-socket",
                rotated_socket,
                "schedule",
                "cancel",
                "id",
            ],
            "Error: Usage: prime-agent schedule cancel <job-id>\n",
        ),
    ];
    for (args, golden) in usage_shapes {
        let out = run_cli(&cli, dir.path(), &agent_dir, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&out), "", "{args:?}");
        assert_eq!(stderr(&out), golden, "{args:?}");
    }

    // Valid shapes pass validation and fail only at the connection attempt.
    let valid_shapes = [
        vec!["schedule", "list"],
        vec!["schedule", "list", "--all"],
        vec!["schedule", "list", "-a"],
        vec!["schedule", "list", "--json"],
        vec!["schedule", "list", "some-agent"],
        vec!["schedule", "cancel", "fake-id"],
        vec!["schedule", "cancel", "--json", "fake-id"],
    ];
    for args in valid_shapes {
        let out = run_cli(&cli, dir.path(), &agent_dir, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let err = stderr(&out);
        assert!(
            err.starts_with("Error: Failed to connect to the Prime Agent daemon:"),
            "{args:?}: {err}"
        );
    }
}

/// Start a TS counterpart at one product's actual default socket. Both public
/// clients use their own defaults; schedule has no socket-override operand.
#[allow(clippy::zombie_processes)]
fn spawn_ts_daemon(ts: &Path, socket: &Path, work: &Path, tmp: &Path, agent_dir: &Path) -> Daemon {
    std::fs::create_dir_all(socket.parent().expect("socket parent")).expect("socket parent dir");
    let mut command = Command::new(ts);
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(socket)
        .current_dir(work)
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("TMPDIR", tmp)
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .env("LC_TIME", "en_US.UTF-8")
        .env("TZ", "UTC")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for key in SCRUB_ENV {
        command.env_remove(key);
    }
    command.env(
        pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
        "15000",
    );
    let child = command.spawn().expect("spawn TS daemon counterpart");
    let daemon = Daemon {
        child,
        socket: socket.to_path_buf(),
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while UnixStream::connect(socket).is_err() {
        assert!(
            Instant::now() < deadline,
            "selected private default socket unreachable"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon
}

/// Install a documented TS custom provider in one private owner. The required
/// literal API key is fixture data, not a credential: streamSimple performs no
/// network I/O. Real SDK startup, model selection, journal writes and wake run.
fn install_ts_scripted_provider(agent_dir: &Path) {
    let extensions = agent_dir.join("extensions");
    std::fs::create_dir_all(&extensions).expect("fixture extension directory");
    std::fs::write(
        extensions.join("daemon-cli-fixture.ts"),
        r#"import { createAssistantMessageEventStream } from "@earendil-works/pi-ai";
export default function (pi) {
    pi.registerProvider("daemon-cli-fixture", {
        api: "daemon-cli-fixture-api",
        baseUrl: "http://127.0.0.1:1/unused-scripted-provider",
        apiKey: "fixture-only-not-a-credential",
        models: [{
            id: "scripted", name: "Scripted fixture", reasoning: false,
            input: ["text"], contextWindow: 128000, maxTokens: 4096,
            cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }
        }],
        streamSimple(model) {
            const stream = createAssistantMessageEventStream();
            const message = {
                role: "assistant", content: [{ type: "text", text: "fixture reply" }],
                api: model.api, provider: model.provider, model: model.id,
                usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0,
                    totalTokens: 0,
                    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
                stopReason: "stop", timestamp: Date.now()
            };
            stream.push({ type: "start", partial: message });
            stream.push({ type: "done", reason: "stop", message });
            stream.end();
            return stream;
        }
    });
}
"#,
    )
    .expect("write documented custom-provider extension");
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({"defaultProvider":"daemon-cli-fixture", "defaultModel":"scripted"}).to_string(),
    )
    .expect("write private saved model selection");
}

/// Prove actual configured selection before sends and after real saved wake.
fn assert_ts_fixture_model(wire: &mut Wire, active_id: &str) {
    let state = wire.request(
        "fixture-model",
        &json!({"type":"get_state", "activeSessionId":active_id}),
    );
    assert_eq!(state["success"], true, "actual owner model state");
    assert_eq!(state["data"]["model"]["provider"], "daemon-cli-fixture");
    assert_eq!(state["data"]["model"]["id"], "scripted");
}

/// Check completed real SDK turns, not merely two matching error receipts.
fn assert_ts_fixture_turns(wire: &mut Wire, name: &str, expected: usize) {
    let listed = wire.request("fixture-live", &json!({"type":"list", "all":false}));
    assert_eq!(listed["success"], true);
    let row = listed["data"]["sessions"]
        .as_array()
        .expect("actual live owner rows")
        .iter()
        .find(|row| row["sessionName"] == name)
        .expect("actual named live owner");
    let active_id = row["id"].as_str().expect("actual active owner ID");
    assert_ts_fixture_model(wire, active_id);
    let idle = wire.request(
        "fixture-idle",
        &json!({"type":"wait_for_idle", "activeSessionId":active_id}),
    );
    assert_eq!(idle["success"], true, "actual completed owner turn");
    let response = wire.request(
        "fixture-messages",
        &json!({"type":"get_messages", "activeSessionId":active_id}),
    );
    assert_eq!(response["success"], true);
    let assistants: Vec<_> = response["data"]["messages"]
        .as_array()
        .expect("actual SDK messages")
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect();
    assert_eq!(
        assistants.len(),
        expected,
        "actual completed SDK assistant turns"
    );
    for message in assistants {
        assert_eq!(message["provider"], "daemon-cli-fixture");
        assert_eq!(message["model"], "scripted");
        assert_eq!(message["stopReason"], "stop");
        assert_eq!(
            message["content"],
            json!([{"type":"text", "text":"fixture reply"}])
        );
    }
}

// TS differential: each CLI uses its own product-default TS counterpart server

#[test]
fn ts_daemon_differential_cli_output() {
    let Some(ts) = ts_binary() else {
        eprintln!("SKIPPED: TS prime-agent binary not found (set PA_TS_BINARY)");
        return;
    };
    let dir = tempfile::TempDir::new().expect("temp dir");
    let ts_agent_dir = dir.path().join("ts-agent");
    let rust_agent_dir = dir.path().join("rust-agent");
    let ts_sessions = ts_agent_dir.join("sessions");
    let rust_sessions = rust_agent_dir.join("sessions");
    let work = dir.path().join("work");
    for path in [&ts_sessions, &rust_sessions, &work] {
        std::fs::create_dir_all(path).expect("private counterpart directories");
    }
    install_ts_scripted_provider(&ts_agent_dir);
    install_ts_scripted_provider(&rust_agent_dir);
    // These namespaces intentionally differ in the product sources.
    // TS schedule does not honor Rust's socket-override environment variable.
    // SCRUB_ENV removes the gate's inherited Rust override before either CLI.
    // Do not mutate the test process's TMPDIR or alias either live endpoint.
    let uid = pa_core::platform::process::current_user_id();
    let ts_socket = dir.path().join(format!("prime-agent-{uid}/daemon.sock"));
    let rust_socket = dir
        .path()
        .join(format!("prime-agent-rust-{uid}/daemon.sock"));
    assert_ne!(ts_socket, rust_socket);
    let _ts_daemon = spawn_ts_daemon(&ts, &ts_socket, &work, dir.path(), &ts_agent_dir);
    let _rust_counterpart = spawn_ts_daemon(&ts, &rust_socket, &work, dir.path(), &rust_agent_dir);
    let (mut ts_wire, ts_hello) = Wire::connect(&ts_socket);
    let (mut rs_wire, rs_hello) = Wire::connect(&rust_socket);
    assert_eq!(
        ts_hello["type"], "daemon_hello",
        "TS selected-default reachability"
    );
    assert_eq!(
        rs_hello["type"], "daemon_hello",
        "Rust selected-default reachability"
    );
    assert!(ts_socket.starts_with(dir.path()) && rust_socket.starts_with(dir.path()));
    let rust = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let pair = CliPair {
        ts: ts.clone(),
        rust: rust.clone(),
        dir: dir.path().to_path_buf(),
        ts_agent_dir: ts_agent_dir.clone(),
        rust_agent_dir: rust_agent_dir.clone(),
    };
    let mut failures: Vec<String> = Vec::new();
    let compare = |failures: &mut Vec<String>, ts_args: &[&str], rs_args: &[&str], label: &str| {
        pair.compare(failures, ts_args, rs_args, label);
    };

    // Text stays a cross-product comparison using the original normalizer.
    compare(&mut failures, &["list"], &["list"], "empty list");
    let ts_primary = create_session(&mut ts_wire, "c1", "parity", &work, &ts_sessions, None);
    let rs_primary = create_session(&mut rs_wire, "c1", "parity", &work, &rust_sessions, None);
    let ts_target = create_session(&mut ts_wire, "c2", "parity-c", &work, &ts_sessions, None);
    let rs_target = create_session(&mut rs_wire, "c2", "parity-c", &work, &rust_sessions, None);
    assert_ne!(ts_primary, rs_primary, "actual independent primary owners");
    assert_ne!(
        ts_target, rs_target,
        "actual independent send-target owners"
    );
    for (wire, ids) in [
        (&mut ts_wire, [&ts_primary, &ts_target]),
        (&mut rs_wire, [&rs_primary, &rs_target]),
    ] {
        for active_id in ids {
            assert_ts_fixture_model(wire, active_id);
        }
    }
    compare(&mut failures, &["list"], &["list"], "session table");
    pair.compare_rpc_json(
        &mut failures,
        &mut (&mut ts_wire, &mut rs_wire),
        &["list", "--json"],
        &json!({"type":"list", "all":false}),
        "list --json",
    );

    // The sessions operator table (TS #2422): the same list RPC as the one-line table.
    compare(
        &mut failures,
        &["sessions"],
        &["sessions"],
        "sessions table",
    );
    compare(
        &mut failures,
        &["sessions", "--all"],
        &["sessions", "--all"],
        "sessions table --all",
    );
    compare(
        &mut failures,
        &["sessions", "--bogus"],
        &["sessions", "--bogus"],
        "sessions unknown option",
    );
    pair.compare_rpc_json(
        &mut failures,
        &mut (&mut ts_wire, &mut rs_wire),
        &["sessions", "--json"],
        &json!({"type":"list", "all":false}),
        "sessions --json",
    );
    let ts_rename = ["rename", ts_primary.as_str(), "parity-renamed"];
    let rs_rename = ["rename", rs_primary.as_str(), "parity-renamed"];
    compare(&mut failures, &ts_rename, &rs_rename, "rename");
    // Sends start a real turn, so each CLI sends to its own session; same receipt shape.
    let ts_send = run_cli(
        &ts,
        dir.path(),
        &ts_agent_dir,
        &["send", ts_target.as_str(), "differential message"],
    );
    let rs_send = run_cli(
        &rust,
        dir.path(),
        &rust_agent_dir,
        &["send", rs_target.as_str(), "differential message"],
    );
    if ts_send.status.code() != Some(0)
        || rs_send.status.code() != Some(0)
        || !stderr(&ts_send).is_empty()
        || !stderr(&rs_send).is_empty()
        || !stdout(&ts_send).starts_with("Sent to ")
        || !stdout(&rs_send).starts_with("Sent to ")
    {
        failures.push(format!(
            "send: ts {:#?} vs rs {:#?}",
            (ts_send.status.code(), stdout(&ts_send), stderr(&ts_send)),
            (rs_send.status.code(), stdout(&rs_send), stderr(&rs_send))
        ));
    }
    if ts_send.status.success() && rs_send.status.success() {
        assert_ts_fixture_turns(&mut ts_wire, "parity-c", 1);
        assert_ts_fixture_turns(&mut rs_wire, "parity-c", 1);
    }
    compare(
        &mut failures,
        &["schedule", "list"],
        &["schedule", "list"],
        "schedule list",
    );
    let ts_add = [
        "schedule",
        "add",
        ts_primary.as_str(),
        "every 10 minutes",
        "--",
        "differential job",
    ];
    let rs_add = [
        "schedule",
        "add",
        rs_primary.as_str(),
        "every 10 minutes",
        "--",
        "differential job",
    ];
    compare(&mut failures, &ts_add, &rs_add, "schedule add");
    compare(
        &mut failures,
        &["schedule", "list"],
        &["schedule", "list"],
        "schedule list after add",
    );
    compare(
        &mut failures,
        &["schedule", "list", "--all"],
        &["schedule", "list", "--all"],
        "schedule list --all",
    );
    pair.compare_rpc_json(
        &mut failures,
        &mut (&mut ts_wire, &mut rs_wire),
        &["schedule", "list", "--json"],
        &json!({"type":"cron_list", "includeInactive":false}),
        "schedule list --json",
    );
    // Each real CLI creates exactly one job in its own owner's store, then
    // cancels that exact returned job. No shared-state second add or fake ID.
    let ts_schedule_json = run_cli(
        &ts,
        dir.path(),
        &ts_agent_dir,
        &["schedule", "list", "--json"],
    );
    let rs_schedule_json = run_cli(
        &rust,
        dir.path(),
        &rust_agent_dir,
        &["schedule", "list", "--json"],
    );
    let ts_jobs: Value =
        serde_json::from_str(&stdout(&ts_schedule_json)).expect("TS schedule JSON");
    let rs_jobs: Value =
        serde_json::from_str(&stdout(&rs_schedule_json)).expect("Rust schedule JSON");
    let ts_jobs = ts_jobs["jobs"].as_array().expect("TS jobs array");
    let rs_jobs = rs_jobs["jobs"].as_array().expect("Rust jobs array");
    assert_eq!(ts_jobs.len(), 1, "one actual TS CLI job");
    assert_eq!(rs_jobs.len(), 1, "one actual Rust CLI job");
    let ts_job_id = ts_jobs[0]["id"].as_str().expect("actual TS job ID");
    let rs_job_id = rs_jobs[0]["id"].as_str().expect("actual Rust job ID");
    compare(
        &mut failures,
        &["schedule", "cancel", ts_job_id],
        &["schedule", "cancel", rs_job_id],
        "schedule cancel",
    );
    compare(
        &mut failures,
        &["schedule", "list"],
        &["schedule", "list"],
        "schedule list after cancel",
    );
    for (binary, owner_dir) in [(&ts, &ts_agent_dir), (&rust, &rust_agent_dir)] {
        let cancelled = run_cli(binary, dir.path(), owner_dir, &["schedule", "list"]);
        assert_eq!(cancelled.status.code(), Some(0));
        assert_eq!(stdout(&cancelled), "No scheduled prompts.\n");
    }
    let ts_socket_arg = ts_socket.to_string_lossy().to_string();
    let rs_socket_arg = rust_socket.to_string_lossy().to_string();
    compare(
        &mut failures,
        &["schedule", "cancel"],
        &["schedule", "cancel"],
        "schedule cancel without id",
    );
    compare(
        &mut failures,
        &["schedule", "cancel", "a", "b"],
        &["schedule", "cancel", "a", "b"],
        "schedule cancel with two ids",
    );
    compare(
        &mut failures,
        &["schedule", "list", "--bogus-flag"],
        &["schedule", "list", "--bogus-flag"],
        "schedule list unknown flag",
    );
    compare(
        &mut failures,
        &[
            "--daemon-socket",
            ts_socket_arg.as_str(),
            "schedule",
            "list",
        ],
        &[
            "--daemon-socket",
            rs_socket_arg.as_str(),
            "schedule",
            "list",
        ],
        "daemon-socket before schedule list",
    );
    compare(
        &mut failures,
        &[
            "--daemon-socket",
            ts_socket_arg.as_str(),
            "schedule",
            "cancel",
            "id",
        ],
        &[
            "--daemon-socket",
            rs_socket_arg.as_str(),
            "schedule",
            "cancel",
            "id",
        ],
        "daemon-socket before schedule cancel",
    );
    // TS stops the primary session, the Rust CLI the secondary: same golden output.
    let ts_stop_args: Vec<&str> = vec!["stop", ts_primary.as_str()];
    let rust_stop_args: Vec<&str> = vec!["stop", rs_target.as_str()];
    compare(&mut failures, &ts_stop_args, &rust_stop_args, "stop");

    // Saved-session wake on the TS daemon: each CLI sends to its own stopped session;
    // the compared observable is the receipt line shape.
    let ts_wake = run_cli(
        &ts,
        dir.path(),
        &ts_agent_dir,
        &["send", "parity-renamed", "wake from ts"],
    );
    let rs_wake = run_cli(
        &rust,
        dir.path(),
        &rust_agent_dir,
        &["send", "parity-c", "wake from rs"],
    );
    if ts_wake.status.code() != Some(0)
        || rs_wake.status.code() != Some(0)
        || !stderr(&ts_wake).is_empty()
        || !stderr(&rs_wake).is_empty()
        || stdout(&ts_wake) != "Sent to parity-renamed\n"
        || stdout(&rs_wake) != "Sent to parity-c\n"
    {
        failures.push(format!(
            "send wakes a saved session: ts {:#?} vs rs {:#?}",
            (ts_wake.status.code(), stdout(&ts_wake), stderr(&ts_wake)),
            (rs_wake.status.code(), stdout(&rs_wake), stderr(&rs_wake))
        ));
    }

    if ts_wake.status.success() && rs_wake.status.success() {
        assert_ts_fixture_turns(&mut ts_wire, "parity-renamed", 1);
        assert_ts_fixture_turns(&mut rs_wire, "parity-c", 2);
    }

    assert!(
        failures.is_empty(),
        "TS differential mismatches:\n{}",
        failures.join("\n---\n")
    );
}
