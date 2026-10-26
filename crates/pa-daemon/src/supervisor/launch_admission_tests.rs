//! Launch admission is private to one supervisor, and closes before terminal drain.
//! These tests never discover or signal an ambient process.
use super::*;

fn fixture() -> (tempfile::TempDir, Arc<Supervisor>) {
    let dir = tempfile::tempdir().expect("fixture");
    let supervisor = Arc::new(
        Supervisor::new(SupervisorOptions {
            socket_path: dir.path().join("supervisor.sock"),
            agent_dir: dir.path().join("agent"),
        })
        .expect("supervisor"),
    );
    (dir, supervisor)
}

async fn stopping(supervisor: &Supervisor) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !supervisor.shutting_down.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown closed launch admission");
}

#[tokio::test]
async fn launch_admission_refuses_after_terminal_stop() {
    let (_dir, supervisor) = fixture();
    supervisor.begin_shutdown().await;
    assert!(supervisor.accept_exit.load(Ordering::SeqCst));
    assert!(supervisor.acquire_launch_admission().is_err());
}

#[tokio::test]
async fn terminal_drain_waits_for_every_clone_of_the_owned_launch_permit() {
    let (_dir, supervisor) = fixture();
    let permit = supervisor
        .acquire_launch_admission()
        .expect("launch admitted");
    let cleanup_permit = Arc::clone(&permit);
    let stopping_supervisor = Arc::clone(&supervisor);
    let shutdown = tokio::spawn(async move { stopping_supervisor.begin_shutdown().await });
    stopping(&supervisor).await;
    assert!(supervisor.launch_admission.try_write().is_err());
    assert!(!supervisor.accept_exit.load(Ordering::SeqCst));
    assert!(!shutdown.is_finished());
    drop(permit);
    // The last cleanup owner, not the original caller, owns the drain barrier.
    assert!(supervisor.launch_admission.try_write().is_err());
    assert!(!supervisor.accept_exit.load(Ordering::SeqCst));
    drop(cleanup_permit);
    tokio::time::timeout(Duration::from_secs(1), shutdown)
        .await
        .expect("drain released")
        .expect("shutdown task");
    assert!(supervisor.accept_exit.load(Ordering::SeqCst));
    assert!(
        supervisor.launch_admission.try_write().is_ok(),
        "exclusive drain is not retained across callbacks"
    );
}

#[tokio::test]
async fn stopped_launch_reentry_refuses_without_waiting_for_the_exclusive_fence() {
    let (_dir, supervisor) = fixture();
    supervisor.shutting_down.store(true, Ordering::SeqCst);
    let exclusive = supervisor.launch_admission.write().await;
    // A callback trying to launch under terminal shutdown must reject BEFORE
    // waiting for a read permit; otherwise a queued/exclusive writer deadlocks it.
    let refused = supervisor.acquire_launch_admission();
    assert!(refused.is_err());
    drop(exclusive);
}

#[tokio::test]
async fn uncertain_launch_cleanup_never_certifies_terminal_exit() {
    let (_dir, supervisor) = fixture();
    supervisor
        .launch_cleanup_uncertain
        .store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(1), supervisor.begin_shutdown())
        .await
        .expect("uncertainty is bounded");
    assert!(!supervisor.accept_exit.load(Ordering::SeqCst));
    assert!(supervisor.launch_cleanup_uncertain.load(Ordering::SeqCst));
}

#[tokio::test]
async fn known_worker_boot_callback_does_not_reenter_a_queued_fair_read_fence() {
    let (dir, supervisor) = fixture();
    let permit = supervisor
        .acquire_launch_admission()
        .expect("launch admitted");
    let descriptor: crate::descriptor::WorkerDescriptor = serde_json::from_value(json!({
        "version": 2, "workerId": "callback", "pid": std::process::id(),
        "socketPath": dir.path().join("worker.sock"),
        "recoveryJournalPath": dir.path().join("recovery.jsonl"),
        "supervisorSocketPath": supervisor.options.socket_path,
        "authenticationToken": "fixture-only", "rootActiveSessionId": "callback",
        "createdAt": "t", "updatedAt": "t", "lifecycle": "starting",
        "createCommand": {}, "consecutiveFailures": 0
    }))
    .expect("descriptor");
    let resident = ResidentWorker::new(
        "callback".into(),
        descriptor,
        dir.path().join("worker.json"),
    );
    supervisor.registry.insert(resident).await;
    let writer = supervisor.launch_admission.write();
    tokio::pin!(writer);
    assert!(
        futures::poll!(writer.as_mut()).is_pending(),
        "writer is queued behind this launch"
    );
    let command = DaemonCommand::WorkerRegister {
        id: None,
        active_session_id: "callback".into(),
        session_id: None,
        socket_path: dir
            .path()
            .join("worker.sock")
            .to_string_lossy()
            .into_owned(),
        worker_instance_id: "fixture".into(),
        token: "fixture-only".into(),
        pid: u64::from(std::process::id()),
        rest: Map::default(),
    };
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        supervisor.handle_worker_register("callback", "worker_register", &command),
    )
    .await
    .expect("known callback must not wait behind writer");
    assert!(response.success, "known callback completes: {response:?}");
    drop(permit);
    drop(writer.await);
}

