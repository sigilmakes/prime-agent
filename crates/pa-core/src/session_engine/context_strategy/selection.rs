//! Idle-only strategy selection and projections of strategy-owned archives.

use super::{active_branch, assemble_for_strategy, route, AgentSession, FileEntry, SessionPlugins};
use crate::session::manager::SessionManager;
use crate::session::SessionContext;
use anyhow::{bail, Context};

/// A strategy only reuses its own accepted archives. Foreign compaction nodes
/// remain ancestry links, while their canonical original message rows survive.
pub(crate) fn project(entries: &[FileEntry], leaf: Option<&str>, strategy: &str) -> SessionContext {
    let projected: Vec<_> = entries
        .iter()
        .map(|entry| match entry {
            FileEntry::Compaction { payload, base }
                if payload
                    .context_archive
                    .as_ref()
                    .map_or(crate::plugins::context::DEFAULT_STRATEGY, |archive| {
                        archive.strategy.as_str()
                    })
                    != strategy =>
            {
                FileEntry::Custom {
                    payload: pa_types::session::CustomEntry {
                        custom_type: "foreign_context_archive".into(),
                        data: None,
                        rest: pa_types::JsonMap::new(),
                    },
                    base: base.clone(),
                }
            }
            entry => entry.clone(),
        })
        .collect();
    crate::session::build_session_context(&projected, leaf)
}

/// Capture before dropping the session lock; any full canonical read is awaited
/// off lock. The ordinary default window retains its cheap startup path.
pub(crate) fn context_snapshot(
    plugins: &SessionPlugins,
    session: &SessionManager,
) -> anyhow::Result<
    impl std::future::Future<Output = anyhow::Result<(SessionContext, Option<String>)>> + Send + 'static,
> {
    let strategy = route(&plugins.context_selection()?);
    let context = session.active_context();
    let full = strategy != crate::plugins::context::DEFAULT_STRATEGY
        || context
            .archive
            .as_ref()
            .is_some_and(|archive| archive.strategy != strategy);
    let snapshot = session.history_snapshot();
    let leaf = session.get_leaf_id().map(str::to_owned);
    Ok(async move {
        if full {
            let entries = snapshot.await?;
            Ok((project(&entries, leaf.as_deref(), &strategy), leaf))
        } else {
            Ok((context, leaf))
        }
    })
}

impl AgentSession {
    /// Explicitly select one enabled strategy without changing canonical history.
    /// Only the typed selection record is appended. Candidate preparation may
    /// reuse valid caches, but cannot publish the selected route or live view.
    ///
    /// # Errors
    /// Rejects busy runs, missing or retired strategies, changed branches,
    /// invalid candidate views, and durable-write failures without publication.
    pub async fn select_context_strategy(&self, requested: &str) -> anyhow::Result<()> {
        if self.agent.state().await.is_streaming {
            bail!("Agent is busy; wait before selecting a context strategy");
        }
        let _flight = self.compaction_flight.lock().await;
        let plugins = self
            .plugins
            .as_ref()
            .context("context strategies require an active plugin host")?;
        let target = plugins.context_target(requested)?;
        let (snapshot, leaf) = {
            let session = self.session.lock().await;
            (
                session.history_snapshot(),
                session.get_leaf_id().map(str::to_owned),
            )
        };
        let entries = snapshot.await?;
        let branch = active_branch(&entries, leaf.as_deref())?;
        let context = project(&branch, leaf.as_deref(), requested);
        let model = self
            .context_model
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let messages = assemble_for_strategy(
            plugins,
            &target,
            context.messages,
            context.archive.as_ref(),
            model.as_ref(),
            leaf.as_deref(),
        )
        .await?;
        let loop_messages = messages
            .iter()
            .filter_map(super::super::session_message_to_loop)
            .collect();
        let mut session = self.session.lock().await;
        if session.get_leaf_id() != leaf.as_deref() {
            bail!("session branch changed while selecting a context strategy; try again");
        }
        self.agent
            .commit_idle_messages(loop_messages, || {
                plugins.commit_context_selection(&target, || {
                    let choice = pa_types::session::ContextSelection {
                        version: 1,
                        strategy: requested.to_owned(),
                    };
                    session.append_custom_entry(
                        pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE,
                        Some(serde_json::to_value(choice)?),
                    )?;
                    self.context_archive
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone_from(&context.archive);
                    Ok(())
                })
            })
            .await
    }
}
