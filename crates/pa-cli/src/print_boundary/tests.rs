use super::*;
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_types::session::FileEntry;
use serde_json::json;

/// The faux model's per-request output budget: threshold fixtures
/// subtract it from the window alongside the headroom.
const FAUX_REQUEST_BUDGET: u64 = 16_384;

/// The faux provider registers process-globally; one test at a time
/// keeps the queued responses deterministic.
static FAUX_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The TS overflow error shape: an Anthropic token-overflow message. The
/// retry-turn `delayMs` puts its settled timestamp strictly after the
/// compaction entry's.
fn overflow_error(delay_ms: u64) -> Value {
    let mut entry = json!({
        "text": "",
        "stopReason": "error",
        "errorMessage": "prompt is too long: 213462 tokens > 200000 maximum",
    });
    if delay_ms > 0 {
        entry["delayMs"] = json!(delay_ms);
    }
    entry
}

const OVERFLOW_ERROR: &str = "prompt is too long: 213462 tokens > 200000 maximum";

/// The compactable compaction settings: the `keepRecentTokens` cut
/// keeps ~10 tokens, so a recovery with pre-cut history summarizes it.
fn compactable_settings() -> Value {
    json!({ "compaction": { "enabled": true, "reserveTokens": 1, "keepRecentTokens": 10 } })
}

/// One faux-driven engine over its own tempdir with explicit compaction
/// settings, optionally resuming a persisted session file.
async fn faux_engine_with_settings(
    script: Value,
    mut settings: Value,
    session_manager: Option<SessionManager>,
) -> (SessionEngine, tempfile::TempDir, Model) {
    settings["plugins"] = json!({"harness":{"enabled":false}});
    let dir = tempfile::TempDir::new().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
    let parsed = pa_ai::faux::script::parse_faux_script(&script).unwrap();
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![parsed.model]),
            ..Default::default()
        });
    registration.set_responses(parsed.responses);
    registration.set_repeat_last_response(parsed.repeat_last_response);
    let model = registration.get_model();
    let stream_fn = pa_core::session_engine::provider_adapter::real_stream_fn(None, model.clone());
    let agent_model: pa_agent::types::Model =
        json_round_trip(&model).expect("the faux model crosses the loop boundary");
    let session_manager = session_manager
        .or_else(|| {
            Some(SessionManager::persisted(
                dir.path(),
                &dir.path().join("sessions"),
            ))
        })
        .expect("a session manager");
    let engine = create_session(SessionEngineConfig {
        on_late_sent_agent_message: None,
        context_startup_boundary: pa_core::session_engine::ContextStartupBoundary::Current,
        context_startup_admitted_user_ids: Vec::new(),
        on_initial_context_selection: None,
        canonical_live_owner: None,
        ollama_cloud: None,
        semantic_edges: None,
        cron_store: None,
        steering_mode: None,
        follow_up_mode: None,
        image_model_router: None,
        cwd: dir.path().to_path_buf(),
        agent_dir,
        mcp_manager: None,
        model: Some(agent_model),
        thinking_level: None,
        stream_fn: Some(stream_fn),
        tools: Vec::new(),
        custom_system_prompt: None,
        prompt_guidelines: Vec::new(),
        generic_mcp_servers: Vec::new(),
        allow_recursion: None,
        session_manager: Some(session_manager),
        extra_host_handlers: None,
        conversation_log_path: None,
        additional_skill_paths: Vec::new(),
        additional_prompt_paths: Vec::new(),
        extra_builtin_skill_overrides: Vec::new(),
        rlm_subagent_host: None,
        rlm_usage_store: None,
        progress_note_emit: None,
        rlm_depth: None,
        daemonless: true,
        model_info: Some(model.clone()),
        prewarm_ipython_kernel: None,
        on_background_work_settled: None,
        queued_goal_context_purge: None,
        queued_steering_probe: None,
        should_stop_after_turn: None,
    })
    .await
    .expect("the faux session assembles");
    (engine, dir, model)
}

/// Admit one prompt through the production flow: the pre-turn check,
/// the prompt, and the settled-turn boundary.
async fn admit(
    boundary: &mut TurnBoundary,
    engine: &SessionEngine,
    model: &Model,
    prompt: String,
) -> Result<(), String> {
    boundary.run_pre_turn(engine, model, None).await?;
    engine
        .session
        .prompt(&prompt, pa_core::session_engine::PromptOptions::default())
        .await
        .expect("the prompt admits");
    engine.session.agent().wait_for_idle().await;
    boundary.run_at_settled_turn(engine, model, None).await
}

