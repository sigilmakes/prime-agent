//! Assistant message event stream: `start` first, then partial updates, terminating with exactly
//! one of `done` (success) or `error` (failure/abort). The event type is owned by `pa-types`, so
//! the helpers below are an extension trait.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll, Waker};

use futures::Stream;
use serde_json::Map;
use tokio_util::sync::CancellationToken;

use crate::utils_inner::stream_failure::ProviderError;

pub use crate::types::{AssistantContent, AssistantMessage, AssistantMessageEvent, StopReason};

/// Provider-side helpers over the shared [`AssistantMessageEvent`] wire enum.
pub trait AssistantMessageEventExt {
    /// Stable event-type name matching the TS wire `type` tag.
    fn event_type(&self) -> &'static str;
    fn is_terminal(&self) -> bool;
    fn terminal_message(&self) -> Option<AssistantMessage>;
    fn partial(&self) -> &AssistantMessage;
    fn content_index(&self) -> Option<u64>;
    fn delta(&self) -> Option<&str>;
}

impl AssistantMessageEventExt for AssistantMessageEvent {
    fn event_type(&self) -> &'static str {
        match self {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::ToolcallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolcallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolcallEnd { .. } => "toolcall_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(
            self,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        )
    }

    fn terminal_message(&self) -> Option<AssistantMessage> {
        match self {
            AssistantMessageEvent::Done { message, .. } => Some(message.clone()),
            AssistantMessageEvent::Error { error, .. } => Some(error.clone()),
            _ => None,
        }
    }

    fn partial(&self) -> &AssistantMessage {
        match self {
            AssistantMessageEvent::Start { partial }
            | AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolcallStart { partial, .. }
            | AssistantMessageEvent::ToolcallDelta { partial, .. }
            | AssistantMessageEvent::ToolcallEnd { partial, .. }
            | AssistantMessageEvent::Done {
                message: partial, ..
            }
            | AssistantMessageEvent::Error { error: partial, .. } => partial,
        }
    }

    fn content_index(&self) -> Option<u64> {
        match self {
            AssistantMessageEvent::TextStart { content_index, .. }
            | AssistantMessageEvent::TextDelta { content_index, .. }
            | AssistantMessageEvent::TextEnd { content_index, .. }
            | AssistantMessageEvent::ThinkingStart { content_index, .. }
            | AssistantMessageEvent::ThinkingDelta { content_index, .. }
            | AssistantMessageEvent::ThinkingEnd { content_index, .. }
            | AssistantMessageEvent::ToolcallStart { content_index, .. }
            | AssistantMessageEvent::ToolcallDelta { content_index, .. }
            | AssistantMessageEvent::ToolcallEnd { content_index, .. } => Some(*content_index),
            _ => None,
        }
    }

    fn delta(&self) -> Option<&str> {
        match self {
            AssistantMessageEvent::TextDelta { delta, .. }
            | AssistantMessageEvent::ThinkingDelta { delta, .. }
            | AssistantMessageEvent::ToolcallDelta { delta, .. } => Some(delta),
            _ => None,
        }
    }
}

#[derive(Default)]
struct SharedState {
    done: bool,
    closed: bool,
    completion_only: bool,
    producer_wakers: Vec<Waker>,
    #[cfg(test)]
    blocked_observer: Option<Waker>,
    queue: VecDeque<AssistantMessageEvent>,
    waker: Option<Waker>,
    resolved: Option<AssistantMessage>,
    waiters: Vec<tokio::sync::oneshot::Sender<AssistantMessage>>,
}

impl SharedState {
    fn wake_producers(&mut self) {
        for waker in self.producer_wakers.drain(..) {
            waker.wake();
        }
    }

    fn accept_event(&mut self, event: AssistantMessageEvent) {
        if event.is_terminal() {
            self.done = true;
            if let Some(message) = event.terminal_message() {
                self.resolve(message);
            }
            self.wake_producers();
        }
        if !self.completion_only {
            self.queue.push_back(event);
        }
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    /// Push a terminal event's message as the final result. First writer wins.
    fn resolve(&mut self, message: AssistantMessage) {
        if self.resolved.is_none() {
            self.resolved = Some(message);
            let waiters = std::mem::take(&mut self.waiters);
            for sender in waiters {
                if let Some(message) = &self.resolved {
                    let _ = sender.send(message.clone());
                }
            }
        }
    }
}

#[derive(Default)]
struct EventDeliveryOptions {
    capacity: Option<usize>,
    signal: Option<CancellationToken>,
}

struct Shared {
    state: Mutex<SharedState>,
    delivery: EventDeliveryOptions,
}

impl Shared {
    fn push(&self, event: AssistantMessageEvent) {
        // Synchronous emission is retained only for legacy/unbounded producers
        // and immediate one-terminal error streams. Bounded producers use send.
        assert!(
            self.delivery.capacity.is_none(),
            "bounded producers must await send"
        );
        let mut state = self.state.lock().unwrap();
        if !state.done {
            state.accept_event(event);
        }
    }

