//! Compact-session tests, the execution family: the persist + rebuild
//! run, the auto-compaction gate, the summary-delta sinks, the
//! harness-digest snapshot, the error response, and the durable row's
//! record.
use super::*;
use serde_json::Map;

#[tokio::test]
async fn execute_compaction_persists_and_rebuilds() {
    let registration = faux_registration();
    let model = registration.get_model();
    let tmp = tempfile::tempdir().unwrap();
    let mut session = session_with_turns(tmp.path(), 3);
    let result = execute_compaction(
        &mut session,
        CompactOptions {
            inference: None,
            model,
            api_key: None,
            custom_instructions: Some("focus on the goal"),
            settings: super::super::compaction::CompactionSettings {
                keep_recent_tokens: 20,
                ..Default::default()
            },
            abort: None,
            auxiliary: None,
            semantic_edges: None,
            summary_delta: None,
        },
    )
    .await
    .unwrap();
    let CompactOutcome::Ran(run) = result else {
        panic!("expected the compaction to run");
    };
    assert!(run.result.summary.contains("summarized goal"));
    assert!(run.result.usage.is_some());
    assert_eq!(run.entry.summary, run.result.summary);
    // A user-message cut is not a split turn: one summarizer wire
    // call, no split marker.
    assert_eq!(registration.call_count(), 1);
    assert!(!run.result.summary.contains("Turn Context (split turn)"));
    assert!(session
        .get_entries()
        .iter()
        .any(|entry| matches!(entry, FileEntry::Compaction { .. })));
    // The live context keeps the summary role; provider conversion
    // still formats it as a user turn.
    let rebuilt = rebuilt_context_after_compaction(&session);
    assert!(!rebuilt.is_empty());
    assert!(matches!(&rebuilt[0], AgentMessage::CompactionSummary(_)));
    let provider_messages = super::super::messages::convert_to_llm(&rebuilt);
    match &provider_messages[0] {
        AgentMessage::User(user) => {
            assert!(user.content.text().contains("[compaction-summary]"));
        }
        other => panic!("expected summary user message, got {other:?}"),
    }
    registration.unregister();
}

#[tokio::test]
async fn rebuilt_live_context_prevents_repeat_auto_compaction_until_new_usage() {
    let registration = faux_registration();
    registration.set_responses(vec![
        pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
            "## Goal\nsummarized goal",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        )),
        pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
            "## Turn Context\nsummarized prefix",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        )),
    ]);
    let model = registration.get_model();
    let tmp = tempfile::tempdir().unwrap();
    let mut session = session_with_turns(tmp.path(), 3);
    let mut assistant = match session.active_context().messages.last().unwrap() {
        AgentMessage::Assistant(assistant) => assistant.clone(),
        other => panic!("expected last assistant, got {other:?}"),
    };
    assistant.usage.input = 126_010;
    assistant.usage.total_tokens = 126_010;
    assistant.timestamp = 1;
    session
        .append_message(AgentMessage::User(pa_types::ai::UserMessage {
            content: UserContent::Text("threshold crossing turn".to_string()),
            timestamp: 0,
            rest: Map::default(),
        }))
        .unwrap();
    session
        .append_message(AgentMessage::Assistant(assistant.clone()))
        .unwrap();
    let settings = super::super::compaction::CompactionSettings {
        reserve_tokens: 127_500,
        keep_recent_tokens: 20,
        ..Default::default()
    };
    assert!(super::super::compaction::threshold_compaction_due(
        &session.active_context().messages,
        128_000,
        0,
        &settings
    ));
    let outcome = execute_compaction(
        &mut session,
        CompactOptions {
            inference: None,
            model,
            api_key: None,
            custom_instructions: None,
            settings,
            abort: None,
            auxiliary: None,
            semantic_edges: None,
            summary_delta: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(outcome, CompactOutcome::Ran(_)));
    let mut live = rebuilt_context_after_compaction(&session);
    let summary_timestamp = match &live[0] {
        AgentMessage::CompactionSummary(summary) => summary.timestamp,
        other => panic!("expected live compaction boundary, got {other:?}"),
    };
    assert!(live.iter().any(|message| matches!(
        message,
        AgentMessage::Assistant(assistant) if assistant.usage.total_tokens == 126_010
    )));
    // Cross the same session/agent wire boundary as `set_messages` and
    // `auto_compaction_due`: the live role must survive both round trips.
    let loop_messages: Vec<pa_agent::types::AgentMessage> = live
        .iter()
        .map(|message| {
            super::super::session_message_to_loop(message)
                .expect("session message must convert to loop message")
        })
        .collect();
    live = loop_messages
        .iter()
        .map(|message| serde_json::to_value(message).expect("loop message must serialize"))
        .map(|value| serde_json::from_value(value).expect("loop message must deserialize"))
        .collect();
    assert!(matches!(&live[0], AgentMessage::CompactionSummary(_)));
    assert!(live.iter().any(|message| matches!(
        message,
        AgentMessage::Assistant(assistant) if assistant.usage.total_tokens == 126_010
    )));
    for (custom_type, text) in [
        ("agent_message", "message from agent"),
        ("ipython_state", "kernel survived compaction"),
    ] {
        live.push(AgentMessage::Custom(pa_types::session::CustomMessage {
            custom_type: custom_type.to_string(),
            content: UserContent::Text(text.to_string()),
            display: true,
            details: None,
            timestamp: summary_timestamp + 1,
            rest: Map::default(),
        }));
    }
    assert!(!super::super::compaction::threshold_compaction_due(
        &live, 128_000, 0, &settings
    ));
    assistant.timestamp = summary_timestamp + 2;
    live.push(AgentMessage::Assistant(assistant));
    assert!(super::super::compaction::threshold_compaction_due(
        &live, 128_000, 0, &settings
    ));
    registration.unregister();
}

