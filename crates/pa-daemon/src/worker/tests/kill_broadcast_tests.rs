//! Kill and aborted-row broadcast e2e tests.
use super::*;

/// The #245 gap: the aborted row broadcasts and persists, but the goal's
/// accounting skips it.
#[allow(clippy::await_holding_lock)] // the faux registry is process-global: the guard must span the async flow
#[tokio::test]
async fn aborted_turn_row_broadcasts_and_persists_through_the_worker_gate() {
    let _faux = crate::agent_engine::tests::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!("pa-worker-aborted-row-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = WorkerConfig {
        socket_path: dir.join("worker.sock"),
        supervisor_socket_path: PathBuf::new(),
        token: "token".to_string(),
        worker_instance_id: String::new(),
        active_session_id: "aborted-row-session".to_string(),
        agent_dir: dir.join("agent"),
        recovery_journal_path: dir.join("recovery.jsonl"),
        telemetry_disabled: None,
        script: Some(json!({
            "engine": "faux",
            "responses": [
                "goal start reply",
                { "text": "held reply", "delayMs": 60000 },
            ],
        })),
    };
    let worker = std::sync::Arc::new(Worker::new(config, None));
    let fixture = crate::agent_engine::tests::abort_fixture::AbortFixture::install(
        worker
            .agent_engine
            .as_ref()
            .expect("the worker owns the real agent engine"),
        crate::agent_engine::tests::abort_fixture::AbortCase::BeforeStream,
    );
    let created = worker
        .dispatch(
            "create",
            &json!({ "noSession": true, "cwd": "/tmp", "name": "aborted-row" }),
        )
        .await;
    assert!(created.success, "create failed: {created:?}");
    let mut subscription = worker.events.subscribe();
    // `/goal`: the goal-start continuation turn runs to completion
    // inside the prompt, its usage accounted.
    let start = worker
        .dispatch(
            "prompt_and_wait",
            &json!({
                "activeSessionId": "aborted-row-session",
                "message": "/goal land the aborted row accounting",
            }),
        )
        .await;
    assert!(start.success, "the goal start failed: {start:?}");
    let goal_before = worker.engine.goal_state_value();
    assert_eq!(
        goal_before["status"],
        json!("active"),
        "state: {goal_before:?}"
    );
    assert!(
        goal_before["tokensUsed"].as_u64().unwrap_or(0) > 0,
        "the goal-start turn's usage accounted: {goal_before:?}"
    );
    // The turn-end mint queues the next continuation; the runner
    // admits it, then the provider fetch holds (the 60s reply).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let events = session_events_since(&mut subscription);
        let admitted = events.iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("message_start")
                && event["message"]["customType"] == "goal_context"
                && event["message"]["details"]["continuationsUsed"] == json!(1)
        });
        if admitted {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the continuation turn was never admitted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    // The actual second provider future is held until cancellation. A context
    // row or a wall-clock delay does not prove that this request has started.
    fixture
        .admitted()
        .await
        .expect("the continuation reached its provider");
    let abort = worker.dispatch("abort", &json!({})).await;
    assert!(abort.success, "abort failed: {abort:?}");
    let idle = worker.dispatch("wait_for_idle", &json!({})).await;
    assert!(idle.success, "never went idle: {idle:?}");
    // The row's own start frame (a no-partial abort begins a new
    // message) and the settled end frame with the aborted shape.
    let events = session_events_since(&mut subscription);
    let aborted_start = events
        .iter()
        .find(|event| {
            event.get("type").and_then(Value::as_str) == Some("message_start")
                && event["message"]["role"] == "assistant"
                && event["message"]["stopReason"] == json!("aborted")
        })
        .cloned()
        .expect("the aborted row's start frame reached the wire");
    assert_eq!(
        aborted_start["message"]["errorMessage"],
        json!("Request was aborted")
    );
    let aborted_end = events
        .iter()
        .rev()
        .find(|event| {
            event.get("type").and_then(Value::as_str) == Some("message_end")
                && event["message"]["role"] == "assistant"
                && event["message"]["stopReason"] == json!("aborted")
        })
        .cloned()
        .expect("the aborted row's end frame reached the wire");
    let row = &aborted_end["message"];
    assert_eq!(row["errorMessage"], json!("Request was aborted"));
    assert_eq!(row["usage"]["totalTokens"], json!(0));
    assert_eq!(row["usage"]["input"], json!(0));
    assert_eq!(row["usage"]["output"], json!(0));
    assert_eq!(row["content"], json!([{ "type": "text", "text": "" }]));
    // The terminal `turn_end` carries the aborted assistant row as
    // its payload; the trailing `Done` stays silent.
    let aborted_turn_end = events
        .iter()
        .rev()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("turn_end"))
        .cloned()
        .expect("the aborted turn's turn_end frame reached the wire");
    assert_eq!(aborted_turn_end["message"], *row);
    assert_eq!(aborted_turn_end["toolResults"], json!([]));
    assert_eq!(aborted_turn_end.get("error"), None);
    let bare_turn_end_count = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("turn_end")
                && event.get("message").is_none()
        })
        .count();
    assert_eq!(
        bare_turn_end_count, 0,
        "no bare turn_end frames: {events:?}"
    );
    // The session file holds the same aborted assistant row.
    let store_row = {
        let core = worker.core.lock().unwrap();
        core.store
            .as_ref()
            .expect("the worker owns a session file")
            .messages()
            .into_iter()
            .rev()
            .find(|message| {
                message.get("role").and_then(Value::as_str) == Some("assistant")
                    && message.get("stopReason").and_then(Value::as_str) == Some("aborted")
            })
            .expect("the aborted row persisted in the session file")
    };
    assert_eq!(store_row["errorMessage"], json!("Request was aborted"));
    assert_eq!(store_row["usage"]["totalTokens"], json!(0));
    assert_eq!(
        store_row["content"],
        json!([{ "type": "text", "text": "" }])
    );
    // The accounting fields are the state the goal-start turn left
    // (the wall-clock fields are time-based).
    let goal_after = worker.engine.goal_state_value();
    assert_eq!(
        goal_after["status"],
        json!("active"),
        "state: {goal_after:?}"
    );
    assert_eq!(goal_after["tokensUsed"], goal_before["tokensUsed"]);
    assert_eq!(
        goal_after["continuationsUsed"],
        goal_before["continuationsUsed"]
    );
    assert_eq!(goal_after["objective"], goal_before["objective"]);
}

