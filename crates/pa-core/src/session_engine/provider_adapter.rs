//! Real-provider stream adapter: pa-ai completion streaming bridged into
//! the pa-agent loop's `StreamFn`/`ModelStream`, crossing the crate
//! boundary by wire-shape (JSON) round-trip; shared by pa-cli and
//! pa-daemon.

use std::sync::Arc;

use pa_agent::stream::{LlmContext, ModelStream, StreamFn, StreamRequestOptions};
use pa_agent::types::{Model as AgentModel, ThinkingLevel};
use pa_types::ai::Model;

tokio::task_local! {
    static NATIVE_INPUT: pa_ai::compaction::NativeResponsesInput;
    static EVENT_BUFFER_CAPACITY: usize;
}

/// Enable bounded native producer delivery only for this plugin-owned opening.
/// Ordinary host requests retain the transport's legacy delivery mode.
pub(crate) async fn with_bounded_native_events<F: std::future::Future>(work: F) -> F::Output {
    EVENT_BUFFER_CAPACITY.scope(16, work).await
}

/// Carry one typed native request through an existing stream seam. This scope is
/// local to the awaiting task; it is neither session state nor a global route.
pub async fn with_native_input<F: std::future::Future>(
    input: pa_ai::compaction::NativeResponsesInput,
    work: F,
) -> F::Output {
    NATIVE_INPUT.scope(input, work).await
}

pub(crate) fn current_native_input() -> Option<pa_ai::compaction::NativeResponsesInput> {
    NATIVE_INPUT.try_with(Clone::clone).ok()
}

