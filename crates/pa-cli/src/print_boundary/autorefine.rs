//! Historical pre-plugin refinement implementation; intentionally not linked by print_boundary.
//! Retained for migration reference. Harness policy now belongs to its optional Lua plugin.

//! The compact-trigger auto-refine machine: the review gates, the durable-row
//! surface, the requested-refinement streaming, and the disposal drain.

use super::{json, Model, PathBuf, SessionAgentMessage, SessionEngine, TurnBoundary};

/// Wall-clock milliseconds (the review-cooldown stamps).
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// Where the compact-trigger auto-refine surfaces: the serialized checkpoint runs
/// mid-run; the disposal drain runs after the print client tore its subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefineSurface {
    /// The serialized checkpoint at a turn boundary.
    Checkpoint,
    /// The session disposal drain (best-effort, silent).
    Dispose,
}

impl TurnBoundary {
    /// The settled non-error, non-aborted assistant turns appended since the last
    /// boundary call, added to the review prompt's trigger counter.
    pub(super) async fn count_settled_turns(&mut self, engine: &SessionEngine) {
        let Some(baseline) = self.entry_baseline else {
            return;
        };
        let entries = engine.session.entries().await;
        let settled = entries
            .iter()
            .skip(baseline)
            .filter(|entry| {
                matches!(
                    entry,
                    pa_types::session::FileEntry::Message {
                        message: SessionAgentMessage::Assistant(assistant),
                        ..
                    } if assistant.stop_reason != pa_types::ai::StopReason::Error
                        && assistant.stop_reason != pa_types::ai::StopReason::Aborted
                )
            })
            .count();
        self.assistant_turns_since_review += settled as u32;
        self.entry_baseline = Some(entries.len());
    }

    /// The disposal drain: a serialized compaction can finish without another model
    /// turn — drain its pending review here so disposal does not lose the
    /// trigger. The subscription is already torn down; only durable rows and
    /// harness state persist. Best-effort.
    pub(crate) async fn drain_compact_auto_refine_at_disposal(
        &mut self,
        engine: &SessionEngine,
        model: &Model,
        api_key: Option<String>,
        global_harness_dir: PathBuf,
    ) {
        let _ = self
            .consume_compact_auto_refine(
                engine,
                model,
                api_key,
                global_harness_dir,
                RefineSurface::Dispose,
            )
            .await;
    }

    /// The compact-trigger auto-refine consumption: gates first — the refine
    /// surface, the settings, the review cooldown — then the review, and only
    /// an approving review runs the refinement. The checkpoint preserves the
    /// trigger through the cooldown; disposal clears it; each attempt stamps
    /// the cooldown and resets the counter.
    pub(super) async fn consume_compact_auto_refine(
        &mut self,
        engine: &SessionEngine,
        model: &Model,
        api_key: Option<String>,
        global_harness_dir: PathBuf,
        surface: RefineSurface,
    ) -> Result<(), String> {
        if !self.compact_auto_refine_pending {
            return Ok(());
        }
        // Sessions without the refine surface drop the trigger
        // outright.
        if !engine.session.auto_refine_allowed() {
            self.compact_auto_refine_pending = false;
            return Ok(());
        }
        let gates = engine.session.auto_refine_gates();
        if !gates.enabled || !gates.compact {
            self.compact_auto_refine_pending = false;
            return Ok(());
        }
        let under_cooldown = self
            .last_auto_refine_review_at
            .is_some_and(|last| now_millis().saturating_sub(last) < gates.cooldown_ms);
        if under_cooldown && surface == RefineSurface::Checkpoint {
            // Preserve the compact trigger for a later boundary while
            // the cooldown is active.
            return Ok(());
        }
        self.compact_auto_refine_pending = false;
        if under_cooldown {
            // The disposal drain clears a cooled-down trigger without a
            // review.
            return Ok(());
        }
        let entries_before = engine.session.entries().await.len();
        let turns = self.assistant_turns_since_review;
        let outcome = engine
            .session
            // The headless print boundary never moves branches (single-branch for the
            // run), so the branch invalidation version stays at its initial 0.
            .auto_refine_after_compaction(model, api_key, global_harness_dir, turns, 0)
            .await;
        // Every review attempt stamps the cooldown and resets the turn
        // counter.
        self.last_auto_refine_review_at = Some(now_millis());
        self.assistant_turns_since_review = 0;
        // The reviewer declined: no refinement, nothing surfaces.
        let streamed = match outcome {
            Ok(None) => return Ok(()),
            Ok(Some(result)) => Ok(result),
            Err(error) => Err(error),
        };
        self.stream_refinement_outcome(
            engine,
            &streamed,
            entries_before,
            surface == RefineSurface::Checkpoint,
            "automatic",
        )
        .await;
        Ok(())
    }