/// The mid-turn-request shape: a `compact.run` scheduled DURING a turn
/// is consumed at that same turn's settled boundary (a pending request
/// never survives to a pre-turn check in the product flow).
async fn admit_turn_with_scheduled_request(
    boundary: &mut TurnBoundary,
    engine: &SessionEngine,
    model: &Model,
    prompt: String,
) -> Result<(), String> {
    engine
        .session
        .prompt(&prompt, pa_core::session_engine::PromptOptions::default())
        .await
        .expect("the prompt admits");
    engine.session.agent().wait_for_idle().await;
    boundary.run_at_settled_turn(engine, model, None).await
}

async fn outcome_rows(engine: &SessionEngine) -> Vec<pa_types::session::CustomMessageEntry> {
    engine
        .session
        .entries()
        .await
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::CustomMessage { payload, .. }
                if payload.custom_type == "compaction_outcome" =>
            {
                Some(payload.clone())
            }
            _ => None,
        })
        .collect()
}

async fn compaction_count(engine: &SessionEngine) -> usize {
    engine
        .session
        .entries()
        .await
        .iter()
        .filter(|entry| matches!(entry, FileEntry::Compaction { .. }))
        .count()
}

/// The persisted user messages (the retry must not re-add one).
async fn user_texts(engine: &SessionEngine) -> Vec<String> {
    engine
        .session
        .entries()
        .await
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Message {
                message: SessionAgentMessage::User(user),
                ..
            } => Some(user.content.text()),
            _ => None,
        })
        .collect()
}

async fn last_assistant(engine: &SessionEngine) -> Option<pa_types::ai::AssistantMessage> {
    if let SessionAgentMessage::Assistant(assistant) =
        engine.session.last_assistant_message().await?
    {
        return Some(assistant);
    }
    None
}

#[tokio::test]
async fn overflow_compacts_retries_once_then_reports_the_failure() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                overflow_error(0),
                {"text": "the summary"},
                overflow_error(25),
            ]
        }),
        compactable_settings(),
        None,
    )
    .await;
    let mut boundary = TurnBoundary::new(false);
    // A large seed turn, so the recovery has pre-cut history to
    // summarize.
    admit(
        &mut boundary,
        &engine,
        &model,
        format!("seed turn {}", "x".repeat(48_000)),
    )
    .await
    .unwrap();
    admit(
        &mut boundary,
        &engine,
        &model,
        format!("overflow probe {}", "x".repeat(48_000)),
    )
    .await
    .unwrap();

    assert_eq!(compaction_count(&engine).await, 1);
    let rows = outcome_rows(&engine).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content.text(), OVERFLOW_RECOVERY_FAILED_MESSAGE);
    assert_eq!(
        rows[0].details,
        Some(json!({ "reason": "overflow", "outcome": "failed" }))
    );
    // The retried turn's settled timestamp lands past the compaction
    // boundary (the serve-time pacing).
    let last = last_assistant(&engine).await.expect("a settled error turn");
    assert_eq!(last.stop_reason, pa_types::ai::StopReason::Error);
    assert_eq!(last.error_message.as_deref(), Some(OVERFLOW_ERROR));
    assert_eq!(user_texts(&engine).await.len(), 2);
}

#[tokio::test]
async fn overflow_retry_succeeds_on_the_compacted_context() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                overflow_error(0),
                {"text": "the summary"},
                {"text": "recovered reply"},
            ]
        }),
        compactable_settings(),
        None,
    )
    .await;
    let mut boundary = TurnBoundary::new(false);
    admit(
        &mut boundary,
        &engine,
        &model,
        format!("seed turn {}", "x".repeat(48_000)),
    )
    .await
    .unwrap();
    admit(
        &mut boundary,
        &engine,
        &model,
        format!("overflow probe {}", "x".repeat(48_000)),
    )
    .await
    .unwrap();

    assert_eq!(compaction_count(&engine).await, 1);
    assert!(outcome_rows(&engine).await.is_empty());
    let last = last_assistant(&engine).await.expect("a settled turn");
    let text = last
        .content
        .iter()
        .filter_map(|block| match block {
            pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "recovered reply");
    assert_eq!(last.stop_reason, pa_types::ai::StopReason::Stop);
    assert_eq!(user_texts(&engine).await.len(), 2);
}