/// Wire-shape conversion at the pa-agent/pa-ai boundary: both sides
/// serialize to the same camelCase wire shapes.
pub fn json_round_trip<T, U>(value: &T) -> Option<U>
where
    T: serde::Serialize,
    U: serde::de::DeserializeOwned,
{
    serde_json::to_value(value)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// Convert the loop's minimal descriptor without resolving a newer catalog model.
/// The anchored model supplied by the caller remains authoritative.
///
/// # Errors
/// Returns an error for a structurally invalid model descriptor.
pub fn native_model(model: &AgentModel) -> anyhow::Result<Model> {
    let mut wire = serde_json::to_value(model)?;
    wire["baseUrl"] = serde_json::Value::String(model.base_url.clone());
    wire["input"] = serde_json::json!(["text"]);
    serde_json::from_value(wire).map_err(Into::into)
}

/// Thinking-level mapping across the two crates.
#[must_use]
pub fn map_thinking_level(level: pa_types::ai::ModelThinkingLevel) -> ThinkingLevel {
    match level {
        pa_types::ai::ModelThinkingLevel::Off => ThinkingLevel::Off,
        pa_types::ai::ModelThinkingLevel::Minimal => ThinkingLevel::Minimal,
        pa_types::ai::ModelThinkingLevel::Low => ThinkingLevel::Low,
        pa_types::ai::ModelThinkingLevel::Medium => ThinkingLevel::Medium,
        pa_types::ai::ModelThinkingLevel::High => ThinkingLevel::High,
        pa_types::ai::ModelThinkingLevel::Xhigh => ThinkingLevel::Xhigh,
        pa_types::ai::ModelThinkingLevel::Max => ThinkingLevel::Max,
    }
}

/// The inverse of [`map_thinking_level`]: the pa-types view of the agent
/// state's thinking level.
#[must_use]
pub fn model_thinking_level(level: ThinkingLevel) -> pa_types::ai::ModelThinkingLevel {
    match level {
        ThinkingLevel::Off => pa_types::ai::ModelThinkingLevel::Off,
        ThinkingLevel::Minimal => pa_types::ai::ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => pa_types::ai::ModelThinkingLevel::Low,
        ThinkingLevel::Medium => pa_types::ai::ModelThinkingLevel::Medium,
        ThinkingLevel::High => pa_types::ai::ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => pa_types::ai::ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => pa_types::ai::ModelThinkingLevel::Max,
    }
}

/// Adapt the loop-level payload hook to the pa-ai hook shape. A failed
/// round-trip keeps the payload unchanged — hooks never fail the request.
fn agent_payload_hook_to_ai(hook: pa_agent::stream::OnPayloadHook) -> pa_ai::types::OnPayloadHook {
    std::sync::Arc::new(move |payload: serde_json::Value, model: &Model| {
        match json_round_trip::<_, pa_agent::types::Model>(model) {
            Some(agent_model) => hook(payload, &agent_model),
            None => Some(payload),
        }
    })
}

/// Adapt the loop-level response hook to the pa-ai hook shape; a failed
/// round-trip drops the hook call (advisory, never fatal).
fn agent_response_hook_to_ai(
    hook: pa_agent::stream::OnResponseHook,
) -> pa_ai::types::OnResponseHook {
    std::sync::Arc::new(
        move |response: pa_ai::types::ProviderResponse, model: &Model| {
            let agent_response = pa_agent::stream::ProviderResponse {
                status: response.status,
                headers: response.headers,
            };
            if let Some(agent_model) = json_round_trip::<_, pa_agent::types::Model>(model) {
                hook(agent_response, &agent_model);
            }
        },
    )
}

/// The mutable provider target a live session's stream reads per call
/// (daemon `set_model` and the failover switch swap it).
#[derive(Debug, Clone)]
pub struct ProviderTarget {
    pub api_key: Option<String>,
    pub model: Model,
    pub service_tier: Option<pa_types::ai::ServiceTier>,
    /// The auth-resolved provider-request headers: a live `set_model` carries
    /// them through to the stream like the build-time resolution does.
    pub headers: Option<std::collections::BTreeMap<String, String>>,
}

/// A real pa-ai stream adapter reading its target from a shared slot the host can swap live
/// (`set_model`, failover); `None` only before the host sets the build-time target.
///
/// # Panics
///
/// Panics at stream time if the provider target lock is poisoned, or if the
/// target slot was never set before the first stream.
pub fn switchable_stream_fn(target: Arc<std::sync::RwLock<Option<ProviderTarget>>>) -> StreamFn {
    Arc::new(
        move |requested: AgentModel, context: LlmContext, options: StreamRequestOptions| {
            let ProviderTarget {
                api_key,
                model,
                service_tier,
                headers,
            } = target
                .read()
                .expect("provider target lock")
                .clone()
                .expect("provider target set before the first stream");
            // The main request keeps its anchored full target, including auth and
            // capabilities. An auxiliary request can deliberately select another model;
            // never replace it with the main session model.
            let selected = requested.id == "unknown"
                || (requested.id == model.id && requested.provider == model.provider);
            Box::pin(async move {
                if selected {
                    stream_once(&model, api_key, service_tier, headers, context, options)
                } else {
                    let requested = native_model(&requested)?;
                    stream_once(
                        &requested,
                        options.api_key.clone(),
                        None,
                        None,
                        context,
                        options,
                    )
                }
            })
        },
    )
}

/// Stream one completion against `model` with `api_key` and the auth-resolved
/// request `headers`: the per-request tail the switchable seams and the CLI's
/// route-authoritative variant share.
///
/// # Errors
///
/// Returns the provider stream's error when the request fails (the
/// per-attempt failures the retry driver classifies).
pub fn stream_once(
    model: &Model,
    api_key: Option<String>,
    service_tier: Option<pa_types::ai::ServiceTier>,
    headers: Option<std::collections::BTreeMap<String, String>>,
    context: LlmContext,
    options: StreamRequestOptions,
) -> anyhow::Result<Box<dyn ModelStream>> {
    stream_once_with_native(
        model,
        api_key,
        service_tier,
        headers,
        context,
        options,
        current_native_input(),
    )
}

/// Stream an authoritative complete Responses array, or an ordinary portable request.
/// A native request keeps its raw bytes and route validation in pa-ai; callers must
/// clear portable messages only after appending the fresh continuation to the array.
///
/// # Errors
/// Returns native route/shape validation and transport errors without fallback.
pub fn stream_once_with_native(
    model: &Model,
    api_key: Option<String>,
    service_tier: Option<pa_types::ai::ServiceTier>,
    headers: Option<std::collections::BTreeMap<String, String>>,
    context: LlmContext,
    options: StreamRequestOptions,
    native_responses_input: Option<pa_ai::compaction::NativeResponsesInput>,
) -> anyhow::Result<Box<dyn ModelStream>> {
    pa_agent::abort::throw_if_aborted(&options.signal)?;
    let messages: Vec<pa_types::ai::Message> = context
        .messages
        .iter()
        .filter_map(json_round_trip)
        .collect();
    let tools: Vec<pa_types::ai::Tool> = context.tools.iter().filter_map(json_round_trip).collect();
    let ai_context = pa_types::ai::Context {
        system_prompt: context.system_prompt,
        messages,
        tools: Some(tools),
    };
    // The turn's abort signal reaches the transport: the in-flight request races this
    // token; the loop's abort paths fire it through [`ModelStream::close`] or the drop.
    let cancel = tokio_util::sync::CancellationToken::new();
    let stream_options = pa_ai::types::SimpleStreamOptions {
        base: pa_ai::types::StreamOptions {
            temperature: options.temperature,
            max_tokens: options.max_tokens,
            signal: Some(cancel.clone()),
            api_key,
            transport: None,
            service_tier,
            cache_retention: None,
            session_id: options.session_id.clone(),
            // The loop-level request hooks cross the crate boundary here: the payload
            // hook may replace the wire payload, the response hook observes headers.
            on_payload: options.on_payload.map(agent_payload_hook_to_ai),
            on_response: options.on_response.map(agent_response_hook_to_ai),
            // StreamOptions carries a plain map; the target's ordered
            // (BTreeMap) resolution converts here, with the request's own
            // headers (the semantic request id) merged over it — TS
            // providers' `mergeHeaders(..., optionsHeaders)` order, the
            // options win.
            headers: {
                let mut merged = headers;
                if let Some(request_headers) = options.headers.clone() {
                    merged
                        .get_or_insert_with(std::collections::BTreeMap::new)
                        .extend(request_headers);
                }
                merged.map(|headers| headers.into_iter().collect())
            },
            metadata: None,
            timeout_ms: None,
            native_responses_input,
            event_buffer_capacity: EVENT_BUFFER_CAPACITY.try_with(|capacity| *capacity).ok(),
        },
        reasoning: Some(model_thinking_level(options.reasoning)),
        thinking_budgets: None,
    };
    let stream = pa_ai::stream_simple(model, &ai_context, Some(stream_options))
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    // Bound the adapter queue. A slow consumer cannot accumulate unbounded
    // normalized events here. The producer cannot publish after close/drop.
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let pump_cancel = cancel.clone();
    let signal = options.signal;
    let pump_signal = signal.clone();
    let forwarder = tokio::spawn(async move {
        let mut stream = stream;
        loop {
            let event = tokio::select! { biased;
                () = pump_cancel.cancelled() => break,
                () = pump_signal.aborted() => { pump_cancel.cancel(); break; },
                event = stream.next_event() => event,
            };
            let Some(event) = event else {
                break;
            };
            if let Some(converted) = convert_stream_event(&event) {
                tokio::select! { biased;
                    () = pump_cancel.cancelled() => break,
                    () = pump_signal.aborted() => { pump_cancel.cancel(); break; },
                    result = sender.send(converted) => if result.is_err() { break; },
                }
            }
        }
    });
    Ok(Box::new(PumpedStream {
        forwarder,
        receiver,
        cancel,
        signal,
        result: None,
        model: json_round_trip(model).ok_or_else(|| anyhow::anyhow!("invalid provider model"))?,
        aborted_delivered: false,
    }))
}

/// A stream adapter pinned to one target (headless runtimes resolve
/// their model once, so the slot never changes).
#[must_use]
pub fn real_stream_fn(api_key: Option<String>, model: Model) -> StreamFn {
    switchable_stream_fn(Arc::new(std::sync::RwLock::new(Some(ProviderTarget {
        api_key,
        model,
        service_tier: None,
        headers: None,
    }))))
}

/// Convert one pa-ai stream event into the pa-agent loop's event enum.
/// Payloads cross the boundary by wire-shape (JSON) round-trip.
///
/// # Panics
///
/// Panics when an assistant message cannot round-trip across the two
/// crates' wire shapes (a structural shape-mismatch bug).
#[must_use]
pub fn convert_stream_event(
    event: &pa_types::ai::AssistantMessageEvent,
) -> Option<pa_agent::stream::AssistantMessageEvent> {
    use pa_agent::stream::AssistantMessageEvent as Out;
    use pa_types::ai::AssistantMessageEvent as In;
    fn convert_partial(
        message: &pa_types::ai::AssistantMessage,
    ) -> pa_agent::types::AssistantMessage {
        json_round_trip(message).expect("assistant wire shapes match")
    }
    Some(match event {
        In::Start { partial } => Out::Start {
            partial: convert_partial(partial),
        },
        In::TextStart {
            content_index,
            partial,
        } => Out::TextStart {
            content_index: *content_index as usize,
            partial: convert_partial(partial),
        },
        In::TextDelta {
            content_index,
            delta,
            partial,
        } => Out::TextDelta {
            content_index: *content_index as usize,
            delta: delta.clone(),
            partial: convert_partial(partial),
        },
        In::TextEnd {
            content_index,
            content,
            partial,
        } => Out::TextEnd {
            content_index: *content_index as usize,
            content: content.clone(),
            partial: convert_partial(partial),
        },
        In::ThinkingStart {
            content_index,
            partial,
        } => Out::ThinkingStart {
            content_index: *content_index as usize,
            partial: convert_partial(partial),
        },
        In::ThinkingDelta {
            content_index,
            delta,
            partial,
        } => Out::ThinkingDelta {
            content_index: *content_index as usize,
            delta: delta.clone(),
            partial: convert_partial(partial),
        },
        In::ThinkingEnd {
            content_index,
            partial,
            ..
        } => Out::ThinkingEnd {
            content_index: *content_index as usize,
            partial: convert_partial(partial),
        },
        In::ToolcallStart {
            content_index,
            partial,
        } => Out::ToolCallStart {
            content_index: *content_index as usize,
            partial: convert_partial(partial),
        },
        In::ToolcallDelta {
            content_index,
            delta,
            partial,
        } => Out::ToolCallDelta {
            content_index: *content_index as usize,
            delta: delta.clone(),
            partial: convert_partial(partial),
        },
        In::ToolcallEnd {
            content_index,
            tool_call,
            partial,
        } => Out::ToolCallEnd {
            content_index: *content_index as usize,
            tool_call: json_round_trip(tool_call).expect("tool call wire shapes match"),
            partial: convert_partial(partial),
        },
        In::Done { reason, message } => Out::Done {
            reason: json_round_trip(reason).expect("stop reason wire shapes match"),
            message: convert_partial(message),
        },
        In::Error { reason, error } => Out::Error {
            reason: json_round_trip(reason).expect("stop reason wire shapes match"),
            error: convert_partial(error),
        },
    })
}

/// Bounded native adapter. Both request cancellation sources fence already queued
/// updates and terminal messages. Drop aborts the producer instead of detaching it.
struct PumpedStream {
    forwarder: tokio::task::JoinHandle<()>,
    receiver: tokio::sync::mpsc::Receiver<pa_agent::stream::AssistantMessageEvent>,
    cancel: tokio_util::sync::CancellationToken,
    signal: pa_agent::abort::AbortSignal,
    result: Option<pa_agent::types::AssistantMessage>,
    model: AgentModel,
    aborted_delivered: bool,
}

impl PumpedStream {
    fn aborted_message(&self) -> pa_agent::types::AssistantMessage {
        pa_agent::types::AssistantMessage {
            // This adapter invents the settled row (it never comes from the
            // provider): a cancelled pump drops the provider's own aborted
            // message, so the shape is minted here. TS starts every assistant
            // message with an empty text block, so no row is aborted with an
            // empty part list; minting `vec![]` handed the loop (and every
            // reader of `result()`) a part-less aborted row.
            content: vec![pa_agent::types::AssistantContent::Text(
                pa_agent::types::TextContent {
                    text: String::new(),
                    text_signature: None,
                },
            )],
            api: self.model.api.clone(),
            provider: self.model.provider.clone(),
            model: self.model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: pa_agent::types::Usage::zero(),
            stop_reason: pa_agent::types::StopReason::Aborted,
            stop_reason_raw: None,
            error_message: Some("Request was aborted".into()),
            timestamp: 0,
        }
    }
}

impl Drop for PumpedStream {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.forwarder.abort();
    }
}