/// The job's cancel is durable and the file archives, so no scheduled wake
/// revives the session.
#[allow(clippy::await_holding_lock)] // the faux registry is process-global: the guard must span the async flow
#[tokio::test]
async fn kill_cancels_the_sessions_scheduled_jobs() {
    let _faux = crate::agent_engine::tests::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!("pa-worker-kill-jobs-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let sessions_dir = dir.join("sessions");
    let config = WorkerConfig {
        socket_path: dir.join("worker.sock"),
        supervisor_socket_path: PathBuf::new(),
        token: "token".to_string(),
        worker_instance_id: String::new(),
        active_session_id: "kill-jobs-session".to_string(),
        agent_dir: dir.join("agent"),
        recovery_journal_path: dir.join("recovery.jsonl"),
        telemetry_disabled: None,
        script: Some(json!({ "engine": "faux", "responses": [] })),
    };
    let worker = std::sync::Arc::new(Worker::new(config, None));
    let created = worker
        .dispatch(
            "create",
            &json!({
                "cwd": dir.to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "name": "kill-jobs",
            }),
        )
        .await;
    assert!(created.success, "create failed: {created:?}");
    let data = created.data.expect("the create answers a summary");
    let session_id = data.get("sessionId").and_then(Value::as_str).expect("id");
    let session_file = data
        .get("sessionFile")
        .and_then(Value::as_str)
        .expect("session file");
    // A lane-liveness heartbeat on the session's artifact store.
    let job = worker
        .scheduled
        .store()
        .create(&pa_core::cron::store::CreateAgentCronJobInput {
            active_session_id: "kill-jobs-session".to_string(),
            session_id: session_id.to_string(),
            session_file: session_file.to_string(),
            cwd: dir.to_string_lossy().to_string(),
            prompt: "lane-liveness ping".to_string(),
            schedule_text: "every 10s".to_string(),
            source: Some("rlm_heartbeat".to_string()),
            now: Some(1),
            ..Default::default()
        })
        .expect("the store creates the job");
    assert_eq!(job.status, pa_core::cron::JobStatus::Active);

    let killed = worker.dispatch("kill", &json!({})).await;
    assert!(killed.success, "kill failed: {killed:?}");

    // The job cancelled durably: no later fire can wake the session.
    let stored = worker.scheduled.store().list();
    let cancelled = stored
        .iter()
        .find(|candidate| candidate.id == job.id)
        .expect("the job stays in the store");
    assert_eq!(cancelled.status, pa_core::cron::JobStatus::Cancelled);
    assert_eq!(cancelled.next_run_at, None);
    // The close archived the session file (the wake scan's state gate).
    let info = crate::session_store::read_session_info(std::path::Path::new(session_file)).unwrap();
    assert_eq!(info.state.as_deref(), Some("archived"));
}

/// The #247 matrix: the aborted row still broadcasts and persists, the
/// `archived` entry lands ahead of the row, and the close surfaces as a
/// `session_closed` frame.
#[allow(clippy::await_holding_lock)] // the faux registry is process-global: the guard must span the async flow
#[tokio::test]
async fn kill_cancels_a_mid_provider_wait_turn_and_surfaces_the_aborted_row() {
    let _faux = crate::agent_engine::tests::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!("pa-worker-kill-path-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = WorkerConfig {
        socket_path: dir.join("worker.sock"),
        supervisor_socket_path: PathBuf::new(),
        token: "token".to_string(),
        worker_instance_id: String::new(),
        active_session_id: "kill-path-session".to_string(),
        agent_dir: dir.join("agent"),
        recovery_journal_path: dir.join("recovery.jsonl"),
        telemetry_disabled: None,
        script: Some(json!({
            "engine": "faux",
            "responses": [
                { "text": "held reply", "delayMs": 60000 },
            ],
        })),
    };
    let worker = std::sync::Arc::new(Worker::new(config, None));
    let created = worker
        .dispatch(
            "create",
            &json!({ "noSession": true, "cwd": "/tmp", "name": "kill-path" }),
        )
        .await;
    assert!(created.success, "create failed: {created:?}");
    let mut subscription = worker.events.subscribe();
    // The turn runs detached (`prompt` answers immediately): its fetch
    // holds on the 60s reply, so the kill below lands mid-provider-wait.
    let prompt = worker
        .dispatch(
            "prompt",
            &json!({
                "activeSessionId": "kill-path-session",
                "message": "held turn for the kill probe",
            }),
        )
        .await;
    assert!(prompt.success, "prompt failed: {prompt:?}");
    // The busy flag is the runner's own admission marker (`await_session_work_settled`
    // parks on it too): no assistant message_start arrives before the kill.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if worker.core.lock().unwrap().busy {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the held turn was never admitted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    // The kill must answer on the cancelled turn, not the 60s hold: the abort
    // funnel fires before the archive/dispose waits on the session mutex.
    let started = std::time::Instant::now();
    let killed = worker.dispatch("kill", &json!({})).await;
    assert!(killed.success, "kill failed: {killed:?}");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "kill waited out the held provider response ({elapsed:?})"
    );
    // The aborted row surfaced. Drain every session-event frame: the close
    // rides the same outbound type as the whole frame (the SessionClosed payload
    // has no `event` wrapper), so it must surface whole.
    let mut events = Vec::new();
    while let Ok(frame) = subscription.try_recv() {
        if frame.outbound_type != "session_event" {
            continue;
        }
        let Ok(outbound) = serde_json::from_slice::<Value>(&frame.payload) else {
            continue;
        };
        match outbound.get("event") {
            Some(event) if event.is_object() => events.push(event.clone()),
            _ => events.push(outbound),
        }
    }
    let aborted_end = events
        .iter()
        .rev()
        .find(|event| {
            event.get("type").and_then(Value::as_str) == Some("message_end")
                && event["message"]["role"] == "assistant"
                && event["message"]["stopReason"] == json!("aborted")
        })
        .cloned()
        .expect("the aborted row's end frame reached the wire");
    assert_eq!(
        aborted_end["message"]["errorMessage"],
        json!("Request was aborted")
    );
    // The close reached the wire as `session_closed` (reason "killed").
    assert!(
        events.iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("session_closed")
                && event.get("reason").and_then(Value::as_str) == Some("killed")
        }),
        "the kill closed the session on the wire: {events:?}"
    );
    // The aborted row persisted, and the `archived` entry lands ahead of it
    // (the archive runs before the abort settles the row).
    let entries = {
        let core = worker.core.lock().unwrap();
        core.store
            .as_ref()
            .expect("the worker owns a session file")
            .entries()
            .to_vec()
    };
    let archived_at = entries
        .iter()
        .position(|entry| {
            entry.type_ == "session_state" && entry.fields["state"]["status"] == json!("archived")
        })
        .expect("the session archived on kill");
    let aborted_at = entries
        .iter()
        .position(|entry| {
            entry.type_ == "message"
                && entry.fields["message"]["role"] == json!("assistant")
                && entry.fields["message"]["stopReason"] == json!("aborted")
        })
        .expect("the aborted row persisted in the session file");
    assert!(
        archived_at < aborted_at,
        "the archived lifecycle entry must precede the aborted row"
    );
    assert_eq!(
        entries[aborted_at].fields["message"]["errorMessage"],
        json!("Request was aborted")
    );
    // The session is closed: `created` fell with the archive.
    assert!(!worker.core.lock().unwrap().created);
}

