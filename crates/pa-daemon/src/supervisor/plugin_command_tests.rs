//! Optional native plugin routing must be bound to the authenticated worker generation.
use super::*;

#[tokio::test]
async fn plugin_commands_never_reach_old_or_reconnected_workers() {
    let dir = tempfile::tempdir().unwrap();
    let supervisor = Arc::new(
        Supervisor::new(SupervisorOptions {
            agent_dir: dir.path().join("agent"),
            socket_path: dir.path().join("d.sock"),
        })
        .unwrap(),
    );
    let descriptor = serde_json::from_value(json!({
        "version":2,"workerId":"plugin-worker","pid":std::process::id(),
        "socketPath":dir.path().join("w.sock"),"recoveryJournalPath":dir.path().join("journal"),
        "supervisorSocketPath":dir.path().join("d.sock"),"authenticationToken":"private",
        "rootActiveSessionId":"plugin-session","createdAt":"t","updatedAt":"t",
        "lifecycle":"ready","createCommand":{},"consecutiveFailures":0,
    }))
    .unwrap();
    let worker = ResidentWorker::new(
        "plugin-worker".into(),
        descriptor,
        dir.path().join("worker"),
    );
    let first = worker.note_connection_live();
    let (tx, mut requests) = mpsc::channel(4);
    worker.install_command_channel(first, tx).await;
    worker.note_session_ready();
    supervisor.registry.insert(Arc::clone(&worker)).await;
    let (events, _receiver) = mpsc::channel(1);
    let attached = subscribers::ClientSubscriptions::new("plugin-client".into(), events);
    let command: DaemonCommand = serde_json::from_value(json!({
        "type":"plugin_command","activeSessionId":"plugin-session","text":"/hello test",
    }))
    .unwrap();
    for (epoch, capable) in [(first, false), (first, true)] {
        worker.set_plugin_command_capability(epoch, capable);
        if capable {
            let supervisor = Arc::clone(&supervisor);
            let route_worker = Arc::clone(&worker);
            let command = command.clone();
            let task = tokio::spawn(async move {
                supervisor
                    .route_command_typed(
                        &route_worker,
                        "plugin_command",
                        serde_json::to_value(command).unwrap(),
                        1000,
                        RouteAdmission::ClientRequest,
                    )
                    .await
            });
            let request = requests.recv().await.unwrap();
            assert_eq!(request.command_type, "plugin_command");
            assert!(worker
                .pending
                .lock()
                .await
                .remove(&request.request_id)
                .unwrap()
                .send(WorkerReply::Typed(response_success(
                    None,
                    "plugin_command",
                    Some(json!({"text":"executed"})),
                )))
                .is_ok());
            assert_eq!(
                task.await.unwrap().unwrap().data,
                Some(json!({"text":"executed"}))
            );
        } else {
            let (response, _) = tokio::time::timeout(
                Duration::from_millis(500),
                supervisor.route_client_command(
                    &command,
                    "plugin-client",
                    &attached,
                    "old".into(),
                    "plugin_command".into(),
                    None,
                ),
            )
            .await
            .expect("unsupported plugin commands must refuse before worker dispatch");
            assert_eq!(response[0]["success"], false);
            assert!(response[0]["error"]
                .as_str()
                .unwrap()
                .contains("does not support native plugin commands"));
            assert!(requests.try_recv().is_err());
        }
    }
    let second = worker.note_connection_live();
    let (tx, mut replacement) = mpsc::channel(4);
    worker.install_command_channel(second, tx).await;
    worker.note_session_ready();
    // A capable old socket's late metadata cannot bless its replacement.
    worker.set_plugin_command_capability(first, true);
    let (response, _) = tokio::time::timeout(
        Duration::from_millis(500),
        supervisor.route_client_command(
            &command,
            "plugin-client",
            &attached,
            "reconnected".into(),
            "plugin_command".into(),
            None,
        ),
    )
    .await
    .expect("late metadata must not authorize the replacement worker");
    assert_eq!(response[0]["success"], false);
    assert!(replacement.try_recv().is_err());
    // Also cover the final enqueue seam, not only client dispatch.
    assert!(supervisor
        .route_command_typed(
            &worker,
            "plugin_command",
            json!({}),
            100,
            RouteAdmission::ClientRequest
        )
        .await
        .is_err());
    assert!(replacement.try_recv().is_err());
}