#[tokio::test]
async fn overflow_recovery_skip_surfaces_the_warning_row() {
    let _faux = FAUX_TEST_LOCK.lock().await;

    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                overflow_error(0),
                {"text": "next reply"},
            ]
        }),
        json!({
            "compaction": {
                "enabled": true, "reserveTokens": 1, "keepRecentTokens": 100_000
            }
        }),
        None,
    )
    .await;
    let mut boundary = TurnBoundary::new(false);
    admit(&mut boundary, &engine, &model, "seed turn".to_string())
        .await
        .unwrap();
    admit(&mut boundary, &engine, &model, "overflow probe".to_string())
        .await
        .unwrap();

    let rows = outcome_rows(&engine).await;
    assert_eq!(rows.len(), 1);
    let skipped =
        "Auto-compaction skipped: Session is too short to compact — try again once it grows";
    assert_eq!(rows[0].content.text(), skipped);
    assert_eq!(
        rows[0].details,
        Some(json!({ "reason": "overflow", "outcome": "skipped" }))
    );
    assert_eq!(compaction_count(&engine).await, 0, "nothing committed");
    admit(&mut boundary, &engine, &model, "next prompt".to_string())
        .await
        .unwrap();
    assert_eq!(compaction_count(&engine).await, 0);
    assert_eq!(outcome_rows(&engine).await.len(), 1);
    let last = last_assistant(&engine).await.expect("a settled turn");
    assert_eq!(last.stop_reason, pa_types::ai::StopReason::Stop);
    assert_eq!(
        user_texts(&engine).await,
        ["seed turn", "overflow probe", "next prompt"]
            .map(str::to_string)
            .to_vec()
    );
}

#[tokio::test]
async fn stale_overflow_error_recovers_before_the_next_prompt_after_a_resume() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let (engine_a, dir_a, model_a) = faux_engine_with_settings(
        json!({ "responses": [{"text": "seed reply"}, overflow_error(0)] }),
        json!({
            "compaction": { "enabled": false, "reserveTokens": 1, "keepRecentTokens": 10 }
        }),
        None,
    )
    .await;
    let mut boundary = TurnBoundary::new(false);
    admit(
        &mut boundary,
        &engine_a,
        &model_a,
        format!("seed turn {}", "x".repeat(48_000)),
    )
    .await
    .unwrap();
    admit(
        &mut boundary,
        &engine_a,
        &model_a,
        format!("overflow probe {}", "x".repeat(48_000)),
    )
    .await
    .unwrap();
    assert_eq!(compaction_count(&engine_a).await, 0);
    assert!(outcome_rows(&engine_a).await.is_empty());

    // Run two: a fresh boundary over the persisted session, compaction enabled;
    // the faux queue carries the summarizer then the recovered turn.
    let session_file = dir_a
        .path()
        .join("sessions")
        .read_dir()
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|extension| extension.to_str()) == Some("jsonl"))
        .expect("the run-one session file");
    let resumed = SessionManager::open(dir_a.path(), &dir_a.path().join("sessions"), &session_file);
    let (engine_b, _dir_b, model_b) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "the stale recovery summary"},
                {"text": "recovered after the resume"},
            ]
        }),
        compactable_settings(),
        Some(resumed),
    )
    .await;
    let mut boundary = TurnBoundary::new(false);
    boundary
        .run_pre_turn(&engine_b, &model_b, None)
        .await
        .unwrap();
    assert_eq!(
        compaction_count(&engine_b).await,
        1,
        "the pre-turn arm compacted the stale overflow"
    );
    assert!(outcome_rows(&engine_b).await.is_empty());
    let stale = last_assistant(&engine_b).await;
    assert!(
        !matches!(
            &stale,
            Some(message) if message.stop_reason == pa_types::ai::StopReason::Error
        ),
        "the stale overflow error is gone from the context"
    );
    admit(
        &mut boundary,
        &engine_b,
        &model_b,
        "next prompt".to_string(),
    )
    .await
    .unwrap();
    let last = last_assistant(&engine_b).await.expect("a settled turn");
    assert_eq!(last.stop_reason, pa_types::ai::StopReason::Stop);
    assert_eq!(user_texts(&engine_b).await.len(), 3);
}

