#![cfg(unix)]
use super::*;
use pa_core::session_engine::rlm_usage::{
    RlmChildUsageAttributions, RlmChildUsageReport, RlmChildUsageSink,
};

struct UsageSink(Arc<RlmChildUsageAttributions>);
impl RlmChildUsageSink for UsageSink {
    fn record(
        &self,
        report: RlmChildUsageReport,
    ) -> pa_core::session_engine::rlm_usage::RlmChildUsageFuture<'_, bool> {
        Box::pin(async move { self.0.record_child_usage(report).await })
    }
    fn forget(
        &self,
        child: &str,
    ) -> pa_core::session_engine::rlm_usage::RlmChildUsageFuture<'_, ()> {
        let child = child.to_owned();
        Box::pin(async move { self.0.forget_child(&child).await })
    }
}

async fn accounting_worker(
    dir: &std::path::Path,
) -> (Arc<Worker>, PathBuf, tokio::task::JoinHandle<()>) {
    let socket = dir.join("supervisor.sock");
    let (_, server) = fake_supervisor(&socket);
    let worker = Arc::new(Worker::new(
        WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: socket,
            token: "token".into(),
            worker_instance_id: String::new(),
            active_session_id: "accounting-parent".into(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({"engine":"faux", "responses":["parent terminal"]})),
        },
        None,
    ));
    let created = worker
        .dispatch(
            "create",
            &json!({
                "cwd":dir, "sessionDir":dir.join("sessions"), "rlmDepth":1,
            }),
        )
        .await;
    assert!(created.success, "{created:?}");
    let path = PathBuf::from(created.data.unwrap()["sessionFile"].as_str().unwrap());
    let response = worker
        .dispatch("prompt_and_wait", &json!({"message":"warm authority"}))
        .await;
    assert!(response.success, "{response:?}");
    assert!(worker
        .core
        .lock()
        .unwrap()
        .store
        .as_ref()
        .unwrap()
        .epoch
        .is_some());
    (worker, path, server)
}

async fn child_with_usage(
    worker: &Worker,
    dir: &std::path::Path,
) -> Arc<crate::rlm_children::SupervisorChildSessions> {
    let children = worker
        .agent_engine
        .as_ref()
        .unwrap()
        .children
        .clone()
        .unwrap();
    let producer = Arc::new(RlmChildUsageAttributions::new(Arc::new(CoreUsageStore(
        worker.core.clone(),
    ))));
    producer.register_spawn("owed-child").await;
    children.set_usage_sink(Arc::new(UsageSink(producer)));
    children
        .push_test_child(crate::rlm_children::RlmChildIdentity {
            rlm_child_id: "owed-child".into(),
            active_session_id: "child-live".into(),
            session_id: Some("child-durable".into()),
            session_name: "child".into(),
        })
        .await;
    let mut child = SessionFile::create(dir.to_str().unwrap(), None, 2);
    child.set_path(dir.join("child.jsonl"));
    child.rewrite().unwrap();
    child
        .persist_entry(
            "message",
            json!({"message":{
                "role":"assistant", "content":[], "stopReason":"stop", "timestamp":1,
                "api":"test", "provider":"test", "model":"test",
                "usage":{"input":17,"output":3,"totalTokens":20,"cacheRead":0,"cacheWrite":0,
                    "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}
            }}),
        )
        .unwrap();
    children
}

#[allow(clippy::await_holding_lock)] // The faux provider registry is process-global.
#[tokio::test]
async fn shutdown_flushes_child_usage_before_closing_canonical_authority() {
    let _faux = crate::agent_engine::tests::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (worker, path, server) = accounting_worker(dir.path()).await;
    let children = child_with_usage(&worker, dir.path()).await;
    children
        .set_test_child_usage_file(
            "owed-child",
            dir.path().join("child.jsonl").display().to_string(),
        )
        .await;
    let before = std::fs::read(&path).unwrap();
    let response = worker.handle_shutdown().await;
    assert!(response.success, "{response:?}");
    let reopened = SessionFile::open(&path).unwrap();
    let rows: Vec<_> = reopened
        .entries()
        .iter()
        .filter(|row| row.type_ == "child_usage_attributed")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "final child accounting must precede successful shutdown"
    );
    assert_eq!(rows[0].fields["childUsage"]["input"], 17);
    assert!(std::fs::read(&path).unwrap().starts_with(&before));
    assert!(!children.any_running().await);
    assert!(worker
        .core
        .lock()
        .unwrap()
        .store
        .as_ref()
        .unwrap()
        .lease
        .is_none());
    server.abort();
}

#[allow(clippy::await_holding_lock)] // The faux provider registry is process-global.
#[tokio::test]
async fn close_refuses_to_discard_owed_usage_after_source_conflict() {
    use std::io::Write;
    let _faux = crate::agent_engine::tests::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for command in ["shutdown", "kill"] {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let (worker, path, server) = accounting_worker(dir.path()).await;
        let children = child_with_usage(&worker, dir.path()).await;
        children
            .set_test_child_usage_file(
                "owed-child",
                dir.path().join("child.jsonl").display().to_string(),
            )
            .await;
        // A foreign incomplete tail is not permission to append or reset authority.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let response = worker.dispatch(command, &json!({})).await;
        assert!(
            !response.success,
            "{command}: uncertain accounting must not acknowledge close"
        );
        {
            let core = worker.core.lock().unwrap();
            let store = core.store.as_ref().unwrap();
            assert!(store.lease.is_some());
            assert!(store.epoch.is_some());
        }
        let retry = worker.dispatch(command, &json!({})).await;
        assert!(
            !retry.success,
            "{command}: a repeated close must not erase source uncertainty"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        worker.agent_engine.as_ref().unwrap().dispose_kernel().await;
        server.abort();
    }
}

#[allow(clippy::await_holding_lock)] // The faux provider registry is process-global.
#[tokio::test]
async fn close_keeps_accounting_reachable_until_child_source_can_be_read() {
    let _faux = crate::agent_engine::tests::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for command in ["shutdown", "kill"] {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let (worker, path, server) = accounting_worker(dir.path()).await;
        let children = child_with_usage(&worker, dir.path()).await;
        children
            .set_test_child_usage_file(
                "owed-child",
                dir.path().join("child.jsonl").display().to_string(),
            )
            .await;
        let child = dir.path().join("child.jsonl");
        let unavailable = dir.path().join("child-unavailable.jsonl");
        std::fs::rename(&child, &unavailable).unwrap();
        let response = worker.dispatch(command, &json!({})).await;
        assert!(
            !response.success,
            "{command}: unread final child spend must remain owed"
        );
        assert!(worker
            .core
            .lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .lease
            .is_some());
        std::fs::rename(&unavailable, &child).unwrap();
        let response = worker.dispatch(command, &json!({})).await;
        assert!(
            response.success,
            "{command}: explicit retry after source recovery: {response:?}"
        );
        let reopened = SessionFile::open(&path).unwrap();
        assert_eq!(
            reopened
                .entries()
                .iter()
                .filter(|row| row.type_ == "child_usage_attributed")
                .count(),
            1
        );
        server.abort();
    }
}
