//! Active context contributions are dynamic model input, never canonical rows.
use super::plugins::SessionPlugins;
use crate::plugins::{ContributionKind, InvocationEvent, InvocationOutcome, PluginError};
use pa_agent::types::{AssistantContent, Message, UserContent, UserPart};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

// New host safety ceilings, independent of plugin policy. An over-budget or
// failing plugin contributes nothing to this call; it never fails the model
// call. Count UTF-8 bytes and separators, but do not impose this budget on the
// mandatory base prompt.
const MAX_CONTRIBUTION_BYTES: usize = 32 * 1024;
const MAX_TOTAL_CONTRIBUTION_BYTES: usize = 128 * 1024;

fn recent_texts(context: &pa_agent::stream::LlmContext) -> Vec<String> {
    context
        .messages
        .iter()
        .rev()
        .filter_map(|message| {
            let mut text = String::new();
            let mut remaining = 512;
            let mut append = |part: &str| {
                for ch in part.chars().take(remaining) {
                    text.push(ch);
                    remaining -= 1;
                }
            };
            match message {
                Message::User(user) => match &user.content {
                    UserContent::Text(value) => append(value),
                    UserContent::Parts(parts) => {
                        for part in parts {
                            if let UserPart::Text(value) = part {
                                append(&value.text);
                            }
                        }
                    }
                },
                Message::Assistant(assistant) => {
                    for part in &assistant.content {
                        if let AssistantContent::Text(value) = part {
                            append(&value.text);
                        }
                    }
                }
                Message::ToolResult(_) => {}
            }
            (!text.is_empty()).then_some(text)
        })
        .take(4)
        .collect()
}