#[tokio::test]
async fn autonomous_continuation_turns_cross_the_boundary_arms() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let (built, dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                overflow_error(0),
                {"text": "the summary"},
                {"text": "recovered reply"},
            ]
        }),
        compactable_settings(),
        None,
    )
    .await;
    let engine = std::sync::Arc::new(built);
    let mut boundary = TurnBoundary::new(false);
    // The autonomous run with the in-run hook wired: the seed turn mints the
    // continuation inside the same agent run.
    let run = std::sync::Arc::new(crate::headless_autonomous::HeadlessAutonomous::from_cli(
        &crate::args::AutonomousConfig {
            max_continuations: Some(1),
            ..Default::default()
        },
        dir.path(),
    ));
    let _accounting = run.wire_accounting(engine.session.agent()).await;
    let goal = std::sync::Arc::new(crate::print_goal::PrintGoalSurface::new(false));
    crate::print_autonomous::wire_continuation_hook(
        &engine,
        engine.session.agent(),
        &model,
        &goal,
        &run,
        /*root_node*/ None,
    );
    admit(
        &mut boundary,
        &engine,
        &model,
        format!("seed turn {}", "x".repeat(48_000)),
    )
    .await
    .unwrap();
    assert_eq!(compaction_count(&engine).await, 1);
    assert!(outcome_rows(&engine).await.is_empty());
    let last = last_assistant(&engine).await.expect("a settled turn");
    assert_eq!(last.stop_reason, pa_types::ai::StopReason::Stop);
    let text = last
        .content
        .iter()
        .filter_map(|block| match block {
            pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "recovered reply");
    // The CLI prompt plus the injected continuation (the retry re-issued
    // without re-adding a user message).
    assert_eq!(user_texts(&engine).await.len(), 2);
    assert!(user_texts(&engine).await[1].starts_with("[autonomous-continuation]"));
    // The limit stop surfaces only through the headless exit contract:
    // no durable `autonomous_status` row.
    assert!(engine
        .session
        .entries()
        .await
        .into_iter()
        .all(|entry| !matches!(
            &entry,
            pa_types::session::FileEntry::CustomMessage { payload, .. }
                if payload.custom_type == "autonomous_status"
        )));
    let stderr = run
        .exit_stderr()
        .await
        .expect("the limit stop exits non-zero");
    assert!(stderr.starts_with("Autonomous run stopped before terminal evidence;"));
}

fn capture_sink() -> (EventSink, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink: EventSink = {
        let events = std::sync::Arc::clone(&events);
        std::sync::Arc::new(move |event: &Value| events.lock().unwrap().push(event.clone()))
    };
    (sink, events)
}

#[tokio::test]
async fn requested_compaction_streams_the_ts_event_pair() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                {"text": "second reply"},
                {"text": "the summary"},
            ]
        }),
        compactable_settings(),
        None,
    )
    .await;
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    admit(
        &mut boundary,
        &engine,
        &model,
        format!("seed turn {}", "x".repeat(48_000)),
    )
    .await
    .unwrap();
    events.lock().unwrap().clear();
    // A mid-turn request consumed at that turn's settled boundary (the
    // threshold never re-evaluates).
    engine
        .turn_boundary
        .schedule_compaction(Some("focus on the goal".to_string()))
        .await;
    admit_turn_with_scheduled_request(
        &mut boundary,
        &engine,
        &model,
        format!("second turn {}", "x".repeat(2_000)),
    )
    .await
    .unwrap();
    let events = events.lock().unwrap().clone();
    let start_at = events
        .iter()
        .position(|event| event["type"] == "compaction_start")
        .expect("the compaction_start event");
    assert_eq!(
        events[start_at],
        json!({
            "type": "compaction_start",
            "reason": "requested",
            "customInstructions": "focus on the goal",
        })
    );
    let end_at = events
        .iter()
        .position(|event| event["type"] == "compaction_end")
        .expect("the compaction_end event");
    assert!(start_at < end_at);
    assert_eq!(events[end_at]["reason"], "requested");
    assert_eq!(events[end_at]["result"]["summary"], "the summary");
    assert_eq!(events[end_at]["aborted"], false);
    assert_eq!(events[end_at]["willRetry"], false);
    assert_eq!(events[end_at]["customInstructions"], "focus on the goal");
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "compaction_start")
            .count(),
        1,
        "exactly one compaction pair"
    );
    assert_eq!(compaction_count(&engine).await, 1, "the compaction ran");
}

