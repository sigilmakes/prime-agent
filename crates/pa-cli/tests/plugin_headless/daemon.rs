//! Production supervisor and worker, with configured loopback model metadata, no faux engine.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use super::Fixture;
use serde_json::{json, Value};

struct Supervisor {
    child: Child,
    socket: PathBuf,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        // Workers have separate process groups. Kill only live children still owned by this
        // supervisor, before reaping it, so a recycled worker PID can never become a target.
        let supervisor = self.child.id();
        let children =
            std::fs::read_to_string(format!("/proc/{supervisor}/task/{supervisor}/children"))
                .unwrap_or_default();
        for pid in children
            .split_whitespace()
            .filter_map(|pid| pid.parse::<i32>().ok())
        {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let parent = stat
                .rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().nth(1))
                .and_then(|parent| parent.parse::<u32>().ok());
            if parent == Some(supervisor) {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Wire {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Wire {
    fn new(stream: UnixStream) -> Self {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Self {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
        }
    }

    fn read(&mut self) -> Value {
        let mut text = String::new();
        assert!(
            self.reader.read_line(&mut text).unwrap() > 0,
            "daemon closed connection"
        );
        serde_json::from_str(&text).unwrap()
    }

    fn request(&mut self, id: &str, command: &Value) -> Value {
        let message = json!({"type":"command","id":id,
            "protocol":{"name":"prime-agent.daemon","version":7},"command":command});
        writeln!(self.writer, "{message}").unwrap();
        self.writer.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "daemon response deadline: {id}");
            let response = self.read();
            if response["id"] == id {
                return response;
            }
        }
    }
}

fn checked(wire: &mut Wire, id: &str, command: &Value) -> Value {
    let response = wire.request(id, command);
    assert_eq!(response["success"], true, "{response}");
    response["data"].clone()
}

fn catalog(wire: &mut Wire, session: &str) -> Vec<Value> {
    checked(
        wire,
        "catalog",
        &json!({"type":"get_commands","activeSessionId":session}),
    )["commands"]
        .as_array()
        .unwrap()
        .clone()
}

// Keep the production lifecycle trace linear so the generation and history assertions stay ordered.
#[allow(clippy::too_many_lines)]
#[test]
fn real_supervisor_worker_discovers_invokes_reloads_and_retires_lua_commands() {
    let fixture = Fixture::new();
    // Bound but never accepted. Any attempted inference is observable, including a failed call.
    let provider = TcpListener::bind("127.0.0.1:0").unwrap();
    provider.set_nonblocking(true).unwrap();
    // Supervisor catalog warm-up is independent of model inference and ignores PI_OFFLINE.
    // Route all non-loopback HTTP(S) through a test-owned bound proxy that never forwards.
    let catalog_proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("http://{}", catalog_proxy.local_addr().unwrap());
    std::fs::write(
        fixture.agent.join("models.json"),
        json!({"providers":{"fixture-local":{
            "baseUrl":format!("http://{}/v1", provider.local_addr().unwrap()),
            "api":"openai-completions","apiKey":"none","models":[{"id":"fixture-model","name":"Fixture model",
            "reasoning":false,"input":["text"],"contextWindow":10000,"maxTokens":1000,
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}}]
        }}})
        .to_string(),
    )
    .unwrap();
    let saved = fixture.agent.join("saved-plugin-data.json");
    std::fs::write(&saved, "preserved").unwrap();
    let socket = fixture.runtime.join("s.sock");
    let stderr = std::fs::File::create(fixture.runtime.join("supervisor.stderr")).unwrap();
    let child = fixture
        .command()
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(&socket)
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("HTTPS_PROXY", &proxy_url)
        .env("https_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env("NO_PROXY", "127.0.0.1,localhost,::1")
        .env("no_proxy", "127.0.0.1,localhost,::1")
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "1000",
        )
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let mut supervisor = Supervisor { child, socket };
    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match UnixStream::connect(&supervisor.socket) {
            Ok(stream) => break stream,
            Err(error) => {
                assert!(
                    supervisor.child.try_wait().unwrap().is_none(),
                    "supervisor exited: {error}"
                );
                assert!(Instant::now() < deadline, "supervisor not ready: {error}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    let mut wire = Wire::new(stream);
    let hello = wire.read();
    assert!(hello["serverCapabilities"]
        .as_array()
        .unwrap()
        .contains(&json!("plugin_commands")));
    let created = checked(
        &mut wire,
        "create",
        &json!({"type":"create","config":{
            "cwd":fixture.cwd,"sessionDir":fixture.runtime.join("sessions"),
            "provider":"fixture-local","model":"fixture-model"
        }}),
    );
    let session = created["activeSessionId"]
        .as_str()
        .or_else(|| created["id"].as_str())
        .unwrap();
    let commands = catalog(&mut wire, session);
    let target = commands
        .iter()
        .find(|entry| entry["source"] == "plugin" && entry["name"] == "plugin_echo")
        .unwrap_or_else(|| {
            let supervisor_log = std::fs::read_to_string(fixture.runtime.join("supervisor.stderr"))
                .unwrap_or_default();
            let mut worker_logs = Vec::new();
            if let Ok(files) = std::fs::read_dir(fixture.agent.join("logs")) {
                for file in files.flatten() {
                    if file.file_name().to_string_lossy().ends_with(".stderr.log") {
                        worker_logs.push((file.path(), std::fs::read_to_string(file.path()).unwrap_or_default()));
                    }
                }
            }
            panic!("worker must advertise configured Lua command; catalog={commands:#?}; created={created:#?}; supervisor={supervisor_log}; workers={worker_logs:#?}");
        })["target"]
        .clone();
    assert!(!commands
        .iter()
        .any(|entry| entry["name"] == "plugin_stream"));
    let invoked = checked(
        &mut wire,
        "invoke",
        &json!({"type":"plugin_command",
        "activeSessionId":session,"text":"/plugin_echo daemon args","target":target}),
    );
    let value: Value = serde_json::from_str(invoked["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["args"], "daemon args");
    assert_eq!(value["value"], "project");
    // Replace Lua source and config in the same live worker, publishing a new generation.
    let replacement_source = super::PLUGIN.replace(
        "value=config.value",
        "value=config.value,source='replacement'",
    );
    std::fs::write(fixture.cwd.join("fixture.lua"), &replacement_source).unwrap();
    fixture.settings(&json!({"plugins":{"fixture":{"config":{"value":"replacement"}}}}));
    checked(
        &mut wire,
        "reload",
        &json!({"type":"plugin_command","activeSessionId":session,"text":"/reload"}),
    );
    let commands = catalog(&mut wire, session);
    let replacement = commands
        .iter()
        .find(|entry| entry["name"] == "plugin_echo")
        .unwrap()["target"]
        .clone();
    assert_ne!(target["generation"], replacement["generation"]);
    let stale = wire.request(
        "stale",
        &json!({"type":"plugin_command","activeSessionId":session,
        "text":"/plugin_echo stale","target":target}),
    );
    assert_eq!(stale["success"], false);
    assert!(stale["error"].as_str().unwrap().contains("stale"));
    let invoked = checked(
        &mut wire,
        "replacement",
        &json!({"type":"plugin_command",
        "activeSessionId":session,"text":"/plugin_echo replacement args","target":replacement}),
    );
    let value: Value = serde_json::from_str(invoked["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["value"], "replacement");
    assert_eq!(value["source"], "replacement");
    // Invalid replacement must fail closed and preserve the last-good live generation.
    std::fs::write(fixture.cwd.join("fixture.lua"), "return {").unwrap();
    let invalid = wire.request(
        "invalid",
        &json!({"type":"plugin_command",
        "activeSessionId":session,"text":"/reload"}),
    );
    assert_eq!(invalid["success"], false);
    let last_good = catalog(&mut wire, session);
    assert_eq!(
        last_good
            .iter()
            .find(|entry| entry["name"] == "plugin_echo")
            .unwrap()["target"],
        replacement
    );
    let invoked = checked(
        &mut wire,
        "last-good",
        &json!({"type":"plugin_command",
        "activeSessionId":session,"text":"/plugin_echo retained args","target":replacement}),
    );
    let value: Value = serde_json::from_str(invoked["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["value"], "replacement");
    assert_eq!(value["source"], "replacement");
    fixture.settings(&json!({"plugins":{"fixture":{"enabled":false}}}));
    checked(
        &mut wire,
        "disable",
        &json!({"type":"reload","activeSessionId":session}),
    );
    assert!(!catalog(&mut wire, session)
        .iter()
        .any(|entry| entry["source"] == "plugin"));
    let disabled = wire.request(
        "disabled",
        &json!({"type":"plugin_command","activeSessionId":session,
        "text":"/plugin_echo disabled","target":replacement}),
    );
    assert_eq!(disabled["success"], false);
    checked(
        &mut wire,
        "stop",
        &json!({"type":"kill","activeSessionId":session}),
    );
    assert_eq!(std::fs::read_to_string(saved).unwrap(), "preserved");
    let entries = fixture.entries();
    assert!(
        !entries
            .iter()
            .any(|entry| entry["message"]["role"] == "user"
                || entry["message"]["role"] == "assistant")
    );
    let command_rows: Vec<_> = entries
        .iter()
        .filter(|entry| entry["type"] == "custom_message")
        .filter(|entry| {
            entry["customType"] == "session_slash_command"
                || entry["customType"] == "session_slash_command_result"
        })
        .collect();
    assert_eq!(
        command_rows.len(),
        6,
        "three successful invocations, each echo plus result"
    );
    assert!(command_rows
        .iter()
        .all(|entry| entry["details"]["command"]["name"] == "plugin_echo"));
    assert_eq!(
        command_rows
            .iter()
            .filter(|entry| entry["customType"] == "session_slash_command_result")
            .map(|entry| entry["details"]["success"].clone())
            .collect::<Vec<_>>(),
        vec![json!(true); 3]
    );
    assert_eq!(
        provider.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "plugin command path must not contact the configured inference provider"
    );
    checked(&mut wire, "shutdown", &json!({"type":"shutdown"}));
    let deadline = Instant::now() + Duration::from_secs(10);
    while supervisor.child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "supervisor did not settle shutdown"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
