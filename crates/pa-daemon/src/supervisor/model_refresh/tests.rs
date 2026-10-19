use super::super::{
    response_success, DaemonCommand, SupervisorOptions, WorkerReply, WorkerRequest,
};
use super::*;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

struct Harness {
    dir: tempfile::TempDir,
    supervisor: Arc<Supervisor>,
    pumps: Vec<JoinHandle<()>>,
}
impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = Arc::new(
            Supervisor::new(SupervisorOptions {
                agent_dir: dir.path().join("agent"),
                socket_path: dir.path().join("daemon.sock"),
            })
            .unwrap(),
        );
        Self {
            dir,
            supervisor,
            pumps: Vec::new(),
        }
    }
    async fn worker(&mut self, id: &str) -> (Arc<ResidentWorker>, mpsc::Receiver<WorkerRequest>) {
        let descriptor = serde_json::from_value(json!({
            "version": 2, "workerId": id, "pid": std::process::id(),
            "socketPath": self.dir.path().join(format!("{id}.sock")),
            "recoveryJournalPath": self.dir.path().join(format!("{id}.jsonl")),
            "supervisorSocketPath": self.dir.path().join("daemon.sock"),
            "authenticationToken":"isolated-test", "rootActiveSessionId":id,
            "createdAt":"t", "updatedAt":"t", "lifecycle":"ready", "createCommand":{},
            "consecutiveFailures":0,
        }))
        .unwrap();
        let worker = ResidentWorker::new(id.into(), descriptor, self.dir.path().join(id));
        let epoch = worker.note_connection_live();
        let (tx, rx) = mpsc::channel(8);
        worker.install_command_channel(epoch, tx).await;
        worker.note_session_ready();
        worker.set_model_refresh_capability(epoch, /*capable*/ true);
        self.supervisor.registry.insert(Arc::clone(&worker)).await;
        (worker, rx)
    }
    fn serve(
        &mut self,
        worker: Arc<ResidentWorker>,
        mut requests: mpsc::Receiver<WorkerRequest>,
        handler: impl Fn(&WorkerRequest) -> Value + Send + 'static,
    ) {
        self.pumps.push(tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                let response =
                    response_success(None, &request.command_type, Some(handler(&request)));
                if let Some(reply) = worker.pending.lock().await.remove(&request.request_id) {
                    let _ = reply.send(WorkerReply::Typed(response));
                }
            }
        }));
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        for pump in &self.pumps {
            pump.abort();
        }
    }
}
fn report(status: &str) -> Value {
    json!({"startedAt":1,"finishedAt":2,"sources":[{"source":"curated","status":status,"modelCount":3}]})
}

#[tokio::test]
async fn zero_workers_never_opens_saved_sessions_or_launches_processes() {
    let h = Harness::new();
    let sessions = h.dir.path().join("agent/sessions/saved");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join("saved.jsonl"), "{not a live worker}").unwrap();
    let result = h.supervisor.refresh_resident_models().await;
    assert_eq!(result.workers, Some(ModelRefreshWorkers::default()));
    assert!(result.sources.is_empty());
    assert!(h.supervisor.registry.list().await.is_empty());
}