    fn poll_send(
        &self,
        cx: &mut TaskContext<'_>,
        event: &mut Option<AssistantMessageEvent>,
    ) -> Poll<Result<(), ProviderError>> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Poll::Ready(Err(ProviderError::Aborted));
        }
        if state.done {
            return Poll::Ready(Ok(()));
        }
        if !state.completion_only
            && self
                .delivery
                .capacity
                .is_some_and(|capacity| state.queue.len() >= capacity)
        {
            if !state
                .producer_wakers
                .iter()
                .any(|waker| waker.will_wake(cx.waker()))
            {
                state.producer_wakers.push(cx.waker().clone());
            }
            #[cfg(test)]
            if let Some(observer) = state.blocked_observer.take() {
                observer.wake();
            }
            return Poll::Pending;
        }
        if let Some(event) = event.take() {
            state.accept_event(event);
        }
        Poll::Ready(Ok(()))
    }

    fn end(&self, result: Option<AssistantMessage>) {
        let mut state = self.state.lock().unwrap();
        state.done = true;
        state.wake_producers();
        if let Some(result) = result {
            state.resolve(result);
        }
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }
}

#[derive(Clone)]
pub struct AssistantMessageEventWriter {
    shared: Arc<Shared>,
}

impl AssistantMessageEventWriter {
    /// Queue an event; events pushed after the stream terminated are
    /// dropped.
    pub fn push(&self, event: AssistantMessageEvent) {
        self.shared.push(event);
    }

    /// Await one ordered event. In legacy mode this is immediately ready. A
    /// bounded send waits without holding the state mutex and respects request
    /// cancellation. Terminal sends ignore request cancellation after transport
    /// teardown, but still wait for capacity or reader closure.
    pub async fn send(&self, event: AssistantMessageEvent) -> Result<(), ProviderError> {
        let terminal = event.is_terminal();
        let mut event = Some(event);
        let send = futures::future::poll_fn(|cx| self.shared.poll_send(cx, &mut event));
        if self.shared.delivery.capacity.is_some() && !terminal {
            if let Some(signal) = &self.shared.delivery.signal {
                return tokio::select! {
                    biased;
                    () = signal.cancelled() => Err(ProviderError::Aborted),
                    result = send => result,
                };
            }
        }
        send.await
    }

    pub fn end(&self, result: Option<AssistantMessage>) {
        self.shared.end(result);
    }

    pub fn is_done(&self) -> bool {
        self.shared.state.lock().unwrap().done
    }
}

/// Async iterable stream of [`AssistantMessageEvent`] with a
/// final-result future.
pub struct AssistantMessageEventStream {
    shared: Arc<Shared>,
    /// Native inference must not keep a detached HTTP task alive after retirement.
    pub(crate) abort_on_drop: Option<tokio::task::AbortHandle>,
}

impl Drop for AssistantMessageEventStream {
    fn drop(&mut self) {
        if self.shared.delivery.capacity.is_some() {
            let mut state = self.shared.state.lock().unwrap();
            state.closed = true;
            state.queue.clear();
            state.wake_producers();
        }
        if let Some(handle) = &self.abort_on_drop {
            handle.abort();
        }
    }
}

impl AssistantMessageEventStream {
    /// Create a linked writer/stream pair.
    #[must_use]
    pub fn new() -> (AssistantMessageEventWriter, Self) {
        Self::with_delivery(EventDeliveryOptions::default())
    }

    fn with_delivery(delivery: EventDeliveryOptions) -> (AssistantMessageEventWriter, Self) {
        let shared = Arc::new(Shared {
            state: Mutex::new(SharedState::default()),
            delivery,
        });
        (
            AssistantMessageEventWriter {
                shared: shared.clone(),
            },
            Self {
                shared,
                abort_on_drop: None,
            },
        )
    }

