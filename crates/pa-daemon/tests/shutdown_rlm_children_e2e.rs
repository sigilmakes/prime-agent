//! Cooperative shutdown with RLM children (the drain deadlock): a session worker that
//! still owns RLM children closes each one with a routed `kill` BEFORE it can exit, so a
//! terminal shutdown gate that refuses that `kill` makes the worker refuse its own stop
//! and the drain can only end in a forced SIGTERM. This drives the real supervisor, a
//! real parent worker and a real child worker: the shutdown must be accepted, both
//! processes must leave, and the daemon log must never record a refusal or a pending stop.
//! Tests skip without a live kernel install.
// Stack-resident futures by design; boxing for a lint tick is a perf regression.
#![allow(clippy::large_futures)]
// 64-bit-only targets; narrowing casts sit at bounded OS/protocol boundaries.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Fn length is a style gate, not correctness.
#![allow(clippy::too_many_lines)]
// API-shape opinions, not defects; the surfaces are deliberate.
#![allow(clippy::unnecessary_wraps)]
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// The timeout panic path cannot wait on the child; the test exits and reaps it.
#[allow(clippy::zombie_processes)]
fn spawn_supervisor(socket: &Path, agent_dir: &Path, kernel_python: &Path) -> Daemon {
    let binary = env!("CARGO_BIN_EXE_pa-daemon");
    let child = Command::new(binary)
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        .env("PRIME_AGENT_KERNEL_PYTHON", kernel_python)
        // Hermetic agent dir: the ambient environment exports a real agent dir; point
        // every fallback at the test sandbox instead.
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env_remove("PRIME_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // A supervisor killed at teardown must not leak its session workers into later
        // test binaries: the supervisor-lost exit runs here.
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "15000",
        )
        .spawn()
        .expect("spawn pa-daemon supervisor");
    Daemon { child }
}

/// The kernel Python; `PA_E2E_KERNEL_PYTHON` points at an explicit interpreter instead.
fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_E2E_KERNEL_PYTHON") {
        let explicit = PathBuf::from(explicit);
        assert!(
            explicit.exists(),
            "PA_E2E_KERNEL_PYTHON {} not found",
            explicit.display()
        );
        return Some(explicit);
    }
    let candidate = PathBuf::from(std::env::var("HOME").map_or_else(
        |_| "/home/ubuntu/.prime/agent/kernel-venv/bin/python".to_string(),
        |home| format!("{home}/.prime/agent/kernel-venv/bin/python"),
    ));
    if candidate.exists() {
        Some(candidate)
    } else {
        eprintln!(
            "kernel python {} not found; skipping live RLM shutdown e2e",
            candidate.display()
        );
        None
    }
}

fn wait_socket_ready(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "supervisor socket never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// JSONL supervisor client (command envelopes, id-matched responses).
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> (Self, Value) {
        let stream = UnixStream::connect(socket).expect("connect supervisor");
        let write_stream = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer: write_stream,
        };
        let hello = client.read_line();
        (client, hello)
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
        let deadline = Instant::now() + Duration::from_mins(2);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set timeout");
        loop {
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("supervisor closed the connection"),
                Ok(_) if line.trim().is_empty() => line.clear(),
                Ok(_) => return serde_json::from_str(line.trim()).expect("parse response line"),
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
        let deadline = Instant::now() + Duration::from_mins(4);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }
}

fn wait_until<T>(budget: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "condition never became true");
        std::thread::sleep(Duration::from_millis(100));
    }
}

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

/// One scripted-response script file.
fn write_script(dir: &Path, name: &str, script: Value) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, script.to_string()).expect("write script");
    path
}

/// The parent's first turn: spawn one RLM child through the product `rlm.spawn` surface
/// and record its child id, so the worker owns a tracked child at shutdown time.
fn spawn_cell(receipt: &Path, error_receipt: &Path) -> String {
    format!(
        "import json, traceback\ntry:\n    handle = await rlm.spawn(\"run the lane task\", name=\"kid\")\n    open({receipt:?}, \"w\").write(json.dumps({{\"rlm_child_id\": handle.rlm_child_id}}))\n    print(handle.rlm_child_id)\nexcept Exception:\n    open({error_receipt:?}, \"w\").write(traceback.format_exc())\n    raise",
        receipt = receipt.display().to_string(),
        error_receipt = error_receipt.display().to_string(),
    )
}

