//! The quota-park tests (the park entry/wake durability, resume/retire clearing).
use super::*;

/// An engine with the quota-park seams live (cron wiring, supervisor link, durable session id).
async fn park_engine(dir: &std::path::Path) -> std::sync::Arc<AgentSessionEngine> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _faux = FAUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::fs::create_dir_all(dir.join("agent")).unwrap();
        let session_file = dir.join("park-session.jsonl");
        let mut session_store =
            crate::session_store::SessionFile::create(dir.to_str().unwrap(), None, 0);
        session_store.set_path(session_file.clone());
        session_store
            .rewrite()
            .expect("initialize the quota session log");
        let store =
            std::sync::Arc::new(pa_core::cron::store::AgentCronJobStore::for_session_artifacts());
        store.register_session_artifact("park-session", &dir.join("artifacts"));
        let cron_store = pa_core::session_engine::runtime_wiring::KernelCronWiring {
            store,
            binding: None,
            mutation_hook: None,
        };
        let engine = std::sync::Arc::new(
            AgentSessionEngine::new(AgentEngineConfig {
                cwd: dir.clone(),
                agent_dir: dir.join("agent"),
                provider: None,
                model: None,
                api_key: None,
                thinking: None,
                session_dir: None,
                session_file: Some(session_file),
                faux_script: Some(json!({"engine":"faux", "responses":[]}).to_string()),
                supervisor_link: Some(SupervisorLinkConfig {
                    socket_path: dir.join("supervisor.sock"),
                    active_session_id: "active-park".to_string(),
                    worker_token: "token".to_string(),
                }),
                telemetry_disabled: None,
                cron_store: Some(cron_store),
                queued_steering_probe: None,
            })
            .unwrap(),
        );
        engine.register_arc();
        own_log_store(&engine, session_store);
        let model = engine.resolve_model().expect("local quota fixture model");
        engine
            .runtime
            .block_on(engine.ensure_core_session_async(&model))
            .expect("bind quota persistence to the serving log owner");
        engine
    })
    .await
    .expect("quota fixture build")
}

fn quota_failure_message(
    kind: Option<&str>,
    retry_after_ms: Option<u64>,
) -> pa_agent::types::AssistantMessage {
    let details = serde_json::json!({
        "kind": kind,
        "retryAfterMs": retry_after_ms,
    });
    pa_agent::types::AssistantMessage {
        content: vec![],
        api: "openai-completions".to_string(),
        provider: "battery".to_string(),
        model: "mock-1".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: Some(vec![pa_agent::types::AssistantMessageDiagnostic {
            kind: "provider_stream_failure".to_string(),
            timestamp: 0,
            error: None,
            details: Some(details),
        }]),
        usage: pa_agent::types::Usage::zero(),
        stop_reason: pa_agent::types::StopReason::Error,
        stop_reason_raw: None,
        error_message: Some("You have hit your usage limit".to_string()),
        timestamp: 0,
    }
}

/// A park record must report a failed write through the installed owner sink.
#[tokio::test]
async fn a_failed_park_entry_write_propagates_to_the_caller() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = park_engine(dir.path()).await;
    let session_file = dir.path().join("park-session.jsonl");
    let handle = engine
        .session
        .lock()
        .await
        .as_ref()
        .unwrap()
        .session
        .shared_persistence();
    // The owner still exists, but its durable path can no longer be read or
    // appended. This fails even when the fixture runs with elevated rights.
    std::fs::remove_file(&session_file).expect("remove temporary session log");
    std::fs::create_dir(&session_file).expect("block temporary session log path");
    let result = engine
        .append_quota_park_entry(
            Some(handle),
            crate::util::now_ms() + 30_000,
            1,
            Some("job-1"),
            Some("battery"),
        )
        .await;
    assert!(
        result.is_err(),
        "a failed durable write must propagate to the park-arming gate"
    );
}

