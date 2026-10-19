//! Invocation-owned native inference and explicit HTTP transport capabilities.
//!
//! No credentials, provider targets, or URLs are discovered. Handles are fenced by
//! session, plugin and generation. Their opening invocation owns cancellation.
//! Reading pulls one item at a time; this adapter does not spawn an event pump.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use futures::{Stream, StreamExt};
use pa_agent::stream::{LlmContext, ModelStream, StreamFn, StreamRequestOptions};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::provider_adapter::{stream_once, ProviderTarget};
use crate::plugins::{CapabilityFuture, CapabilityRequest, Contribution, HostCapabilities};

const MAX_HANDLES: usize = 32;
const MAX_DELEGATES: usize = 32;
const MAX_VALUE_BYTES: usize = 8 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_HTTP_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct Owner {
    session: String,
    plugin: String,
    generation: Uuid,
    invocation: Option<Uuid>,
}

impl From<&CapabilityRequest> for Owner {
    fn from(request: &CapabilityRequest) -> Self {
        Self {
            session: request.session.clone(),
            plugin: request.plugin.clone(),
            generation: request.generation,
            invocation: Some(request.invocation),
        }
    }
}

struct Delegate {
    owner: Owner,
    target: ProviderTarget,
    context: LlmContext,
    options: StreamRequestOptions,
    native: StreamFn,
    native_input: Option<pa_ai::compaction::NativeResponsesInput>,
    cancel: CancellationToken,
}

/// Keeps the typed original request alive without routing it through Lua JSON.
/// Dropping the lease revokes the request and every stream opened from it.
pub struct DelegateLease {
    id: String,
    shared: Weak<Shared>,
    cancel: CancellationToken,
}

impl DelegateLease {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for DelegateLease {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(shared) = self.shared.upgrade() {
            shared
                .delegates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.id);
        }
    }
}

struct Entry {
    owner: Owner,
    cancel: CancellationToken,
    stream: tokio::sync::Mutex<Transport>,
    is_inference: bool,
}

enum Transport {
    Inference(Box<dyn ModelStream>),
    Http(HttpStream),
}

impl Transport {
    fn close(&mut self) {
        if let Self::Inference(stream) = self {
            stream.close();
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.close();
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Shared {
    session: String,
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
    delegates: Mutex<BTreeMap<String, Arc<Delegate>>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        for entry in self
            .entries
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            entry.cancel.cancel();
        }
        for delegate in self
            .delegates
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            delegate.cancel.cancel();
        }
    }
}

/// Session-local backend for `native.inference.*` and `native.http.*`.
/// HTTP requires an explicit URL and headers; redirects and ambient proxies are disabled.
/// Native inference goes directly to the adapter, never back through plugin routing.
#[derive(Clone)]
pub struct InferenceCapabilities {
    shared: Arc<Shared>,
}

impl InferenceCapabilities {
    #[must_use]
    pub fn new(session: String) -> Self {
        Self {
            shared: Arc::new(Shared {
                session,
                entries: Mutex::default(),
                delegates: Mutex::default(),
            }),
        }
    }

    /// Bind a typed request to exactly one contribution. The returned guard must
    /// outlive its plugin invocation. No authentication lookup occurs here.
    ///
    /// # Errors
    /// Returns an error when too many request leases remain live.
    pub fn prepare_delegate(
        &self,
        contribution: &Contribution,
        target: ProviderTarget,
        context: LlmContext,
        options: StreamRequestOptions,
        native: StreamFn,
    ) -> Result<DelegateLease, String> {
        let mut delegates = self
            .shared
            .delegates
            .lock()
            .map_err(|_| "delegate lock poisoned")?;
        if delegates.len() >= MAX_DELEGATES {
            return Err("too many native request leases".into());
        }
        let id = Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();
        delegates.insert(
            id.clone(),
            Arc::new(Delegate {
                owner: Owner {
                    session: self.shared.session.clone(),
                    plugin: contribution.plugin.clone(),
                    generation: contribution.generation,
                    invocation: None,
                },
                target,
                context,
                options,
                native,
                native_input: super::provider_adapter::current_native_input(),
                cancel: cancel.clone(),
            }),
        );
        Ok(DelegateLease {
            id,
            shared: Arc::downgrade(&self.shared),
            cancel,
        })
    }