#[tokio::test]
async fn queued_terminal_writer_does_not_invert_unknown_registration_and_boot_adoption_guards() {
    let (_dir, supervisor) = fixture();
    let worker_id = "admission-inversion";
    let (entered, release) = super::launch_admission::pause_admission_at(&supervisor);
    // Unknown registration holds the real worker adoption guard BEFORE
    // acquiring launch admission. Force its first stop check to see false.
    let registering = Arc::clone(&supervisor);
    let runtime = tokio::runtime::Handle::current();
    let callback = tokio::task::spawn_blocking(move || {
        let _guard = runtime.block_on(registering.registry.adoption_guard(worker_id));
        registering.acquire_launch_admission()
    });
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .expect("registration passed first stop check")
        .expect("pause entered");

    // Boot adoption uses the opposite order: admission BEFORE the same
    // registry adoption guard. Poll the actual guard future to prove it blocks.
    let (blocked_tx, blocked) = tokio::sync::oneshot::channel();
    let adopting = Arc::clone(&supervisor);
    let boot = tokio::spawn(async move {
        let _admission = adopting.acquire_launch_admission().expect("boot admitted");
        let guard = adopting.registry.adoption_guard(worker_id);
        tokio::pin!(guard);
        assert!(
            futures::poll!(guard.as_mut()).is_pending(),
            "registration owns this guard"
        );
        blocked_tx.send(()).expect("guard ordering observed");
        let _guard = guard.await;
    });
    tokio::time::timeout(Duration::from_secs(1), blocked)
        .await
        .expect("boot blocked behind registration guard")
        .expect("blocked");
    let stopping_supervisor = Arc::clone(&supervisor);
    let shutdown = tokio::spawn(async move { stopping_supervisor.begin_shutdown().await });
    stopping(&supervisor).await;
    assert!(
        !shutdown.is_finished(),
        "boot read admission prevents terminal drain"
    );
    assert!(supervisor.launch_admission.try_write().is_err());

    // The callback passed stopping=false before the writer queued. Waiting
    // for read_owned here would complete the cycle and deadlock until timeout.
    release.send(()).expect("release registration admission");
    assert!(tokio::time::timeout(Duration::from_secs(1), callback)
        .await
        .expect("registration refuses instead of queueing behind writer")
        .expect("registration task")
        .is_err());
    tokio::time::timeout(Duration::from_secs(1), boot)
        .await
        .expect("registration released the actual adoption guard")
        .expect("boot task");
    tokio::time::timeout(Duration::from_secs(1), shutdown)
        .await
        .expect("boot released admission and terminal drained")
        .expect("shutdown task");
    assert!(supervisor.accept_exit.load(Ordering::SeqCst));
    assert!(supervisor.registry.list().await.is_empty());
    assert!(!supervisor.launch_cleanup_uncertain.load(Ordering::SeqCst));
}

#[tokio::test]
async fn admission_fast_check_race_refuses_after_terminal_writer_queues() {
    let (_dir, supervisor) = fixture();
    let outer = supervisor
        .acquire_launch_admission()
        .expect("outer launch admitted");
    let (entered, release) = super::launch_admission::pause_admission_at(&supervisor);
    let admitting = Arc::clone(&supervisor);
    let callback = tokio::task::spawn_blocking(move || admitting.acquire_launch_admission());
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .expect("first check observed stopping=false")
        .expect("pause entered");
    let stopping_supervisor = Arc::clone(&supervisor);
    let shutdown = tokio::spawn(async move { stopping_supervisor.begin_shutdown().await });
    stopping(&supervisor).await;
    assert!(!shutdown.is_finished());
    release.send(()).expect("resume after queued writer");
    assert!(tokio::time::timeout(Duration::from_secs(1), callback)
        .await
        .expect("must refuse without waiting for outer permit")
        .expect("callback task")
        .is_err());
    assert!(!shutdown.is_finished(), "outer admission is still held");
    drop(outer);
    tokio::time::timeout(Duration::from_secs(1), shutdown)
        .await
        .expect("terminal drain finished")
        .expect("shutdown task");
    assert!(supervisor.accept_exit.load(Ordering::SeqCst));
}
