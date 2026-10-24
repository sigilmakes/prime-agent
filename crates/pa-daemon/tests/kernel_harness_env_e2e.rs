//! Native harness e2e: a persisted session's real kernel uses the explicitly
//! enabled Lua plugin and host-owned logical session/global scopes. Writes land
//! in the single native store, never legacy Python-owned harness files.
//! Skips (with a note) without a live kernel install.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The kernel Python with prime-agent-runtime installed; set
/// `PA_E2E_KERNEL_PYTHON` to point at an explicit interpreter instead.
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
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live kernel-harness-env e2e",
        candidate.display()
    );
    None
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(clippy::zombie_processes)]
fn spawn_supervisor(socket: &Path, agent_dir: &Path, kernel_python: &Path) -> Daemon {
    std::fs::create_dir_all(agent_dir).expect("agent dir");
    let child = Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        .env("PRIME_AGENT_KERNEL_PYTHON", kernel_python)
        // Hermetic agent dir: point every fallback at the test sandbox.
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        // Hermetic harness resolution: ambient session/harness dirs would
        // mask a missing kernel-env export (the regression this test guards).
        .env_remove("RLM_SESSION_DIR")
        .env_remove("RLM_HARNESS_STATE_DIR")
        .env_remove("RLM_GLOBAL_HARNESS_STATE_DIR")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "15000",
        )
        .spawn()
        .expect("spawn pa-daemon supervisor");
    Daemon { child }
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
        let writer = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
        };
        let hello = client.read_line();
        (client, hello)
    }

    fn send_command(&mut self, id: &str, command: &Value) {
        self.write_line(&json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        }));
    }

    fn write_line(&mut self, value: &Value) {
        let mut line = serde_json::to_string(value).expect("serialize line");
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
                Ok(_) => return serde_json::from_str(line.trim()).expect("parse line"),
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

/// The scripted cell creates and reads scoped raw records through the native
/// plugin. Payload identity fields stay inside `value`, separate from host identity.
fn harness_cell(receipt: &Path) -> String {
    format!(
        r#"import json, traceback, rlm
receipt = {{"ok": False, "error": ""}}
try:
    assert not hasattr(rlm, 'harness') and not hasattr(rlm.rlm, 'harness')
    local_value = {{"id":"kernel-local-memory", "title":"kernel session-local memory", "content":"persisted from the session kernel"}}
    global_value = {{"id":"kernel-global-memory", "title":"kernel global memory", "content":"persisted from the session kernel"}}
    await plugin.harness.create(kind="memory", id="kernel-local-memory", value=local_value)
    local_row = await plugin.harness.get(kind="memory", id="kernel-local-memory")
    assert local_row["value"] == local_value
    assert local_row["identity"]["scope"].startswith("session:")
    await plugin.harness.create(kind="memory", id="kernel-global-memory", value=global_value, scope="global")
    global_row = await plugin.harness.get(kind="memory", id="kernel-global-memory", scope="global")
    assert global_row["value"] == global_value
    assert global_row["identity"]["scope"] == "global"
    assert await plugin.harness.get(kind="memory", id="kernel-local-memory", scope="global") is None
    assert await plugin.harness.get(kind="memory", id="kernel-global-memory") is None
    receipt = {{"ok": True, "error": "", "local":local_row, "global":global_row}}
except Exception:
    receipt = {{"ok": False, "error": traceback.format_exc()}}
open({receipt_path:?}, "w").write(json.dumps(receipt))
"#,
        receipt_path = receipt.to_string_lossy(),
    )
}

fn write_faux_script(dir: &Path, cell: &str) -> PathBuf {
    let script = dir.join("faux.json");
    std::fs::write(
        &script,
        json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "python_repl", "arguments": {
                        "code": cell,
                    } },
                ] },
                { "text": "done" },
            ],
        })
        .to_string(),
    )
    .expect("write faux script");
    script
}

