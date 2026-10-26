//! The worker-connection boundary oracles: the handshake owns its channel
//! privately until the auth answer installs it for routing — the TS
//! `pendingClient`/`worker.client` boundary — and a lost connection fails
//! its in-flight routes (one family per module).

use super::*;

/// A worker socket the test drives by hand: it stands in for the real worker on the
/// far side of `connect_worker`, so the oracles hold the handshake at exact points.
struct FakeWorkerSocket {
    // The listener is held so the bound socket path stays owned by the
    // test for the connection's whole lifetime.
    #[allow(dead_code)]
    listener: Box<dyn pa_types::platform::transport::TransportListener>,
    read_half: Box<dyn pa_types::platform::transport::AsyncReadHalf>,
    write_half: Box<dyn pa_types::platform::transport::AsyncWriteHalf>,
}

async fn bind_fake_worker(
    socket_path: &Path,
) -> Box<dyn pa_types::platform::transport::TransportListener> {
    pa_types::platform::transport::bind_transport(socket_path)
        .await
        .expect("bind fake worker socket")
}

/// Accept the connection `connect_worker` dials (its probe already
/// succeeded), once the connect task is in flight.
async fn accept_fake_worker(
    listener: Box<dyn pa_types::platform::transport::TransportListener>,
) -> FakeWorkerSocket {
    let stream = listener.accept().await.expect("accept fake worker");
    let (read_half, write_half) = stream.split();
    FakeWorkerSocket {
        listener,
        read_half,
        write_half,
    }
}

/// Read the next private frame the supervisor sent (its header carries the
/// request id the test's answer must echo).
async fn read_supervisor_frame(socket: &mut FakeWorkerSocket) -> crate::framing::PrivateFrame {
    let mut reader = PrivateFrameReader::new(&mut socket.read_half, DEFAULT_PRIVATE_FRAME_LIMITS);
    reader
        .read_frame()
        .await
        .expect("the supervisor's frame")
        .expect("the connection stays open")
}

/// Answer one request the way the real worker does: the response frame
/// with the line-serialized [`DaemonResponse`] as its payload.
async fn answer_supervisor_frame(socket: &mut FakeWorkerSocket, request_id: &str, command: &str) {
    let payload = crate::protocol::response_line_bytes(&DaemonResponse {
        id: None,
        command: command.to_string(),
        success: true,
        data: Some(json!({ "capabilities": ["agent_roster"] })),
        error: None,
        error_info: None,
    });
    write_frame(
        &mut socket.write_half,
        &json!({
            "kind": "outbound",
            "requestId": request_id,
            "outboundType": "response",
        }),
        &payload,
        DEFAULT_PRIVATE_FRAME_LIMITS,
    )
    .await
    .expect("write the auth answer");
}