pub(super) fn wrap_stream_fn(
    plugins: Arc<SessionPlugins>,
    stream: pa_agent::stream::StreamFn,
) -> pa_agent::stream::StreamFn {
    Arc::new(move |model, mut context, options| {
        let plugins = Arc::clone(&plugins);
        let stream = Arc::clone(&stream);
        Box::pin(async move {
            let host = plugins.host();
            let snapshot = host.snapshot();
            let recent_texts = recent_texts(&context);
            let mut contribution_bytes = 0usize;
            for target in &snapshot.contributions {
                if target.kind != ContributionKind::Hook || target.name != "context_contribution" {
                    continue;
                }
                let cancellation = CancellationToken::new();
                let _guard = cancellation.clone().drop_guard();
                let mut invocation = match host.invoke(
                    target,
                    json!({"stage":"context_contribution", "recent_texts":recent_texts}),
                    &cancellation,
                ) {
                    Ok(invocation) => invocation,
                    Err(error) => {
                        warn!(plugin = %target.plugin, %error, "context contribution unavailable; continuing without it");
                        continue;
                    }
                };
                let result = loop {
                    let event = tokio::select! {biased;
                        ()=options.signal.aborted()=>return Err(pa_agent::abort::aborted_error()),
                        event=invocation.next()=>event,
                    };
                    match event {
                        Some(InvocationEvent::Finished(Ok(
                            InvocationOutcome::Unary(value) | InvocationOutcome::Streamed(value),
                        ))) => break Ok(value),
                        Some(InvocationEvent::Finished(Err(error))) => break Err(error),
                        Some(InvocationEvent::Update(_)) => {}
                        None => break Err(PluginError::Interrupted),
                    }
                };
                let value = match result {
                    Ok(value) => value,
                    Err(error) => {
                        warn!(plugin = %target.plugin, %error, "context contribution failed; continuing without it");
                        continue;
                    }
                };
                if let Err(error) = host.with_current_contributions(&[target], || ()) {
                    warn!(plugin = %target.plugin, %error, "context contribution was retired before use; skipping it");
                    continue;
                }
                let Some(instructions) = value.get("instructions") else {
                    continue;
                };
                let Some(instructions) = instructions.as_str() else {
                    warn!(plugin = %target.plugin, "context instructions must be a string; skipping this contribution");
                    continue;
                };
                if instructions.len() > MAX_CONTRIBUTION_BYTES {
                    warn!(
                        plugin = %target.plugin,
                        bytes = instructions.len(),
                        limit = MAX_CONTRIBUTION_BYTES,
                        "context contribution exceeds its per-plugin byte budget; skipping it"
                    );
                    continue;
                }
                contribution_bytes += instructions.len() + 2;
                if contribution_bytes > MAX_TOTAL_CONTRIBUTION_BYTES {
                    warn!(
                        plugin = %target.plugin,
                        bytes = contribution_bytes,
                        limit = MAX_TOTAL_CONTRIBUTION_BYTES,
                        "context contributions exceed the session byte budget; skipping the rest"
                    );
                    break;
                }
                let prompt = context.system_prompt.get_or_insert_with(String::new);
                prompt.push_str("\n\n");
                prompt.push_str(instructions);
            }
            stream(model, context, options).await
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_engine::plugins::SessionPlugins;
    use pa_agent::stream::{event_stream, LlmContext, ModelStream, StreamRequestOptions};
    use std::sync::Mutex;

    #[test]
    fn recent_context_signal_is_bounded_text_only_newest_first() {
        let mut context = pa_agent::stream::LlmContext {
            system_prompt: Some("not task signal".into()),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        for index in 0..6 {
            context
                .messages
                .push(Message::User(pa_agent::types::UserMessage {
                    rest: serde_json::Map::default(),
                    content: UserContent::Text(format!("{index}{}", "界".repeat(600))),
                    timestamp: index,
                }));
        }
        context.messages.push(
            serde_json::from_value(json!({
                "role":"toolResult","toolCallId":"fixture","toolName":"fixture",
                "content":[{"type":"text","text":"not user or assistant signal"}],
                "isError":false
            }))
            .unwrap(),
        );
        assert_eq!(
            recent_texts(&context),
            (2..6)
                .rev()
                .map(|index| format!("{index}{}", "界".repeat(511)))
                .collect::<Vec<_>>()
        );
        context.messages.push(
            serde_json::from_value(json!({
                "role":"user","content":[{"type":"image","data":"fixture","mimeType":"image/png"},
                    {"type":"text","text":"東京"},{"type":"text","text":" quantum"}]
            }))
            .unwrap(),
        );
        assert_eq!(recent_texts(&context)[0], "東京 quantum");
    }

    #[tokio::test]
    async fn a_failing_plugin_contribution_never_fails_the_model_call() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("good.lua"),
            "return {hooks={{name='context_contribution',run=function() return {instructions='GOOD-INSTRUCTIONS'} end}}}",
        )
        .unwrap();
        std::fs::write(
            agent.join("broken.lua"),
            "return {hooks={{name='context_contribution',run=function() error('boom') end}}}",
        )
        .unwrap();
        std::fs::write(
            agent.join("settings.json"),
            json!({"plugins": {
                "good": {"enabled": true, "path": "good.lua"},
                "broken": {"enabled": true, "path": "broken.lua"}
            }})
            .to_string(),
        )
        .unwrap();
        let plugins = Arc::new(SessionPlugins::new(
            "contribution-failure".into(),
            cwd,
            agent,
            0,
        ));
        plugins.reload().await.unwrap();

        let captured = Arc::new(Mutex::new(None));
        let inner_captured = Arc::clone(&captured);
        let inner: pa_agent::stream::StreamFn = Arc::new(move |_model, context, _options| {
            let captured = Arc::clone(&inner_captured);
            Box::pin(async move {
                *captured.lock().unwrap() = context.system_prompt.clone();
                let (_handle, stream) = event_stream();
                Ok(Box::new(stream) as Box<dyn ModelStream>)
            })
        });
        let wrapped = wrap_stream_fn(Arc::clone(&plugins), inner);
        let model: pa_agent::types::Model = serde_json::from_value(json!({
            "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
            "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap();
        let context = LlmContext {
            system_prompt: Some("BASE-PROMPT".into()),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let result = wrapped(model, context, StreamRequestOptions::default()).await;
        assert!(
            result.is_ok(),
            "a plugin failure must not fail the model call"
        );
        let prompt = captured.lock().unwrap().clone().unwrap();
        assert!(prompt.contains("BASE-PROMPT"));
        assert_eq!(prompt.matches("GOOD-INSTRUCTIONS").count(), 1);
        assert!(!prompt.contains("boom"));
    }
}
