//! The engine trait-side test battery.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[test]
fn scripted_engine_replays_then_echoes() {
    let engine = ScriptedEngine::from_value(
        &json!({"responses": ["first", {"text": "second", "delayMs": 0}]}),
    )
    .unwrap();
    let request_for = |message: &str| PromptRequest {
        batch: Vec::new(),
        images: Vec::new(),
        message: message.to_string(),
        source: "test".to_string(),
        agent_message_id: None,
        custom_message: None,
    };
    let collect = |engine: &ScriptedEngine, index: usize, message: &str| {
        let mut final_message = None;
        engine.run_prompt(index, request_for(message), &|| false, &mut |event| {
            if let EngineEvent::AssistantMessage(value) = event {
                final_message = Some(value);
            }
            true
        });
        final_message
    };
    for (index, prompt, expected) in [
        (0, "hi", "first"),
        (1, "go", "second"),
        (2, "more", "echo: more"),
    ] {
        let message = collect(&engine, index, prompt).unwrap();
        assert_eq!(
            message["content"],
            json!([{ "type": "text", "text": expected }])
        );
        let typed: pa_types::ai::Message = serde_json::from_value(message).unwrap();
        assert!(matches!(typed, pa_types::ai::Message::Assistant(_)));
    }
}

#[test]
fn scripted_assistant_and_tool_events_use_complete_native_message_wires() {
    let engine = ScriptedEngine::from_value(&json!({"responses": [{
        "text": "running the scripted tool",
        "toolCalls": [{
            "toolCallId": "call-scripted-1",
            "toolName": "bash",
            "args": {"command": "echo done"},
            "result": "done",
            "isError": false
        }]
    }]}))
    .unwrap();
    let mut messages = Vec::new();
    engine.run_prompt(
        0,
        PromptRequest {
            batch: Vec::new(),
            images: Vec::new(),
            message: "run the tool".into(),
            source: "test".into(),
            agent_message_id: None,
            custom_message: None,
        },
        &|| false,
        &mut |event| {
            match event {
                EngineEvent::AssistantUpdate {
                    message: AssistantSnapshot::Wire(value),
                    ..
                }
                | EngineEvent::AssistantMessage(value)
                | EngineEvent::ToolResultMessage(value) => messages.push(value),
                _ => {}
            }
            true
        },
    );
    assert_eq!(messages.len(), 3);
    for message in &messages {
        let _: pa_types::ai::Message = serde_json::from_value(message.clone()).unwrap();
    }
    assert_eq!(messages[0]["content"], json!([]));
    assert_eq!(messages[1]["stopReason"], "toolUse");
    assert_eq!(messages[1]["content"][1]["id"], "call-scripted-1");
    assert_eq!(messages[2]["toolCallId"], "call-scripted-1");
    assert_eq!(messages[2]["content"][0]["text"], "done");
}

#[test]
fn cancellation_stops_the_prompt() {
    let engine = ScriptedEngine::default();
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_clone = seen.clone();
    engine.run_prompt(
        0,
        PromptRequest {
            batch: Vec::new(),
            images: Vec::new(),
            message: "x".into(),
            source: "test".into(),
            agent_message_id: None,
            custom_message: None,
        },
        &|| false,
        &mut |event| {
            seen_clone.fetch_add(1, Ordering::SeqCst);
            match event {
                EngineEvent::UserMessage(_) => false, // cancel right away
                _ => true,
            }
        },
    );
    assert_eq!(seen.load(Ordering::SeqCst), 2); // user message + done
}