/// The sink receives every summarizer text delta in order; the final result
/// still comes from the terminal assistant message.
#[tokio::test]
async fn execute_compaction_streams_summary_deltas_to_the_sink() {
    let registration = faux_registration();
    let model = registration.get_model();
    let tmp = tempfile::tempdir().unwrap();
    let mut session = session_with_turns(tmp.path(), 3);
    let deltas: std::sync::Arc<std::sync::Mutex<Vec<String>>> = std::sync::Arc::default();
    let sink_deltas = std::sync::Arc::clone(&deltas);
    let sink: SummaryDeltaSink = std::sync::Arc::new(move |delta| {
        sink_deltas.lock().unwrap().push(delta.to_string());
    });
    let outcome = execute_compaction(
        &mut session,
        CompactOptions {
            inference: None,
            model,
            api_key: None,
            custom_instructions: None,
            settings: super::super::compaction::CompactionSettings {
                keep_recent_tokens: 20,
                ..Default::default()
            },
            abort: None,
            auxiliary: None,
            semantic_edges: None,
            summary_delta: Some(sink),
        },
    )
    .await
    .unwrap();
    let CompactOutcome::Ran(run) = outcome else {
        panic!("expected the compaction to run");
    };
    let deltas = deltas.lock().unwrap().join("");
    // The streamed deltas concatenate to exactly the generated
    // summary.
    assert!(!deltas.is_empty(), "the sink saw at least one delta");
    assert_eq!(deltas, "## Goal\nsummarized goal");
    // The convergence invariant: a client accumulating every delta
    // holds exactly the committed summary the settled end carries.
    assert_eq!(deltas, run.result.summary);
    registration.unregister();
}

