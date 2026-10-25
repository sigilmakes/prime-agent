//! Active context contributions are dynamic model input, never canonical rows.
use super::plugins::SessionPlugins;
use crate::plugins::{ContributionKind, InvocationEvent, InvocationOutcome};
use anyhow::{bail, Context};
use pa_agent::types::{AssistantContent, Message, UserContent, UserPart};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// New host safety ceilings, independent of plugin policy. Reject the whole call
// rather than silently truncating a plugin's directives. Count UTF-8 bytes and
// separators, but do not impose this budget on the mandatory base prompt.
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
                let mut invocation = host.invoke(
                    target,
                    json!({"stage":"context_contribution", "recent_texts":recent_texts}),
                    &cancellation,
                )?;
                let result = loop {
                    let event = tokio::select! {biased;
                        ()=options.signal.aborted()=>return Err(pa_agent::abort::aborted_error()),
                        event=invocation.next()=>event,
                    };
                    match event {
                        Some(InvocationEvent::Finished(Ok(
                            InvocationOutcome::Unary(value) | InvocationOutcome::Streamed(value),
                        ))) => break value,
                        Some(InvocationEvent::Finished(Err(error))) => return Err(error.into()),
                        Some(InvocationEvent::Update(_)) => {}
                        None => bail!("context contribution ended without result"),
                    }
                };
                host.with_current_contributions(&[target], || ())?;
                if let Some(instructions) = result.get("instructions") {
                    let instructions = instructions
                        .as_str()
                        .context("context instructions must be a string")?;
                    if instructions.len() > MAX_CONTRIBUTION_BYTES {
                        bail!("context contribution exceeds {MAX_CONTRIBUTION_BYTES} UTF-8 bytes; refusing to truncate instructions");
                    }
                    contribution_bytes += instructions.len() + 2;
                    if contribution_bytes > MAX_TOTAL_CONTRIBUTION_BYTES {
                        bail!("context contributions exceed {MAX_TOTAL_CONTRIBUTION_BYTES} total UTF-8 bytes; refusing to truncate instructions");
                    }
                    let prompt = context.system_prompt.get_or_insert_with(String::new);
                    prompt.push_str("\n\n");
                    prompt.push_str(instructions);
                }
            }
            stream(model, context, options).await
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