#[tokio::test]
async fn requested_compaction_skip_streams_the_warning_row_and_end_event() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    // `keepRecentTokens` beyond the whole session: nothing to
    // summarize.
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({ "responses": [{"text": "seed reply"}] }),
        json!({
            "compaction": {
                "enabled": true, "reserveTokens": 1, "keepRecentTokens": 100_000
            }
        }),
        None,
    )
    .await;
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    engine.turn_boundary.schedule_compaction(None).await;
    admit_turn_with_scheduled_request(&mut boundary, &engine, &model, "seed turn".to_string())
        .await
        .unwrap();
    let events = events.lock().unwrap().clone();
    let row_at = events
        .iter()
        .position(|event| {
            event["type"] == "message_start"
                && event["message"]["customType"] == "compaction_outcome"
        })
        .expect("the outcome row's message pair");
    assert_eq!(
        events[row_at]["message"]["details"],
        json!({ "reason": "requested", "outcome": "skipped" })
    );
    let end_at = events
        .iter()
        .position(|event| event["type"] == "compaction_end")
        .expect("the compaction_end event");
    assert!(row_at < end_at, "the row pair precedes the end event");
    assert_eq!(events[end_at]["reason"], "requested");
    assert_eq!(events[end_at]["willRetry"], false);
    assert_eq!(
        events[end_at]["errorMessage"],
        "Requested compaction skipped: Session is too short to compact — try again once it grows"
    );
    assert_eq!(events[end_at]["errorSeverity"], "warning");
}

#[tokio::test]
async fn threshold_compaction_streams_the_ts_event_pair() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let (probe, _probe_dir, probe_model) = faux_engine_with_settings(
        json!({ "responses": [{"text": "seed reply"}] }),
        compactable_settings(),
        None,
    )
    .await;
    let mut probe_boundary = TurnBoundary::new(false);
    admit(
        &mut probe_boundary,
        &probe,
        &probe_model,
        "seed turn".to_string(),
    )
    .await
    .unwrap();
    let baseline = last_assistant(&probe)
        .await
        .map(|message| message.usage.total_tokens)
        .expect("probe turn produced usage");
    assert!(baseline < 100_000, "implausible baseline: {baseline}");

    // The crossing prompt adds ~12k tokens; the headroom sits halfway.
    let big_prompt = format!("seed turn {} crossing", "x".repeat(48_000));
    let big_tokens = (48_000 + "seed turn  crossing".len() as u64).div_ceil(4);
    let headroom = baseline + big_tokens / 2;
    let settings = json!({
        "compaction": {
            "enabled": true,
            "reserveTokens": 128_000u64.saturating_sub(FAUX_REQUEST_BUDGET + headroom).max(1),
            "keepRecentTokens": 10,
        }
    });
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                {"text": "crossing reply"},
                {"text": "the summary"},
            ]
        }),
        settings,
        None,
    )
    .await;
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    admit(&mut boundary, &engine, &model, "seed turn".to_string())
        .await
        .unwrap();
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event["type"] != "compaction_start"),
        "no compaction below the headroom"
    );
    admit(&mut boundary, &engine, &model, big_prompt)
        .await
        .unwrap();
    let events = events.lock().unwrap().clone();
    let start_at = events
        .iter()
        .position(|event| event["type"] == "compaction_start")
        .expect("the compaction_start event");
    assert_eq!(
        events[start_at],
        json!({"type": "compaction_start", "reason": "threshold"})
    );
    let end_at = events
        .iter()
        .position(|event| event["type"] == "compaction_end")
        .expect("the compaction_end event");
    assert!(start_at < end_at);
    assert_eq!(events[end_at]["reason"], "threshold");
    assert_eq!(events[end_at]["result"]["summary"], "the summary");
    assert_eq!(events[end_at]["aborted"], false);
    assert_eq!(events[end_at]["willRetry"], false);
    assert!(
        events[end_at].get("customInstructions").is_none(),
        "the threshold arm carries no request instructions"
    );
    assert_eq!(compaction_count(&engine).await, 1, "the compaction ran");
}