#[tokio::test]
async fn handshake_channel_stays_private_until_auth_answers() {
    let dir = std::env::temp_dir().join(format!("pa-handshake-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("worker.sock");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let supervisor = Arc::new(
        Supervisor::new(SupervisorOptions {
            socket_path: dir.join("daemon.sock"),
            agent_dir: agent_dir.clone(),
        })
        .expect("supervisor"),
    );
    let descriptor: DaemonWorkerDescriptor = serde_json::from_value(serde_json::json!({
        "version": 2,
        "workerId": "w-handshake",
        "pid": 4242,
        "socketPath": socket_path.to_string_lossy(),
        "recoveryJournalPath": "/tmp/none.jsonl",
        "supervisorSocketPath": "/tmp/none.sock",
        "authenticationToken": "handshake-token",
        "rootActiveSessionId": "none",
        "createdAt": "2026-09-23T00:00:00Z",
        "updatedAt": "2026-09-23T00:00:00Z",
        "lifecycle": "ready",
        "createCommand": {},
        "consecutiveFailures": 0,
    }))
    .expect("descriptor");
    let resident = ResidentWorker::new(
        "w-handshake".to_string(),
        descriptor,
        dir.join("w-handshake.json"),
    );
    supervisor.registry.insert(Arc::clone(&resident)).await;

    let listener = bind_fake_worker(&socket_path).await;
    let connect = {
        let supervisor = Arc::clone(&supervisor);
        let resident = Arc::clone(&resident);
        tokio::spawn(async move {
            supervisor
                .connect_worker(&resident, worker_connect_deadline())
                .await
        })
    };
    let mut fake = accept_fake_worker(listener).await;
    // The handshake's auth frame is the FIRST thing on the wire.
    let frame = read_supervisor_frame(&mut fake).await;
    assert_eq!(frame.header.get("commandType"), Some(&json!("worker_auth")));

    // MID-HANDSHAKE (served-path): the resident has no routable channel.
    assert!(
        resident.cmd_tx.lock().await.is_none(),
        "the handshake channel is private until the auth answers"
    );

    // The auth answer installs the channel (the worker.client boundary).
    let request_id = frame
        .header
        .get("requestId")
        .and_then(Value::as_str)
        .expect("request id")
        .to_string();
    answer_supervisor_frame(&mut fake, &request_id, "worker_auth").await;
    connect
        .await
        .expect("the connect task lives")
        .expect("the handshake completes");
    assert!(
        resident.cmd_tx.lock().await.is_some(),
        "the answered handshake installs the channel for routing"
    );
    assert!(
        !resident.supports_model_refresh(),
        "an old worker remains routable without the optional refresh capability"
    );
}

/// A tombstoned stop's entire silent-peer authentication uses the strict
/// one-second budget, closes both transport halves, and drops its pending slot.
#[tokio::test]
async fn silent_peer_stop_auth_closes_socket_without_leaving_pending() {
    run_silent_peer_auth(false).await;
}

#[tokio::test]
async fn cancelled_auth_closes_socket_without_leaving_pending() {
    run_silent_peer_auth(true).await;
}

async fn run_silent_peer_auth(cancel_connect: bool) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = dir.path().join("worker.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let supervisor = Arc::new(
        Supervisor::new(SupervisorOptions {
            socket_path: dir.path().join("daemon.sock"),
            agent_dir,
        })
        .expect("supervisor"),
    );
    let descriptor: DaemonWorkerDescriptor = serde_json::from_value(json!({
        "version": 2, "workerId": "w-silent", "pid": 4242,
        "socketPath": socket_path.to_string_lossy(),
        "recoveryJournalPath": "/tmp/none.jsonl", "supervisorSocketPath": "/tmp/none.sock",
        "authenticationToken": "silent-token", "rootActiveSessionId": "none",
        "createdAt": "2026-09-23T00:00:00Z", "updatedAt": "2026-09-23T00:00:00Z",
        "lifecycle": "ready", "createCommand": {}, "consecutiveFailures": 0,
    }))
    .expect("descriptor");
    let resident = ResidentWorker::new(
        "w-silent".to_string(),
        descriptor,
        dir.path().join("w-silent.json"),
    );
    let listener = bind_fake_worker(&socket_path).await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(150);
    let connect = {
        let supervisor = Arc::clone(&supervisor);
        let resident = Arc::clone(&resident);
        tokio::spawn(async move {
            supervisor
                .connect_worker_for_stop(&resident, deadline)
                .await
        })
    };
    let mut fake = accept_fake_worker(listener).await;
    let frame = read_supervisor_frame(&mut fake).await;
    assert_eq!(frame.header.get("commandType"), Some(&json!("worker_auth")));
    if cancel_connect {
        connect.abort();
        assert!(connect.await.is_err(), "connect future must be cancelled");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !resident.pending.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled auth pending cleanup");
    } else {
        let result = tokio::time::timeout(Duration::from_secs(1), connect)
            .await
            .expect("strict deadline")
            .expect("join");
        assert!(result.is_err(), "silent peer must time out");
    }
    assert!(
        resident.pending.lock().await.is_empty(),
        "auth pending slot must be cleared"
    );
    let mut reader = PrivateFrameReader::new(&mut fake.read_half, DEFAULT_PRIVATE_FRAME_LIMITS);
    let closed = tokio::time::timeout(Duration::from_secs(1), reader.read_frame())
        .await
        .expect("peer must close promptly")
        .expect("clean EOF");
    assert!(closed.is_none(), "failed auth must close the socket");
}

