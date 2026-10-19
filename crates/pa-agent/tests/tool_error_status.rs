//! Typed reported failures keep their payload and normal loop semantics.

use std::sync::{Arc, Mutex, OnceLock};

use pa_agent::abort::AbortSignal;
use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{
    AfterToolCallResult, AgentEvent, AgentMessage, AgentTool, AgentToolResult,
    AgentToolUpdateCallback, Message, Model,
};
use serde_json::{json, Value};

#[test]
fn execution_status_is_internal_and_old_payloads_decode_as_success() {
    let mut result = AgentToolResult::text("reported failure");
    result.details = json!({"status": "error", "opaque": [1, {"keep": true}]});
    result.is_error = true;
    let payload = json!({
        "content": [{"type": "text", "text": "reported failure"}],
        "details": {"status": "error", "opaque": [1, {"keep": true}]},
    });
    assert_eq!(serde_json::to_value(&result).unwrap(), payload);
    // The authoritative wire error bits belong to the outer execution-end
    // event and canonical toolResult. This nested payload has no such bit.
    let decoded: AgentToolResult = serde_json::from_value(payload).unwrap();
    assert!(!decoded.is_error);
    assert_eq!(decoded.content, result.content);
    assert_eq!(decoded.details, result.details);
    assert!(!AgentToolResult::default().is_error);
    assert!(!AgentToolResult::text("ok").is_error);
    assert!(AgentToolResult::error("thrown").is_error);
}

struct ReportedTool {
    result: AgentToolResult,
}

impl AgentTool for ReportedTool {
    fn name(&self) -> &'static str {
        "reported"
    }
    fn description(&self) -> &'static str {
        "Reports a typed execution status."
    }
    fn parameters(&self) -> &Value {
        static PARAMETERS: OnceLock<Value> = OnceLock::new();
        PARAMETERS.get_or_init(|| json!({"type": "object"}))
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        _params: Value,
        _signal: AbortSignal,
        on_update: AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            on_update(AgentToolResult::text("preview"));
            Ok(self.result.clone())
        })
    }
}

struct StatusCase {
    input_is_error: bool,
    override_is_error: Option<bool>,
    terminate: Option<bool>,
}

// Keep the hook, typed event, canonical result, and continuation assertions together.
#[allow(clippy::too_many_lines)]
async fn verify_status_case(case: StatusCase) {
    let mut model = Model::unknown();
    model.id = "test-model".into();
    model.context_window = 100_000;
    model.max_tokens = 4_096;
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(None, vec![("call-1", "reported", json!({}))]);
    provider.push_text_turn("continued normally");
    let mut result = AgentToolResult::text("error words are not status authority");
    result.details = json!({"status": "error", "opaque": [3, {"kept": "exact"}]});
    result.is_error = case.input_is_error;
    result.terminate = case.terminate;
    let expected = result.clone();
    let hook_seen = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            model: Some(model),
            tools: Some(vec![Arc::new(ReportedTool { result })]),
            ..Default::default()
        },
        stream_fn: Some(provider.stream_fn()),
        after_tool_call: Some(Arc::new({
            let hook_seen = Arc::clone(&hook_seen);
            move |context, _signal| {
                let hook_seen = Arc::clone(&hook_seen);
                Box::pin(async move {
                    hook_seen
                        .lock()
                        .unwrap()
                        .push((context.is_error, context.result));
                    Ok(Some(AfterToolCallResult {
                        is_error: case.override_is_error,
                        ..Default::default()
                    }))
                })
            }
        })),
        ..Default::default()
    });
    agent
        .subscribe({
            let events = Arc::clone(&events);
            move |event, _signal| {
                let events = Arc::clone(&events);
                Box::pin(async move {
                    events.lock().unwrap().push(event);
                    Ok(())
                })
            }
        })
        .await;
    agent.prompt("run the tool").await.unwrap();
    agent.wait_for_idle().await;
    assert_eq!(
        *hook_seen.lock().unwrap(),
        vec![(case.input_is_error, expected.clone())]
    );
    let final_error = case.override_is_error.unwrap_or(case.input_is_error);
    let events = events.lock().unwrap().clone();
    let terminal: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => Some((tool_call_id, tool_name, result, is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(terminal.len(), 1);
    let (call_id, name, result, is_error) = terminal[0];
    let mut finalized = expected;
    finalized.is_error = final_error;
    assert_eq!(
        (call_id.as_str(), name.as_str(), result, *is_error),
        ("call-1", "reported", &finalized, final_error)
    );
    let previews: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionUpdate { partial_result, .. } => Some(partial_result),
            _ => None,
        })
        .collect();
    assert_eq!(previews, vec![&AgentToolResult::text("preview")]);
    drop(events);
    let state = agent.state().await;
    let tool_results: Vec<_> = state
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Standard(Message::ToolResult(result)) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].is_error, final_error);
    assert_eq!(tool_results[0].content, finalized.content);
    assert_eq!(tool_results[0].details.as_ref(), Some(&finalized.details));
    let calls = provider.calls();
    if case.terminate == Some(true) {
        assert_eq!(calls.len(), 1, "terminate still ends the tool batch");
        assert_eq!(state.messages.len(), 3);
    } else {
        assert_eq!(calls.len(), 2, "reported errors still reach the next model");
        assert_eq!(state.messages.len(), 4);
        let next_result = calls[1]
            .messages
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) => Some(result),
                _ => None,
            })
            .expect("next model receives the canonical result");
        assert_eq!(next_result, tool_results[0]);
        let AgentMessage::Standard(Message::Assistant(done)) = &state.messages[3] else {
            panic!("missing final assistant");
        };
        assert_eq!(
            done.content,
            vec![pa_agent::types::AssistantContent::Text(
                pa_agent::types::TextContent {
                    text: "continued normally".into(),
                    text_signature: None
                }
            )]
        );
    }
    assert!(!state.is_streaming);
    assert!(state.error_message.is_none());
}

#[tokio::test]
async fn a_reported_error_continues_with_its_payload() {
    verify_status_case(StatusCase {
        input_is_error: true,
        override_is_error: None,
        terminate: None,
    })
    .await;
}

#[tokio::test]
async fn success_does_not_infer_error_from_details() {
    verify_status_case(StatusCase {
        input_is_error: false,
        override_is_error: None,
        terminate: None,
    })
    .await;
}

#[tokio::test]
async fn after_tool_call_can_clear_a_reported_error() {
    verify_status_case(StatusCase {
        input_is_error: true,
        override_is_error: Some(false),
        terminate: None,
    })
    .await;
}

#[tokio::test]
async fn after_tool_call_can_mark_a_success_as_error() {
    verify_status_case(StatusCase {
        input_is_error: false,
        override_is_error: Some(true),
        terminate: None,
    })
    .await;
}

#[tokio::test]
async fn a_reported_error_preserves_explicit_batch_termination() {
    verify_status_case(StatusCase {
        input_is_error: true,
        override_is_error: None,
        terminate: Some(true),
    })
    .await;
}
