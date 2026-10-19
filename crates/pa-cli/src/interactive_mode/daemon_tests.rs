//! Credential-free peers for the ensure boundary. Every listener is private;
//! bound sockets and EOF/acknowledgements provide readiness, never sleeps.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use pa_types::platform::transport::{bind_transport, connect_transport, TransportStream};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

use super::{ensure_daemon_running, ensure_daemon_running_with};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

fn native_hello() -> Value {
    json!({
        "type": "daemon_hello",
        "protocol": {
            "name": pa_types::daemon::DAEMON_PROTOCOL_NAME,
            "version": pa_types::daemon::DAEMON_PROTOCOL_VERSION,
        },
        "schemaId": pa_types::daemon::DAEMON_SCHEMA_ID,
        "clientId": "private-test-peer",
        "serverCapabilities": [],
    })
}

/// An idle peer answers all commands, including a mutant's list/shutdown, so
/// rejection cannot accidentally pass because the fake hung on a request.
async fn serve_peer(stream: Box<dyn TransportStream>, hello: &str) -> Vec<Value> {
    let (reader, mut writer) = stream.split();
    writer
        .write_all(hello.as_bytes())
        .await
        .expect("send hello");
    writer.write_all(b"\n").await.expect("hello delimiter");
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut commands = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await.expect("read command") == 0 {
            return commands;
        }
        if line.trim().is_empty() {
            continue;
        }
        let envelope: Value = serde_json::from_str(&line).expect("command JSON");
        let response = json!({
            "type": "response",
            "id": envelope["id"],
            "command": envelope["command"]["type"],
            "success": true,
            "data": {"sessions": []},
        });
        commands.push(envelope);
        writer
            .write_all(response.to_string().as_bytes())
            .await
            .expect("idle response");
        writer.write_all(b"\n").await.expect("response delimiter");
    }
}

async fn start_peer(socket: &Path, hello: String) -> (oneshot::Sender<()>, JoinHandle<Vec<Value>>) {
    let listener = bind_transport(socket).await.expect("bind private peer");
    let (stop, mut stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => {
                    let stream = accepted.expect("accept private probe");
                    let hello = hello.clone();
                    connections.spawn(async move { serve_peer(stream, &hello).await });
                }
            }
        }
        let mut commands = Vec::new();
        while let Some(result) = connections.join_next().await {
            commands.extend(result.expect("peer connection"));
        }
        commands
    });
    (stop, task)
}

fn assert_refusal(error: &anyhow::Error, socket: &Path) {
    let message = error.to_string();
    assert!(message.contains("Prime Agent Rust client"), "{message}");
    assert!(message.contains(&socket.display().to_string()), "{message}");
    assert!(message.contains("--daemon-socket"), "{message}");
    assert!(
        message.contains("No shutdown or replacement was attempted"),
        "{message}"
    );
    assert!(!message.contains("shutdown --force"), "{message}");
}

#[tokio::test]
async fn idle_foreign_peers_are_refused_by_both_ensure_entrypoints_without_commands() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let mut foreign_schema = native_hello();
        foreign_schema["schemaId"] = json!("typescript-or-foreign-schema");
        let mut foreign_version = native_hello();
        foreign_version["protocol"]["version"] =
            json!(pa_types::daemon::DAEMON_PROTOCOL_VERSION + 1);
        let mut foreign_name = native_hello();
        foreign_name["protocol"]["name"] = json!("foreign.daemon");
        let mut missing_protocol = native_hello();
        missing_protocol.as_object_mut().unwrap().remove("protocol");
        let mut missing_schema = native_hello();
        missing_schema.as_object_mut().unwrap().remove("schemaId");
        let mut wrong_frame = native_hello();
        wrong_frame["type"] = json!("response");
        for hello in [
            foreign_schema.to_string(),
            foreign_version.to_string(),
            foreign_name.to_string(),
            missing_protocol.to_string(),
            missing_schema.to_string(),
            wrong_frame.to_string(),
            "not-json".to_string(),
        ] {
            let dir = tempfile::TempDir::new().expect("private fixture");
            let socket = dir.path().join("explicit.sock");
            let (stop, task) = start_peer(&socket, hello).await;
            let error = ensure_daemon_running(&socket, dir.path())
                .await
                .expect_err("foreign refusal");
            assert_refusal(&error, &socket);
            let error =
                ensure_daemon_running_with(&dir.path().join("must-not-spawn"), &socket, dir.path())
                    .await
                    .expect_err("foreign refusal before spawn");
            assert_refusal(&error, &socket);
            stop.send(()).expect("stop fixture");
            assert_eq!(
                task.await.expect("peer task"),
                Vec::<Value>::new(),
                "no list or shutdown, even on an idle peer"
            );
            assert!(socket.exists(), "leave the selected peer socket intact");
        }
    })
    .await
    .expect("foreign refusals are bounded");
}

