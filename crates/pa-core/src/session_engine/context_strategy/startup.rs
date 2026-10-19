//! An explicit view boundary for hosts that journal a prompt before lazy startup.
//! The complete native manager remains the source of identity and authority.

use anyhow::{bail, Context};
use pa_types::session::{AgentMessage, FileEntry};

use super::{active_branch, project, SessionPlugins};
use crate::session::{manager::SessionManager, SessionContext};

/// Initial strategy projection relative to already-journaled prompt admissions.
/// This changes neither the canonical branch nor the persisted strategy choice.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ContextStartupBoundary {
    /// Ordinary SDK behavior: assemble the complete current canonical branch.
    #[default]
    Current,
    /// No historical prefix, apart from canonical bootstrap metadata.
    Empty,
    /// Assemble only through this real ancestor of the current canonical leaf.
    Leaf(String),
}

/// Validate admission identity before invoking any strategy callback. In
/// particular, Empty is never a substitute for an oversized historical body.
pub(crate) fn project_at(
    entries: &[FileEntry],
    current_leaf: Option<&str>,
    strategy: &str,
    boundary: &ContextStartupBoundary,
    admitted_user_ids: &[String],
) -> anyhow::Result<(SessionContext, Option<String>)> {
    if matches!(boundary, ContextStartupBoundary::Current) {
        if !admitted_user_ids.is_empty() {
            bail!("Current startup context cannot carry admitted-user proof");
        }
        return Ok((
            project(entries, current_leaf, strategy),
            current_leaf.map(str::to_owned),
        ));
    }
    let mut ids = std::collections::HashSet::new();
    for id in entries.iter().filter_map(FileEntry::id) {
        if id.is_empty() {
            bail!("startup source contains an empty canonical entry ID");
        }
        if !ids.insert(id) {
            bail!("startup source contains duplicate canonical entry IDs");
        }
    }
    let branch = active_branch(entries, current_leaf)?;
    let (prefix, suffix, leaf) = match boundary {
        ContextStartupBoundary::Current => unreachable!(),
        ContextStartupBoundary::Empty => (&branch[..0], branch.as_slice(), None),
        ContextStartupBoundary::Leaf(leaf) => {
            let index = branch
                .iter()
                .position(|entry| entry.id() == Some(leaf.as_str()))
                .context("startup context leaf is not an ancestor of the current canonical leaf")?;
            (&branch[..=index], &branch[index + 1..], Some(leaf.clone()))
        }
    };
    let mut actual_ids = Vec::new();
    for entry in suffix {
        match entry {
            FileEntry::Message {
                message: AgentMessage::User(_),
                ..
            } => {
                actual_ids.push(
                    entry
                        .id()
                        .context("admitted user row lacks canonical ID")?
                        .to_owned(),
                );
            }
            entry if crate::session::is_bootstrap_entry(entry) => {}
            _ => bail!("startup context boundary would omit non-admitted canonical history"),
        }
    }
    let unique: std::collections::HashSet<_> = admitted_user_ids.iter().collect();
    if unique.len() != admitted_user_ids.len() || actual_ids != admitted_user_ids {
        bail!("startup context admitted-user IDs do not match the canonical suffix");
    }
    // Passing None into build_session_context means latest, not empty. Use an
    // actually empty projection, without changing or trimming the native source.
    let context = if matches!(boundary, ContextStartupBoundary::Empty) {
        crate::session::build_session_context(&[], None)
    } else {
        project(prefix, leaf.as_deref(), strategy)
    };
    Ok((context, leaf))
}

/// Capture the complete native source while locked, then read and project it
/// off lock. The ordinary Current path keeps its existing window fast path.
pub(crate) fn snapshot_at(
    plugins: &SessionPlugins,
    session: &SessionManager,
    boundary: &ContextStartupBoundary,
    admitted_user_ids: &[String],
) -> anyhow::Result<
    impl std::future::Future<Output = anyhow::Result<(SessionContext, Option<String>)>> + Send + 'static,
> {
    let current = super::context_snapshot(plugins, session)?;
    let snapshot = session.history_snapshot();
    let current_leaf = session.get_leaf_id().map(str::to_owned);
    let strategy = super::route(&plugins.context_selection()?);
    let boundary = boundary.clone();
    let admitted_user_ids = admitted_user_ids.to_vec();
    Ok(async move {
        if matches!(boundary, ContextStartupBoundary::Current) {
            if !admitted_user_ids.is_empty() {
                bail!("Current startup context cannot carry admitted-user proof");
            }
            current.await
        } else {
            let entries = snapshot.await?;
            project_at(
                &entries,
                current_leaf.as_deref(),
                &strategy,
                &boundary,
                &admitted_user_ids,
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::manager::SessionManager;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text(text.into()),
            timestamp: 0,
            rest: pa_types::JsonMap::new(),
        })
    }

    #[test]
    fn startup_empty_accepts_bootstrap_but_not_unproven_body_or_archive() {
        let mut manager = SessionManager::in_memory(std::path::Path::new("/project"));
        manager.append_model_change("test", "model").unwrap();
        manager.append_thinking_level_change("off").unwrap();
        manager.append_service_tier_change(None).unwrap();
        manager
            .append_custom_entry(
                pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE,
                Some(serde_json::json!({"version":1,"strategy":"policy/a"})),
            )
            .unwrap();
        let admitted = manager.append_message(user("current")).unwrap();
        let entries = manager.retained_entries();
        let (context, leaf) = project_at(
            entries,
            manager.get_leaf_id(),
            "policy/a",
            &ContextStartupBoundary::Empty,
            std::slice::from_ref(&admitted),
        )
        .unwrap();
        assert!(context.messages.is_empty());
        assert!(context.archive.is_none());
        assert!(leaf.is_none());
        assert!(project_at(
            entries,
            manager.get_leaf_id(),
            "policy/a",
            &ContextStartupBoundary::Empty,
            &[]
        )
        .is_err());
        assert!(project_at(
            entries,
            manager.get_leaf_id(),
            "policy/a",
            &ContextStartupBoundary::Empty,
            &[admitted.clone(), admitted]
        )
        .is_err());
        let mut archived = manager.retained_entries().to_vec();
        let archive_entry: FileEntry = serde_json::from_value(serde_json::json!({
            "type":"compaction", "id":"archive", "parentId":manager.get_leaf_id(),
            "timestamp":"2026-01-01T00:00:00Z", "summary":"history",
            "firstKeptEntryId":manager.get_leaf_id(), "tokensBefore":100,
        }))
        .unwrap();
        archived.push(archive_entry);
        assert!(project_at(
            &archived,
            Some("archive"),
            "policy/a",
            &ContextStartupBoundary::Empty,
            &[]
        )
        .is_err());
        manager
            .append_custom_message(
                "unproven",
                pa_types::ai::UserContent::Text("body".into()),
                true,
                None,
            )
            .unwrap();
        assert!(project_at(
            manager.retained_entries(),
            manager.get_leaf_id(),
            "policy/a",
            &ContextStartupBoundary::Empty,
            &[]
        )
        .is_err());
    }
}