#[tokio::test]
async fn pre_turn_check_consumes_a_pending_request_before_the_prompt() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                {"text": "second reply"},
                {"text": "the summary"},
                {"text": "next reply"},
            ]
        }),
        compactable_settings(),
        None,
    )
    .await;
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    // A large seed turn, so the pre-turn compaction has pre-cut
    // history to summarize.
    admit(
        &mut boundary,
        &engine,
        &model,
        format!("seed turn {}", "x".repeat(48_000)),
    )
    .await
    .unwrap();
    // The second turn absorbs the keep-recent budget on its own, so
    // the cut leaves the seed turn as summarizable history.
    admit(
        &mut boundary,
        &engine,
        &model,
        format!("second turn {}", "x".repeat(2_000)),
    )
    .await
    .unwrap();
    // A pending request consumes at the next prompt's pre-turn check.
    engine
        .turn_boundary
        .schedule_compaction(Some("focus on the goal".to_string()))
        .await;
    events.lock().unwrap().clear();
    boundary.run_pre_turn(&engine, &model, None).await.unwrap();
    let captured = events.lock().unwrap().clone();
    assert_eq!(
        captured[0],
        json!({
            "type": "compaction_start",
            "reason": "requested",
            "customInstructions": "focus on the goal",
        })
    );
    assert_eq!(captured[1]["type"], "compaction_end");
    assert_eq!(captured[1]["reason"], "requested");
    assert_eq!(captured[1]["result"]["summary"], "the summary");
    assert_eq!(captured[1]["willRetry"], false);
    assert_eq!(
        captured
            .iter()
            .filter(|event| event["type"] == "compaction_start")
            .count(),
        1,
        "exactly one compaction pair"
    );
    assert_eq!(compaction_count(&engine).await, 1);
    admit(&mut boundary, &engine, &model, "next prompt".to_string())
        .await
        .unwrap();
    let events = events.lock().unwrap().clone();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "compaction_start")
            .count(),
        1,
        "no second compaction"
    );
    assert_eq!(compaction_count(&engine).await, 1);
    assert_eq!(user_texts(&engine).await.len(), 3);
    let last = last_assistant(&engine).await.expect("a settled turn");
    let text = last
        .content
        .iter()
        .filter_map(|block| match block {
            pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "next reply");
}

#[tokio::test]
async fn pre_turn_threshold_arm_compacts_a_resumed_session_before_the_prompt() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    // Run one: compaction disabled, the crossing turn settles above
    // the headroom.
    let (engine_a, dir_a, model_a) = faux_engine_with_settings(
        json!({
            "contextWindow": 20000,
            // A small output budget keeps the combined input+output
            // ceiling satisfiable on the 20k window.
            "maxTokens": 2000,
            "responses": [{"text": "seed reply"}, {"text": "crossing reply"}],
        }),
        json!({
            "compaction": { "enabled": false, "reserveTokens": 1, "keepRecentTokens": 10 }
        }),
        None,
    )
    .await;
    let mut boundary = TurnBoundary::new(false);
    admit(&mut boundary, &engine_a, &model_a, "seed turn".to_string())
        .await
        .unwrap();
    admit(
        &mut boundary,
        &engine_a,
        &model_a,
        format!("crossing turn {}", "x".repeat(100_000)),
    )
    .await
    .unwrap();
    assert_eq!(compaction_count(&engine_a).await, 0);
    assert!(outcome_rows(&engine_a).await.is_empty());

    // Run two: a fresh boundary over the persisted session — the pre-turn arm
    // compacts first.
    let session_file = dir_a
        .path()
        .join("sessions")
        .read_dir()
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|extension| extension.to_str()) == Some("jsonl"))
        .expect("the run-one session file");
    let resumed = SessionManager::open(dir_a.path(), &dir_a.path().join("sessions"), &session_file);
    let (engine_b, _dir_b, model_b) = faux_engine_with_settings(
        json!({
            "contextWindow": 20000,
            "maxTokens": 2000,
            "responses": [{"text": "the resumed summary"}, {"text": "recovered after the resume"}],
        }),
        json!({
            "compaction": { "enabled": true, "reserveTokens": 1, "keepRecentTokens": 10 }
        }),
        Some(resumed),
    )
    .await;
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    boundary
        .run_pre_turn(&engine_b, &model_b, None)
        .await
        .unwrap();
    let events = events.lock().unwrap().clone();
    assert_eq!(
        events[0],
        json!({"type": "compaction_start", "reason": "threshold"})
    );
    assert_eq!(events[1]["type"], "compaction_end");
    assert_eq!(events[1]["reason"], "threshold");
    assert_eq!(events[1]["result"]["summary"], "the resumed summary");
    assert_eq!(events[1]["willRetry"], false);
    assert_eq!(compaction_count(&engine_b).await, 1);
    assert!(outcome_rows(&engine_b).await.is_empty());
    // The settled boundary stays quiet (the stale-usage guard holds).
    admit(
        &mut boundary,
        &engine_b,
        &model_b,
        "next prompt".to_string(),
    )
    .await
    .unwrap();
    assert_eq!(compaction_count(&engine_b).await, 1);
    let last = last_assistant(&engine_b).await.expect("a settled turn");
    assert_eq!(last.stop_reason, pa_types::ai::StopReason::Stop);
    assert_eq!(user_texts(&engine_b).await.len(), 3);
}