#[tokio::test]
async fn quota_failure_beyond_cap_parks_with_a_durable_wake() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = park_engine(dir.path()).await;
    let message = quota_failure_message(Some("rate_limit"), Some(3_600_000));
    // Captured BEFORE the park call: test-process delay can only widen
    // the slack, never shrink it under the window's lower bound.
    let before_park_ms = crate::util::now_ms();
    let outcome = engine
            .park_for_quota_reset(
                &message,
                "Provider requested a 3600s wait before retrying (above retry.provider.maxRetryDelayMs=60000ms)",
            )
            .await
            .expect("the park fires");
    assert!(
        outcome.status_message.contains("Session parked until ")
            && outcome.status_message.contains(
                "and will resume automatically (retry.provider.waitForUsage.pauseUntilReset)"
            )
    );
    assert!(engine.is_quota_parked());
    let park = engine
        .quota_park
        .lock()
        .expect("park state")
        .clone()
        .expect("parked");
    assert_eq!(park.park_count, 1);
    let job_id = park.job_id.expect("the wake job id");
    let store = engine.config.cron_store.as_ref().expect("wiring").clone();
    let job = store
        .store
        .list()
        .into_iter()
        .find(|job| job.id == job_id)
        .expect("the durable wake job");
    assert_eq!(
        job.label.as_deref(),
        Some("quota-resume"),
        "the wake carries the quota-resume label"
    );
    assert_eq!(
        job.prompt,
        pa_core::session_engine::provider_park::QUOTA_RESUME_MARKER_TEXT
    );
    assert_eq!(job.schedule.kind, pa_core::cron::ScheduleKind::Once);
    assert_eq!(job.active_session_id, "active-park");
    assert_eq!(job.session_id, "park-session");
    let reopened = crate::session_store::SessionFile::open(&dir.path().join("park-session.jsonl"))
        .expect("replay the parked session log");
    let parked_row = reopened
        .entries()
        .iter()
        .find(|entry| {
            entry.fields.get("customType").and_then(Value::as_str)
                == Some(pa_core::session_engine::provider_park::PROVIDER_QUOTA_PARK_ENTRY)
        })
        .expect("the installed sink durably recorded the park");
    assert_eq!(parked_row.fields["data"]["jobId"], job_id);
    assert_eq!(parked_row.fields["data"]["parkCount"], 1);
    // The wake sits ~30s past the reported reset (the grace).
    let resume_slack = park.resume_at_ms as i64 - (before_park_ms as i64 + 3_600_000);
    assert!(
        (25_000..=40_000).contains(&resume_slack),
        "resume slack {resume_slack}"
    );
}

#[tokio::test]
async fn repeat_failure_while_parked_keeps_the_scheduled_wake() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = park_engine(dir.path()).await;
    let message = quota_failure_message(Some("rate_limit"), Some(3_600_000));
    let first = engine
        .park_for_quota_reset(&message, "abort sentence")
        .await
        .expect("the first park fires");
    assert!(first.status_message.contains("Session parked until"));
    let before = engine
        .quota_park
        .lock()
        .expect("park state")
        .clone()
        .expect("parked");
    let second = engine
        .park_for_quota_reset(&message, "abort sentence")
        .await
        .expect("the already-parked arm surfaces");
    assert!(
        second
            .status_message
            .starts_with("Session is parked until ")
            && second
                .status_message
                .contains("this turn ended without a retry"),
        "the already-parked wording surfaces: {second:?}"
    );
    let after = engine
        .quota_park
        .lock()
        .expect("park state")
        .clone()
        .expect("still parked");
    assert_eq!(before.job_id, after.job_id, "the wake is not rescheduled");
    assert_eq!(after.park_count, 1, "the repeat consumes no park");
    let store = engine.config.cron_store.as_ref().expect("wiring").clone();
    assert_eq!(store.store.list().len(), 1, "no second wake job is created");
}

/// Non-quota failures never park; a quota failure without a reported reset keeps the abort.
#[tokio::test]
async fn non_quota_and_no_reset_failures_do_not_park() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = park_engine(dir.path()).await;
    let non_quota = quota_failure_message(Some("server_error"), Some(3_600_000));
    assert!(engine
        .park_for_quota_reset(&non_quota, "abort")
        .await
        .is_none());
    let no_reset = quota_failure_message(Some("rate_limit"), None);
    assert!(engine
        .park_for_quota_reset(&no_reset, "abort")
        .await
        .is_none());
    assert!(!engine.is_quota_parked());
}

