//! Codex WebSocket session state: connection cache, historical debug stats, and socket-bound
//! continuation bookkeeping.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tokio::time::Instant;

use tokio::sync::mpsc;

use serde_json::Value;

use crate::providers::openai_codex_responses::errors::CodexStreamError;
use crate::providers::openai_codex_responses::websocket::{ContinuationState, WorkerCommand};

pub use crate::providers::openai_codex_responses::websocket::WebSocketDebugStats;

pub struct CachedConnection {
    /// Command channel to the connection's worker task.
    pub worker: mpsc::Sender<WorkerCommand>,
    pub busy: bool,
    pub continuation: Option<ContinuationState>,
    /// Monotonic socket creation time; checkout rotates at the absolute age limit.
    pub created_at: Instant,
    /// Unique id of this connection (monotonic).
    pub connection_id: u64,
}

#[derive(Default)]
pub struct SessionState {
    pub connections: HashMap<String, CachedConnection>,
    pub stats: HashMap<String, WebSocketDebugStats>,
}

pub fn session_state() -> &'static Mutex<SessionState> {
    static STATE: OnceLock<Mutex<SessionState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(SessionState::default()))
}

pub fn record_websocket_sse_fallback(session_id: Option<&str>) {
    let Some(session_id) = session_id else {
        return;
    };
    let Ok(mut state) = session_state().lock() else {
        return;
    };
    let session_stats = state.stats.entry(session_id.to_string()).or_default();
    session_stats.sse_fallbacks += 1;
    session_stats.websocket_fallback_active = Some(false);
}

/// Retain failure history without changing the next logical request's transport choice.
pub fn record_websocket_failure(session_id: Option<&str>, error: &CodexStreamError) {
    let Some(session_id) = session_id else {
        return;
    };
    let Ok(mut state) = session_state().lock() else {
        return;
    };
    let session_stats = state.stats.entry(session_id.to_string()).or_default();
    session_stats.websocket_failures += 1;
    session_stats.last_websocket_error = Some(error.to_string());
    session_stats.websocket_fallback_active = Some(false);
}

#[must_use]
pub fn get_debug_stats(session_id: &str) -> Option<WebSocketDebugStats> {
    session_state().lock().ok()?.stats.get(session_id).cloned()
}

pub fn reset_debug_stats(session_id: Option<&str>) {
    let Ok(mut state) = session_state().lock() else {
        return;
    };
    if let Some(session_id) = session_id {
        state.stats.remove(session_id);
    } else {
        state.stats.clear();
    }
}

/// Close the cached connection for one session or all of them; the agent layer calls it when a
/// session ends (the TS registers a session resource cleanup).
pub fn close_websocket_sessions(session_id: Option<&str>) {
    let Ok(mut state) = session_state().lock() else {
        return;
    };
    let entries: Vec<mpsc::Sender<WorkerCommand>> = match session_id {
        Some(session_id) => state
            .connections
            .remove(session_id)
            .map(|entry| vec![entry.worker])
            .unwrap_or_default(),
        None => state
            .connections
            .drain()
            .map(|(_, entry)| entry.worker)
            .collect(),
    };
    for worker in entries {
        let _ = worker.try_send(WorkerCommand::Close);
    }
}

/// Record debug stats for a request (mirrors the stats block in `processWebSocketStream`).
pub fn record_request_stats(
    session_id: &str,
    reused: bool,
    use_cached_context: bool,
    request_body: &Value,
) {
    let Ok(mut state) = session_state().lock() else {
        return;
    };
    let session_stats = state.stats.entry(session_id.to_string()).or_default();
    session_stats.requests += 1;
    if reused {
        session_stats.connections_reused += 1;
    } else {
        session_stats.connections_created += 1;
    }
    if use_cached_context {
        session_stats.cached_context_requests += 1;
    }
    if request_body.get("store").and_then(Value::as_bool) == Some(true) {
        session_stats.store_true_requests += 1;
    }
    let input_items = request_body
        .get("input")
        .and_then(Value::as_array)
        .map_or(0, |items| items.len() as u64);
    session_stats.last_input_items = input_items;
    if let Some(previous_response_id) = request_body
        .get("previous_response_id")
        .and_then(Value::as_str)
    {
        session_stats.delta_requests += 1;
        session_stats.last_delta_input_items = Some(input_items);
        session_stats.last_previous_response_id = Some(previous_response_id.to_string());
    } else {
        session_stats.full_context_requests += 1;
        session_stats.last_delta_input_items = None;
        session_stats.last_previous_response_id = None;
    }
}

/// Evict only the socket owned by this lease, including its continuation. A concurrent request
/// may have installed a different cached socket for the same session.
pub fn evict_connection(session_id: &str, connection_id: u64) {
    let Ok(mut state) = session_state().lock() else {
        return;
    };
    if state
        .connections
        .get(session_id)
        .is_none_or(|entry| entry.connection_id != connection_id)
    {
        return;
    }
    if let Some(entry) = state.connections.remove(session_id) {
        let _ = entry.worker.try_send(WorkerCommand::Close);
    }
}

/// Take the continuation for `connection_id`, if one is anchored to it (part of
/// `buildCachedWebSocketRequestBody` in the TS).
pub fn take_continuation_for(session_id: &str, connection_id: u64) -> Option<ContinuationState> {
    session_state()
        .lock()
        .ok()?
        .connections
        .get_mut(session_id)
        .filter(|entry| entry.connection_id == connection_id)
        .and_then(|entry| entry.continuation.take())
}

/// Restore (or clear) the continuation after a failed continuation attempt.
pub fn clear_continuation(session_id: &str, connection_id: u64) {
    let Ok(mut state) = session_state().lock() else {
        return;
    };
    if let Some(entry) = state.connections.get_mut(session_id) {
        if entry.connection_id == connection_id {
            entry.continuation = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::openai_codex_responses::errors::WebSocketTransportError;

    #[test]
    fn sse_fallback_lifecycle() {
        let session = format!("test-session-{}", std::process::id());
        reset_debug_stats(Some(&session));
        // A fallback counter is historical, not a transport policy for later requests.
        record_websocket_sse_fallback(Some(&session));
        let stats = get_debug_stats(&session).expect("stats recorded");
        assert_eq!(stats.sse_fallbacks, 1);
        assert_eq!(stats.websocket_fallback_active, Some(false));
        reset_debug_stats(Some(&session));
        assert!(get_debug_stats(&session).is_none());
    }

    #[tokio::test]
    async fn records_failure_history_without_pinning_fallback() {
        let session = format!("failure-session-{}", std::process::id());
        reset_debug_stats(Some(&session));
        record_websocket_failure(
            Some(&session),
            &CodexStreamError::Transport(WebSocketTransportError::runtime("boom")),
        );
        let stats = get_debug_stats(&session).expect("stats recorded");
        assert_eq!(stats.websocket_failures, 1);
        assert_eq!(stats.websocket_fallback_active, Some(false));
        assert_eq!(stats.last_websocket_error.as_deref(), Some("boom"));
        close_websocket_sessions(Some(&session));
        reset_debug_stats(Some(&session));
    }
}