#[tokio::test]
async fn pre_turn_abort_arm_drops_pending_requests_and_continues() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                {"text": "", "stopReason": "aborted"},
                {"text": "next reply"},
            ]
        }),
        compactable_settings(),
        None,
    )
    .await;
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    admit(&mut boundary, &engine, &model, "seed turn".to_string())
        .await
        .unwrap();
    admit(&mut boundary, &engine, &model, "abort me".to_string())
        .await
        .unwrap();
    let last = last_assistant(&engine).await.expect("a settled turn");
    assert_eq!(last.stop_reason, pa_types::ai::StopReason::Aborted);
    engine
        .turn_boundary
        .schedule_compaction(Some("stale request".to_string()))
        .await;
    events.lock().unwrap().clear();
    boundary.run_pre_turn(&engine, &model, None).await.unwrap();
    assert!(!engine.turn_boundary.compaction_scheduled().await);
    assert!(events.lock().unwrap().is_empty(), "no compaction events");
    assert_eq!(compaction_count(&engine).await, 0);
    assert!(outcome_rows(&engine).await.is_empty());
    admit(&mut boundary, &engine, &model, "next prompt".to_string())
        .await
        .unwrap();
    assert_eq!(compaction_count(&engine).await, 0);
    let last = last_assistant(&engine).await.expect("a settled turn");
    assert_eq!(last.stop_reason, pa_types::ai::StopReason::Stop);
}

#[tokio::test]
async fn non_overflow_error_never_triggers_the_arm() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                {"text": "", "stopReason": "error", "errorMessage": "529 overloaded"},
            ]
        }),
        compactable_settings(),
        None,
    )
    .await;
    let mut boundary = TurnBoundary::new(false);
    admit(&mut boundary, &engine, &model, "seed turn".to_string())
        .await
        .unwrap();
    admit(&mut boundary, &engine, &model, "flaky turn".to_string())
        .await
        .unwrap();
    assert_eq!(compaction_count(&engine).await, 0);
    assert!(outcome_rows(&engine).await.is_empty());
    let last = last_assistant(&engine)
        .await
        .expect("the settled error turn");
    assert_eq!(last.error_message.as_deref(), Some("529 overloaded"));
}

#[tokio::test]
async fn overflow_error_with_compaction_disabled_ends_without_recovery() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                overflow_error(0),
                {"text": "the summary"},
            ]
        }),
        json!({
            "compaction": { "enabled": false, "reserveTokens": 1, "keepRecentTokens": 10 }
        }),
        None,
    )
    .await;
    let mut boundary = TurnBoundary::new(false);
    admit(&mut boundary, &engine, &model, "seed turn".to_string())
        .await
        .unwrap();
    admit(&mut boundary, &engine, &model, "overflow probe".to_string())
        .await
        .unwrap();
    assert_eq!(compaction_count(&engine).await, 0);
    assert!(outcome_rows(&engine).await.is_empty());
    let last = last_assistant(&engine)
        .await
        .expect("the settled error turn");
    assert_eq!(last.error_message.as_deref(), Some(OVERFLOW_ERROR));
}