    /// One refinement outcome's surface: the durable rows' message pairs plus
    /// `refine_complete` on success, `refine_failed` on failure; `emit` false keeps
    /// the stream quiet (the rows still persist).
    pub(super) async fn stream_refinement_outcome(
        &self,
        engine: &SessionEngine,
        outcome: &anyhow::Result<pa_core::refinement::RefinementResult>,
        entries_before: usize,
        emit: bool,
        kind: &str,
    ) {
        match outcome {
            Ok(result) => {
                if emit && self.json_mode {
                    // The refinement rows this run appended: the outcome row always, the
                    // model-facing notice when edits applied.
                    for row in Self::refinement_rows_since(engine, entries_before).await {
                        let value = crate::headless_autonomous::custom_row_wire_value(&row);
                        for event_type in ["message_start", "message_end"] {
                            (self.sink)(&json!({ "type": event_type, "message": value }));
                        }
                    }
                    self.emit_json(&json!({
                        "type": "refine_complete",
                        "result": serde_json::to_value(result)
                            .unwrap_or(serde_json::Value::Null),
                    }));
                }
            }
            Err(error) => {
                if emit {
                    if self.json_mode {
                        self.emit_json(&json!({
                            "type": "refine_failed",
                            "error": format!("{error}"),
                        }));
                    } else {
                        eprintln!("pa-cli: {kind} refinement failed: {error:#}");
                    }
                }
            }
        }
    }

    /// The durable refinement rows appended after an entry count: the outcome row,
    /// then the model-facing notice when edits applied, in that order.
    pub(crate) async fn refinement_rows_since(
        engine: &SessionEngine,
        entries_before: usize,
    ) -> Vec<pa_types::session::CustomMessage> {
        engine
            .session
            .entries()
            .await
            .into_iter()
            .skip(entries_before)
            .filter_map(|entry| match entry {
                pa_types::session::FileEntry::CustomMessage { payload, base }
                    if payload.custom_type
                        == pa_core::session_engine::refine::REFINEMENT_OUTCOME_CUSTOM_TYPE
                        || payload.custom_type
                            == pa_core::session_engine::refine::REFINEMENT_NOTICE_CUSTOM_TYPE =>
                {
                    Some(pa_types::session::CustomMessage {
                        custom_type: payload.custom_type.clone(),
                        content: payload.content.clone(),
                        display: payload.display,
                        details: payload.details.clone(),
                        timestamp: base
                            .timestamp
                            .as_deref()
                            .map(pa_core::session::timestamp_to_millis)
                            .unwrap_or_default(),
                        rest: payload.rest,
                    })
                }
                _ => None,
            })
            .collect()
    }
}