/// Poll for a kernel cell's receipt content.
fn await_receipt(receipt: &Path, error_receipt: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_mins(2);
    loop {
        if let Ok(content) = std::fs::read_to_string(receipt) {
            if !content.is_empty() {
                return serde_json::from_str(&content).expect("receipt json");
            }
        }
        if let Ok(error) = std::fs::read_to_string(error_receipt) {
            panic!("the spawn cell failed: {error}");
        }
        assert!(
            Instant::now() < deadline,
            "kernel cell receipt never appeared at {}",
            receipt.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The worker process behind one resident descriptor: its recorded pid, guarded by the
/// session id in its environment (a stale descriptor pid is never signalled or awaited).
fn worker_pid(agent_dir: &Path, socket: &Path, active_session_id: &str) -> Option<u32> {
    let descriptor_path = pa_daemon::descriptor::descriptor_dir(agent_dir, socket)
        .join(format!("{active_session_id}.json"));
    let descriptor: Value = serde_json::from_str(&std::fs::read_to_string(descriptor_path).ok()?).ok()?;
    let pid = descriptor["pid"].as_u64()? as u32;
    let environ = std::fs::read_to_string(format!("/proc/{pid}/environ")).ok()?;
    environ
        .contains(&format!("={active_session_id}\0"))
        .then_some(pid)
}

/// A cooperative shutdown of a session that owns RLM children completes without a
/// refused stop and without the escalation the refusal used to force.
#[tokio::test]
async fn shutdown_with_rlm_children_drains_cooperatively() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let receipt = dir.path().join("spawn.json");
    let error_receipt = dir.path().join("spawn-error.txt");
    let parent_script = write_script(
        dir.path(),
        "parent.json",
        json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "python_repl",
                      "arguments": { "code": spawn_cell(&receipt, &error_receipt) } },
                ] },
                { "text": "spawn turn done" },
            ],
        }),
    );
    let child_script = write_script(
        dir.path(),
        "child.json",
        json!({ "engine": "faux", "responses": [ { "text": "kid answer" } ] }),
    );
    let mut daemon = spawn_supervisor(&socket, &agent_dir, &kernel_python);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    let sessions_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    client.send_command(
        "create",
        &json!({
            "type": "create",
            "name": "parent",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": parent_script.to_string_lossy(),
                "childScript": child_script.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("create");
    assert_eq!(created["success"], true, "create parent failed: {created}");
    let parent_id = created["data"]["activeSessionId"]
        .as_str()
        .expect("parent active session id")
        .to_string();

    client.send_command(
        "spawn",
        &json!({ "type": "prompt", "activeSessionId": parent_id, "message": "spawn the lane" }),
    );
    let prompted = client.read_response("spawn");
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
    client.send_command(
        "spawn-idle",
        &json!({ "type": "wait_for_idle", "activeSessionId": parent_id }),
    );
    assert_eq!(
        client.read_response("spawn-idle")["success"],
        true,
        "the spawn turn never settled"
    );
    let child = await_receipt(&receipt, &error_receipt);
    let child_id = child["rlm_child_id"].as_str().expect("child id").to_string();
    // The parent worker tracks the child: the close walk at shutdown kills it.
    let children = wait_until(Duration::from_secs(30), || {
        client.send_command(
            "children",
            &json!({ "type": "get_rlm_children", "activeSessionId": parent_id }),
        );
        let response = client.read_response("children");
        let rows = response["data"]["children"].as_array().cloned().unwrap_or_default();
        rows.iter()
            .any(|row| row["id"] == child_id.as_str())
            .then_some(rows)
    });
    assert!(!children.is_empty(), "the parent tracks its child");
    let parent_pid = wait_until(Duration::from_secs(30), || {
        worker_pid(&agent_dir, &socket, &parent_id)
    });
    let child_pid = wait_until(Duration::from_secs(30), || {
        children
            .iter()
            .filter_map(|row| row["activeSessionId"].as_str())
            .find_map(|active| worker_pid(&agent_dir, &socket, active))
    });
    assert!(process_alive(parent_pid), "the parent worker runs");
    assert!(process_alive(child_pid), "the child worker runs");

    client.send_command("shutdown", &json!({ "type": "shutdown" }));
    let shutdown = client.read_response("shutdown");
    assert_eq!(
        shutdown["success"], true,
        "a cooperative shutdown must be accepted, not refused as pending: {shutdown}"
    );
    let exit = wait_until(Duration::from_secs(60), || {
        daemon.child.try_wait().ok().flatten()
    });
    assert!(
        exit.success(),
        "the drained supervisor exits cleanly (no forced signal death): {exit:?}"
    );
    assert!(!socket.exists(), "the drained supervisor cleans its socket up");
    for (pid, who) in [(parent_pid, "parent"), (child_pid, "child")] {
        wait_until(Duration::from_secs(60), || (!process_alive(pid)).then_some(()));
        eprintln!("{who} worker {pid} left");
    }

    // The refusal is the exact shape of the deadlock: the drain never recorded one, and
    // no resident stop stayed pending.
    let log = std::fs::read_to_string(pa_daemon::paths::daemon_log_path(&socket, &agent_dir))
        .expect("daemon log");
    assert!(
        !log.contains("refused graceful shutdown"),
        "no worker refused the drain: {log}"
    );
    assert!(
        !log.contains("terminal shutdown remains pending"),
        "the drain completed without a pending stop: {log}"
    );
}