#[tokio::test]
async fn spent_park_budget_clears_the_stale_park() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = park_engine(dir.path()).await;
    // Arm a wake, then age the park state past its wake time: the
    // budget check runs against a wake that already fired.
    let fired_job_id = engine
        .create_quota_resume_job(crate::util::now_ms() + 60_000)
        .await
        .expect("wake job");
    *engine.quota_park.lock().expect("park state") = Some(QuotaParkState {
        park_count: engine.park_policy().max_parks,
        resume_at_ms: crate::util::now_ms() - 60_000,
        job_id: Some(fired_job_id),
        wake_retries: 0,
    });
    let message = quota_failure_message(Some("rate_limit"), Some(3_600_000));
    assert!(
        engine
            .park_for_quota_reset(&message, "abort")
            .await
            .is_none(),
        "the spent budget declines the park"
    );
    assert!(
        !engine.is_quota_parked(),
        "the stale park clears when the episode ends"
    );
}

#[tokio::test]
async fn consumed_wake_without_reset_re_arms_bounded() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = park_engine(dir.path()).await;
    let fired_job_id = engine
        .create_quota_resume_job(crate::util::now_ms() + 60_000)
        .await
        .expect("wake job");
    *engine.quota_park.lock().expect("park state") = Some(QuotaParkState {
        park_count: 1,
        resume_at_ms: crate::util::now_ms() - 60_000,
        job_id: Some(fired_job_id),
        wake_retries: 0,
    });
    let no_reset = quota_failure_message(Some("rate_limit"), None);
    let outcome = engine
        .park_for_quota_reset(&no_reset, "abort")
        .await
        .expect("the re-arm fires");
    assert!(outcome
        .status_message
        .starts_with("Session is parked until "));
    let park = engine
        .quota_park
        .lock()
        .expect("park state")
        .clone()
        .expect("re-armed");
    assert_eq!(park.park_count, 1, "the re-arm consumes no park");
    assert_eq!(park.wake_retries, 1);
    let store = engine.config.cron_store.as_ref().expect("wiring").clone();
    assert_eq!(
        store.store.list().len(),
        2,
        "the re-arm creates a replacement wake"
    );
    // Spend the re-arm budget: the park drops and the give-up stands.
    *engine.quota_park.lock().expect("park state") = Some(QuotaParkState {
        park_count: park.park_count,
        resume_at_ms: crate::util::now_ms() - 60_000,
        job_id: park.job_id,
        wake_retries: QUOTA_WAKE_MAX_RETRIES,
    });
    assert!(
        engine
            .park_for_quota_reset(&no_reset, "abort")
            .await
            .is_none(),
        "the spent re-arm budget drops the park"
    );
    assert!(!engine.is_quota_parked());
}

/// The replacement teardown ends the retired session's park with it,
/// so an unparked replacement is never reported quota-parked.
#[tokio::test]
async fn retire_session_runtime_clears_the_live_park() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = park_engine(dir.path()).await;
    let message = quota_failure_message(Some("rate_limit"), Some(3_600_000));
    engine
        .park_for_quota_reset(&message, "abort")
        .await
        .expect("the park fires");
    assert!(engine.is_quota_parked());
    engine.retire_session_runtime().await.unwrap();
    assert!(
        !engine.is_quota_parked(),
        "the retired session's park ends with it"
    );
}

/// A parked session that completes a model call resumes; an early success
/// queues the marker.
#[tokio::test]
async fn early_resume_clears_the_park_and_cancels_the_wake() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = park_engine(dir.path()).await;
    let admissions = goal_admission_collector(&engine);
    let message = quota_failure_message(Some("rate_limit"), Some(3_600_000));
    engine
        .park_for_quota_reset(&message, "abort")
        .await
        .expect("the park fires");
    let job_id = engine
        .quota_park
        .lock()
        .expect("park state")
        .clone()
        .expect("parked")
        .job_id
        .expect("wake");
    let store = engine.config.cron_store.as_ref().expect("wiring").clone();
    assert_eq!(store.store.list().len(), 1);
    engine.resume_quota_park(false).await;
    assert!(!engine.is_quota_parked(), "the park clears on resume");
    let job = store
        .store
        .list()
        .into_iter()
        .find(|job| job.id == job_id)
        .expect("the wake job still exists");
    assert_eq!(
        job.status,
        pa_core::cron::JobStatus::Cancelled,
        "the pending wake cancels on resume"
    );
    let admissions = admissions.lock().unwrap();
    assert!(
        matches!(admissions.as_slice(), [crate::engine::GoalTurnEndWork::Continuation(item)]
        if item.request.message == pa_core::session_engine::provider_park::QUOTA_RESUME_MARKER_TEXT)
    );
}