// Historical pre-plugin tests, retained with the retired implementation (not compiled).
async fn refine_rows(engine: &SessionEngine) -> Vec<pa_types::session::CustomMessage> {
    engine
        .session
        .entries()
        .await
        .into_iter()
        .filter_map(|entry| match entry {
            FileEntry::CustomMessage { payload, base } => {
                let row = pa_types::session::CustomMessage {
                    custom_type: payload.custom_type.clone(),
                    content: payload.content.clone(),
                    display: payload.display,
                    details: payload.details.clone(),
                    timestamp: base
                        .timestamp
                        .as_deref()
                        .map(pa_core::session::timestamp_to_millis)
                        .unwrap_or_default(),
                    rest: payload.rest,
                };
                (row.custom_type == "refinement_outcome" || row.custom_type == "refinement_notice")
                    .then_some(row)
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn requested_refinement_failure_emits_the_refine_failed_event() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    // The refiner consumes the next faux response; a non-JSON reply fails the
    // plan parse.
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({ "responses": [{"text": "seed reply"}, {"text": "not a plan"}] }),
        compactable_settings(),
        None,
    )
    .await;
    let global_dir = tempfile::TempDir::new().unwrap().keep();
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    engine
        .turn_boundary
        .schedule_refine(pa_core::session_engine::turn_boundary::PendingRefine {
            instructions: None,
            global: true,
        })
        .await;
    admit_with_harness_dir(
        &mut boundary,
        &engine,
        &model,
        "seed turn".to_string(),
        global_dir,
    )
    .await
    .unwrap();
    let events = events.lock().unwrap().clone();
    let failed = events
        .iter()
        .find(|event| event["type"] == "refine_failed")
        .expect("the refine_failed event");
    let error = failed["error"].as_str().unwrap_or_default();
    assert!(!error.is_empty(), "the failure message rides the event");
    assert!(
        events
            .iter()
            .all(|event| event["type"] != "compaction_start"),
        "no compaction ran"
    );
}

#[tokio::test]
async fn requested_refinement_streams_rows_and_refine_complete() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let plan = r#"{"summary":"note it","rationale":"repeated","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","id":"m1","title":"Tactic","content":"Use tactic A"}]}"#;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({ "responses": [{"text": "seed reply"}, {"text": plan}] }),
        compactable_settings(),
        None,
    )
    .await;
    let global_dir = tempfile::TempDir::new().unwrap().keep();
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    engine
        .turn_boundary
        .schedule_refine(pa_core::session_engine::turn_boundary::PendingRefine {
            instructions: None,
            global: true,
        })
        .await;
    admit_with_harness_dir(
        &mut boundary,
        &engine,
        &model,
        "seed turn".to_string(),
        global_dir,
    )
    .await
    .unwrap();
    let events = events.lock().unwrap().clone();
    let outcome_at = events
        .iter()
        .position(|event| {
            event["type"] == "message_start"
                && event["message"]["customType"] == "refinement_outcome"
        })
        .expect("the outcome row pair");
    let notice_at = events
        .iter()
        .position(|event| {
            event["type"] == "message_start"
                && event["message"]["customType"] == "refinement_notice"
        })
        .expect("the notice row pair");
    assert!(outcome_at < notice_at);
    assert_eq!(events[outcome_at]["message"]["display"], true);
    assert_eq!(events[notice_at]["message"]["display"], false);
    let complete_at = events
        .iter()
        .position(|event| event["type"] == "refine_complete")
        .expect("the refine_complete event");
    assert!(notice_at < complete_at);
    assert_eq!(events[complete_at]["result"]["summary"], "note it");
    assert_eq!(events[complete_at]["result"]["appliedEdits"][0]["id"], "m1");
    let rows = refine_rows(&engine).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].custom_type, "refinement_outcome");
    assert_eq!(rows[1].custom_type, "refinement_notice");
}

#[tokio::test]
async fn compact_trigger_auto_refine_streams_at_the_next_boundary() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let review = r#"{"shouldRefine": true, "rationale": "the crossing turn shows a reusable tactic", "instructions": "record the tactic"}"#;
    let plan = r#"{"summary":"note it","rationale":"repeated","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","id":"m1","title":"Tactic","content":"Use tactic A"}]}"#;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                {"text": "second reply"},
                {"text": "the summary"},
                {"text": "third reply"},
                {"text": review},
                {"text": plan},
            ]
        }),
        compactable_settings(),
        None,
    )
    .await;
    let global_dir = tempfile::TempDir::new().unwrap().keep();
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    admit_with_harness_dir(
        &mut boundary,
        &engine,
        &model,
        format!("seed turn {}", "x".repeat(48_000)),
        global_dir.clone(),
    )
    .await
    .unwrap();
    // A requested compaction: the trigger is scheduled with no further
    // turn to consume it here.
    engine.turn_boundary.schedule_compaction(None).await;
    admit_turn_with_scheduled_request(
        &mut boundary,
        &engine,
        &model,
        format!("second turn {}", "x".repeat(2_000)),
    )
    .await
    .unwrap();
    assert_eq!(compaction_count(&engine).await, 1, "the compaction ran");
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event["type"] != "refine_complete"),
        "no auto-refine before the next boundary"
    );
    admit_with_harness_dir(
        &mut boundary,
        &engine,
        &model,
        "third turn".to_string(),
        global_dir,
    )
    .await
    .unwrap();
    let events = events.lock().unwrap().clone();
    let outcome_at = events
        .iter()
        .position(|event| {
            event["type"] == "message_start"
                && event["message"]["customType"] == "refinement_outcome"
        })
        .expect("the outcome row pair");
    let notice_at = events
        .iter()
        .position(|event| {
            event["type"] == "message_start"
                && event["message"]["customType"] == "refinement_notice"
        })
        .expect("the notice row pair");
    let complete_at = events
        .iter()
        .position(|event| event["type"] == "refine_complete")
        .expect("the refine_complete event");
    assert!(outcome_at < notice_at && notice_at < complete_at);
    assert_eq!(events[complete_at]["result"]["summary"], "note it");
    assert_eq!(events[notice_at]["message"]["details"]["source"], "auto");
    let rows = refine_rows(&engine).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].custom_type, "refinement_outcome");
    assert_eq!(rows[1].custom_type, "refinement_notice");
}