/// TS `compact()` detaches from agent events before the abort, so this row
/// stays off the wire and file while `abort` broadcasts it.
#[allow(clippy::await_holding_lock)] // the faux registry is process-global: the guard must span the async flow
#[tokio::test]
async fn compact_interrupt_swallows_the_aborted_row() {
    let _faux = crate::agent_engine::tests::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir =
        std::env::temp_dir().join(format!("pa-worker-compact-abort-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = WorkerConfig {
        socket_path: dir.join("worker.sock"),
        supervisor_socket_path: PathBuf::new(),
        token: "token".to_string(),
        worker_instance_id: String::new(),
        active_session_id: "compact-abort-session".to_string(),
        agent_dir: dir.join("agent"),
        recovery_journal_path: dir.join("recovery.jsonl"),
        telemetry_disabled: None,
        script: Some(json!({
            "engine": "faux",
            "responses": [{ "text": "held reply", "delayMs": 60000 }],
        })),
    };
    let worker = std::sync::Arc::new(Worker::new(config, None));
    let created = worker
        .dispatch(
            "create",
            &json!({ "noSession": true, "cwd": "/tmp", "name": "compact-abort" }),
        )
        .await;
    assert!(created.success, "create failed: {created:?}");
    let mut subscription = worker.events.subscribe();
    let turn = worker
        .dispatch(
            "prompt",
            &json!({
                "activeSessionId": "compact-abort-session",
                "message": "held turn for the compact interrupt",
            }),
        )
        .await;
    assert!(turn.success, "the prompt failed: {turn:?}");
    // Let the admitted turn reach the provider (the 60s hold), then compact: the
    // aborted row must stay off the wire (TS `_disconnectFromAgent`).
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let compact = worker
        .dispatch(
            "compact",
            &json!({ "activeSessionId": "compact-abort-session" }),
        )
        .await;
    // The faux engine's compact outcome is not the claim; the turn settled before it.
    let _ = compact;
    let idle = worker.dispatch("wait_for_idle", &json!({})).await;
    assert!(idle.success, "never went idle: {idle:?}");
    let events = session_events_since(&mut subscription);
    let aborted_rows = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("message_end")
                && event["message"]["role"] == "assistant"
                && event["message"]["stopReason"] == "aborted"
        })
        .count();
    assert_eq!(
        aborted_rows, 0,
        "the compact path swallows the aborted row: {events:?}"
    );
    // The suppressed run's `agent_end` stays off the wire entirely — neither
    // the engine's per-run frame nor the worker's trailing fallback.
    let agent_ends = events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("agent_end"))
        .count();
    assert_eq!(
        agent_ends, 0,
        "the compact path emits no agent_end for the suppressed run: {events:?}"
    );
    // And out of the session file.
    let aborted_store_rows = {
        let core = worker.core.lock().unwrap();
        core.store
            .as_ref()
            .expect("the worker owns a session file")
            .messages()
            .into_iter()
            .filter(|message| {
                message.get("role").and_then(Value::as_str) == Some("assistant")
                    && message.get("stopReason").and_then(Value::as_str) == Some("aborted")
            })
            .count()
    };
    assert_eq!(
        aborted_store_rows, 0,
        "the compact path never persists the aborted row"
    );
}