    fn insert(
        &self,
        owner: Owner,
        cancel: CancellationToken,
        transport: Transport,
    ) -> Result<String, String> {
        let entry = Arc::new(Entry {
            owner,
            cancel: cancel.clone(),
            is_inference: matches!(transport, Transport::Inference(_)),
            stream: tokio::sync::Mutex::new(transport),
        });
        let id = Uuid::new_v4().to_string();
        {
            let mut entries = self
                .shared
                .entries
                .lock()
                .map_err(|_| "stream lock poisoned")?;
            entries.retain(|_, entry| !entry.cancel.is_cancelled());
            if entries.len() >= MAX_HANDLES {
                return Err("too many native streams".into());
            }
            entries.insert(id.clone(), entry.clone());
        }
        // The cancellation watcher owns no session reference. While the plugin is
        // idle, invocation cancellation still drops sockets and closes inference.
        let shared = Arc::downgrade(&self.shared);
        let watch_id = id.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            if let Some(shared) = shared.upgrade() {
                shared
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&watch_id);
            }
            entry.stream.lock().await.close();
        });
        Ok(id)
    }

    #[tracing::instrument(skip_all)]
    async fn execute(
        &self,
        request: CapabilityRequest,
        cancellation: CancellationToken,
    ) -> Result<Value, String> {
        if request.session != self.shared.session {
            return Err("native capability session mismatch".into());
        }
        if cancellation.is_cancelled() {
            return Err("native capability cancelled".into());
        }
        if serde_json::to_vec(&request.input)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_VALUE_BYTES
        {
            return Err("native capability input exceeds limit".into());
        }
        let owner = Owner::from(&request);
        match request.operation.as_str() {
            "native.json.decode" => {
                let input: JsonDecode =
                    serde_json::from_value(request.input).map_err(|error| error.to_string())?;
                if input.text.len() > MAX_VALUE_BYTES {
                    return Err("native JSON text exceeds limit".into());
                }
                serde_json::from_str(&input.text)
                    .map_err(|error| format!("invalid native JSON: {error}"))
            }
            "native.json.encode" => {
                let input: JsonEncode =
                    serde_json::from_value(request.input).map_err(|error| error.to_string())?;
                let text =
                    serde_json::to_string(&input.value).map_err(|error| error.to_string())?;
                if text.len() > MAX_VALUE_BYTES {
                    return Err("native JSON text exceeds limit".into());
                }
                Ok(Value::String(text))
            }
            "native.inference.open" => {
                let input: InferenceOpen =
                    serde_json::from_value(request.input).map_err(|error| error.to_string())?;
                let explicit_target = input.model.is_some()
                    || input.api_key.is_some()
                    || input.headers.is_some()
                    || input.service_tier.is_some();
                let delegate = if let Some(id) = input.request {
                    let mut delegates = self
                        .shared
                        .delegates
                        .lock()
                        .map_err(|_| "delegate lock poisoned")?;
                    let delegate = delegates.get(&id).ok_or("unknown native request lease")?;
                    if delegate.owner.session != owner.session
                        || delegate.owner.plugin != owner.plugin
                        || delegate.owner.generation != owner.generation
                        || delegate.cancel.is_cancelled()
                    {
                        return Err("native request lease owner mismatch or retired".into());
                    }
                    delegates.remove(&id)
                } else {
                    None
                };
                let native_input = delegate
                    .as_ref()
                    .and_then(|delegate| delegate.native_input.clone());
                if native_input.is_some()
                    && (input.model.is_some()
                        || input
                            .context
                            .as_ref()
                            .is_some_and(|context| !context.messages.is_empty()))
                {
                    return Err(
                        "native request lease forbids model overrides or portable context messages"
                            .into(),
                    );
                }
                let mut target = if let Some(delegate) = &delegate {
                    delegate.target.clone()
                } else {
                    ProviderTarget {
                        model: input
                            .model
                            .clone()
                            .ok_or("native inference requires explicit model or request lease")?,
                        api_key: None,
                        headers: None,
                        service_tier: None,
                    }
                };
                if let Some(model) = input.model {
                    target.model = model;
                }
                if let Some(api_key) = input.api_key {
                    target.api_key = Some(api_key);
                }
                if let Some(headers) = input.headers {
                    target.headers = Some(headers);
                }
                if let Some(tier) = input.service_tier {
                    target.service_tier = Some(tier);
                }
                let context = input
                    .context
                    .or_else(|| delegate.as_ref().map(|delegate| delegate.context.clone()))
                    .ok_or("native inference requires context")?;
                let mut options = delegate
                    .as_ref()
                    .map_or_else(StreamRequestOptions::default, |delegate| {
                        delegate.options.clone()
                    });
                if let Some(settings) = input.options {
                    if let Some(temperature) = settings.temperature {
                        options.temperature = Some(temperature);
                    }
                    if let Some(max_tokens) = settings.max_tokens {
                        options.max_tokens = Some(max_tokens);
                    }
                    if let Some(reasoning) = settings.reasoning {
                        options.reasoning = reasoning;
                    }
                }
                options.session_id = Some(self.shared.session.clone());
                let cancel = cancellation.child_token();
                let cancel_on_error = cancel.clone().drop_guard();
                let native = delegate
                    .as_ref()
                    .filter(|_| !explicit_target)
                    .map(|delegate| delegate.native.clone());
                if let Some(delegate) = delegate {
                    let stream_cancel = cancel.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            () = delegate.cancel.cancelled() => stream_cancel.cancel(),
                            () = stream_cancel.cancelled() => {},
                        }
                    });
                }
                let opening = async {
                    let stream = if let Some(native) = native {
                        let model = super::provider_adapter::json_round_trip(&target.model)
                            .ok_or("native model conversion failed")?;
                        tokio::select! {
                            biased;
                            () = cancel.cancelled() => return Err("native request lease cancelled".into()),
                            stream = native(model, context, options) => stream.map_err(|error| error.to_string())?,
                        }
                    } else {
                        // The generic path has no permission to discover ambient credentials.
                        // These protocols discover credentials or replace the endpoint from environment configuration.
                        if matches!(
                            target.model.api.as_str(),
                            "bedrock-converse-stream" | "google-vertex" | "azure-openai-responses"
                        ) {
                            return Err("native explicit transport requires non-discovering authentication; use an original request lease".into());
                        }
                        stream_once(
                            &target.model,
                            Some(target.api_key.unwrap_or_default()),
                            target.service_tier,
                            target.headers,
                            context,
                            options,
                        )
                        .map_err(|error| error.to_string())?
                    };
                    Ok::<_, String>(stream)
                };
                let opening = super::provider_adapter::with_bounded_native_events(opening);
                let stream = if let Some(input) = native_input {
                    super::provider_adapter::with_native_input(input, opening).await?
                } else {
                    opening.await?
                };
                let id = self.insert(owner, cancel, Transport::Inference(stream))?;
                cancel_on_error.disarm();
                Ok(json!({"handle":id}))
            }
            "native.http.open" => {
                let input: HttpOpen =
                    serde_json::from_value(request.input).map_err(|error| error.to_string())?;
                let url = reqwest::Url::parse(&input.url).map_err(|error| error.to_string())?;
                if !matches!(url.scheme(), "http" | "https")
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return Err("HTTP URL must use http(s) without embedded credentials".into());
                }
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(120))
                    .build()
                    .map_err(|error| error.to_string())?;
                let method = reqwest::Method::from_bytes(input.method.as_bytes())
                    .map_err(|error| error.to_string())?;
                let mut outgoing = client.request(method, url);
                for (name, value) in input.headers {
                    outgoing = outgoing.header(name, value);
                }
                if let Some(body) = input.body {
                    outgoing = outgoing.json(&body);
                }
                let response = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err("native HTTP cancelled".into()),
                    response = outgoing.send() => response.map_err(|_| "native HTTP request failed")?,
                };
                let status = response.status().as_u16();
                let headers: BTreeMap<_, _> = response
                    .headers()
                    .iter()
                    .filter_map(|(name, value)| {
                        value
                            .to_str()
                            .ok()
                            .map(|value| (name.as_str().to_owned(), value.to_owned()))
                    })
                    .collect();
                let stream = HttpStream {
                    body: Box::pin(response.bytes_stream().map(|result| {
                        result
                            .map(|bytes| bytes.to_vec())
                            .map_err(|_| "native HTTP body failed".into())
                    })),
                    pending: Vec::new(),
                    frame: SseFrame::default(),
                    format: input.format,
                    eof: false,
                    total: 0,
                };
                let id = self.insert(owner, cancellation.child_token(), Transport::Http(stream))?;
                Ok(json!({"handle":id,"status":status,"headers":headers}))
            }
            "native.inference.next"
            | "native.inference.result"
            | "native.inference.close"
            | "native.http.next"
            | "native.http.close" => {
                let input: HandleInput =
                    serde_json::from_value(request.input).map_err(|error| error.to_string())?;
                let entry = self
                    .shared
                    .entries
                    .lock()
                    .map_err(|_| "stream lock poisoned")?
                    .get(&input.handle)
                    .cloned()
                    .ok_or("unknown native stream handle")?;
                if entry.owner != owner {
                    return Err("native stream owner mismatch".into());
                }
                let inference = request.operation.starts_with("native.inference.");
                if inference != entry.is_inference {
                    return Err("native stream transport mismatch".into());
                }
                if matches!(
                    request.operation.as_str(),
                    "native.inference.close" | "native.http.close"
                ) {
                    entry.cancel.cancel();
                    return Ok(Value::Null);
                }
                let mut stream = entry
                    .stream
                    .try_lock()
                    .map_err(|_| "native stream already has an active reader")?;
                // Dropping an in-flight read must close its transport, even when the
                // caller drops our future instead of cancelling its supplied token.
                let guard = entry.cancel.clone().drop_guard();
                let output = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => Err("native stream read cancelled".into()),
                    () = entry.cancel.cancelled() => Err("native stream lease cancelled".into()),
                    output = async {
                        match &mut *stream {
                            Transport::Inference(stream) if request.operation.ends_with(".result") => {
                                serde_json::to_value(stream.result().await.map_err(|error| error.to_string())?).map_err(|error| error.to_string())
                            }
                            Transport::Inference(stream) => {
                                let event = stream.next_event().await.map(event_value).transpose()?;
                                Ok(json!({"event":event}))
                            }
                            Transport::Http(stream) => Ok(json!({"event":stream.next().await?})),
                        }
                    } => output,
                };
                if cancellation.is_cancelled() || entry.cancel.is_cancelled() {
                    return Err("native stream lease cancelled".into());
                }
                let output = output?;
                if serde_json::to_vec(&output)
                    .map_err(|error| error.to_string())?
                    .len()
                    > MAX_VALUE_BYTES
                {
                    return Err("native stream output exceeds limit".into());
                }
                guard.disarm();
                Ok(output)
            }
            _ => Err(format!(
                "unsupported native capability {}",
                request.operation
            )),
        }
    }
}

