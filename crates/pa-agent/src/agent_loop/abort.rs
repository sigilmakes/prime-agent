//! Abort and settlement helpers for the agent loop: the `raceWithAbort`
//! wrapper, `settlePostTurn`, `pollMessagesUnlessAborted`, and aborted
//! assistant-message construction.

use crate::abort::{is_abort_error, AbortSignal, ABORT_ERROR_MESSAGE};
use crate::types::{AgentMessage, AssistantContent, AssistantMessage, StopReason};
use std::future::Future;

use super::{AgentLoopConfig, PollMessagesFn};

pub(crate) async fn race_with_abort<T>(
    operation: impl Future<Output = anyhow::Result<T>>,
    signal: Option<&AbortSignal>,
) -> anyhow::Result<T> {
    match signal {
        None => operation.await,
        Some(signal) => crate::abort::race_with_abort(operation, signal)
            .await
            .and_then(|result| result),
    }
}

pub(crate) enum PostTurnResult<T> {
    Completed(T),
    Aborted,
}

/// Abort rejections collapse into `Aborted`; every other error propagates.
pub(crate) async fn settle_post_turn<T>(
    operation: impl Future<Output = anyhow::Result<T>>,
    signal: Option<&AbortSignal>,
) -> anyhow::Result<PostTurnResult<T>> {
    match operation.await {
        Ok(value) => Ok(PostTurnResult::Completed(value)),
        Err(error) => {
            if signal.is_some_and(AbortSignal::is_aborted) && is_abort_error(&error) {
                Ok(PostTurnResult::Aborted)
            } else {
                Err(error)
            }
        }
    }
}

pub(crate) async fn poll_messages_unless_aborted(
    poll: Option<&PollMessagesFn>,
    signal: Option<&AbortSignal>,
) -> anyhow::Result<Vec<AgentMessage>> {
    let Some(poll) = poll else {
        return Ok(Vec::new());
    };
    if signal.is_some_and(AbortSignal::is_aborted) {
        return Ok(Vec::new());
    }
    race_with_abort(poll(), signal).await
}

fn clone_assistant_content(content: &[AssistantContent]) -> Vec<AssistantContent> {
    content
        .iter()
        .map(|part| match part {
            // TS clones the arguments object per toolCall; Value::clone is a
            // full copy, which is at least as safe.
            AssistantContent::ToolCall(tool_call) => AssistantContent::ToolCall(tool_call.clone()),
            AssistantContent::Text(text) => AssistantContent::Text(text.clone()),
            AssistantContent::Thinking(thinking) => AssistantContent::Thinking(thinking.clone()),
        })
        .collect()
}

/// A settled aborted message always carries at least one content part.
///
/// TS starts every assistant message with an empty text block, so no TS row is
/// aborted with an empty part list. A provider can abort before its first delta,
/// which leaves a partial that exists with no parts at all; normalising here
/// covers every provider at once instead of teaching each one the shape.
pub(crate) fn settle_aborted_content(mut message: AssistantMessage) -> AssistantMessage {
    if message.stop_reason == StopReason::Aborted && message.content.is_empty() {
        message
            .content
            .push(AssistantContent::Text(crate::types::TextContent {
                text: String::new(),
                text_signature: None,
            }));
    }
    message
}

pub(crate) fn create_aborted_assistant_message(
    config: &AgentLoopConfig,
    partial_message: Option<&AssistantMessage>,
) -> AssistantMessage {
    settle_aborted_content(AssistantMessage {
        content: partial_message.map_or_else(
            || {
                vec![AssistantContent::Text(crate::types::TextContent {
                    text: String::new(),
                    text_signature: None,
                })]
            },
            |partial| clone_assistant_content(&partial.content),
        ),
        api: partial_message.map_or_else(|| config.model.api.clone(), |p| p.api.clone()),
        provider: partial_message
            .map_or_else(|| config.model.provider.clone(), |p| p.provider.clone()),
        model: partial_message.map_or_else(|| config.model.id.clone(), |p| p.model.clone()),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: partial_message.map_or_else(crate::types::Usage::zero, |p| p.usage.clone()),
        stop_reason: StopReason::Aborted,
        stop_reason_raw: None,
        error_message: Some(ABORT_ERROR_MESSAGE.to_string()),
        timestamp: crate::now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{TextContent, Usage};

    fn partial(content: Vec<AssistantContent>) -> AssistantMessage {
        AssistantMessage {
            content,
            api: "faux".into(),
            provider: "faux".into(),
            model: "faux-1".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::zero(),
            stop_reason: StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
        }
    }

    /// A partial that exists before its first content part: the aborted row must
    /// still carry one empty text part, because no TS row is aborted with none.
    #[test]
    fn an_aborted_row_with_no_parts_yet_still_carries_one_empty_text_part() {
        let mut message = partial(Vec::new());
        message.stop_reason = StopReason::Aborted;
        let settled = settle_aborted_content(message);
        assert_eq!(settled.stop_reason, StopReason::Aborted);
        assert_eq!(
            settled.content,
            vec![AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: None,
            })]
        );
    }

    /// Text that had already streamed is never replaced by the placeholder.
    #[test]
    fn an_aborted_row_keeps_the_parts_it_already_has() {
        let content = vec![AssistantContent::Text(TextContent {
            text: "half a sentence".into(),
            text_signature: None,
        })];
        let mut message = partial(content.clone());
        message.stop_reason = StopReason::Aborted;
        let settled = settle_aborted_content(message);
        assert_eq!(settled.content, content);
    }

    /// A message that did not abort is untouched, even with no parts.
    #[test]
    fn a_message_that_did_not_abort_is_left_alone() {
        let settled = settle_aborted_content(partial(Vec::new()));
        assert!(settled.content.is_empty());
        assert_eq!(settled.stop_reason, StopReason::Stop);
    }
}
