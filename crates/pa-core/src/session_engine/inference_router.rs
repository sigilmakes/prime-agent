//! Session-scoped provider policy. Provider identity selects Lua; model API selects
//! the native transport only when no provider contribution is active. No global writes.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use pa_agent::{
    abort::AbortSignal,
    stream::{AssistantMessageEvent, LlmContext, ModelStream, StreamFn, StreamRequestOptions},
    types::{AssistantMessage, Model, StopReason, Usage},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{
    inference_capabilities::{DelegateLease, InferenceCapabilities},
    provider_adapter::{self, ProviderTarget},
};
use crate::plugins::{
    Contribution, ContributionKind, Invocation, InvocationEvent, InvocationMode, InvocationOutcome,
    PluginHost,
};

/// Wrap a native stream exactly once. Every caller, including auxiliary calls,
/// passes its own model. The wrapper never substitutes the session's main target.
pub(crate) fn wrap_stream_fn(
    host: Arc<PluginHost>,
    capabilities: Arc<InferenceCapabilities>,
    native: StreamFn,
) -> StreamFn {
    Arc::new(move |model, context, options| {
        let host = Arc::clone(&host);
        let capabilities = Arc::clone(&capabilities);
        let native = Arc::clone(&native);
        Box::pin(async move {
            let target = provider_target(&host, &model)?;
            let Some(target) = target else {
                return native(model, context, options).await;
            };
            route(
                host,
                &capabilities,
                target,
                native,
                model,
                &context,
                options,
            )
        })
    })
}

fn provider_target(host: &PluginHost, model: &Model) -> anyhow::Result<Option<Contribution>> {
    let targets: Vec<_> = host
        .snapshot()
        .contributions
        .iter()
        .filter(|target| target.kind == ContributionKind::Provider && target.name == model.provider)
        .cloned()
        .collect();
    if targets.len() > 1 {
        bail!("ambiguous plugin provider: {}", model.provider);
    }
    Ok(targets.into_iter().next())
}

fn route(
    host: Arc<PluginHost>,
    capabilities: &InferenceCapabilities,
    target: Contribution,
    native: StreamFn,
    model: Model,
    context: &LlmContext,
    options: StreamRequestOptions,
) -> anyhow::Result<Box<dyn ModelStream>> {
    if target.mode != InvocationMode::Streaming {
        bail!("plugin provider requires streaming mode");
    }
    pa_agent::abort::throw_if_aborted(&options.signal)?;
    let ai_model = provider_adapter::native_model(&model)?;
    let signal = options.signal.clone();
    let input_options = json!({"temperature":options.temperature,"maxTokens":options.max_tokens,
        "reasoning":options.reasoning,"sessionId":options.session_id,"serviceTier":options.service_tier});
    // Credentials and typed native input stay in Rust. Lua can explicitly delegate
    // this invocation, or supply another explicit endpoint/credential to native IO.
    let lease = capabilities
        .prepare_delegate(
            &target,
            ProviderTarget {
                api_key: options.api_key.clone(),
                model: ai_model,
                service_tier: None,
                headers: None,
            },
            context.clone(),
            options,
            native,
        )
        .map_err(|error| anyhow!(error))?;
    let input =
        json!({"request":lease.id(),"model":model,"context":context,"options":input_options});
    let cancel = CancellationToken::new();
    let invocation = host.invoke(&target, input, &cancel)?;
    Ok(Box::new(ProviderStream {
        host,
        target,
        invocation,
        _lease: lease,
        signal,
        cancel,
        model,
        started: false,
        terminal: None,
        result: None,
        finished: false,
    }))
}

/// One session's auxiliary inference entry point. It shares provider contributions
/// with main inference, but every call owns its full model and explicit credentials.
/// It never reads the main model target or re-resolves a catalog definition.
pub(crate) struct AuxiliaryInference {
    host: Arc<PluginHost>,
    capabilities: Arc<InferenceCapabilities>,
}

impl AuxiliaryInference {
    pub(crate) fn new(host: Arc<PluginHost>, capabilities: Arc<InferenceCapabilities>) -> Self {
        Self { host, capabilities }
    }

    #[tracing::instrument(skip_all)]
    pub(crate) async fn complete(
        &self,
        model: &pa_types::ai::Model,
        context: pa_types::ai::Context,
        options: StreamRequestOptions,
        on_delta: Option<super::compaction_exec::SummaryDeltaSink>,
    ) -> anyhow::Result<pa_types::ai::AssistantMessage> {
        let requested = model.clone();
        let native: StreamFn = Arc::new(move |_, context, options| {
            let model = requested.clone();
            Box::pin(async move {
                // Auxiliary portable context must not inherit a main request's
                // authoritative native prefix, even inside a nested plugin call.
                provider_adapter::stream_once_with_native(
                    &model,
                    options.api_key.clone(),
                    options
                        .service_tier
                        .and_then(|tier| provider_adapter::json_round_trip(&tier)),
                    options.headers.clone(),
                    context,
                    options,
                    None,
                )
            })
        });
        let stream = wrap_stream_fn(
            Arc::clone(&self.host),
            Arc::clone(&self.capabilities),
            native,
        );
        let agent_model =
            provider_adapter::json_round_trip(model).context("invalid auxiliary model")?;
        let context = LlmContext {
            system_prompt: context.system_prompt,
            messages: context
                .messages
                .iter()
                .map(|message| {
                    provider_adapter::json_round_trip(message).context("invalid auxiliary message")
                })
                .collect::<anyhow::Result<_>>()?,
            tools: context
                .tools
                .unwrap_or_default()
                .iter()
                .map(|tool| {
                    provider_adapter::json_round_trip(tool).context("invalid auxiliary tool")
                })
                .collect::<anyhow::Result<_>>()?,
        };
        let mut stream = stream(agent_model, context, options).await?;
        while let Some(event) = stream.next_event().await {
            if let (Some(on_delta), AssistantMessageEvent::TextDelta { delta, .. }) =
                (&on_delta, event)
            {
                on_delta(&delta);
            }
        }
        provider_adapter::json_round_trip(&stream.result().await?)
            .context("invalid auxiliary result")
    }
}

