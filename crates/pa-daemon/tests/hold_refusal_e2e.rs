//! Session-hold refusal e2e: a create whose file's holder is a live FOREIGN
//! process (another Rust daemon's worker on the shared session store) rejects
//! with the descriptive refusal and its typed wire info, and
//! the daemon logs the conflict. Untyped create failures keep the wrap (control).
#![allow(clippy::large_futures)]
// 64-bit-only targets; the narrowing casts sit at bounded OS boundaries.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Fn length is a style gate, not correctness.
#![allow(clippy::too_many_lines)]
// API-shape opinions, not defects; the surfaces are deliberate.
#![allow(
    clippy::unnecessary_wraps,
    clippy::zero_sized_map_values,
    clippy::struct_excessive_bools,
    clippy::struct_field_names
)]
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Daemon {
    child: Child,
    #[allow(dead_code)]
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// The timeout panic path cannot wait on the child; the test process exits and reaps it.
#[allow(clippy::zombie_processes)]
fn spawn_daemon(socket: &Path, agent_dir: &Path) -> Daemon {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pa-daemon"));
    command
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "15000",
        );
    let child = command.spawn().expect("spawn pa-daemon supervisor");
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

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("connect supervisor: {error}"),
            }
        };
        let write_stream = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer: write_stream,
        };
        let hello = client.read_line();
        assert_eq!(hello["type"], "daemon_hello");
        client
    }

    fn send_command(&mut self, id: &str, command: &Value) {
        let mut line = serde_json::to_string(&json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        }))
        .expect("serialize command");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set timeout");
        loop {
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("supervisor closed the connection"),
                Ok(_) if line.trim().is_empty() => line.clear(),
                Ok(_) => {
                    return serde_json::from_str(line.trim()).expect("parse response line");
                }
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for supervisor line: {error}"
                    );
                }
            }
        }
    }

    fn read_response(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(|v| v.as_str()) == Some(id) {
                return line;
            }
        }
    }
}

fn write_script(dir: &Path, responses: &[&str]) -> PathBuf {
    let script_path = dir.join("script.json");
    let scripted: Vec<Value> = responses
        .iter()
        .map(|text| json!({ "text": text }))
        .collect();
    std::fs::write(&script_path, json!({ "responses": scripted }).to_string())
        .expect("write script");
    script_path
}

fn session_file_of(client: &mut Client, id: &str, request_id: &str) -> String {
    client.send_command(
        request_id,
        &json!({ "type": "get_session_stats", "activeSessionId": id }),
    );
    let stats = client.read_response(request_id);
    assert_eq!(stats["success"], true, "stats failed: {stats}");
    stats["data"]["sessionFile"]
        .as_str()
        .expect("session file in stats")
        .to_string()
}

fn create_session(client: &mut Client, request_id: &str, config: &Value) -> (String, Value) {
    client.send_command(request_id, &json!({ "type": "create", "config": config }));
    let created = client.read_response(request_id);
    assert_eq!(created["success"], true, "create failed: {created}");
    let id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id in create response")
        .to_string();
    (id, created)
}