#[tokio::test]
async fn overflow_retry_compact_trigger_streams_the_ts_surface() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let review = r#"{"shouldRefine": true, "rationale": "the overflow recovery is reusable"}"#;
    let plan = r#"{"summary":"note it","rationale":"repeated","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","id":"m1","title":"Tactic","content":"Use tactic A"}]}"#;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                overflow_error(0),
                {"text": "the summary"},
                {"text": "recovered reply"},
                {"text": review},
                {"text": plan},
            ]
        }),
        compactable_settings(),
        None,
    )
    .await;
    let global_dir = tempfile::TempDir::new().unwrap().keep();
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    admit_with_harness_dir(
        &mut boundary,
        &engine,
        &model,
        format!("seed turn {}", "x".repeat(48_000)),
        global_dir,
    )
    .await
    .unwrap();
    admit_with_harness_dir(
        &mut boundary,
        &engine,
        &model,
        format!("overflow probe {}", "x".repeat(48_000)),
        std::path::PathBuf::new(),
    )
    .await
    .unwrap();
    let events = events.lock().unwrap().clone();
    let end_at = events
        .iter()
        .position(|event| event["type"] == "compaction_end")
        .expect("the compaction_end event");
    assert_eq!(events[end_at]["willRetry"], true);
    let outcome_at = events
        .iter()
        .position(|event| {
            event["type"] == "message_start"
                && event["message"]["customType"] == "refinement_outcome"
        })
        .expect("the outcome row pair");
    let complete_at = events
        .iter()
        .position(|event| event["type"] == "refine_complete")
        .expect("the refine_complete event");
    assert!(end_at < outcome_at && outcome_at < complete_at);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "compaction_start")
            .count(),
        1,
        "exactly one compaction"
    );
    assert_eq!(refine_rows(&engine).await.len(), 2);
}

#[tokio::test]
async fn compact_trigger_drains_at_disposal_off_the_stream() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let review = r#"{"shouldRefine": true, "rationale": "the seed turn shows a reusable tactic"}"#;
    let plan = r#"{"summary":"note it","rationale":"repeated","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","id":"m1","title":"Tactic","content":"Use tactic A"}]}"#;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                {"text": "second reply"},
                {"text": "the summary"},
                {"text": review},
                {"text": plan},
            ]
        }),
        compactable_settings(),
        None,
    )
    .await;
    let global_dir = tempfile::TempDir::new().unwrap().keep();
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    admit_with_harness_dir(
        &mut boundary,
        &engine,
        &model,
        format!("seed turn {}", "x".repeat(48_000)),
        global_dir.clone(),
    )
    .await
    .unwrap();
    engine.turn_boundary.schedule_compaction(None).await;
    admit_turn_with_scheduled_request(
        &mut boundary,
        &engine,
        &model,
        format!("second turn {}", "x".repeat(2_000)),
    )
    .await
    .unwrap();
    assert_eq!(compaction_count(&engine).await, 1);
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event["type"] != "refine_complete"),
        "no auto-refine before disposal"
    );
    // The print runtime's disposal order: the subscription is gone, so
    // the drain runs the round silently.
    boundary
        .drain_compact_auto_refine_at_disposal(&engine, &model, None, global_dir)
        .await;
    let rows = refine_rows(&engine).await;
    assert_eq!(rows.len(), 2, "the durable rows persisted");
    assert_eq!(rows[0].custom_type, "refinement_outcome");
    assert_eq!(rows[1].custom_type, "refinement_notice");
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event["type"] != "refine_complete"),
        "the drain stays off the event stream"
    );
}

#[tokio::test]
async fn auto_refine_review_decline_surfaces_nothing() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let review = r#"{"shouldRefine": false, "rationale": "one-off tool output"}"#;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                {"text": "second reply"},
                {"text": "the summary"},
                {"text": "third reply"},
                {"text": review},
            ]
        }),
        compactable_settings(),
        None,
    )
    .await;
    let global_dir = tempfile::TempDir::new().unwrap().keep();
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    admit_with_harness_dir(
        &mut boundary,
        &engine,
        &model,
        format!("seed turn {}", "x".repeat(48_000)),
        global_dir.clone(),
    )
    .await
    .unwrap();
    engine.turn_boundary.schedule_compaction(None).await;
    admit_turn_with_scheduled_request(
        &mut boundary,
        &engine,
        &model,
        format!("second turn {}", "x".repeat(2_000)),
    )
    .await
    .unwrap();
    assert_eq!(compaction_count(&engine).await, 1, "the compaction ran");
    admit_with_harness_dir(
        &mut boundary,
        &engine,
        &model,
        "third turn".to_string(),
        global_dir,
    )
    .await
    .unwrap();
    assert!(refine_rows(&engine).await.is_empty(), "no refinement ran");
    let events = events.lock().unwrap().clone();
    assert!(
        events
            .iter()
            .all(|event| event["type"] != "refine_complete" && event["type"] != "refine_failed"),
        "the decline surfaces nothing"
    );
    // The unconsumed review response stays queued.
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "compaction_start")
            .count(),
        1
    );
}