/// A registration that lands mid-handshake must not kill the launch:
/// the registration's roster refresh routes onto its own authenticated
/// channel once the handshake installs it, while the handshake's
/// `worker_auth` keeps the unauthenticated connection to itself until
/// the answer arrives. Served-path:
/// the wire carries EXACTLY the auth frame (no route rides the private
/// channel), the registration itself succeeds, and the launch completes.
#[tokio::test]
async fn a_mid_handshake_registration_cannot_kill_the_handshake() {
    let dir = std::env::temp_dir().join(format!("pa-wedge-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("worker.sock");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let supervisor = Arc::new(
        Supervisor::new(SupervisorOptions {
            socket_path: dir.join("daemon.sock"),
            agent_dir: agent_dir.clone(),
        })
        .expect("supervisor"),
    );
    let descriptor: DaemonWorkerDescriptor = serde_json::from_value(serde_json::json!({
        "version": 2,
        "workerId": "w-wedge",
        "pid": std::process::id(),
        "socketPath": socket_path.to_string_lossy(),
        "recoveryJournalPath": "/tmp/none.jsonl",
        "supervisorSocketPath": "/tmp/none.sock",
        "authenticationToken": "wedge-token",
        "rootActiveSessionId": "none",
        "createdAt": "2026-09-23T00:00:00Z",
        "updatedAt": "2026-09-23T00:00:00Z",
        "lifecycle": "ready",
        "createCommand": {},
        "consecutiveFailures": 0,
    }))
    .expect("descriptor");
    let resident = ResidentWorker::new("w-wedge".to_string(), descriptor, dir.join("w-wedge.json"));
    supervisor.registry.insert(Arc::clone(&resident)).await;

    let listener = bind_fake_worker(&socket_path).await;
    let connect = {
        let supervisor = Arc::clone(&supervisor);
        let resident = Arc::clone(&resident);
        tokio::spawn(async move {
            supervisor
                .connect_worker(&resident, worker_connect_deadline())
                .await
        })
    };
    let mut fake = accept_fake_worker(listener).await;
    let frame = read_supervisor_frame(&mut fake).await;
    assert_eq!(frame.header.get("commandType"), Some(&json!("worker_auth")));
    let request_id = frame
        .header
        .get("requestId")
        .and_then(Value::as_str)
        .expect("request id")
        .to_string();

    // The registration lands while the handshake is still in flight.
    let command = DaemonCommand::WorkerRegister {
        id: None,
        active_session_id: "w-wedge".to_string(),
        session_id: None,
        socket_path: socket_path.to_string_lossy().to_string(),
        worker_instance_id: String::new(),
        token: "wedge-token".to_string(),
        pid: u64::from(std::process::id()),
        rest: Map::default(),
    };
    let response = supervisor
        .handle_worker_register("r1", "worker_register", &command)
        .await;
    assert!(
        response.success,
        "the mid-handshake registration itself succeeds: {response:?}"
    );
    assert!(
        resident.cmd_tx.lock().await.is_none(),
        "the registration installs no channel of its own"
    );

    // SERVED-PATH: the wire carries exactly the handshake — the refresh found no channel,
    // so no `get_state` raced the auth frame.
    let raced =
        tokio::time::timeout(Duration::from_millis(100), read_supervisor_frame(&mut fake)).await;
    assert!(
        raced.is_err(),
        "no route may ride the private handshake channel"
    );

    // The launch completes: the handshake answers and installs.
    answer_supervisor_frame(&mut fake, &request_id, "worker_auth").await;
    connect
        .await
        .expect("the connect task lives")
        .expect("the handshake survives the mid-flight registration");
    assert!(
        resident.cmd_tx.lock().await.is_some(),
        "the answered handshake installs the channel for routing"
    );
}

/// The install guard's TOCTOU pin: a stale connect that passed its
/// epoch check before a newer connection installed must never
/// overwrite the newer channel — the recheck happens under the channel
/// lock, so the stale install is dropped and the newer channel stays
/// routable.
#[tokio::test]
async fn a_stale_epoch_never_overwrites_the_installed_channel() {
    let dir = std::env::temp_dir().join(format!("pa-install-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let descriptor: DaemonWorkerDescriptor = serde_json::from_value(serde_json::json!({
        "version": 2,
        "workerId": "w-install",
        "pid": 0,
        "socketPath": "/tmp/none.sock",
        "recoveryJournalPath": "/tmp/none.jsonl",
        "supervisorSocketPath": "/tmp/none.sock",
        "authenticationToken": "install-token",
        "rootActiveSessionId": "none",
        "createdAt": "2026-09-23T00:00:00Z",
        "updatedAt": "2026-09-23T00:00:00Z",
        "lifecycle": "ready",
        "createCommand": {},
        "consecutiveFailures": 0,
    }))
    .expect("descriptor");
    let resident = ResidentWorker::new(
        "w-install".to_string(),
        descriptor,
        dir.join("w-install.json"),
    );
    let (newer_tx, mut newer_rx) =
        mpsc::channel::<WorkerRequest>(crate::backpressure::WORKER_INFLIGHT_CAPACITY);
    let (stale_tx, _stale_rx) =
        mpsc::channel::<WorkerRequest>(crate::backpressure::WORKER_INFLIGHT_CAPACITY);

    // The stale epoch is a real issued one (an epoch-0 oracle would pass a guard that
    // only rejects the never-issued epoch, leaving the live stale interleaving unexercised).
    let stale_epoch = resident.note_connection_live();
    // The newer connection installs first; the stale connect's epoch is
    // now superseded.
    let newer_epoch = resident.note_connection_live();
    resident
        .install_command_channel(newer_epoch, newer_tx)
        .await;
    assert!(
        resident.cmd_tx.lock().await.is_some(),
        "the newer connection installs"
    );

    // The stale connect installs last (the TOCTOU window: its pre-lock check passed
    // before the newer install) — under the lock the recheck drops it.
    resident
        .install_command_channel(stale_epoch, stale_tx)
        .await;
    let routed = {
        let cmd_tx = resident.cmd_tx.lock().await;
        cmd_tx
            .as_ref()
            .expect("the channel stays installed")
            .clone()
    };
    routed
        .send(WorkerRequest {
            request_id: "r1".to_string(),
            command_type: "get_state".to_string(),
            payload: json!({}),
        })
        .await
        .expect("the newer channel routes");
    let frame = tokio::time::timeout(Duration::from_secs(2), newer_rx.recv())
        .await
        .expect("the newer channel answers")
        .expect("the channel stays open");
    assert_eq!(frame.command_type, "get_state");
}

