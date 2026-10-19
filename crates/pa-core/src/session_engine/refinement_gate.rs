//! Read-only suppression of legacy host-owned harness context.
//! Active plugins supply new context explicitly; saved legacy rows never replay.
//! Durable history and user-authored text are untouched.

use pa_types::session::AgentMessage;

/// Remove only typed harness rows; never interpret a user's quoted text as metadata.
pub(crate) fn without_refinements(messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            AgentMessage::Custom(custom)
                if matches!(
                    custom.custom_type.as_str(),
                    "harness_digest" | "refinement_notice" | "refinement_outcome"
                ) =>
            {
                None
            }
            AgentMessage::CompactionSummary(mut summary) => {
                summary.harness_digest = None;
                summary.harness_state_fingerprint = None;
                Some(AgentMessage::CompactionSummary(summary))
            }
            message @ (AgentMessage::User(_)
            | AgentMessage::Assistant(_)
            | AgentMessage::ToolResult(_)
            | AgentMessage::BashExecution(_)
            | AgentMessage::Custom(_)
            | AgentMessage::BranchSummary(_)) => Some(message),
        })
        .collect()
}

/// Filter at every provider request, including tool continuations and retries.
/// Provenance stays typed until after the legacy context has been removed.
pub(crate) fn convert_to_llm() -> pa_agent::agent_loop::ConvertToLlmFn {
    std::sync::Arc::new(move |messages| {
        Box::pin(async move {
            let messages = messages
                .into_iter()
                .filter_map(|message| match message {
                    pa_agent::types::AgentMessage::Custom(custom) => {
                        let typed =
                            serde_json::from_value(serde_json::to_value(custom).ok()?).ok()?;
                        without_refinements(vec![typed])
                            .into_iter()
                            .next()
                            .and_then(|row| super::session_message_to_loop(&row))
                    }
                    message @ pa_agent::types::AgentMessage::Standard(_) => Some(message),
                })
                .collect();
            Ok(super::messages::loop_convert_to_llm(messages))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_types::ai::{UserContent, UserMessage};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
            rest: serde_json::Map::default(),
        })
    }

    fn custom(kind: &str, text: &str) -> AgentMessage {
        AgentMessage::Custom(pa_types::session::CustomMessage {
            custom_type: kind.to_string(),
            content: UserContent::Text(text.to_string()),
            display: false,
            details: None,
            timestamp: 0,
            rest: serde_json::Map::default(),
        })
    }

    fn compaction() -> AgentMessage {
        AgentMessage::CompactionSummary(pa_types::session::CompactionSummaryMessage {
            rest: pa_types::JsonMap::new(),
            summary: "Earlier decisions still apply".to_string(),
            tokens_before: 10,
            retained_message_count: None,
            custom_instructions: None,
            harness_digest: Some("saved memory".to_string()),
            harness_state_fingerprint: Some("fp".to_string()),
            timestamp: 0,
        })
    }

    #[tokio::test]
    async fn legacy_rows_never_replay_beside_new_plugin_context_or_quoted_user_text() {
        let quote = user(&format!(
            "{}saved memory{}\n[refinement-notice] quoted, not metadata",
            super::super::messages::HARNESS_DIGEST_PREFIX,
            super::super::messages::HARNESS_DIGEST_SUFFIX,
        ));
        // Plugin activation does not authorize replay of old host-owned rows.
        let plugin = custom("plugin.harness.context", "new explicit contribution");
        let canonical = vec![
            custom("harness_digest", "saved memory"),
            custom("refinement_notice", "old notice"),
            custom("refinement_outcome", "old outcome"),
            compaction(),
            quote.clone(),
            plugin.clone(),
        ];
        let saved = canonical.clone();
        let mut summary = compaction();
        let AgentMessage::CompactionSummary(clean) = &mut summary else {
            unreachable!();
        };
        clean.harness_digest = None;
        clean.harness_state_fingerprint = None;
        let expected = vec![summary, quote, plugin];
        assert_eq!(without_refinements(canonical.clone()), expected);
        assert_eq!(
            canonical, saved,
            "filtering must not mutate canonical history"
        );

        // Bypass the admission helper to test raw restored loop rows at the provider boundary.
        let raw = canonical
            .iter()
            .map(|message| serde_json::from_value(serde_json::to_value(message).unwrap()).unwrap())
            .collect();
        let expected_loop = expected
            .iter()
            .filter_map(super::super::session_message_to_loop)
            .collect();
        let convert = convert_to_llm();
        assert_eq!(
            convert(raw).await.unwrap(),
            super::super::messages::loop_convert_to_llm(expected_loop),
        );
        assert_eq!(canonical, saved);
    }

    #[test]
    fn session_rebuild_filters_legacy_metadata_without_touching_user_quotes() {
        let quoted = user("[harness-digest]\n<harness_state>quoted memory</harness_state>");
        let canonical = vec![custom("harness_digest", "old memory"), quoted, compaction()];
        let saved = canonical.clone();
        let rebuilt = super::super::rebuilt_loop_messages(canonical.clone());
        let expected = without_refinements(canonical.clone())
            .iter()
            .filter_map(super::super::session_message_to_loop)
            .collect::<Vec<_>>();
        assert_eq!(rebuilt, expected);
        assert_eq!(rebuilt.len(), 2);
        assert_eq!(canonical, saved);
    }
}