    pub async fn collect(mut self) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.next_event().await {
            events.push(event);
        }
        events
    }

    /// Await the next event, or None once the stream terminated.
    pub async fn next_event(&mut self) -> Option<AssistantMessageEvent> {
        futures::future::poll_fn(|cx| self.poll_next_event(cx)).await
    }

    /// Poll for the next queued event, or `None` once the stream terminated.
    ///
    /// # Panics
    ///
    /// Panics if the shared-state `Mutex` is poisoned.
    pub fn poll_next_event(
        &mut self,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<AssistantMessageEvent>> {
        let mut state = self.shared.state.lock().unwrap();
        if let Some(event) = state.queue.pop_front() {
            state.wake_producers();
            return Poll::Ready(Some(event));
        }
        if state.done {
            return Poll::Ready(None);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    /// Await the final assistant message carried by the terminal event.
    /// Bounded streams enter completion-only mode: intermediate events are no
    /// longer retained, so a result-only caller cannot block its own producer.
    ///
    /// # Panics
    ///
    /// Panics if the shared-state `Mutex` is poisoned.
    ///
    /// A stream terminated without a resolved message (`end(None)`) never resolves: this future
    /// hangs.
    pub async fn result(self) -> AssistantMessage {
        if let Some(message) = self.try_result() {
            return message;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut state = self.shared.state.lock().unwrap();
            if self.shared.delivery.capacity.is_some() {
                // result consumes the only reader: no caller can observe these
                // intermediate events. Stop retaining them and unblock producers.
                state.completion_only = true;
                state.queue.clear();
                state.wake_producers();
            }
            if let Some(message) = &state.resolved {
                return message.clone();
            }
            state.waiters.push(tx);
        }
        match rx.await {
            Ok(message) => message,
            Err(_) => {
                // Sender dropped without send: re-check the resolved slot.
                self.try_result()
                    .expect("event stream resolved without a final message")
            }
        }
    }

    fn try_result(&self) -> Option<AssistantMessage> {
        self.shared.state.lock().unwrap().resolved.clone()
    }
}

impl Default for AssistantMessageEventStream {
    fn default() -> Self {
        Self::new().1
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        self.poll_next_event(cx)
    }
}

pub fn create_assistant_message_event_stream(
) -> (AssistantMessageEventWriter, AssistantMessageEventStream) {
    AssistantMessageEventStream::new()
}

/// Configure builtin delivery before starting any producer/network work. Invalid
/// capacities return an already-resolved one-terminal error stream.
pub(crate) fn create_provider_event_stream(
    model: &crate::types::Model,
    options: Option<&crate::types::StreamOptions>,
) -> Result<(AssistantMessageEventWriter, AssistantMessageEventStream), AssistantMessageEventStream>
{
    let capacity = options.and_then(|options| options.event_buffer_capacity);
    if capacity == Some(0) {
        let (writer, reader) = create_assistant_message_event_stream();
        let mut message = initial_assistant_message(&model.api, &model.provider, &model.id);
        message.stop_reason = StopReason::Error;
        message.error_message = Some("Event buffer capacity must be greater than zero".into());
        writer.push(AssistantMessageEvent::Error {
            reason: crate::types::ErrorStopReason::Error,
            error: message.clone(),
        });
        writer.end(Some(message));
        return Err(reader);
    }
    Ok(AssistantMessageEventStream::with_delivery(
        EventDeliveryOptions {
            capacity,
            signal: options.and_then(|options| options.signal.clone()),
        },
    ))
}

/// Initial assistant message shape shared by every provider.
#[allow(dead_code)] // provider constructors use this once each port lands
pub fn initial_assistant_message(api: &str, provider: &str, model_id: &str) -> AssistantMessage {
    // Epoch millis fit u64 for ~584 million years; the u128 duration's millis are the wire's u64 convention.
    #[allow(clippy::cast_possible_truncation)]
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    AssistantMessage {
        content: Vec::new(),
        api: api.to_string(),
        provider: provider.to_string(),
        model: model_id.to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: crate::types::Usage::default(),
        stop_reason: StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp: timestamp_ms,
        rest: Map::default(),
    }
}

#[allow(dead_code)] // provider constructors use this once each port lands
pub fn empty_content() -> Vec<AssistantContent> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantContent, DoneStopReason, ErrorStopReason, TextContent, Usage};

    fn sample_message(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
                rest: Map::default(),
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test-model".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: Map::default(),
        }
    }

    #[tokio::test]
    async fn streams_events_in_order_and_resolves_result() {
        let (writer, mut stream) = AssistantMessageEventStream::new();
        let final_message = sample_message("hi");
        let start_message = sample_message("");
        writer.push(AssistantMessageEvent::Start {
            partial: start_message.clone(),
        });
        writer.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "hi".into(),
            partial: start_message,
        });
        writer.push(AssistantMessageEvent::Done {
            reason: DoneStopReason::Stop,
            message: final_message.clone(),
        });

        let mut types = Vec::new();
        while let Some(event) = stream.next_event().await {
            types.push(event.event_type().to_string());
        }
        assert_eq!(types, vec!["start", "text_delta", "done"]);
        assert_eq!(final_message.content.len(), 1);
    }

    #[tokio::test]
    async fn result_resolves_from_terminal_event() {
        let (writer, stream) = AssistantMessageEventStream::new();
        let mut message = sample_message("done");
        message.stop_reason = StopReason::Error;
        writer.push(AssistantMessageEvent::Error {
            reason: ErrorStopReason::Error,
            error: message.clone(),
        });
        let resolved = stream.result().await;
        assert_eq!(resolved.stop_reason, StopReason::Error);
        assert_eq!(resolved.content, message.content);
    }

    #[tokio::test]
    async fn drops_events_after_done() {
        let (writer, mut stream) = AssistantMessageEventStream::new();
        writer.push(AssistantMessageEvent::Done {
            reason: DoneStopReason::Stop,
            message: sample_message("done"),
        });
        assert!(writer.is_done());
        writer.push(AssistantMessageEvent::Start {
            partial: sample_message(""),
        });
        let first = stream.next_event().await.unwrap();
        assert_eq!(first.event_type(), "done");
        assert!(stream.next_event().await.is_none());
    }
}

#[cfg(test)]
#[path = "bounded_event_tests.rs"]
mod bounded_tests;

#[cfg(test)]
#[path = "providers/openai_responses_lifecycle_tests.rs"]
mod responses_lifecycle_tests;
