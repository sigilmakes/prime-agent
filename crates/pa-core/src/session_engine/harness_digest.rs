//! Read-only compatibility helpers for old session metadata.
//! Legacy harness context is suppressed at the typed model boundary.

use std::path::PathBuf;

use pa_types::session::{AgentMessage as SessionAgentMessage, FileEntry};

/// Session artifact directory implied by a conversation-log path
/// (`dirname(dirname(file))/session-artifacts/<id>`).
#[must_use]
pub fn session_artifact_dir_for_log(log: &std::path::Path) -> Option<PathBuf> {
    let id = log.file_stem()?.to_string_lossy().to_string();
    let artifacts_root = log.parent()?.parent()?.join("session-artifacts");
    Some(artifacts_root.join(id))
}

/// Session-local harness state directory implied by a conversation-log path
/// (the artifact dir plus the harness subdir).
#[must_use]
pub fn local_harness_dir_for_log(log: &std::path::Path) -> Option<PathBuf> {
    session_artifact_dir_for_log(log).map(|dir| dir.join(crate::refinement::HARNESS_STATE_DIR_NAME))
}

/// Session message view of the digest entries (resume context rebuild).
#[must_use]
pub fn digest_session_message(entry: &FileEntry) -> Option<SessionAgentMessage> {
    let FileEntry::CustomMessage { payload, .. } = entry else {
        return None;
    };
    if payload.custom_type != super::headless::HARNESS_DIGEST_CUSTOM_TYPE {
        return None;
    }
    Some(SessionAgentMessage::Custom(
        pa_types::session::CustomMessage {
            custom_type: payload.custom_type.clone(),
            content: payload.content.clone(),
            display: payload.display,
            details: payload.details.clone(),
            timestamp: crate::session::timestamp_to_millis(entry.timestamp()),
            rest: serde_json::Map::default(),
        },
    ))
}

// Delivery mechanics: the `AgentSession` methods that drive the
// cold-boundary delivery invariant (private fields reachable here).

/// Decode audit outcomes from old session files without enabling refinement policy.
/// This is a read-only compatibility view; no harness state is read or changed.
#[must_use]
pub fn session_refinement_history(
    entries: &[FileEntry],
) -> Vec<crate::refinement::RefinementResult> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Custom { payload, .. }
                if payload.custom_type == "prime-agent.refinement" =>
            {
                payload.data.as_ref().and_then(|data| {
                    serde_json::from_value::<crate::refinement::RefinementResult>(data.clone()).ok()
                })
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod legacy_tests {
    use super::*;

    #[test]
    fn audit_history_decodes_only_typed_legacy_records_without_changing_history() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = crate::session::manager::SessionManager::in_memory(tmp.path());
        let result = crate::refinement::RefinementResult {
            id: "legacy-refinement".to_string(),
            summary: "Earlier result".to_string(),
            rationale: "Recorded evidence".to_string(),
            expected_outcome: "Preserve the audit history".to_string(),
            applied_edits: Vec::new(),
            harness_state_path: "saved-path".to_string(),
            rollback_of: None,
            scope: None,
        };
        let data = serde_json::to_value(&result).unwrap();
        session
            .append_custom_entry("prime-agent.refinement", Some(data.clone()))
            .unwrap();
        session
            .append_custom_entry("other-audit", Some(data.clone()))
            .unwrap();
        session
            .append_custom_entry(
                "prime-agent.refinement",
                Some(serde_json::json!({"bad": true})),
            )
            .unwrap();
        session
            .append_message(SessionAgentMessage::User(pa_types::ai::UserMessage {
                content: pa_types::ai::UserContent::Text(data.to_string()),
                timestamp: 0,
                rest: serde_json::Map::default(),
            }))
            .unwrap();
        let canonical = serde_json::to_value(session.get_all_entries()).unwrap();
        assert_eq!(
            session_refinement_history(session.get_all_entries()),
            vec![result]
        );
        assert_eq!(
            serde_json::to_value(session.get_all_entries()).unwrap(),
            canonical
        );
    }
}
