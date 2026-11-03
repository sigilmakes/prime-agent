//! The rlm-children tests (the persisted max-depth scan, the settled-child release probe).
use super::*;
use pa_core::session_engine::rlm_host::persisted_rlm_max_depth;

#[test]
fn persisted_rlm_max_depth_reads_validated_log_and_rejects_damage() {
    use pa_core::session_log::{Outcome, SessionLog};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("depth.jsonl");
    let path_str = path.to_str().unwrap();
    let mut log = SessionLog::open(dir.path(), "depth", std::time::Instant::now()).unwrap();
    for row in [
        json!({"type":"session","version":3,"id":"depth","cwd":"/tmp","timestamp":"2026-01-01T00:00:00Z"}),
        json!({"type":"custom","id":"first","customType":"rlm_max_depth_state","data":{"maxDepth":5}}),
        json!({"type":"custom","id":"second","parentId":"first","customType":"rlm_max_depth_state","data":{"maxDepth":11}}),
        json!({"type":"custom","id":"invalid","parentId":"second","customType":"rlm_max_depth_state","data":{"maxDepth":"many"}}),
    ] {
        assert!(matches!(
            log.append(&row, std::time::Instant::now()),
            Outcome::Committed { .. }
        ));
    }
    assert_eq!(persisted_rlm_max_depth(Some(path_str)), Some(11));
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.pop();
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(persisted_rlm_max_depth(Some(path_str)), None);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::write(
        &path,
        "{\"type\":\"custom\",\"customType\":\"rlm_max_depth_state\",\"data\":{\"maxDepth\":5}}\n",
    )
    .unwrap();
    assert_eq!(persisted_rlm_max_depth(Some(path_str)), None);
    assert_eq!(persisted_rlm_max_depth(None), None);
    assert_eq!(
        persisted_rlm_max_depth(Some("/nonexistent/depth.jsonl")),
        None
    );
}

/// The settled-child kernel release policy (TS #2483): with no registered
/// jobs the release probe fires; a jobs probe reporting this session's jobs
/// defers it.
#[test]
fn release_settled_child_kernel_defers_to_scheduled_jobs_and_fires_the_release_probe() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = std::sync::Arc::new(bare_engine(dir.path()));
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe_fired = std::sync::Arc::clone(&fired);
    *engine
        .kernel_release_probe
        .lock()
        .expect("kernel release probe lock") = Some(std::sync::Arc::new(move || {
        probe_fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(std::future::ready(()))
    }));
    // No children and no jobs probe: the release probe fires once.
    engine
        .runtime
        .block_on(crate::engine::SessionEngine::release_settled_child_kernel(
            &*engine,
        ));
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
    // A jobs probe reporting jobs defers the release.
    *engine
        .registered_jobs_probe
        .lock()
        .expect("registered jobs probe lock") = Some(std::sync::Arc::new(|| true));
    engine
        .runtime
        .block_on(crate::engine::SessionEngine::release_settled_child_kernel(
            &*engine,
        ));
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// The whole-worker idle passivation gate accepts exactly when the
/// kernel release would fire under the shared settled gates, plus the
/// worker-only registry rule: a child record keeps the worker resident
/// (a revival rebuilds only settled ledger rows; the passivation stop
/// closes every row) while the kernel release still fires — releasing
/// a kernel keeps the worker and its registry resident.
#[test]
fn can_passivate_worker_mirrors_the_release_gates_and_adds_the_registry_rule() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = std::sync::Arc::new(
        AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            provider: None,
            model: None,
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: Some(crate::agent_engine::SupervisorLinkConfig {
                socket_path: dir.path().join("dead.sock"),
                active_session_id: "parent-session".to_string(),
                worker_token: "token".to_string(),
            }),
            cron_store: None,
            queued_steering_probe: None,
        })
        .unwrap(),
    );
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe_fired = std::sync::Arc::clone(&fired);
    *engine
        .kernel_release_probe
        .lock()
        .expect("kernel release probe lock") = Some(std::sync::Arc::new(move || {
        probe_fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(std::future::ready(()))
    }));
    assert!(engine
        .runtime
        .block_on(crate::engine::SessionEngine::can_passivate_worker(&*engine)));
    // A live background bash handle blocks the passivation, and the release consumes
    // the same gate: the probe never fires while a handle runs.
    *engine
        .background_bash_probe
        .lock()
        .expect("background bash probe lock") = Some(std::sync::Arc::new(|| true));
    assert!(!engine
        .runtime
        .block_on(crate::engine::SessionEngine::can_passivate_worker(&*engine)));
    engine
        .runtime
        .block_on(crate::engine::SessionEngine::release_settled_child_kernel(
            &*engine,
        ));
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 0);
    *engine
        .background_bash_probe
        .lock()
        .expect("background bash probe lock") = None;
    *engine
        .registered_jobs_probe
        .lock()
        .expect("registered jobs probe lock") = Some(std::sync::Arc::new(|| true));
    assert!(!engine
        .runtime
        .block_on(crate::engine::SessionEngine::can_passivate_worker(&*engine)));
    engine
        .runtime
        .block_on(crate::engine::SessionEngine::release_settled_child_kernel(
            &*engine,
        ));
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 0);
    *engine
        .registered_jobs_probe
        .lock()
        .expect("registered jobs probe lock") = None;
    // A settled child record keeps the worker resident (a revival
    // rebuilds only settled ledger rows; the passivation stop closes
    // every row) while the kernel release still fires: the registry
    // rule is whole-worker only.
    let children = engine.children.clone().expect("children registry");
    engine.runtime.block_on(async {
        children
            .push_test_child(crate::rlm_children::RlmChildIdentity {
                rlm_child_id: "child-1".to_string(),
                active_session_id: "child-session".to_string(),
                session_id: None,
                session_name: "worker-1".to_string(),
            })
            .await;
        children.settle_test_child("child-session").await;
    });
    assert!(!engine
        .runtime
        .block_on(crate::engine::SessionEngine::can_passivate_worker(&*engine)));
    engine
        .runtime
        .block_on(crate::engine::SessionEngine::release_settled_child_kernel(
            &*engine,
        ));
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
}