#[tokio::test]
async fn auto_refine_disabled_settings_drop_the_trigger() {
    let _faux = FAUX_TEST_LOCK.lock().await;
    let (engine, _dir, model) = faux_engine_with_settings(
        json!({
            "responses": [
                {"text": "seed reply"},
                {"text": "second reply"},
                {"text": "the summary"},
                {"text": "third reply"},
            ]
        }),
        json!({
            "compaction": { "enabled": true, "reserveTokens": 1, "keepRecentTokens": 10 },
            "autoRefine": { "enabled": false }
        }),
        None,
    )
    .await;
    let global_dir = tempfile::TempDir::new().unwrap().keep();
    let (sink, events) = capture_sink();
    let mut boundary = TurnBoundary::with_sink(true, sink);
    admit_with_harness_dir(
        &mut boundary,
        &engine,
        &model,
        format!("seed turn {}", "x".repeat(48_000)),
        global_dir.clone(),
    )
    .await
    .unwrap();
    engine.turn_boundary.schedule_compaction(None).await;
    admit_turn_with_scheduled_request(
        &mut boundary,
        &engine,
        &model,
        format!("second turn {}", "x".repeat(2_000)),
    )
    .await
    .unwrap();
    admit_with_harness_dir(
        &mut boundary,
        &engine,
        &model,
        "third turn".to_string(),
        global_dir,
    )
    .await
    .unwrap();
    assert_eq!(compaction_count(&engine).await, 1, "the compaction ran");
    assert!(refine_rows(&engine).await.is_empty(), "no refinement ran");
    let events = events.lock().unwrap().clone();
    assert!(
        events
            .iter()
            .all(|event| event["type"] != "refine_complete" && event["type"] != "refine_failed"),
        "the disabled trigger surfaces nothing"
    );
}

// Historical canonical-digest e2e assertion, replaced by model-only plugin context tests.
/// The harness digest pair (TS commit-time injection): the first turn
/// streams the digest pair between `turn_start` and the user pair.
#[test]
fn print_mode_json_streams_the_harness_digest_pair() {
    let home = isolated_home();
    let script = serde_json::json!({ "responses": ["json answer"] });
    let (stdout, stderr, code) = run_in_home(home.path(), &["--mode", "json", "-p", "hi"], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    let events: Vec<serde_json::Value> = stdout
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .unwrap();
    let digest_at = events
        .iter()
        .position(|event| {
            event["type"] == "message_start"
                && event["message"]["role"] == "custom"
                && event["message"]["customType"] == "harness_digest"
        })
        .expect("the digest message_start event");
    let turn_start_at = events
        .iter()
        .position(|event| event["type"] == "turn_start")
        .expect("turn_start");
    let user_at = events
        .iter()
        .position(|event| event["type"] == "message_start" && event["message"]["role"] == "user")
        .expect("the user message pair");
    assert!(turn_start_at < digest_at && digest_at < user_at);
    let digest = &events[digest_at]["message"];
    assert!(digest["content"].is_string());
    assert!(digest["content"]
        .as_str()
        .unwrap()
        .starts_with("[harness-digest]"));
    assert!(digest["content"]
        .as_str()
        .unwrap()
        .ends_with("</harness_state>"));
    assert_eq!(digest["display"], false);
    assert!(digest["details"]["digest"].is_string());
    assert!(digest["details"]["digest"]
        .as_str()
        .unwrap()
        .contains("# Continual Harness State"));
    let digest_end_at = events
        .iter()
        .position(|event| {
            event["type"] == "message_end"
                && event["message"]["role"] == "custom"
                && event["message"]["customType"] == "harness_digest"
        })
        .expect("the digest message_end event");
    assert_eq!(
        events[digest_at]["message"],
        events[digest_end_at]["message"]
    );
}