#[tokio::test]
async fn current_native_peer_keeps_both_ensure_entrypoints_and_client_reconnect_usable() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let dir = tempfile::TempDir::new().expect("private fixture");
        let socket = dir.path().join("explicit.sock");
        let hello = native_hello();
        let (stop, task) = start_peer(&socket, hello.to_string()).await;
        for _ in 0..2 {
            ensure_daemon_running(&socket, dir.path())
                .await
                .expect("current peer");
            ensure_daemon_running_with(&dir.path().join("must-not-spawn"), &socket, dir.path())
                .await
                .expect("reuse current peer without spawn");
            // The actual TUI handshake still works after each read-only probe,
            // including a fresh connection after the previous client closes.
            let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(&socket)
                .await
                .expect("native client reconnect");
            assert_eq!(client.hello(), &hello);
            let data = client
                .request_ok(pa_types::daemon::DaemonCommand::List {
                    id: None,
                    all: None,
                    cwd: None,
                    session_dir: None,
                    include_client_owned: None,
                    rest: serde_json::Map::default(),
                })
                .await
                .expect("native command round trip");
            assert_eq!(data, json!({"sessions": []}));
            client.close();
        }
        stop.send(()).expect("stop fixture");
        let commands = task.await.expect("peer task");
        assert_eq!(commands.len(), 2, "only the explicit reconnect commands");
        assert!(commands
            .iter()
            .all(|frame| frame["command"]["type"] == "list"));
    })
    .await
    .expect("current reconnect is bounded");
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Re-enter only this test in the detached fixture process. The normal test
/// invocation does nothing. It binds only the socket named by the parent and
/// reports all received commands over a second private socket before exiting.
#[tokio::test]
async fn startup_peer_process() {
    let Some(socket) = std::env::var_os("PA_ENSURE_GUARD_SOCKET") else {
        return;
    };
    let socket = std::path::PathBuf::from(socket);
    let hello = std::env::var("PA_ENSURE_GUARD_HELLO").expect("fixture hello");
    tokio::time::timeout(TEST_TIMEOUT, async {
        let listener = bind_transport(&socket).await.expect("bind startup fixture");
        let stream = listener.accept().await.expect("accept startup poll");
        let commands = serve_peer(stream, &hello).await;
        let ack = connect_transport(&socket.with_extension("ack"))
            .await
            .expect("connect private ack");
        let (_reader, mut writer) = ack.split();
        writer
            .write_all(serde_json::to_string(&commands).unwrap().as_bytes())
            .await
            .expect("write observed commands");
        writer.write_all(b"\n").await.expect("ack delimiter");
    })
    .await
    .expect("startup fixture lifetime is bounded");
}

#[tokio::test]
async fn startup_poll_accepts_current_but_refuses_a_foreign_winner_without_commands() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let mut foreign = native_hello();
        foreign["schemaId"] = json!("foreign-startup-winner");
        for hello in [native_hello(), foreign] {
            let dir = tempfile::TempDir::new().expect("private startup fixture");
            let socket = dir.path().join("startup.sock");
            let ack = bind_transport(&socket.with_extension("ack")).await.expect("bind ack before spawn");
            let exe = dir.path().join("fixture-supervisor");
            let test_exe = std::env::current_exe().expect("test executable");
            let script = format!(
                "#!/bin/sh\nPA_ENSURE_GUARD_SOCKET={} PA_ENSURE_GUARD_HELLO={} exec {} --exact interactive_mode::daemon::tests::startup_peer_process --nocapture\n",
                shell_quote(socket.to_str().unwrap()),
                shell_quote(&hello.to_string()),
                shell_quote(test_exe.to_str().unwrap()),
            );
            std::fs::write(&exe, script).expect("write fixture launcher");
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).expect("executable fixture");
            let result = ensure_daemon_running_with(&exe, &socket, dir.path()).await;
            if hello == native_hello() {
                result.expect("current native startup");
            } else {
                assert_refusal(&result.expect_err("foreign startup refusal"), &socket);
            }
            let stream = ack.accept().await.expect("startup fixture ack");
            let (reader, _writer) = stream.split();
            let mut line = String::new();
            BufReader::new(reader).read_line(&mut line).await.expect("read ack");
            assert_eq!(serde_json::from_str::<Vec<Value>>(&line).unwrap(), Vec::<Value>::new());
        }
    }).await.expect("startup polling completes without takeover or a timeout retry loop");
}