/// A create over a file another daemon's live worker holds answers the
/// session-hold refusal with the typed wire info.
#[test]
fn a_foreign_lease_holder_rejects_the_create_with_the_hold_refusal() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    // The sessions dir must exist before the create: on macOS a not-yet-existing
    // parent canonicalizes differently than the existing file (the /var ->
    // /private/var symlink), which breaks the kill path's append ownership.
    std::fs::create_dir_all(agent_dir.join("sessions")).expect("sessions dir");
    let _daemon = spawn_daemon(&socket, &agent_dir);

    let script_path = write_script(dir.path(), &["first scripted"]);
    let create_config = json!({
        "cwd": dir.path().to_string_lossy(),
        "sessionDir": agent_dir.join("sessions").to_string_lossy(),
        "script": script_path.to_string_lossy(),
    });

    let mut client = Client::connect(&socket);
    let (worker_id, _created) = create_session(&mut client, "c1", &create_config);
    let session_file = session_file_of(&mut client, &worker_id, "s1");

    // The worker dies so no resident serves the file: the holder below is
    // genuinely foreign to this daemon.
    client.send_command(
        "k1",
        &json!({ "type": "kill", "activeSessionId": worker_id }),
    );
    let killed = client.read_response("k1");
    assert_eq!(killed["success"], true, "kill failed: {killed}");

    // A worker of a second supervisor holds the lease. Both supervisors use the
    // same executable, so classification does not depend on the Cargo target path.
    let holder_socket = dir.path().join("holder.sock");
    let _holder_daemon = spawn_daemon(&holder_socket, &agent_dir);
    let mut holder_client = Client::connect(&holder_socket);
    holder_client.send_command(
        "h1",
        &json!({
            "type": "create",
            "sessionPath": session_file,
            "config": create_config,
        }),
    );
    let held = holder_client.read_response("h1");
    assert_eq!(held["success"], true, "holder create failed: {held}");
    let holder_id = held["data"]["id"]
        .as_str()
        .or_else(|| held["data"]["sessionId"].as_str())
        .expect("holder session id");
    let holder = pa_daemon::lease::live_lease_owner(&agent_dir, Path::new(&session_file))
        .expect("the second daemon's worker holds the lease");
    assert_eq!(holder.active_session_id.as_deref(), Some(holder_id));

    client.send_command(
        "c2",
        &json!({
            "type": "create",
            "sessionPath": session_file,
            "config": create_config,
        }),
    );
    let rejected = client.read_response("c2");
    assert_eq!(
        rejected["success"], false,
        "a create over a foreign-held file must reject: {rejected}"
    );
    let error = rejected["error"].as_str().expect("error text");
    assert!(
        error.starts_with("This session is currently open"),
        "the rejection leads with what happened: {error}"
    );
    assert!(
        error.contains(&format!("(active in {holder_id})")),
        "the rejection names the holder's active session: {error}"
    );
    assert!(
        error.contains(&format!(
            "prime-agent --daemon-socket <socket> --resume '{holder_id}'"
        )),
        "the continue path is the exact attach command: {error}"
    );
    assert!(
        error.contains(&format!("kill {}", holder.pid)),
        "the take-over path names the holder pid: {error}"
    );
    assert!(
        error.contains(&format!("Session: {holder_id}")),
        "the footer names the session: {error}"
    );
    // The holder worker runs the refusing supervisor's executable, so it is
    // another Rust build, never the TypeScript product.
    assert!(
        error.starts_with("This session is currently open in another Rust build of Prime Agent"),
        "the holder is classified, not guessed: {error}"
    );
    assert!(
        !error.contains("Session is already active in"),
        "the bare TS dump stays off the user-facing headline: {error}"
    );
    assert!(
        !error.contains("session worker create failed"),
        "the typed rejection relays verbatim, never under the untyped wrap: {error}"
    );

    // The typed wire info carries the raw fields (the TS `serializeDaemonError` shape)
    // for clients that render or act on it.
    assert_eq!(
        rejected["errorInfo"]["code"], "session_already_active",
        "{rejected}"
    );
    assert_eq!(
        rejected["errorInfo"]["activeSessionId"], holder_id,
        "{rejected}"
    );
    let info_path = rejected["errorInfo"]["sessionPath"]
        .as_str()
        .expect("sessionPath in errorInfo");
    let canonical = std::fs::canonicalize(&session_file).expect("canonicalize");
    assert_eq!(
        std::path::Path::new(info_path),
        canonical.as_path(),
        "the wire info carries the canonical session path: {rejected}"
    );

    // The daemon logs the detected conflict itself in the rotating log beside its
    // socket.
    let log_path = pa_daemon::paths::daemon_log_path(&socket, &agent_dir);
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains("create refused: session file"),
        "the daemon log records the refused create: {log}"
    );
    assert!(
        log.contains(&format!("is already active in {holder_id}")),
        "the daemon log names the holder: {log}"
    );
    assert!(
        log.contains("This session is currently open"),
        "the daemon log carries the refusal: {log}"
    );

    // An untyped create failure keeps the supervisor's wrap (the control): a
    // corrupt session file fails inside the worker's open.
    let corrupt = dir.path().join("corrupt-session.jsonl");
    std::fs::write(&corrupt, "this is not session jsonl\n").expect("write corrupt file");
    client.send_command(
        "c3",
        &json!({
            "type": "create",
            "sessionPath": corrupt.to_string_lossy(),
            "config": create_config,
        }),
    );
    let untyped = client.read_response("c3");
    assert_eq!(untyped["success"], false, "{untyped}");
    assert!(
        untyped["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("session worker create failed: "),
        "untyped create failures keep the wrap: {untyped}"
    );

    holder_client.send_command(
        "hk1",
        &json!({ "type": "kill", "activeSessionId": holder_id }),
    );
    let holder_killed = holder_client.read_response("hk1");
    assert_eq!(
        holder_killed["success"], true,
        "holder kill failed: {holder_killed}"
    );
}