impl HostCapabilities for InferenceCapabilities {
    fn call(
        &self,
        request: CapabilityRequest,
        cancellation: CancellationToken,
    ) -> CapabilityFuture {
        let backend = self.clone();
        Box::pin(async move { backend.execute(request, cancellation).await })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InferenceOpen {
    request: Option<String>,
    model: Option<pa_types::ai::Model>,
    #[serde(alias = "api_key")]
    api_key: Option<String>,
    service_tier: Option<pa_types::ai::ServiceTier>,
    headers: Option<BTreeMap<String, String>>,
    context: Option<LlmContext>,
    options: Option<InferenceOptions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InferenceOptions {
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    #[serde(default)]
    reasoning: Option<pa_agent::types::ThinkingLevel>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonDecode {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonEncode {
    value: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandleInput {
    handle: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpOpen {
    url: String,
    #[serde(default = "post_method")]
    method: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    body: Option<Value>,
    format: HttpFormat,
}
fn post_method() -> String {
    "POST".into()
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum HttpFormat {
    Json,
    Sse,
}

// The boxed stream is the dynamic reqwest transport seam. Chunks remain pull-based.
type Body = std::pin::Pin<Box<dyn Stream<Item = Result<Vec<u8>, String>> + Send>>;
struct HttpStream {
    body: Body,
    pending: Vec<u8>,
    frame: SseFrame,
    format: HttpFormat,
    eof: bool,
    total: usize,
}

#[derive(Default)]
struct SseFrame {
    data: Vec<String>,
    event: Option<String>,
    id: Option<String>,
    bytes: usize,
}

impl SseFrame {
    fn finish(&mut self) -> Option<Value> {
        let frame = std::mem::take(self);
        if frame.data.is_empty() {
            return None;
        }
        Some(json!({"data":frame.data.join("\n"),"event":frame.event,"id":frame.id}))
    }
}

impl HttpStream {
    async fn next(&mut self) -> Result<Option<Value>, String> {
        loop {
            match self.format {
                HttpFormat::Json if self.eof => {
                    if self.pending.is_empty() {
                        return Ok(None);
                    }
                    let bytes = std::mem::take(&mut self.pending);
                    return serde_json::from_slice(&bytes)
                        .map(Some)
                        .map_err(|error| format!("invalid HTTP JSON: {error}"));
                }
                HttpFormat::Sse => {
                    // Handle CR, LF and CRLF, including a CRLF split across chunks.
                    while let Some(index) = self
                        .pending
                        .iter()
                        .position(|byte| matches!(byte, b'\r' | b'\n'))
                    {
                        if self.pending[index] == b'\r'
                            && index + 1 == self.pending.len()
                            && !self.eof
                        {
                            break;
                        }
                        let width = if self.pending[index] == b'\r'
                            && self.pending.get(index + 1) == Some(&b'\n')
                        {
                            2
                        } else {
                            1
                        };
                        let line = String::from_utf8(self.pending[..index].to_vec())
                            .map_err(|_| "invalid HTTP SSE UTF-8")?;
                        self.pending.drain(..index + width);
                        if line.is_empty() {
                            if let Some(frame) = self.frame.finish() {
                                return Ok(Some(frame));
                            }
                        } else {
                            self.frame.bytes += line.len();
                            if self.frame.bytes > MAX_FRAME_BYTES {
                                return Err("HTTP SSE frame exceeds limit".into());
                            }
                            let (field, value) = line.split_once(':').unwrap_or((&line, ""));
                            let value = value.strip_prefix(' ').unwrap_or(value);
                            match field {
                                "data" => self.frame.data.push(value.to_owned()),
                                "event" => self.frame.event = Some(value.to_owned()),
                                "id" if !value.contains('\0') => {
                                    self.frame.id = Some(value.to_owned());
                                }
                                _ => {}
                            }
                        }
                    }
                    if self.pending.len() > MAX_FRAME_BYTES {
                        return Err("HTTP SSE line exceeds limit".into());
                    }
                    // SSE dispatch requires a blank line. An incomplete final event is discarded.
                    if self.eof {
                        return Ok(None);
                    }
                }
                HttpFormat::Json => {}
            }
            match self.body.next().await {
                Some(Ok(chunk)) => {
                    self.total = self
                        .total
                        .checked_add(chunk.len())
                        .ok_or("HTTP body exceeds limit")?;
                    if self.total > MAX_HTTP_BYTES {
                        return Err("HTTP body exceeds limit".into());
                    }
                    if matches!(self.format, HttpFormat::Json) && self.total > MAX_VALUE_BYTES {
                        return Err("HTTP JSON exceeds limit".into());
                    }
                    self.pending.extend(chunk);
                }
                Some(Err(error)) => return Err(error),
                None => self.eof = true,
            }
        }
    }
}

fn event_value(event: pa_agent::stream::AssistantMessageEvent) -> Result<Value, String> {
    use pa_agent::stream::AssistantMessageEvent as E;
    let value = match event {
        E::Start { partial } => json!({"type":"start","partial":partial}),
        E::TextStart {
            content_index,
            partial,
        } => json!({"type":"text_start","contentIndex":content_index,"partial":partial}),
        E::TextDelta {
            content_index,
            delta,
            partial,
        } => {
            json!({"type":"text_delta","contentIndex":content_index,"delta":delta,"partial":partial})
        }
        E::TextEnd {
            content_index,
            content,
            partial,
        } => {
            json!({"type":"text_end","contentIndex":content_index,"content":content,"partial":partial})
        }
        E::ThinkingStart {
            content_index,
            partial,
        } => json!({"type":"thinking_start","contentIndex":content_index,"partial":partial}),
        E::ThinkingDelta {
            content_index,
            delta,
            partial,
        } => {
            json!({"type":"thinking_delta","contentIndex":content_index,"delta":delta,"partial":partial})
        }
        E::ThinkingEnd {
            content_index,
            partial,
        } => {
            let content = serde_json::to_value(&partial)
                .map_err(|error| error.to_string())?
                .get("content")
                .and_then(|content| content.get(content_index))
                .and_then(|content| content.get("thinking"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            json!({"type":"thinking_end","contentIndex":content_index,"content":content,"partial":partial})
        }
        E::ToolCallStart {
            content_index,
            partial,
        } => json!({"type":"toolcall_start","contentIndex":content_index,"partial":partial}),
        E::ToolCallDelta {
            content_index,
            delta,
            partial,
        } => {
            json!({"type":"toolcall_delta","contentIndex":content_index,"delta":delta,"partial":partial})
        }
        E::ToolCallEnd {
            content_index,
            tool_call,
            partial,
        } => {
            json!({"type":"toolcall_end","contentIndex":content_index,"toolCall":tool_call,"partial":partial})
        }
        E::Done { reason, message } => json!({"type":"done","reason":reason,"message":message}),
        E::Error { reason, error } => json!({"type":"error","reason":reason,"error":error}),
    };
    // Validate the public DTO, not a private lookalike event protocol.
    let event: pa_types::ai::AssistantMessageEvent =
        serde_json::from_value(value).map_err(|error| error.to_string())?;
    serde_json::to_value(event).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{ContributionKind, InvocationMode};
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn contribution() -> Contribution {
        Contribution {
            plugin: "delegate".into(),
            generation: Uuid::new_v4(),
            kind: ContributionKind::Export,
            name: "test".into(),
            description: String::new(),
            mode: InvocationMode::Streaming,
            context_data: crate::plugins::ContextDataMode::Json,
        }
    }

    fn request(owner: &Contribution, operation: &str, input: Value) -> CapabilityRequest {
        CapabilityRequest {
            session: "session".into(),
            plugin: owner.plugin.clone(),
            generation: owner.generation,
            invocation: owner.generation,
            operation: operation.into(),
            input,
        }
    }

    fn message() -> pa_agent::types::AssistantMessage {
        super::super::provider_adapter::json_round_trip(&pa_ai::faux::faux_assistant_text_message(
            "ok",
            pa_ai::faux::FauxAssistantMessageOptions {
                timestamp: Some(1),
                ..Default::default()
            },
        ))
        .unwrap()
    }

    struct PullStream {
        pulls: Arc<AtomicUsize>,
        closed: CancellationToken,
        held: bool,
    }
    impl ModelStream for PullStream {
        fn next_event(
            &mut self,
        ) -> pa_agent::BoxFut<'_, Option<pa_agent::stream::AssistantMessageEvent>> {
            self.pulls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if self.held {
                    std::future::pending::<()>().await;
                }
                Some(pa_agent::stream::AssistantMessageEvent::Start { partial: message() })
            })
        }
        fn result(
            &mut self,
        ) -> pa_agent::BoxFut<'_, anyhow::Result<pa_agent::types::AssistantMessage>> {
            Box::pin(async { Ok(message()) })
        }
        fn close(&mut self) {
            self.closed.cancel();
        }
    }

    fn target() -> ProviderTarget {
        ProviderTarget {
            model: super::super::provider_adapter::native_model(&pa_agent::types::Model::unknown())
                .unwrap(),
            api_key: None,
            headers: None,
            service_tier: None,
        }
    }

    fn native(pulls: Arc<AtomicUsize>, closed: CancellationToken, held: bool) -> StreamFn {
        Arc::new(move |_, _, _| {
            let stream: Box<dyn ModelStream> = Box::new(PullStream {
                pulls: pulls.clone(),
                closed: closed.clone(),
                held,
            });
            Box::pin(async { Ok(stream) })
        })
    }

    #[tokio::test]
    async fn json_helpers_parse_sse_data_and_reject_malformed_input() {
        let backend = InferenceCapabilities::new("session".into());
        let owner = contribution();
        let value = json!({"delta":"wire ✓","nested":[1,true,null]});
        let text = backend
            .call(
                request(&owner, "native.json.encode", json!({"value":value})),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let (url, job) = server(
            format!("data: {}\n\n", text.as_str().unwrap()).into_bytes(),
            "text/event-stream",
        )
        .await;
        let opened = backend
            .call(
                request(
                    &owner,
                    "native.http.open",
                    json!({"url":url,"format":"sse"}),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let next = backend
            .call(
                request(
                    &owner,
                    "native.http.next",
                    json!({"handle":opened["handle"]}),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            backend
                .call(
                    request(
                        &owner,
                        "native.json.decode",
                        json!({"text":next["event"]["data"]})
                    ),
                    CancellationToken::new()
                )
                .await
                .unwrap(),
            value
        );
        assert!(backend
            .call(
                request(&owner, "native.json.decode", json!({"text":"{broken"})),
                CancellationToken::new()
            )
            .await
            .is_err());
        backend
            .call(
                request(
                    &owner,
                    "native.http.close",
                    json!({"handle":opened["handle"]}),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        job.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_native_open_streams_real_faux_provider() {
        let registration =
            pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
                api: Some("capability-native-faux-test".into()),
                ..Default::default()
            });
        registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
            pa_ai::faux::faux_assistant_text_message(
                "real faux",
                pa_ai::faux::FauxAssistantMessageOptions {
                    timestamp: Some(1),
                    ..Default::default()
                },
            ),
        )]);
        let backend = InferenceCapabilities::new("session".into());
        let owner = contribution();
        let invocation = CancellationToken::new();
        let opened = backend.call(request(&owner, "native.inference.open", json!({"model":registration.get_model(),"apiKey":"explicit-only","context":LlmContext::default()})), invocation.clone()).await.unwrap();
        let work = async {
            loop {
                let next = backend
                    .call(
                        request(&owner, "native.inference.next", opened.clone()),
                        invocation.child_token(),
                    )
                    .await
                    .unwrap();
                if next["event"]["type"] == "done" {
                    break;
                }
                assert!(!next["event"].is_null());
            }
            let result = backend
                .call(
                    request(&owner, "native.inference.result", opened),
                    invocation.child_token(),
                )
                .await
                .unwrap();
            assert_eq!(result["content"][0]["text"], "real faux");
        };
        tokio::time::timeout(Duration::from_secs(3), work)
            .await
            .unwrap();
        invocation.cancel();
        registration.unregister();
    }

    #[tokio::test]
    async fn raw_delegate_allows_current_instructions_but_not_portable_messages_or_model_changes() {
        use pa_ai::compaction::{
            CompactionLimits, CompactionRoute, ResponsesCompactor, ResponsesWindow,
        };
        let route = CompactionRoute {
            provider: "test".into(),
            model: "test".into(),
            endpoint: "http://localhost:1/responses/compact".into(),
        };
        let adapter = ResponsesCompactor::new(
            route.clone(),
            Some(route.clone()),
            CompactionLimits::default(),
        )
        .unwrap();
        let raw = ResponsesWindow::from_stateless_input(
            &route,
            r#"[{"type":"compaction","encrypted_content":"opaque=="}]"#,
            CompactionLimits::default(),
        )
        .unwrap()
        .inference_input(&adapter, &CancellationToken::new())
        .unwrap();
        let backend = InferenceCapabilities::new("session".into());
        let owner = contribution();
        let instructions = LlmContext {
            system_prompt: Some("current instructions".into()),
            ..LlmContext::default()
        };
        let mut portable = instructions.clone();
        portable.messages.push(pa_agent::types::Message::User(
            pa_agent::types::UserMessage {
                content: pa_agent::types::UserContent::Text("portable".into()),
                timestamp: 1,
            },
        ));
        for (context, model) in [
            (instructions, None),
            (portable, None),
            (LlmContext::default(), Some(target().model)),
        ] {
            let allowed = context.messages.is_empty() && model.is_none();
            let lease = super::super::provider_adapter::with_native_input(raw.clone(), async {
                backend.prepare_delegate(
                    &owner,
                    target(),
                    LlmContext::default(),
                    StreamRequestOptions::default(),
                    native(
                        Arc::new(AtomicUsize::new(0)),
                        CancellationToken::new(),
                        /*held*/ false,
                    ),
                )
            })
            .await
            .unwrap();
            let mut input = json!({"request":lease.id(),"context":context});
            if let Some(model) = model {
                input["model"] = serde_json::to_value(model).unwrap();
            }
            let opened = backend
                .call(
                    request(&owner, "native.inference.open", input),
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(opened.is_ok(), allowed);
        }
    }

    #[tokio::test]
    async fn typed_delegate_is_single_use_fenced_and_pull_based() {
        let backend = InferenceCapabilities::new("session".into());
        let owner = contribution();
        let pulls = Arc::new(AtomicUsize::new(0));
        let closed = CancellationToken::new();
        let lease = backend
            .prepare_delegate(
                &owner,
                target(),
                LlmContext::default(),
                StreamRequestOptions::default(),
                native(pulls.clone(), closed.clone(), false),
            )
            .unwrap();
        let input = json!({"request":lease.id()});
        let mut foreign = owner.clone();
        foreign.generation = Uuid::new_v4();
        assert!(backend
            .call(
                request(&foreign, "native.inference.open", input.clone()),
                CancellationToken::new()
            )
            .await
            .is_err());
        let opened = backend
            .call(
                request(&owner, "native.inference.open", input.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(pulls.load(Ordering::SeqCst), 0);
        assert!(backend
            .call(
                request(&owner, "native.inference.open", input),
                CancellationToken::new()
            )
            .await
            .is_err());
        let handle = json!({"handle":opened["handle"]});
        let mut wrong_session = request(&owner, "native.inference.next", handle.clone());
        wrong_session.session = "another".into();
        assert!(backend
            .call(wrong_session, CancellationToken::new())
            .await
            .is_err());
        assert!(backend
            .call(
                request(&foreign, "native.inference.next", handle.clone()),
                CancellationToken::new()
            )
            .await
            .is_err());
        let mut other_invocation = request(&owner, "native.inference.next", handle.clone());
        other_invocation.invocation = Uuid::new_v4();
        assert!(backend
            .call(other_invocation, CancellationToken::new())
            .await
            .is_err());
        let next = backend
            .call(
                request(&owner, "native.inference.next", handle.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(pulls.load(Ordering::SeqCst), 1);
        let event: pa_types::ai::AssistantMessageEvent =
            serde_json::from_value(next["event"].clone()).unwrap();
        assert!(matches!(
            event,
            pa_types::ai::AssistantMessageEvent::Start { .. }
        ));
        assert_eq!(
            backend
                .call(
                    request(&owner, "native.inference.result", handle.clone()),
                    CancellationToken::new()
                )
                .await
                .unwrap(),
            serde_json::to_value(message()).unwrap()
        );
        drop(lease);
        tokio::time::timeout(Duration::from_secs(2), closed.cancelled())
            .await
            .unwrap();
        assert!(backend
            .call(
                request(&owner, "native.inference.next", handle),
                CancellationToken::new()
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn dropping_a_read_future_cancels_its_open_transport() {
        let backend = InferenceCapabilities::new("session".into());
        let owner = contribution();
        let pulls = Arc::new(AtomicUsize::new(0));
        let closed = CancellationToken::new();
        let lease = backend
            .prepare_delegate(
                &owner,
                target(),
                LlmContext::default(),
                StreamRequestOptions::default(),
                native(pulls.clone(), closed.clone(), true),
            )
            .unwrap();
        let opened = backend
            .call(
                request(
                    &owner,
                    "native.inference.open",
                    json!({"request":lease.id()}),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut read = backend.call(
            request(&owner, "native.inference.next", opened),
            CancellationToken::new(),
        );
        assert!(futures::poll!(&mut read).is_pending());
        assert_eq!(pulls.load(Ordering::SeqCst), 1);
        drop(read);
        tokio::time::timeout(Duration::from_secs(2), closed.cancelled())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn close_interrupts_an_active_reader_and_session_drop_closes_idle_streams() {
        let backend = InferenceCapabilities::new("session".into());
        let owner = contribution();
        let closed = CancellationToken::new();
        let lease = backend
            .prepare_delegate(
                &owner,
                target(),
                LlmContext::default(),
                StreamRequestOptions::default(),
                native(Arc::new(AtomicUsize::new(0)), closed.clone(), true),
            )
            .unwrap();
        let opened = backend
            .call(
                request(
                    &owner,
                    "native.inference.open",
                    json!({"request":lease.id()}),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut read = backend.call(
            request(&owner, "native.inference.next", opened.clone()),
            CancellationToken::new(),
        );
        assert!(futures::poll!(&mut read).is_pending());
        backend
            .call(
                request(&owner, "native.inference.close", opened),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(read.await.is_err());
        tokio::time::timeout(Duration::from_secs(2), closed.cancelled())
            .await
            .unwrap();

        let closed = CancellationToken::new();
        let lease = backend
            .prepare_delegate(
                &owner,
                target(),
                LlmContext::default(),
                StreamRequestOptions::default(),
                native(Arc::new(AtomicUsize::new(0)), closed.clone(), false),
            )
            .unwrap();
        backend
            .call(
                request(
                    &owner,
                    "native.inference.open",
                    json!({"request":lease.id()}),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        drop(backend);
        tokio::time::timeout(Duration::from_secs(2), closed.cancelled())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn invocation_cancellation_closes_idle_inference() {
        let backend = InferenceCapabilities::new("session".into());
        let owner = contribution();
        let closed = CancellationToken::new();
        let lease = backend
            .prepare_delegate(
                &owner,
                target(),
                LlmContext::default(),
                StreamRequestOptions::default(),
                native(Arc::new(AtomicUsize::new(0)), closed.clone(), false),
            )
            .unwrap();
        let invocation = CancellationToken::new();
        backend
            .call(
                request(
                    &owner,
                    "native.inference.open",
                    json!({"request":lease.id()}),
                ),
                invocation.clone(),
            )
            .await
            .unwrap();
        invocation.cancel();
        tokio::time::timeout(Duration::from_secs(2), closed.cancelled())
            .await
            .unwrap();
    }

    async fn server(
        body: Vec<u8>,
        content_type: &str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/explicit", listener.local_addr().unwrap());
        let content_type = content_type.to_owned();
        let job = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut input = Vec::new();
            let mut buf = [0; 1024];
            loop {
                let count = socket.read(&mut buf).await.unwrap();
                if count == 0 {
                    break;
                }
                input.extend_from_slice(&buf[..count]);
                if input.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
            }
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
            socket.shutdown().await.unwrap();
            String::from_utf8(input).unwrap()
        });
        (url, job)
    }

    #[tokio::test]
    async fn explicit_http_json_and_sse_cross_host_capabilities() {
        let backend = InferenceCapabilities::new("session".into());
        let owner = contribution();
        for (format, content_type, body, expected) in [
            (
                "json",
                "application/json",
                b"{\"ok\":true}".to_vec(),
                json!({"ok":true}),
            ),
            (
                "sse",
                "text/event-stream",
                b":comment\r\nevent: delta\r\nid: 7\r\ndata: first\r\ndata: second\r\n\r\n"
                    .to_vec(),
                json!({"event":"delta","id":"7","data":"first\nsecond"}),
            ),
        ] {
            let (url, job) = server(body, content_type).await;
            let opened = backend.call(request(&owner, "native.http.open", json!({"url":url,"method":"GET","headers":{"x-explicit":"only"},"format":format})), CancellationToken::new()).await.unwrap();
            let handle = json!({"handle":opened["handle"]});
            assert_eq!(opened["status"], 200);
            let next = backend
                .call(
                    request(&owner, "native.http.next", handle.clone()),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(next, json!({"event":expected}));
            assert_eq!(
                backend
                    .call(
                        request(&owner, "native.http.next", handle.clone()),
                        CancellationToken::new()
                    )
                    .await
                    .unwrap(),
                json!({"event":null})
            );
            backend
                .call(
                    request(&owner, "native.http.close", handle),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(job
                .await
                .unwrap()
                .to_lowercase()
                .contains("x-explicit: only"));
        }
    }

    #[tokio::test]
    async fn oversized_sse_frame_errors_and_revokes_handle() {
        let backend = InferenceCapabilities::new("session".into());
        let owner = contribution();
        let mut body = b"data: ".to_vec();
        body.extend(vec![b'x'; MAX_FRAME_BYTES + 1]);
        body.extend_from_slice(b"\n\n");
        let (url, job) = server(body, "text/event-stream").await;
        let opened = backend
            .call(
                request(
                    &owner,
                    "native.http.open",
                    json!({"url":url,"format":"sse"}),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let handle = json!({"handle":opened["handle"]});
        assert!(backend
            .call(
                request(&owner, "native.http.next", handle.clone()),
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .contains("exceeds limit"));
        assert!(backend
            .call(
                request(&owner, "native.http.next", handle),
                CancellationToken::new()
            )
            .await
            .is_err());
        job.await.unwrap();
    }

    #[tokio::test]
    async fn bounded_native_explicit_and_delegated_result_only_drain_more_than_sixteen_events() {
        let registration =
            pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
                api: Some("plugin-bounded-native-result-only".into()),
                token_size_min: Some(1),
                token_size_max: Some(1),
                ..Default::default()
            });
        let observed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&observed);
        let text = "token".repeat(64);
        let reply = text.clone();
        let response = pa_ai::faux::FauxResponseStep::Factory(Arc::new(move |_, options, _, _| {
            captured
                .lock()
                .unwrap()
                .push(options.and_then(|options| options.event_buffer_capacity));
            Ok(pa_ai::faux::faux_assistant_text_message(
                &reply,
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
        }));
        registration.set_responses(vec![response.clone(), response.clone(), response]);
        let model = registration.get_model();
        let backend = InferenceCapabilities::new("session".into());
        let owner = contribution();
        for mode in ["explicit", "delegate"] {
            let lease = if mode == "delegate" {
                Some(
                    backend
                        .prepare_delegate(
                            &owner,
                            ProviderTarget {
                                model: model.clone(),
                                api_key: Some("test-only".into()),
                                service_tier: None,
                                headers: None,
                            },
                            LlmContext::default(),
                            StreamRequestOptions::default(),
                            super::super::provider_adapter::real_stream_fn(
                                Some("test-only".into()),
                                model.clone(),
                            ),
                        )
                        .unwrap(),
                )
            } else {
                None
            };
            let input = lease.as_ref().map_or_else(
                || json!({"model":model,"apiKey":"test-only","context":LlmContext::default()}),
                |lease| json!({"request":lease.id()}),
            );
            let opened = backend
                .call(
                    request(&owner, "native.inference.open", input),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            // No next() call: ModelStream.result must consume the bounded core
            // bridge while the bounded pa-ai producer applies async backpressure.
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                backend.call(
                    request(&owner, "native.inference.result", opened.clone()),
                    CancellationToken::new(),
                ),
            )
            .await
            .expect("result-only must drain both sixteen-slot queues")
            .unwrap();
            assert_eq!(result["content"][0]["text"], text);
            backend
                .call(
                    request(&owner, "native.inference.close", opened),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            drop(lease);
        }
        let ordinary = super::super::provider_adapter::real_stream_fn(
            Some("ordinary-test-only".into()),
            model,
        );
        let mut stream = ordinary(
            pa_agent::types::Model::unknown(),
            LlmContext::default(),
            StreamRequestOptions::default(),
        )
        .await
        .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), stream.result())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.content.len(), 1);
        assert_eq!(*observed.lock().unwrap(), vec![Some(16), Some(16), None]);
        registration.unregister();
    }

    #[tokio::test]
    async fn bounded_native_result_only_drains_a_real_http_stream_larger_than_both_queues() {
        let mut body = String::new();
        for _ in 0..96 {
            let chunk = json!({"id":"bounded-local","choices":[{"index":0,"delta":{"content":"x"},"finish_reason":null}]});
            writeln!(body, "data: {chunk}\n").unwrap();
        }
        let done =
            json!({"id":"bounded-local","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
        writeln!(body, "data: {done}\n\ndata: [DONE]\n").unwrap();
        let (url, job) = server(body.into_bytes(), "text/event-stream").await;
        let mut model =
            super::super::provider_adapter::native_model(&pa_agent::types::Model::unknown())
                .unwrap();
        model.api = "openai-completions".into();
        model.provider = "bounded-local".into();
        model.base_url = format!("{}/v1", url.strip_suffix("/explicit").unwrap());
        let backend = InferenceCapabilities::new("session".into());
        let owner = contribution();
        let opened = backend
            .call(
                request(
                    &owner,
                    "native.inference.open",
                    json!({
                        "model":model,"apiKey":"local-only","context":LlmContext::default()
                    }),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            backend.call(
                request(&owner, "native.inference.result", opened.clone()),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("completion-only path must consume the core and native bounded queues")
        .unwrap();
        assert_eq!(result["content"][0]["text"], "x".repeat(96));
        assert_eq!(result["stopReason"], "stop");
        backend
            .call(
                request(&owner, "native.inference.close", opened),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(job
            .await
            .unwrap()
            .starts_with("POST /v1/chat/completions HTTP/1.1"));
    }
}