#[tokio::test]
async fn coalesces_clients_and_only_fetches_missing_sibling_sources() {
    let mut h = Harness::new();
    let (leader, mut leader_rx) = h.worker("01-leader").await;
    let (sibling, sibling_rx) = h.worker("02-sibling").await;
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sibling_calls = Arc::clone(&calls);
    h.serve(sibling, sibling_rx, move |request| {
        let mut calls = sibling_calls.lock().unwrap();
        calls.push((request.command_type.clone(), request.payload.clone()));
        if request.command_type == "refresh_models" { return report("unchanged"); }
        json!({"applied":[],"skipped":[],"missing":if calls.len()==1 { vec!["curated"] } else {vec![]}})
    });
    let supervisor = Arc::clone(&h.supervisor);
    let first = tokio::spawn(async move { supervisor.refresh_resident_models().await });
    let request = leader_rx.recv().await.unwrap();
    let supervisor = Arc::clone(&h.supervisor);
    let mut second = std::pin::pin!(supervisor.refresh_resident_models());
    assert!(futures::poll!(second.as_mut()).is_pending());
    assert!(leader
        .pending
        .lock()
        .await
        .remove(&request.request_id)
        .unwrap()
        .send(WorkerReply::Typed(response_success(
            None,
            "refresh_models",
            Some(report("updated")),
        )))
        .is_ok());
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    assert_eq!(first, second);
    assert_eq!(
        first.workers,
        Some(ModelRefreshWorkers {
            total: 2,
            succeeded: 2,
            failed: 0
        })
    );
    assert_eq!(
        serde_json::to_value(first.sources).unwrap(),
        json!([{"source":"curated","status":"updated"}])
    );
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|(command, _)| command.as_str())
            .collect::<Vec<_>>(),
        vec![
            "apply_model_snapshots",
            "refresh_models",
            "apply_model_snapshots"
        ]
    );
    assert_eq!(calls[1].1["sources"], json!(["curated"]));
    assert!(leader_rx.try_recv().is_err());
}

#[tokio::test]
async fn leader_election_skips_invalid_reports_and_does_not_leak_errors() {
    let mut h = Harness::new();
    let (bad, bad_rx) = h.worker("01-invalid").await;
    h.serve(
        bad,
        bad_rx,
        |_| json!({"error":"credential: must not escape"}),
    );
    let (good, good_rx) = h.worker("02-good").await;
    h.serve(good, good_rx, |_| report("updated"));
    let result = h.supervisor.refresh_resident_models().await;
    assert_eq!(
        result.workers,
        Some(ModelRefreshWorkers {
            total: 2,
            succeeded: 1,
            failed: 1
        })
    );
    let wire = serde_json::to_value(result).unwrap();
    assert_eq!(
        wire["sources"],
        json!([
            {"source":"curated","status":"updated"},
            {"source":"daemon-workers","status":"failed","reason":"worker-unavailable"}
        ])
    );
    assert!(!wire.to_string().contains("credential"));
    assert!(!wire.to_string().contains("01-invalid"));
}

#[tokio::test]
async fn never_restarts_or_wakes_unready_dead_retired_or_old_workers() {
    let mut h = Harness::new();
    let mut channels = Vec::new();
    for id in [
        "01-starting",
        "02-disconnected",
        "03-retired",
        "04-dead",
        "05-old",
    ] {
        let (worker, rx) = h.worker(id).await;
        match id {
            "01-starting" => {
                worker.descriptor.lock().await.lifecycle = DaemonWorkerLifecycle::Starting;
            }
            "02-disconnected" => {
                worker.note_connection_lost(1);
            }
            "03-retired" => worker.note_retired(),
            "04-dead" => worker.descriptor.lock().await.pid = i32::MAX as u64,
            "05-old" => worker.set_model_refresh_capability(1, /*capable*/ false),
            _ => unreachable!(),
        }
        channels.push(rx);
    }
    let result = h.supervisor.refresh_resident_models().await;
    assert_eq!(
        result.workers,
        Some(ModelRefreshWorkers {
            total: 1,
            succeeded: 0,
            failed: 1
        })
    );
    for mut rx in channels {
        assert!(rx.try_recv().is_err());
    }
    assert_eq!(h.supervisor.registry.list().await.len(), 5);
}

#[tokio::test]
async fn reports_worst_per_source_without_credential_scoped_counts() {
    for (first, second, expected) in [
        ("skipped", "updated", "updated"),
        ("unchanged", "updated", "updated"),
        ("stale", "updated", "stale"),
        ("failed", "updated", "failed"),
    ] {
        let mut h = Harness::new();
        let (leader, rx) = h.worker("01").await;
        h.serve(leader, rx, move |_| report(first));
        let (sibling, rx) = h.worker("02").await;
        h.serve(sibling, rx, move |request| {
            if request.command_type == "refresh_models" {
                report(second)
            } else {
                json!({"applied":[],"skipped":[],"missing":["curated"]})
            }
        });
        let result = h.supervisor.refresh_resident_models().await;
        assert_eq!(
            serde_json::to_value(result.sources).unwrap(),
            json!([{"source":"curated","status":expected}])
        );
    }
}

