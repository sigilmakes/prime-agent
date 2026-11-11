//! Namespaced `_meta` payloads for wv capabilities that ACP has
//! no native concept for (cwd reporting, quiescence observation,
//! correlation). Vanilla ACP clients ignore `_meta` keys; a wv-aware
//! client reads them; non-standard fields never appear at an ACP root.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Reverse-domain namespace for every wv `_meta` payload.
pub const PRIME_AGENT_META_NAMESPACE: &str = "ai.primeintellect.prime-agent";

/// A client-requested cwd that differs from the agent's actual startup cwd.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrimeAgentCwdMeta {
    pub requested: String,
    pub actual: String,
}

/// Observed subagent count at a completion point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeAgentQuiescenceMeta {
    pub outstanding_subagents: u64,
}

/// Producer-side ordering of an update inside its prompt turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrimeAgentEventPhase {
    /// Ordinary streamed work.
    #[serde(rename = "event")]
    Event,
    /// The correlated boundary in front of a prompt response.
    #[serde(rename = "responseBoundary")]
    ResponseBoundary,
    /// The final settled state after the response boundary.
    #[serde(rename = "terminalQuiescence")]
    TerminalQuiescence,
}

/// The outcome carried by a response boundary and terminal envelope. ACP
/// transport stop reasons (including `end_turn`) are never a causal
/// completion signal, so this is deliberately only `result` and `error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrimeAgentOutcome {
    #[serde(rename = "result")]
    Result,
    #[serde(rename = "error")]
    Error,
}

/// The wv payload under the `_meta` namespace key.
///
/// `prompt_turn_id` is allocated when ACP accepts a prompt, never inferred
/// from the running prompt; `0` means a session-scoped event with no prompt
/// origin. `event_sequence` is connection-wide, strictly increasing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeAgentSessionMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_turn_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<PrimeAgentEventPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<PrimeAgentOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_quiescence_expected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PrimeAgentCwdMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiescence: Option<PrimeAgentQuiescenceMeta>,
    /// Rich kernel output reported by the ipython tool (attachments the cell
    /// loaded into context, plus the number of diffs it displayed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipython: Option<Value>,
    /// Set when the session's heartbeat or cron schedule changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeats_changed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<PrimeAgentGoalMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_message: Option<PrimeAgentAgentMessageMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<PrimeAgentCompactionMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagents: Option<Vec<PrimeAgentSubagentMeta>>,
}

/// A goal's live state, surfaced after `/goal` commands and driver turns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeAgentGoalMeta {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<u64>,
}

/// An agent-to-agent message sent from inside a kernel cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeAgentAgentMessageMeta {
    pub tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_status: Option<String>,
}

/// One compaction that ran during the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeAgentCompactionMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// One RLM subagent roster row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeAgentSubagentMeta {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Map a finished turn onto an ACP stop reason. Precedence: an explicit
/// cancel, then the turn's final assistant stop reason (#3363): a per-call
/// output-token truncation (`StopReason::Length`) is an honest
/// `max_tokens`, so `end_turn` stays reserved for a finished answer;
/// every other final reason (and none) keeps TS's `end_turn`.
pub fn acp_stop_reason(
    cancelled: bool,
    assistant_stop_reason: Option<pa_types::ai::StopReason>,
) -> super::types::AcpStopReason {
    use pa_types::ai::StopReason;
    if cancelled {
        return super::types::AcpStopReason::Cancelled;
    }
    match assistant_stop_reason {
        Some(StopReason::Length) => super::types::AcpStopReason::MaxTokens,
        Some(StopReason::Stop | StopReason::ToolUse | StopReason::Error | StopReason::Aborted)
        | None => super::types::AcpStopReason::EndTurn,
    }
}

/// Wrap a wv payload in its reverse-domain `_meta` envelope.
pub fn prime_agent_meta(payload: &PrimeAgentSessionMeta) -> Value {
    json!({ PRIME_AGENT_META_NAMESPACE: payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_wraps_under_the_namespace_key() {
        let wrapped = prime_agent_meta(&PrimeAgentSessionMeta {
            prompt_turn_id: Some(1),
            event_sequence: Some(2),
            phase: Some(PrimeAgentEventPhase::ResponseBoundary),
            outcome: Some(PrimeAgentOutcome::Error),
            ..Default::default()
        });
        assert_eq!(
            wrapped,
            json!({ "ai.primeintellect.prime-agent": {
                "promptTurnId": 1,
                "eventSequence": 2,
                "phase": "responseBoundary",
                "outcome": "error",
            }})
        );
    }

    #[test]
    fn acp_stop_reason_keeps_cancel_and_truncation() {
        use super::super::types::AcpStopReason;
        use pa_types::ai::StopReason;
        // An explicit cancel wins over the turn's own truncated stop reason.
        assert_eq!(
            acp_stop_reason(/*cancelled*/ true, Some(StopReason::Length)),
            AcpStopReason::Cancelled
        );
        // A length stop maps to max_tokens.
        assert_eq!(
            acp_stop_reason(/*cancelled*/ false, Some(StopReason::Length)),
            AcpStopReason::MaxTokens
        );
        // A settled answer keeps end_turn.
        assert_eq!(
            acp_stop_reason(/*cancelled*/ false, Some(StopReason::Stop)),
            AcpStopReason::EndTurn
        );
    }

    #[test]
    fn quiescence_serializes_camel_case() {
        let wrapped = prime_agent_meta(&PrimeAgentSessionMeta {
            quiescence: Some(PrimeAgentQuiescenceMeta {
                outstanding_subagents: 0,
            }),
            ..Default::default()
        });
        assert_eq!(
            wrapped[PRIME_AGENT_META_NAMESPACE]["quiescence"],
            json!({ "outstandingSubagents": 0 })
        );
    }
}