/// A provider's own terminal Aborted row is not the loop's synthetic no-partial
/// row. Its actual empty vector must survive the worker's collector and writer.
#[allow(clippy::await_holding_lock)] // the unchanged first response uses the process-global faux registry
#[tokio::test]
async fn provider_aborted_empty_content_survives_worker_wire_and_journal() {
    use crate::agent_engine::tests::abort_fixture::{AbortCase, AbortFixture, NATIVE_RESPONSE_ID};

    let _faux = crate::agent_engine::tests::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().expect("fixture directory");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session directory");
    let worker = Arc::new(Worker::new(
        WorkerConfig {
            socket_path: dir.path().join("worker.sock"),
            supervisor_socket_path: PathBuf::new(),
            token: "native-abort-fixture".to_owned(),
            worker_instance_id: String::new(),
            active_session_id: "native-abort-session".to_owned(),
            agent_dir: dir.path().join("agent"),
            recovery_journal_path: dir.path().join("recovery.jsonl"),
            telemetry_disabled: Some(true),
            script: Some(json!({
                "engine": "faux", "responses": [{"text": "goal start reply"}],
            })),
        },
        None,
    ));
    // Worker::new has created the concrete engine but no SDK session. Install
    // synchronously, before create can start its background SDK build.
    assert!(!worker.core.lock().unwrap().created);
    let engine = Arc::clone(worker.agent_engine.as_ref().expect("real agent engine"));
    let fixture = AbortFixture::install(&engine, AbortCase::NativeTerminal);
    let created = worker
        .dispatch(
            "create",
            &json!({
                "cwd": dir.path().to_string_lossy(), "sessionDir": session_dir,
                "name": "native-abort-contract",
            }),
        )
        .await;
    assert!(
        created.success,
        "create failed before any turn: {created:?}"
    );
    let mut subscription = worker.events.subscribe();

    // Readiness errors are values until the held run has been cancelled and
    // observed idle. No readiness/shape assertion can strand a provider wait.
    let mut start_task = None;
    let observed: anyhow::Result<_> = async {
        fixture.built().await?;
        let prompt_worker = Arc::clone(&worker);
        start_task = Some(tokio::spawn(async move {
            prompt_worker
                .dispatch(
                    "prompt_and_wait",
                    &json!({
                        "activeSessionId": "native-abort-session",
                        "message": "/goal preserve the native aborted provider row",
                    }),
                )
                .await
        }));
        let start_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            start_task.as_mut().expect("owned initial prompt task"),
        )
        .await?;
        // A completed JoinHandle must not be polled twice.
        start_task.take();
        let start = start_result?;
        anyhow::ensure!(start.success, "goal start failed: {start:?}");
        fixture.admitted().await?;
        let before = worker.engine.goal_state_value();
        let path = {
            let core = worker.core.lock().unwrap();
            let store = core
                .store
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing canonical store"))?;
            anyhow::ensure!(
                store.lease.is_some(),
                "the canonical writer must own its real runtime lease"
            );
            store.path.clone()
        };
        let history_before = std::fs::read(&path)?;
        fixture.release();
        // No abort command and no aborted signal in this case: the provider
        // itself returns Error(Aborted, []) with a distinctive response id.
        let idle = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            worker.dispatch("wait_for_idle", &json!({})),
        )
        .await?;
        anyhow::ensure!(idle.success, "native provider turn never settled: {idle:?}");
        let messages = worker.dispatch("get_messages", &json!({})).await;
        anyhow::ensure!(messages.success, "get_messages failed: {messages:?}");
        Ok((before, path, history_before, messages))
    }
    .await;
    let mut cleanup_error = None;
    if observed.is_err() {
        fixture.cancel();
        let cleanup = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            worker.dispatch("wait_for_idle", &json!({})),
        )
        .await;
        if !cleanup.as_ref().is_ok_and(|idle| idle.success) {
            cleanup_error = Some(format!("bounded worker idle cleanup failed: {cleanup:?}"));
        }
    }
    // Explicit bounded join of our own producer on every error path before panic.
    // Worker internal runner handles remain production-owned; idle is their fence.
    if let Some(mut task) = start_task.take() {
        task.abort();
        if tokio::time::timeout(std::time::Duration::from_secs(10), &mut task)
            .await
            .is_err()
        {
            cleanup_error = Some("initial prompt task did not join within 10s".to_owned());
        }
    }
    assert!(
        cleanup_error.is_none(),
        "{cleanup_error:?}; observation: {observed:?}"
    );
    let (before, path, history_before, messages) = observed.expect("native abort fixture settled");
    let events = session_events_since(&mut subscription);
    let named = |kind: &str| -> &Value {
        let matching: Vec<_> = events
            .iter()
            .filter(|event| {
                event["type"] == kind && event["message"]["responseId"] == NATIVE_RESPONSE_ID
            })
            .collect();
        assert_eq!(matching.len(), 1, "expected one {kind}: {events:?}");
        &matching[0]["message"]
    };
    let start = named("message_start");
    let end = named("message_end");
    let terminal = named("turn_end");
    assert_eq!(
        start["content"],
        json!([]),
        "native Start must not be synthesized"
    );
    assert_eq!(
        end["content"],
        json!([]),
        "provider's actual content is exactly []"
    );
    assert_eq!(terminal["content"], json!([]));
    assert_eq!(end["role"], json!("assistant"));
    assert_eq!(end["stopReason"], json!("aborted"));
    assert_eq!(end["errorMessage"], json!("Request was aborted"));
    assert_eq!(end["usage"]["totalTokens"], json!(0));
    assert_eq!(end["usage"]["input"], json!(0));
    assert_eq!(end["usage"]["output"], json!(0));
    assert_eq!(end["responseId"], json!(NATIVE_RESPONSE_ID));
    assert_eq!(start, end);
    assert_eq!(terminal, end);
    let messages = messages.data.as_ref().expect("message response")["messages"]
        .as_array()
        .expect("messages array");
    let displayed = messages
        .iter()
        .find(|message| message["responseId"] == NATIVE_RESPONSE_ID)
        .expect("native row in get_messages");
    assert_eq!(displayed["content"], json!([]));
    assert_eq!(displayed, end);

    let physical = std::fs::read(&path).expect("physical canonical journal");
    assert!(
        physical.starts_with(&history_before),
        "prior canonical history changed"
    );
    let entries = crate::session_store::parse_session_entries(
        std::str::from_utf8(&physical).expect("canonical UTF-8"),
    );
    let disk_row = &entries
        .iter()
        .find(|entry| {
            entry["type"] == "message" && entry["message"]["responseId"] == NATIVE_RESPONSE_ID
        })
        .expect("native row physically journaled")["message"];
    assert_eq!(disk_row["content"], json!([]));
    assert_eq!(disk_row["stopReason"], json!("aborted"));
    assert_eq!(disk_row, end);
    let reopened =
        crate::session_store::SessionFile::open(&path).expect("reopen canonical journal");
    let reopened_messages = reopened.messages();
    let restored = reopened_messages
        .iter()
        .find(|message| message["responseId"] == NATIVE_RESPONSE_ID)
        .expect("native row survives reopen");
    assert_eq!(restored["content"], json!([]));
    assert_eq!(restored, end);
    assert_eq!(before["status"], json!("active"));
    assert!(before["tokensUsed"].as_u64().unwrap_or(0) > 0);
    let after = worker.engine.goal_state_value();
    assert_eq!(after["status"], json!("active"));
    assert_eq!(after["objective"], before["objective"]);
    assert_eq!(after["tokensUsed"], before["tokensUsed"]);
    assert_eq!(after["continuationsUsed"], before["continuationsUsed"]);
    assert_eq!(fixture.calls(), 2, "no further paid provider work");
    let core = worker.core.lock().unwrap();
    assert!(!core.busy);
    assert!(core.steering.is_empty() && core.follow_up.is_empty());
}