/// The durable native slot preserves raw payload fields and host-owned scope.
fn assert_stored_memory(state: &Value, id: &str, title: &str, content: &str, scope: &str) {
    let identity = json!({"scope":scope, "kind":"memory", "id":id});
    let records = state["records"].as_array().expect("native records");
    let matching: Vec<_> = records
        .iter()
        .filter(|row| row["identity"] == identity)
        .collect();
    assert_eq!(matching.len(), 1, "exactly one scoped native memory");
    let entry = matching[0];
    assert!(entry["revision"]
        .as_u64()
        .is_some_and(|revision| revision > 0));
    assert_eq!(
        entry["value"],
        json!({
            "state":"Present",
            "value":{"id":id, "title":title, "content":content}
        })
    );
}

fn read_state(path: &Path) -> Value {
    let content = std::fs::read_to_string(path).expect("read harness state");
    serde_json::from_str(&content).expect("parse harness state")
}

#[test]
fn persistent_session_kernel_writes_the_session_local_harness_store() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({"enableBuiltinSkills":false,"bundledSkills":{"websearch":false},"autoRefine":{"enabled":false},"plugins":{"harness":{"enabled":true}}}).to_string(),
    ).expect("explicitly enable native harness plugin");
    let socket = dir.path().join("supervisor.sock");
    let receipts_dir = dir.path().join("receipts");
    std::fs::create_dir_all(&receipts_dir).expect("receipts dir");
    let receipt = receipts_dir.join("harness.json");
    let script = write_faux_script(dir.path(), &harness_cell(&receipt));

    let _daemon = spawn_supervisor(&socket, &agent_dir, &kernel_python);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    // A persisted session: its artifact dir is `dirname(sessionDir)/session-artifacts/<id>`
    // (the same rule the session manager applies).
    let sessions_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    client.send_command(
        "c1",
        &json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": script.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["sessionId"].as_str().expect("session id");

    client.send_command(
        "t1",
        &json!({ "type": "prompt", "activeSessionId": session_id, "message": "run the cell" }),
    );
    let prompted = client.read_response("t1");
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
    client.send_command(
        "t1-idle",
        &json!({ "type": "wait_for_idle", "activeSessionId": session_id }),
    );
    let idle = client.read_response("t1-idle");
    assert_eq!(idle["success"], true, "wait_for_idle failed: {idle}");

    // The turn ended, so the cell's receipt is written: a `false` ok carries the
    // kernel-side failure (the instructive error when the env vars are missing).
    let cell_receipt = std::fs::read_to_string(&receipt)
        .unwrap_or_else(|error| panic!("kernel cell receipt missing: {error}"));
    let cell_receipt: Value =
        serde_json::from_str(&cell_receipt).expect("parse kernel cell receipt");
    assert_eq!(
        cell_receipt["ok"],
        true,
        "the kernel cell failed: {}",
        cell_receipt["error"].as_str().unwrap_or_default()
    );

    // Both logical scopes persist through the single host-owned native store.
    let native_state_file = agent_dir.join("plugins/harness/store.json");
    let state = read_state(&native_state_file);
    assert_eq!(state["schema"], 1);
    let local_scope = format!("session:{session_id}");
    assert_eq!(cell_receipt["local"]["identity"]["scope"], local_scope);
    assert_stored_memory(
        &state,
        "kernel-local-memory",
        "kernel session-local memory",
        "persisted from the session kernel",
        &local_scope,
    );
    assert_stored_memory(
        &state,
        "kernel-global-memory",
        "kernel global memory",
        "persisted from the session kernel",
        "global",
    );
    // Native writes never recreate either removed Python legacy writer.
    let artifact_dir = dir.path().join("session-artifacts").join(session_id);
    assert!(!artifact_dir.join("harness/harness_state.json").exists());
    assert!(!agent_dir.join("harness/harness_state.json").exists());
}