/// A lost worker connection fails its in-flight routes right away (TS
/// `notifyClosed` -> `rejectAll`): the reader drains the connection's reply
/// slots when the socket ends, so a route waiting on the worker's answer
/// cannot outlive the connection it was sent on.
#[tokio::test]
async fn a_lost_worker_connection_fails_its_in_flight_route() {
    let dir = std::env::temp_dir().join(format!("pa-lost-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("worker.sock");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let supervisor = Arc::new(
        Supervisor::new(SupervisorOptions {
            socket_path: dir.join("daemon.sock"),
            agent_dir,
        })
        .expect("supervisor"),
    );
    let descriptor: DaemonWorkerDescriptor = serde_json::from_value(serde_json::json!({
        "version": 2,
        "workerId": "w-lost",
        "pid": 4242,
        "socketPath": socket_path.to_string_lossy(),
        "recoveryJournalPath": "/tmp/none.jsonl",
        "supervisorSocketPath": "/tmp/none.sock",
        "authenticationToken": "lost-token",
        "rootActiveSessionId": "none",
        "createdAt": "2026-09-23T00:00:00Z",
        "updatedAt": "2026-09-23T00:00:00Z",
        "lifecycle": "ready",
        "createCommand": {},
        "consecutiveFailures": 0,
    }))
    .expect("descriptor");
    let resident = ResidentWorker::new("w-lost".to_string(), descriptor, dir.join("w-lost.json"));
    supervisor.registry.insert(Arc::clone(&resident)).await;

    let listener = bind_fake_worker(&socket_path).await;
    let connect = {
        let supervisor = Arc::clone(&supervisor);
        let resident = Arc::clone(&resident);
        tokio::spawn(async move {
            supervisor
                .connect_worker(&resident, worker_connect_deadline())
                .await
        })
    };
    let mut fake = accept_fake_worker(listener).await;
    let frame = read_supervisor_frame(&mut fake).await;
    assert_eq!(frame.header.get("commandType"), Some(&json!("worker_auth")));
    let request_id = frame
        .header
        .get("requestId")
        .and_then(Value::as_str)
        .expect("request id")
        .to_string();
    answer_supervisor_frame(&mut fake, &request_id, "worker_auth").await;
    connect
        .await
        .expect("the connect task lives")
        .expect("the handshake completes");

    // A turn-long route in flight on the live connection.
    let route = {
        let supervisor = Arc::clone(&supervisor);
        let resident = Arc::clone(&resident);
        tokio::spawn(async move {
            supervisor
                .route_command_typed(
                    &resident,
                    "prompt_and_wait",
                    json!({ "activeSessionId": "w-lost", "message": "go" }),
                    super::routing::WORKER_REQUEST_TIMEOUT_MS,
                    RouteAdmission::ClientRequest,
                )
                .await
        })
    };
    let frame = read_supervisor_frame(&mut fake).await;
    assert_eq!(
        frame.header.get("commandType"),
        Some(&json!("prompt_and_wait"))
    );
    drop(fake);
    let error = tokio::time::timeout(Duration::from_secs(5), route)
        .await
        .expect("the lost connection fails the in-flight route")
        .expect("the route task lives")
        .expect_err("the drained route fails");
    assert_eq!(error.to_string(), super::routing::WORKER_SOCKET_CLOSED);
}

/// New supervisors must not send optional commands to old or partial workers.
#[tokio::test]
async fn model_refresh_requires_negotiated_worker_capability_and_revision() {
    for (data, expected) in [
        (
            json!({"capabilities":["model_refresh"],"schemaRevision":31}),
            false,
        ),
        (json!({"capabilities":[],"schemaRevision":32}), false),
        (json!({"capabilities":["model_refresh"]}), false),
        (
            json!({"capabilities":["model_refresh"],"schemaRevision":32}),
            true,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("worker.sock");
        let supervisor = Arc::new(
            Supervisor::new(SupervisorOptions {
                socket_path: dir.path().join("supervisor.sock"),
                agent_dir: dir.path().join("agent"),
            })
            .unwrap(),
        );
        let descriptor = serde_json::from_value(json!({
            "version":2,"workerId":"refresh-handshake","pid":std::process::id(),
            "socketPath":socket_path,"recoveryJournalPath":dir.path().join("recovery.jsonl"),
            "supervisorSocketPath":dir.path().join("supervisor.sock"),
            "authenticationToken":"isolated","rootActiveSessionId":"refresh-handshake",
            "createdAt":"t","updatedAt":"t","lifecycle":"ready","createCommand":{},
            "consecutiveFailures":0,
        }))
        .unwrap();
        let resident = ResidentWorker::new(
            "refresh-handshake".into(),
            descriptor,
            dir.path().join("descriptor.json"),
        );
        let listener = bind_fake_worker(&socket_path).await;
        let connect = {
            let supervisor = Arc::clone(&supervisor);
            let resident = Arc::clone(&resident);
            tokio::spawn(async move {
                supervisor
                    .connect_worker(&resident, worker_connect_deadline())
                    .await
            })
        };
        let mut fake = accept_fake_worker(listener).await;
        let frame = read_supervisor_frame(&mut fake).await;
        let payload = crate::protocol::response_line_bytes(&response_success(
            None,
            "worker_auth",
            Some(data),
        ));
        write_frame(
            &mut fake.write_half,
            &json!({
                "kind":"outbound","requestId":frame.header["requestId"],"outboundType":"response",
            }),
            &payload,
            DEFAULT_PRIVATE_FRAME_LIMITS,
        )
        .await
        .unwrap();
        connect.await.unwrap().unwrap();
        assert_eq!(resident.supports_model_refresh(), expected);
        assert!(
            resident.cmd_tx.lock().await.is_some(),
            "optional features cannot block normal attachment"
        );
    }
}

/// A real private worker channel and real public client transport. The worker's
/// only fake is its terminal verdict; accounting refusal must survive both hops.
async fn accounting_refusal_fixture(
    name: &str,
) -> (
    tempfile::TempDir,
    Arc<Supervisor>,
    Arc<ResidentWorker>,
    tokio::sync::mpsc::Sender<bool>,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::Builder::new()
        .prefix("pa-close-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket_path = dir.path().join("w.sock");
    let supervisor = Arc::new(
        Supervisor::new(SupervisorOptions {
            socket_path: dir.path().join("d.sock"),
            agent_dir: dir.path().join("agent"),
        })
        .unwrap(),
    );
    let descriptor = serde_json::from_value(json!({
        "version": 2, "workerId": name, "pid": 0,
        "socketPath": socket_path, "recoveryJournalPath": dir.path().join("recovery"),
        "supervisorSocketPath": supervisor.options.socket_path,
        "authenticationToken": "close-fixture", "rootActiveSessionId": name,
        "createdAt": "t", "updatedAt": "t", "lifecycle": "ready",
        "createCommand": {}, "consecutiveFailures": 0
    }))
    .unwrap();
    let resident = ResidentWorker::new(name.into(), descriptor, dir.path().join("w.json"));
    supervisor.registry.insert(Arc::clone(&resident)).await;
    let listener = bind_fake_worker(&socket_path).await;
    let connect = {
        let supervisor = Arc::clone(&supervisor);
        let resident = Arc::clone(&resident);
        tokio::spawn(async move {
            supervisor
                .connect_worker(&resident, worker_connect_deadline())
                .await
        })
    };
    let mut fake = accept_fake_worker(listener).await;
    let frame = read_supervisor_frame(&mut fake).await;
    answer_supervisor_frame(
        &mut fake,
        frame.header["requestId"].as_str().unwrap(),
        "worker_auth",
    )
    .await;
    connect.await.unwrap().unwrap();
    resident.note_session_ready();
    let (verdict, mut verdicts) = tokio::sync::mpsc::channel::<bool>(4);
    let worker = tokio::spawn(async move {
        while let Some(success) = verdicts.recv().await {
            let frame = read_supervisor_frame(&mut fake).await;
            let payload = crate::protocol::response_line_bytes(&DaemonResponse {
                id: None,
                command: frame.header["commandType"].as_str().unwrap().into(),
                success,
                data: None,
                error: (!success).then(|| "Final child accounting remains owed".into()),
                error_info: None,
            });
            write_frame(&mut fake.write_half, &json!({
                "kind": "outbound", "requestId": frame.header["requestId"], "outboundType": "response",
            }), &payload, DEFAULT_PRIVATE_FRAME_LIMITS).await.unwrap();
        }
    });
    (dir, supervisor, resident, verdict, worker)
}

fn assert_no_retirement(supervisor: &Supervisor, resident: &ResidentWorker) {
    assert!(!supervisor.accept_exit.load(Ordering::SeqCst));
    assert!(!resident.intentional_stop.load(Ordering::SeqCst));
    assert!(!resident.route_state().retired);
    assert!(super::worker_lifecycle::retirement_overrides()
        .lock()
        .unwrap()
        .contains_key(&resident.descriptor_path));
}

#[cfg(unix)]
#[tokio::test]
async fn public_graceful_shutdown_refusal_preserves_worker_then_retry_or_force_completes() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for (command, force) in [("shutdown", false), ("restart", false), ("shutdown", true)] {
        let (_dir, supervisor, resident, verdict, worker) =
            accounting_refusal_fixture("w-close").await;
        super::worker_lifecycle::retirement_overrides()
            .lock()
            .unwrap()
            .insert(
                resident.descriptor_path.clone(),
                super::worker_lifecycle::WorkerRetirement::Retired,
            );
        let (server, client) = tokio::net::UnixStream::pair().unwrap();
        let serving = {
            let supervisor = Arc::clone(&supervisor);
            tokio::spawn(async move { supervisor.handle_client(Box::new(server)).await })
        };
        let (read, mut write) = client.into_split();
        let mut read = BufReader::new(read);
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["type"],
            "daemon_hello"
        );
        for attempt in 0..2 {
            verdict.send(attempt == 1 && !force).await.unwrap();
            let command = json!({ "type": "command", "id": format!("close-{attempt}"),
                "protocol": { "name": "prime-agent.daemon", "version": 7 },
                "command": { "type": command, "force": attempt == 1 && force },
            });
            write
                .write_all(format!("{command}\n").as_bytes())
                .await
                .unwrap();
            let response = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    line.clear();
                    read.read_line(&mut line).await.unwrap();
                    let response: Value = serde_json::from_str(&line).unwrap();
                    if response["type"] == "heartbeats_changed" {
                        continue;
                    }
                    assert_eq!(
                        response["type"], "response",
                        "no early daemon_closing: {response}"
                    );
                    break response;
                }
            })
            .await
            .unwrap();
            assert_eq!(response["success"], attempt == 1, "{response}");
            if attempt == 0 {
                assert_no_retirement(&supervisor, &resident);
                assert!(supervisor.registry.get(&resident.worker_id).await.is_some());
                assert!(resident.descriptor.lock().await.stop_requested_at.is_none());
                assert!(!supervisor.shutting_down.load(Ordering::SeqCst));
                assert!(!supervisor.shutdown_started.load(Ordering::SeqCst));
            }
        }
        line.clear();
        read.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["type"],
            "daemon_closing"
        );
        serving.await.unwrap().unwrap();
        assert!(supervisor.accept_exit.load(Ordering::SeqCst));
        assert!(supervisor.registry.get(&resident.worker_id).await.is_none());
        assert!(!super::worker_lifecycle::retirement_overrides()
            .lock()
            .unwrap()
            .contains_key(&resident.descriptor_path));
        worker.abort();
    }
}