/// A split-turn compaction keeps the live stream in final order: the history
/// streams live, the turn-prefix call never streams raw chunks, and the
/// accumulated stream converges to the committed summary.
#[tokio::test]
async fn split_turn_compaction_streams_in_final_order_and_converges() {
    let registration = faux_registration();
    let model = registration.get_model();
    let tmp = tempfile::tempdir().unwrap();
    let mut session = SessionManager::in_memory(tmp.path());
    let user = |text: &str| {
        AgentMessage::User(pa_types::ai::UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
            rest: Map::default(),
        })
    };
    let reply = |text: &str| {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![pa_types::ai::AssistantContentBlock::Text(
                pa_types::ai::TextContent {
                    text: text.to_string(),
                    text_signature: None,
                    rest: Map::default(),
                },
            )],
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "compact-m".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: pa_types::ai::Usage::default(),
            stop_reason: pa_types::ai::StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: Map::default(),
        })
    };
    session.append_message(user("turn one")).unwrap();
    session.append_message(reply("reply one")).unwrap();
    session
        .append_message(user(&format!("big turn {}", "x".repeat(4_000))))
        .unwrap();
    session
        .append_message(reply(&format!("reply {}", "y".repeat(4_000))))
        .unwrap();
    session.append_message(user("turn three")).unwrap();
    session.append_message(reply("reply three")).unwrap();
    // The tiny keep-recent budget lands the cut on the big turn's
    // assistant reply — a mid-turn (split) cut.
    let (cut, _) = compute_cut(&session, 10);
    assert!(cut.is_split_turn);

    // Two scripted summaries (the factories answer in issue order,
    // whichever call reaches the faux provider first).
    registration.set_responses(vec![
        pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
            "the history summary",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        )),
        pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
            "the turn prefix summary",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        )),
    ]);
    let deltas: std::sync::Arc<std::sync::Mutex<Vec<String>>> = std::sync::Arc::default();
    let sink_deltas = std::sync::Arc::clone(&deltas);
    let sink: SummaryDeltaSink = std::sync::Arc::new(move |delta| {
        sink_deltas.lock().unwrap().push(delta.to_string());
    });
    let outcome = execute_compaction(
        &mut session,
        CompactOptions {
            inference: None,
            model,
            api_key: None,
            custom_instructions: None,
            settings: super::super::compaction::CompactionSettings {
                keep_recent_tokens: 10,
                ..Default::default()
            },
            abort: None,
            auxiliary: None,
            semantic_edges: None,
            summary_delta: Some(sink),
        },
    )
    .await
    .unwrap();
    let CompactOutcome::Ran(run) = outcome else {
        panic!("expected the compaction to run");
    };
    assert_eq!(registration.call_count(), 2, "both wire calls ran");
    let summary = &run.result.summary;
    let marker = "\n\n---\n\n**Turn Context (split turn):**\n\n";
    let split = summary
        .split_once(marker)
        .unwrap_or_else(|| panic!("the committed summary carries the split marker: {summary:?}"));
    let (history_text, prefix_text) = (split.0, split.1);
    let mut scripted = vec!["the history summary", "the turn prefix summary"];
    scripted.sort_unstable();
    let mut committed = vec![history_text, prefix_text];
    committed.sort_unstable();
    assert_eq!(committed, scripted);

    let deltas = deltas.lock().unwrap().clone();
    let marker_positions: Vec<usize> = deltas
        .iter()
        .enumerate()
        .filter(|(_, delta)| delta.contains(marker))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        marker_positions,
        vec![deltas.len() - 1],
        "the split marker rides exactly one chunk, the final flush: {deltas:?}"
    );
    let live_history = deltas[..deltas.len() - 1].concat();
    assert_eq!(live_history, history_text, "deltas: {deltas:?}");
    // The convergence invariant: the accumulated stream IS the
    // committed summary.
    assert_eq!(deltas.concat(), *summary);
    registration.unregister();
}

/// An error-stop summarizer response fails the compaction, never an
/// empty-summary success.
#[tokio::test]
async fn execute_compaction_fails_on_an_error_summarizer_response() {
    let registration = faux_registration();
    let model = registration.get_model();
    registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "",
            pa_ai::faux::FauxAssistantMessageOptions {
                stop_reason: Some(pa_types::ai::StopReason::Error),
                error_message: Some("summarizer exploded".to_string()),
                ..Default::default()
            },
        ),
    )]);
    let tmp = tempfile::tempdir().unwrap();
    let mut session = session_with_turns(tmp.path(), 3);
    let error = execute_compaction(
        &mut session,
        CompactOptions {
            inference: None,
            model,
            api_key: None,
            custom_instructions: None,
            settings: super::super::compaction::CompactionSettings {
                keep_recent_tokens: 20,
                ..Default::default()
            },
            abort: None,
            auxiliary: None,
            semantic_edges: None,
            summary_delta: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Summarization failed: summarizer exploded"
    );
    assert!(session
        .get_entries()
        .iter()
        .all(|entry| !matches!(entry, FileEntry::Compaction { .. })));
    registration.unregister();
}

/// The durable row carries the full record — `fromHook: false`, the usage,
/// and the file-operation details — not just the summary boundary.
#[tokio::test]
async fn durable_compaction_row_carries_the_ts_record() {
    let registration = faux_registration();
    let model = registration.get_model();
    let tmp = tempfile::tempdir().unwrap();
    let mut session = session_with_turns(tmp.path(), 3);
    let before = context_tokens(session.get_all_entries(), session.get_leaf_id());
    let outcome = execute_compaction(
        &mut session,
        CompactOptions {
            inference: None,
            model,
            api_key: None,
            custom_instructions: None,
            settings: super::super::compaction::CompactionSettings {
                keep_recent_tokens: 20,
                ..Default::default()
            },
            abort: None,
            auxiliary: None,
            semantic_edges: None,
            summary_delta: None,
        },
    )
    .await
    .unwrap();
    let CompactOutcome::Ran(run) = outcome else {
        panic!("expected the compaction to run");
    };
    assert_eq!(run.entry.from_hook, Some(false));
    assert_eq!(run.entry.usage, run.result.usage);
    assert_eq!(run.entry.tokens_before, before);
    // The persisted session record is the full entry, byte-for-byte.
    let persisted = session
        .get_entries()
        .iter()
        .rev()
        .find_map(|entry| match entry {
            FileEntry::Compaction { payload, .. } => Some(payload.clone()),
            _ => None,
        })
        .expect("compaction entry persisted");
    assert_eq!(persisted, run.entry);
}