impl ModelStream for PumpedStream {
    fn next_event(
        &mut self,
    ) -> pa_agent::BoxFut<'_, Option<pa_agent::stream::AssistantMessageEvent>> {
        Box::pin(async move {
            let event = tokio::select! { biased;
                () = self.cancel.cancelled() => None,
                () = self.signal.aborted() => { self.cancel.cancel(); None },
                event = self.receiver.recv() => event,
            };
            if self.cancel.is_cancelled() || self.signal.is_aborted() {
                if self.aborted_delivered {
                    return None;
                }
                self.aborted_delivered = true;
                let message = self.aborted_message();
                self.result = Some(message.clone());
                return Some(pa_agent::stream::AssistantMessageEvent::Error {
                    reason: pa_agent::types::StopReason::Aborted,
                    error: message,
                });
            }
            if let Some(message) = event
                .as_ref()
                .and_then(pa_agent::stream::AssistantMessageEvent::terminal_message)
            {
                self.result = Some(message.clone());
            }
            event
        })
    }

    fn result(
        &mut self,
    ) -> pa_agent::BoxFut<'_, anyhow::Result<pa_agent::types::AssistantMessage>> {
        Box::pin(async move {
            while self.result.is_none() {
                if self.next_event().await.is_none() {
                    break;
                }
            }
            if self.cancel.is_cancelled() || self.signal.is_aborted() {
                return Ok(self.aborted_message());
            }
            self.result
                .clone()
                .ok_or_else(|| anyhow::anyhow!("provider stream ended without terminal event"))
        })
    }

    fn close(&mut self) {
        self.cancel.cancel();
        self.forwarder.abort();
        self.receiver.close();
    }
}