#[tokio::test]
async fn per_session_graceful_stop_refusal_does_not_retire_and_can_retry() {
    let (_dir, supervisor, resident, verdict, worker) = accounting_refusal_fixture("w-close").await;
    super::worker_lifecycle::retirement_overrides()
        .lock()
        .unwrap()
        .insert(
            resident.descriptor_path.clone(),
            super::worker_lifecycle::WorkerRetirement::Retired,
        );
    verdict.send(false).await.unwrap();
    assert!(supervisor.stop_worker(&resident).await.is_err());
    assert_no_retirement(&supervisor, &resident);
    assert!(resident.descriptor.lock().await.stop_requested_at.is_none());
    assert!(supervisor.registry.get(&resident.worker_id).await.is_some());
    verdict.send(true).await.unwrap();
    supervisor.stop_worker(&resident).await.unwrap();
    assert!(supervisor.registry.get(&resident.worker_id).await.is_none());
    worker.abort();
}

#[tokio::test]
async fn concurrent_shutdown_attempt_cannot_duplicate_stop_or_reopen_completed_gate() {
    let (_dir, supervisor, resident, verdict, worker) = accounting_refusal_fixture("w-close").await;
    let first = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move {
            supervisor.prepare_shutdown(/*force*/ false).await
        })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        while resident.pending.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(supervisor.prepare_shutdown(/*force*/ false).await.is_err());
    assert!(supervisor.shutdown_started.load(Ordering::SeqCst));
    verdict.send(false).await.unwrap();
    assert!(first.await.unwrap().is_err());
    assert!(!supervisor.shutdown_started.load(Ordering::SeqCst));
    assert!(!supervisor.accept_exit.load(Ordering::SeqCst));
    worker.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn public_plain_kill_refusal_does_not_run_finally_retirement() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_dir, supervisor, resident, verdict, worker) = accounting_refusal_fixture("w-close").await;
    super::worker_lifecycle::retirement_overrides()
        .lock()
        .unwrap()
        .insert(
            resident.descriptor_path.clone(),
            super::worker_lifecycle::WorkerRetirement::Retired,
        );
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let serving = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.handle_client(Box::new(server)).await })
    };
    let (read, mut write) = client.into_split();
    let mut read = BufReader::new(read);
    let mut line = String::new();
    read.read_line(&mut line).await.unwrap();
    verdict.send(false).await.unwrap();
    let command = json!({ "type": "command", "id": "kill-owed",
        "protocol": { "name": "prime-agent.daemon", "version": 7 },
        "command": { "type": "kill", "activeSessionId": "w-close" },
    });
    write
        .write_all(format!("{command}\n").as_bytes())
        .await
        .unwrap();
    line.clear();
    tokio::time::timeout(Duration::from_secs(2), read.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["success"], false, "{response}");
    assert!(
        response["error"].as_str().unwrap().contains("accounting"),
        "{response}"
    );
    assert_no_retirement(&supervisor, &resident);
    assert!(resident.descriptor.lock().await.stop_requested_at.is_none());
    assert!(supervisor.registry.get(&resident.worker_id).await.is_some());
    serving.abort();
    worker.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn first_signal_accounting_refusal_preserves_worker_without_closing_event() {
    let (_dir, supervisor, resident, verdict, worker) = accounting_refusal_fixture("w-close").await;
    super::worker_lifecycle::retirement_overrides()
        .lock()
        .unwrap()
        .insert(
            resident.descriptor_path.clone(),
            super::worker_lifecycle::WorkerRetirement::Retired,
        );
    let mut events = supervisor.events.subscribe();
    verdict.send(false).await.unwrap();
    assert!(supervisor.begin_signal_drain());
    tokio::time::timeout(Duration::from_secs(1), async {
        while supervisor.shutting_down.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_no_retirement(&supervisor, &resident);
    assert!(
        events.try_recv().is_err(),
        "refusal cannot announce terminal closing"
    );
    assert!(supervisor.registry.get(&resident.worker_id).await.is_some());
    assert!(
        !supervisor.begin_signal_drain(),
        "the operator's repeated signal remains force"
    );
    worker.abort();
}

#[tokio::test]
async fn idle_eviction_fences_racing_client_routes_until_refusal_or_retirement() {
    let (_dir, supervisor, resident, verdict, worker) = accounting_refusal_fixture("w-idle").await;
    super::worker_lifecycle::retirement_overrides()
        .lock()
        .unwrap()
        .insert(
            resident.descriptor_path.clone(),
            super::worker_lifecycle::WorkerRetirement::Retired,
        );
    for success in [false, true] {
        let stopping = {
            let supervisor = Arc::clone(&supervisor);
            tokio::spawn(async move {
                supervisor
                    .handle_worker_idle_passivation(
                        "idle",
                        "worker_idle_passivation",
                        "close-fixture",
                        None,
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while resident.pending.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown reached the worker");
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            supervisor.route_command_typed(
                &resident,
                "prompt",
                json!({ "message": "racing user work" }),
                ROUTE_TIMEOUT_MS,
                RouteAdmission::ClientRequest,
            ),
        )
        .await
        .expect("fenced client gets an immediate refusal")
        .unwrap();
        assert!(!response.success, "idle eviction cannot accept racing work");
        assert!(resident.descriptor.lock().await.stop_requested_at.is_none());
        assert!(!resident.route_state().retired);
        verdict.send(success).await.unwrap();
        assert_eq!(stopping.await.unwrap().success, success);
        if !success {
            assert_no_retirement(&supervisor, &resident);
            assert_eq!(
                resident.inflight.available_permits(),
                crate::backpressure::WORKER_INFLIGHT_CAPACITY,
                "refusal reopens client admission",
            );
        }
    }
    assert!(supervisor.registry.get(&resident.worker_id).await.is_none());
    assert!(resident.route_state().retired);
    worker.abort();
}

#[tokio::test]
async fn partial_shutdown_retry_targets_only_the_worker_that_refused_accounting() {
    let (_dir, supervisor, closed, closed_verdict, closed_worker) =
        accounting_refusal_fixture("a-closed").await;
    let (_other_dir, _other_supervisor, owed, owed_verdict, owed_worker) =
        accounting_refusal_fixture("z-owed").await;
    supervisor.registry.insert(Arc::clone(&owed)).await;
    for resident in [&closed, &owed] {
        super::worker_lifecycle::retirement_overrides()
            .lock()
            .unwrap()
            .insert(
                resident.descriptor_path.clone(),
                super::worker_lifecycle::WorkerRetirement::Retired,
            );
    }
    for resident in [&closed, &owed] {
        supervisor.roster.lock().unwrap().write_summary(
            json!({
                "sessionId": resident.worker_id,
                "type": "root",
                "activeSessionId": resident.worker_id,
                "status": "idle",
            }),
            Some(&resident.worker_id),
            None,
        );
    }
    let mut events = supervisor.events.subscribe();
    closed_verdict.send(true).await.unwrap();
    owed_verdict.send(false).await.unwrap();
    assert!(supervisor.prepare_shutdown(/*force*/ false).await.is_err());
    assert!(supervisor.registry.get(&closed.worker_id).await.is_none());
    assert!(supervisor.registry.get(&owed.worker_id).await.is_some());
    assert_no_retirement(&supervisor, &owed);
    {
        let roster = supervisor.roster.lock().unwrap();
        let rows = roster.entries();
        let closed_row = rows
            .iter()
            .find(|row| row.summary["sessionId"] == closed.worker_id)
            .unwrap();
        assert!(closed_row.worker_id.is_none());
        assert_eq!(closed_row.summary["isSessionActive"], false);
        assert_eq!(roster.entries_for_worker(&owed.worker_id).len(), 1);
    }
    let mut heartbeat_invalidated = false;
    let mut roster_changed = false;
    while let Ok((_, event)) = events.try_recv() {
        heartbeat_invalidated |= event["type"] == "heartbeats_changed";
        roster_changed |= event["type"] == "roster_update";
    }
    assert!(
        heartbeat_invalidated,
        "partial close invalidates job catalogs"
    );
    assert!(
        roster_changed,
        "partial close publishes passive roster rows"
    );
    owed_verdict.send(true).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        supervisor.prepare_shutdown(/*force*/ false),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(supervisor.registry.list().await.is_empty());
    assert!(
        !supervisor.accept_exit.load(Ordering::SeqCst),
        "exit waits for public response completion"
    );
    supervisor.ensure_shutdown_started().await;
    assert!(supervisor.accept_exit.load(Ordering::SeqCst));
    closed_worker.abort();
    owed_worker.abort();
}
