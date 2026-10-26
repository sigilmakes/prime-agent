// Bounded integration futures and a single complete boundary assertion helper.
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! Source/native SDK-style leased integration, not an installed CLI test.
//! The real runtime `SessionLease` owns the actual persisted session file before
//! append ownership is raised and until all model/event/history/disk checks finish.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{AgentEvent, AgentMessage};
use pa_types::session::FileEntry;
use serde_json::{json, Value};

use pa_core::session::manager::SessionManager;
use pa_core::session::window::AppendOwnership;
use pa_core::session_engine::tool_bridge::bridge_tool;
use pa_core::session_engine::{AgentSession, PromptOptions};
use pa_core::{ToolContentBlock, ToolDefinition, ToolExecutionResult};
use pa_daemon::lease::{
    acquire_runtime_session_lease, canonical_session_path, live_lease_owner,
    SessionAlreadyActiveError,
};

// Only the per-run message timestamp is normalized. No payload or envelope fields are removed.
fn normalized_message(mut message: Value) -> Value {
    assert!(message["timestamp"].as_i64().is_some_and(|stamp| stamp > 0));
    message["timestamp"] = json!(0);
    message
}

async fn assert_persisted_status(is_error: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    let session_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut manager = SessionManager::persisted(&cwd, &session_dir);
    let path = manager.get_session_file().unwrap().to_path_buf();
    let session_id = manager.get_session_id().to_string();
    let agent_dir = tmp.path().join("agent");
    let lease = acquire_runtime_session_lease(&path, &agent_dir).unwrap();
    assert_eq!(lease.session_path, canonical_session_path(&path));
    let owner = live_lease_owner(&agent_dir, &path).expect("a real live runtime lease owner");
    assert_eq!(owner.pid, std::process::id());
    let refusal = acquire_runtime_session_lease(&path, &agent_dir).unwrap_err();
    let refusal = refusal.downcast_ref::<SessionAlreadyActiveError>().unwrap();
    assert_eq!(refusal.holder_pid, Some(owner.pid));
    assert_eq!(refusal.session_path, lease.session_path.to_string_lossy());
    assert_eq!(refusal.active_session_id, owner.active_session_id);
    assert!(
        !path.exists(),
        "the runtime lease precedes the first durable write"
    );
    manager.set_append_ownership(AppendOwnership::SessionLeaseHeld);

    // Deliberately contradict the typed flag. Arbitrary details are data, not status authority.
    let details = json!({
        "status": if is_error { "ok" } else { "error" },
        "error": if is_error { "no failure" } else { "Traceback: misleading metadata" },
        "isError": !is_error,
        "unknown": {
            "nested": [null, 7, {"status": "failed", "error": "quoted", "is_error": !is_error}],
            "keep": {"empty": [], "unicode": "résultat", "enabled": true}
        }
    });
    let content = json!([
        {"type": "text", "text": "rich result\nsecond line"},
        {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}
    ]);
    let definition = ToolDefinition {
        name: "rich_status".to_string(),
        label: "Rich status".to_string(),
        description: "Return a rich result with typed status".to_string(),
        prompt_snippet: String::new(),
        parameters: json!({"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]}),
        execution_mode: None,
        prepare_arguments: None,
        execute: {
            let details = details.clone();
            Arc::new(move |id, args, signal, _on_update| {
                assert_eq!(id, "call-rich-status");
                assert_eq!(args, json!({"input": "inspect"}));
                assert!(signal.is_some_and(|signal| !signal.is_cancelled()));
                let details = details.clone();
                Box::pin(async move {
                    Ok(ToolExecutionResult {
                        content: vec![
                            ToolContentBlock::text("rich result\nsecond line"),
                            ToolContentBlock::Image {
                                data: "iVBORw0KGgo=".to_string(),
                                mime_type: "image/png".to_string(),
                            },
                        ],
                        details: Some(details),
                        is_error,
                    })
                })
            })
        },
    };
    let model = pa_agent::types::Model {
        id: "status-model".into(),
        name: "Status model".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 10_000,
        max_tokens: 100,
        input: Vec::new(),
        max_tokens_explicit: false,
    };
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(
        Some("checking"),
        vec![(
            "call-rich-status",
            "rich_status",
            json!({"input": "inspect"}),
        )],
    );
    provider.push_text_turn("done");
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            model: Some(model),
            ..Default::default()
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }));
    agent.set_tools(vec![bridge_tool(definition)]).await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    agent
        .subscribe(move |event, _signal| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                sink.lock().unwrap().push(event);
                Ok(())
            })
        })
        .await;
    let session = AgentSession::new(agent, manager, vec![]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        session
            .prompt("inspect status", PromptOptions::default())
            .await
            .unwrap();
        session.agent().wait_for_idle().await;
    })
    .await
    .expect("the scripted tool turn must finish");
    assert_eq!(session.session_id().await, session_id);

    let expected = json!({
        "role": "toolResult",
        "toolCallId": "call-rich-status",
        "toolName": "rich_status",
        "content": content,
        "details": details,
        "isError": is_error,
        "timestamp": 0
    });
    let calls = provider.calls();
    assert_eq!(
        calls.len(),
        2,
        "status must not terminate or retry the turn"
    );
    let first = serde_json::to_value(&calls[0].messages).unwrap();
    assert_eq!(first.as_array().unwrap().len(), 1);
    assert_eq!(first[0]["role"], "user");
    assert_eq!(
        first[0]["content"],
        json!([{"type": "text", "text": "inspect status"}])
    );
    let second = serde_json::to_value(&calls[1].messages).unwrap();
    assert_eq!(
        second
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "assistant", "toolResult"]
    );
    assert_eq!(normalized_message(second[2].clone()), expected);
    let state = session.agent().state().await;
    assert_eq!(
        state
            .messages
            .iter()
            .map(AgentMessage::role)
            .collect::<Vec<_>>(),
        ["user", "assistant", "toolResult", "assistant"]
    );
    assert_eq!(
        normalized_message(serde_json::to_value(&state.messages[2]).unwrap()),
        expected
    );
    let final_message = serde_json::to_value(&state.messages[3]).unwrap();
    assert_eq!(
        final_message["content"],
        json!([{"type": "text", "text": "done"}])
    );

    let captured = events.lock().unwrap().clone();
    let execution_ends = captured
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd {
                result,
                is_error: outer,
                ..
            } => {
                assert_eq!(
                    result.is_error, is_error,
                    "the bridge must retain the typed flag"
                );
                assert_eq!(*outer, is_error, "the loop must publish the typed flag");
                assert_eq!(result.terminate, None);
                pa_core::session_engine::session_events::agent_event_json(event)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        execution_ends,
        vec![json!({
            "type": "tool_execution_end",
            "toolCallId": "call-rich-status",
            "toolName": "rich_status",
            "isError": is_error,
            // No internal is_error/isError leaks into this nested SDK result.
            "result": {"content": content, "details": details}
        })]
    );
    let message_ends = captured
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd { message } if message.role() == "toolResult" => {
                pa_core::session_engine::session_events::agent_event_json(event)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(message_ends.len(), 1);
    let mut message_end = message_ends[0].clone();
    message_end["message"] = normalized_message(message_end["message"].clone());
    assert_eq!(
        message_end,
        json!({"type": "message_end", "message": expected})
    );

    let entries = session.entries().await;
    let messages = entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Message { message, .. } => Some(serde_json::to_value(message).unwrap()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        messages
            .iter()
            .map(|row| row["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "assistant", "toolResult", "assistant"]
    );
    assert_eq!(normalized_message(messages[2].clone()), expected);
    let rows = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows[0]["type"], "session");
    assert_eq!(rows[0]["id"], session_id);
    assert_eq!(
        rows[1..],
        entries
            .iter()
            .map(|entry| serde_json::to_value(entry).unwrap())
            .collect::<Vec<_>>()
    );
    let mut ids = HashSet::new();
    let mut parent = Value::Null;
    for row in &rows[1..] {
        let id = row["id"].as_str().unwrap();
        assert_eq!(id.len(), 8);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(ids.insert(id));
        assert_eq!(row["parentId"], parent);
        assert!(row["timestamp"].as_str().is_some());
        parent = row["id"].clone();
    }
    let tool_rows = rows
        .iter()
        .filter(|row| row["message"]["role"] == "toolResult")
        .collect::<Vec<_>>();
    assert_eq!(tool_rows.len(), 1);
    assert_eq!(
        normalized_message(tool_rows[0]["message"].clone()),
        expected
    );
    let assistant = rows
        .iter()
        .find(|row| row["message"]["role"] == "assistant")
        .unwrap();
    assert_eq!(tool_rows[0]["parentId"], assistant["id"]);
    let handle = session.shared_persistence();
    let snapshot = handle.lock().await.history_snapshot();
    assert_eq!(
        serde_json::to_value(snapshot.await.unwrap()).unwrap(),
        json!(rows)
    );
}

#[tokio::test]
async fn error_flag_survives_rich_result_to_provider_sdk_events_and_disk() {
    assert_persisted_status(/*is_error*/ true).await;
}

#[tokio::test]
async fn success_flag_ignores_error_strings_in_details_through_all_boundaries() {
    assert_persisted_status(/*is_error*/ false).await;
}