#[cfg(test)]
mod tests {
    //! Regression guard for the pa-agent -> pa-ai message boundary: `UserPart`
    //! must stay `type`-tagged like the TS wire format; an untagged variant
    //! silently dropped every `AgentPromptInput::Text` prompt, leaving the
    //! provider with a system prompt only.

    use super::*;

    #[tokio::test]
    async fn live_target_service_tier_reaches_provider_and_reset() {
        let registration =
            pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
                api: Some("restored-service-tier-test".to_owned()),
                ..Default::default()
            });
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = received.clone();
        let factory =
            pa_ai::faux::FauxResponseStep::Factory(Arc::new(move |_, options, _, model| {
                captured.lock().unwrap().push((
                    model.id.clone(),
                    options.and_then(|options| options.service_tier),
                ));
                Ok(pa_ai::faux::faux_assistant_text_message(
                    "ok",
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ))
            }));
        registration.set_responses(vec![factory.clone(), factory]);
        let model = registration.get_model();
        let target = Arc::new(std::sync::RwLock::new(Some(ProviderTarget {
            api_key: None,
            model: model.clone(),
            service_tier: Some(pa_types::ai::ServiceTier::Priority),
            headers: None,
        })));
        let stream_fn = switchable_stream_fn(target.clone());
        for tier in [Some(pa_types::ai::ServiceTier::Priority), None] {
            target.write().unwrap().as_mut().unwrap().service_tier = tier;
            let mut stream = stream_fn(
                AgentModel::unknown(),
                LlmContext::default(),
                StreamRequestOptions::default(),
            )
            .await
            .unwrap();
            stream.result().await.unwrap();
        }
        assert_eq!(
            *received.lock().unwrap(),
            vec![
                (model.id.clone(), Some(pa_types::ai::ServiceTier::Priority)),
                (model.id, None),
            ]
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn auxiliary_model_is_not_replaced_by_the_main_switchable_target() {
        let registration =
            pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
                api: Some("session-auxiliary-target-test".into()),
                ..Default::default()
            });
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&received);
        let factory = pa_ai::faux::FauxResponseStep::Factory(Arc::new(move |_, _, _, model| {
            captured
                .lock()
                .unwrap()
                .push((model.id.clone(), model.base_url.clone()));
            Ok(pa_ai::faux::faux_assistant_text_message(
                "ok",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
        }));
        registration.set_responses(vec![factory.clone(), factory]);
        let model = registration.get_model();
        let stream_fn =
            switchable_stream_fn(Arc::new(std::sync::RwLock::new(Some(ProviderTarget {
                api_key: Some("test-only".into()),
                model: model.clone(),
                service_tier: None,
                headers: None,
            }))));
        let main: AgentModel = json_round_trip(&model).unwrap();
        let mut auxiliary = main.clone();
        auxiliary.id = "auxiliary".into();
        auxiliary.base_url = "http://127.0.0.1:1/auxiliary".into();
        stream_fn(main, LlmContext::default(), StreamRequestOptions::default())
            .await
            .unwrap()
            .result()
            .await
            .unwrap();
        stream_fn(
            auxiliary.clone(),
            LlmContext::default(),
            StreamRequestOptions {
                api_key: Some("aux-test-only".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .result()
        .await
        .unwrap();
        assert_eq!(
            *received.lock().unwrap(),
            vec![
                (model.id, model.base_url),
                (auxiliary.id, auxiliary.base_url)
            ]
        );
        registration.unregister();
    }

    #[test]
    fn prompt_text_message_round_trips() {
        let message = pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::User(
            pa_agent::types::UserMessage {
                content: pa_agent::types::UserContent::Parts(vec![
                    pa_agent::types::UserPart::Text(pa_agent::types::TextContent {
                        text: "reply with ok".into(),
                        text_signature: None,
                    }),
                ]),
                timestamp: 1,
            },
        ));
        let converted: Option<pa_types::ai::Message> = json_round_trip(&message);
        assert_eq!(
            converted,
            Some(pa_types::ai::Message::User(pa_types::ai::UserMessage {
                content: pa_types::ai::UserContent::Blocks(vec![
                    pa_types::ai::UserContentBlock::Text(pa_types::ai::TextContent {
                        text: "reply with ok".into(),
                        text_signature: None,
                        rest: serde_json::Map::default(),
                    }),
                ]),
                timestamp: 1,
                rest: serde_json::Map::default(),
            }))
        );
    }

    /// Regression guard: provider signatures must survive the round-trip. pa-agent has no
    /// catch-all field, so a key-casing mismatch silently dropped them — a Rust-written
    /// session lost the signature TS-written ones carry.
    #[test]
    fn provider_signatures_round_trip_both_directions() {
        let thinking = pa_types::ai::ThinkingContent {
            thinking: "trace".into(),
            thinking_signature: Some("sig-1".into()),
            redacted: None,
            rest: serde_json::Map::default(),
        };
        let wire = serde_json::to_value(&thinking).unwrap();
        assert_eq!(
            wire.get("thinkingSignature").and_then(|v| v.as_str()),
            Some("sig-1"),
            "the TS wire key is camelCase: {wire}"
        );
        // pa-ai stream output -> the pa-agent loop's message form.
        let agent_thinking: pa_agent::types::ThinkingContent =
            serde_json::from_value(wire).unwrap();
        assert_eq!(agent_thinking.thinking_signature.as_deref(), Some("sig-1"));
        // The loop's message -> the provider-facing pa-ai form again.
        let back: pa_types::ai::ThinkingContent =
            serde_json::from_value(serde_json::to_value(&agent_thinking).unwrap()).unwrap();
        assert_eq!(back.thinking_signature.as_deref(), Some("sig-1"));
        // The tool-call thought signature (Google) rides the same boundary.
        let tool_call = pa_types::ai::ToolCall {
            id: "toolu_1".into(),
            name: "bash".into(),
            arguments: serde_json::Map::new(),
            thought_signature: Some("sig-2".into()),
            rest: serde_json::Map::default(),
        };
        let wire = serde_json::to_value(&tool_call).unwrap();
        assert_eq!(
            wire.get("thoughtSignature").and_then(|v| v.as_str()),
            Some("sig-2"),
            "the TS wire key is camelCase: {wire}"
        );
        let agent_tool_call: pa_agent::types::ToolCall = serde_json::from_value(wire).unwrap();
        assert_eq!(agent_tool_call.thought_signature.as_deref(), Some("sig-2"));
    }

    /// A delayed faux response holds the request mid-wait; `ModelStream::close` cancels the fetch
    /// NOW, so the stream settles on the aborted message instead of waiting out the hold.
    #[tokio::test]
    async fn closing_the_stream_cancels_a_held_fetch_immediately() {
        let registration =
            pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
                api: Some("held-fetch-close-test".to_string()),
                ..Default::default()
            });
        let model = registration.get_model();
        registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Delayed {
            message: pa_ai::faux::faux_assistant_text_message(
                "held reply",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ),
            delay_ms: 60_000,
        }]);
        let target = Arc::new(std::sync::RwLock::new(Some(ProviderTarget {
            api_key: None,
            model: model.clone(),
            service_tier: None,
            headers: None,
        })));
        let stream_fn = switchable_stream_fn(target);
        let mut stream = stream_fn(
            pa_agent::types::Model::unknown(),
            LlmContext::default(),
            StreamRequestOptions::default(),
        )
        .await
        .expect("stream start");
        // Abort the turn mid-wait.
        stream.close();
        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), stream.result())
            .await
            .expect("the closed stream settles immediately, not after the 60s hold")
            .expect("stream result");
        assert_eq!(settled.stop_reason, pa_agent::types::StopReason::Aborted);
        assert_eq!(
            settled.error_message.as_deref(),
            Some("Request was aborted")
        );
        // No usage on a mid-wait abort (TS `EMPTY_USAGE`: no partial
        // message ever streamed).
        let usage = settled.usage;
        assert_eq!(usage.total_tokens, 0);
        assert_eq!(usage.input, 0);
        assert_eq!(usage.output, 0);
        assert_eq!(usage.cost.total, 0.0);
        registration.unregister();
    }
}