struct ProviderStream {
    host: Arc<PluginHost>,
    target: Contribution,
    invocation: Invocation,
    _lease: DelegateLease,
    signal: AbortSignal,
    cancel: CancellationToken,
    model: Model,
    started: bool,
    terminal: Option<AssistantMessageEvent>,
    result: Option<Result<AssistantMessage, String>>,
    finished: bool,
}

impl ProviderStream {
    fn authority(&self) -> anyhow::Result<()> {
        pa_agent::abort::throw_if_aborted(&self.signal)?;
        if self.cancel.is_cancelled() {
            bail!("plugin inference cancelled");
        }
        if !self.host.snapshot().contributions.contains(&self.target) {
            bail!("plugin provider generation retired");
        }
        Ok(())
    }

    fn failure(&mut self, error: impl std::fmt::Display) -> AssistantMessageEvent {
        self.cancel.cancel();
        self.finished = true;
        let error = error.to_string();
        self.result = Some(Err(error.clone()));
        AssistantMessageEvent::Error {
            reason: StopReason::Error,
            error: AssistantMessage {
                content: vec![],
                api: self.model.api.clone(),
                provider: self.model.provider.clone(),
                model: self.model.id.clone(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: Usage::zero(),
                stop_reason: StopReason::Error,
                stop_reason_raw: None,
                error_message: Some(error),
                timestamp: 0,
            },
        }
    }

    async fn receive(&mut self) -> Option<AssistantMessageEvent> {
        if self.finished {
            return None;
        }
        loop {
            if let Err(error) = self.authority() {
                return Some(self.failure(error));
            }
            let event = tokio::select! { biased;
                () = self.signal.aborted() => return Some(self.failure("Request was aborted")),
                event = self.invocation.next() => event,
            };
            if let Err(error) = self.authority() {
                return Some(self.failure(error));
            }
            match event {
                Some(InvocationEvent::Update(value)) => {
                    if self.terminal.is_some() {
                        return Some(self.failure("plugin emitted after terminal event"));
                    }
                    let event =
                        match serde_json::from_value::<pa_types::ai::AssistantMessageEvent>(value)
                            .map_err(anyhow::Error::from)
                            .and_then(|event| {
                                provider_adapter::convert_stream_event(&event)
                                    .context("unsupported plugin inference event")
                            }) {
                            Ok(event) => event,
                            Err(error) => {
                                return Some(
                                    self.failure(format!(
                                        "invalid plugin inference event: {error}"
                                    )),
                                )
                            }
                        };
                    if matches!(event, AssistantMessageEvent::Start { .. }) {
                        if self.started {
                            return Some(self.failure("duplicate plugin stream start"));
                        }
                        self.started = true;
                    } else if !self.started {
                        return Some(self.failure("plugin stream must start before updates"));
                    }
                    if event.terminal_message().is_some() {
                        // Do not accept a terminal message before the handler completes.
                        // Its final result must agree; retirement can still fence it.
                        self.terminal = Some(event);
                    } else {
                        return Some(event);
                    }
                }
                Some(InvocationEvent::Finished(Ok(InvocationOutcome::Streamed(value)))) => {
                    let message: AssistantMessage = match serde_json::from_value(value) {
                        Ok(message) => message,
                        Err(error) => {
                            return Some(
                                self.failure(format!("invalid plugin inference result: {error}")),
                            )
                        }
                    };
                    if self
                        .terminal
                        .as_ref()
                        .and_then(AssistantMessageEvent::terminal_message)
                        != Some(&message)
                    {
                        return Some(self.failure("plugin terminal event and result disagree"));
                    }
                    self.finished = true;
                    self.result = Some(Ok(message));
                    return self.terminal.take();
                }
                Some(InvocationEvent::Finished(Ok(InvocationOutcome::Unary(_)))) => {
                    return Some(self.failure("plugin provider returned unary outcome"))
                }
                Some(InvocationEvent::Finished(Err(error))) => return Some(self.failure(error)),
                None => return Some(self.failure("plugin inference ended without result")),
            }
        }
    }
}

impl ModelStream for ProviderStream {
    fn next_event(&mut self) -> pa_agent::BoxFut<'_, Option<AssistantMessageEvent>> {
        Box::pin(self.receive())
    }
    fn result(&mut self) -> pa_agent::BoxFut<'_, anyhow::Result<AssistantMessage>> {
        Box::pin(async move {
            while !self.finished {
                self.receive().await;
            }
            if let Some(Err(error)) = &self.result {
                return Err(anyhow!(error.clone()));
            }
            self.authority()?;
            self.result
                .as_ref()
                .context("plugin inference missing result")?
                .clone()
                .map_err(|error| anyhow!(error))
        })
    }
    fn close(&mut self) {
        self.cancel.cancel();
    }
}

impl Drop for ProviderStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests;
