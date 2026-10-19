//! Active context contributions are dynamic model input, never canonical rows.
use super::plugins::SessionPlugins;
use crate::plugins::{ContributionKind, InvocationEvent, InvocationOutcome};
use anyhow::{bail, Context};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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
            for target in &snapshot.contributions {
                if target.kind != ContributionKind::Hook || target.name != "context_contribution" {
                    continue;
                }
                let cancellation = CancellationToken::new();
                let _guard = cancellation.clone().drop_guard();
                let mut invocation = host.invoke(
                    target,
                    json!({"stage":"context_contribution"}),
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
                    let prompt = context.system_prompt.get_or_insert_with(String::new);
                    prompt.push_str("\n\n");
                    prompt.push_str(instructions);
                }
            }
            stream(model, context, options).await
        })
    })
}