#[tokio::test]
async fn sweep_survives_requester_cancellation() {
    let mut h = Harness::new();
    let (leader, mut rx) = h.worker("01").await;
    let supervisor = Arc::clone(&h.supervisor);
    let request = tokio::spawn(async move { supervisor.refresh_resident_models().await });
    let wire = rx.recv().await.unwrap();
    request.abort();
    let supervisor = Arc::clone(&h.supervisor);
    let mut follower = std::pin::pin!(supervisor.refresh_resident_models());
    assert!(futures::poll!(follower.as_mut()).is_pending());
    assert!(leader
        .pending
        .lock()
        .await
        .remove(&wire.request_id)
        .unwrap()
        .send(WorkerReply::Typed(response_success(
            None,
            "refresh_models",
            Some(report("updated")),
        )))
        .is_ok());
    assert_eq!(
        follower.await.workers,
        Some(ModelRefreshWorkers {
            total: 1,
            succeeded: 1,
            failed: 0
        })
    );
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn whole_sweep_deadline_marks_unvisited_workers_unavailable() {
    let mut h = Harness::new();
    let mut workers = Vec::new();
    let mut receivers = Vec::new();
    for id in ["01", "02", "03", "04", "05", "06"] {
        let (worker, rx) = h.worker(id).await;
        workers.push(worker);
        receivers.push(rx);
    }
    let supervisor = Arc::clone(&h.supervisor);
    let sweep = tokio::spawn(async move { supervisor.refresh_resident_models().await });
    let _first = receivers[0].recv().await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(24)).await;
    assert!(
        !sweep.is_finished(),
        "a worker gets its bounded request budget"
    );
    tokio::time::advance(Duration::from_secs(86)).await;
    let result = sweep.await.unwrap();
    assert_eq!(
        result.workers,
        Some(ModelRefreshWorkers {
            total: 6,
            succeeded: 0,
            failed: 6
        })
    );
    for mut rx in receivers {
        assert!(rx.try_recv().is_err());
    }
    for worker in workers {
        assert!(worker.pending.lock().await.is_empty());
    }
}

#[tokio::test]
async fn snapshot_fanout_never_exceeds_four_workers() {
    let mut h = Harness::new();
    let (leader, rx) = h.worker("00").await;
    h.serve(leader, rx, |_| report("updated"));
    let (observed, mut calls) = mpsc::unbounded_channel();
    let (release, gate) = tokio::sync::watch::channel(false);
    for id in ["01", "02", "03", "04", "05", "06"] {
        let (worker, mut requests) = h.worker(id).await;
        let observed = observed.clone();
        let mut gate = gate.clone();
        h.pumps.push(tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                observed.send(request.command_type.clone()).unwrap();
                while !*gate.borrow_and_update() {
                    gate.changed().await.unwrap();
                }
                let response = response_success(
                    None,
                    &request.command_type,
                    Some(json!({"applied":["curated"],"skipped":[],"missing":[]})),
                );
                if let Some(reply) = worker.pending.lock().await.remove(&request.request_id) {
                    let _ = reply.send(WorkerReply::Typed(response));
                }
            }
        }));
    }
    let supervisor = Arc::clone(&h.supervisor);
    let sweep = tokio::spawn(async move { supervisor.refresh_resident_models().await });
    for _ in 0..4 {
        assert_eq!(calls.recv().await.as_deref(), Some("apply_model_snapshots"));
    }
    assert!(
        calls.try_recv().is_err(),
        "next batch cannot start before the first settles"
    );
    release.send(true).unwrap();
    assert_eq!(
        sweep.await.unwrap().workers,
        Some(ModelRefreshWorkers {
            total: 7,
            succeeded: 7,
            failed: 0
        })
    );
    for _ in 0..2 {
        assert_eq!(calls.recv().await.as_deref(), Some("apply_model_snapshots"));
    }
    assert!(calls.try_recv().is_err());
}

#[tokio::test]
async fn reports_preserve_leader_source_order_and_snapshot_followers_do_not_fetch() {
    let mut h = Harness::new();
    let (leader, rx) = h.worker("01").await;
    h.serve(leader, rx, |_| {
        json!({"startedAt":1,"finishedAt":2,"sources":[
            {"source":"z-first","status":"updated","modelCount":2},
            {"source":"a-second","status":"unchanged","modelCount":3}
        ]})
    });
    let (sibling, rx) = h.worker("02").await;
    h.serve(sibling, rx, |request| {
        assert_eq!(request.command_type, "apply_model_snapshots");
        json!({"applied":["z-first","a-second"],"skipped":[],"missing":[]})
    });
    let result = h.supervisor.refresh_resident_models().await;
    assert_eq!(
        serde_json::to_value(result.sources).unwrap(),
        json!([
            {"source":"z-first","status":"updated"},
            {"source":"a-second","status":"unchanged"}
        ])
    );
    assert_eq!(
        result.workers,
        Some(ModelRefreshWorkers {
            total: 2,
            succeeded: 2,
            failed: 0
        })
    );
}

#[tokio::test]
async fn client_refresh_has_a_two_minute_route_budget_and_rejects_old_workers() {
    let mut h = Harness::new();
    let (worker, mut requests) = h.worker("refresh-route").await;
    let (events, _receiver) = mpsc::channel(1);
    let attached =
        super::super::subscribers::ClientSubscriptions::new("refresh-client".into(), events);
    let command: DaemonCommand = serde_json::from_value(json!({
        "type": "refresh_models", "activeSessionId": "refresh-route", "sources": []
    }))
    .unwrap();
    // The same client route used by protocol dispatch must refuse an old worker
    // without queueing an unknown optional command on its channel.
    worker.set_model_refresh_capability(1, /*capable*/ false);
    let (response, _) = h
        .supervisor
        .route_client_command(
            &command,
            "refresh-client",
            &attached,
            "old".into(),
            "refresh_models".into(),
            None,
        )
        .await;
    assert_eq!(response[0]["success"], false);
    assert!(response[0]["error"]
        .as_str()
        .unwrap()
        .contains("does not support model refresh"));
    assert!(requests.try_recv().is_err());

    worker.set_model_refresh_capability(1, /*capable*/ true);
    let supervisor = Arc::clone(&h.supervisor);
    let route = tokio::spawn(async move {
        supervisor
            .route_client_command(
                &command,
                "refresh-client",
                &attached,
                "new".into(),
                "refresh_models".into(),
                None,
            )
            .await
    });
    let request = requests.recv().await.unwrap();
    assert_eq!(request.command_type, "refresh_models");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(119)).await;
    assert!(
        !route.is_finished(),
        "refresh must not use the short control budget"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    let (response, _) = route.await.unwrap();
    assert_eq!(response[0]["success"], false);
    assert!(worker.pending.lock().await.is_empty());
    let new_epoch = worker.note_connection_live();
    let (new_channel, mut old_peer_requests) = mpsc::channel(1);
    worker.install_command_channel(new_epoch, new_channel).await;
    worker.note_session_ready();
    // A late response from the superseded capable worker must not authorize
    // commands on the newly connected old peer.
    worker.set_model_refresh_capability(1, /*capable*/ true);
    let command = serde_json::from_value(json!({
        "type": "refresh_models", "activeSessionId": "refresh-route"
    }))
    .unwrap();
    let (events, _receiver) = mpsc::channel(1);
    let attached =
        super::super::subscribers::ClientSubscriptions::new("refresh-client".into(), events);
    let (response, _) = h
        .supervisor
        .route_client_command(
            &command,
            "refresh-client",
            &attached,
            "reconnected".into(),
            "refresh_models".into(),
            None,
        )
        .await;
    assert_eq!(response[0]["success"], false);
    assert!(old_peer_requests.try_recv().is_err());
}
