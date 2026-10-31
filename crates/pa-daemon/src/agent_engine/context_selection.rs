//! The daemon's canonical owner acknowledges the SDK's selected strategy.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Weak;

use pa_types::session::{ContextSelection, SessionHeader};

/// One immutable binding to the canonical owner that supplied the mirror.
/// A weak lease identity fences replacement without retaining another owner.
#[derive(Clone)]
pub(crate) struct ContextSelectionSource {
    pub(crate) header: SessionHeader,
    pub(crate) path: std::path::PathBuf,
    pub(crate) lease: Weak<crate::lease::SessionLease>,
}

/// `SessionFile`'s wire-header loader retains the known `type` tag in its flat
/// map. Deserialize the tagged entry form for the SDK header representation;
/// all actual header fields and unknown maps remain unchanged.
pub(crate) fn canonical_mirror_header(header: &SessionHeader) -> anyhow::Result<SessionHeader> {
    match serde_json::from_value::<pa_types::session::FileEntry>(
        crate::session_store::session_header_line(header),
    )? {
        pa_types::session::FileEntry::Header { header } => Ok(header),
        _ => anyhow::bail!("canonical session header is invalid"),
    }
}

pub(crate) type ContextSelectionCommitFuture =
    Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;

/// A log-transported session's mirror seed: the serving fold the store
/// already holds, replayed from the session log. The canonical capture/refusal
/// machinery is the other transport's read arm; this seed replaces it.
pub(crate) struct MirrorSeed {
    pub(crate) header: SessionHeader,
    pub(crate) entries: Vec<pa_types::session::FileEntry>,
}

pub(crate) trait InitialContextSelectionStore: Send + Sync {
    /// The transport switch's read arm: a log-transported serving fold, or
    /// `None` (the canonical capture/refusal path serves instead).
    fn mirror_seed(&self) -> anyhow::Result<Option<MirrorSeed>> {
        Ok(None)
    }

    fn entry_writer(&self, source: ContextSelectionSource) -> std::sync::Arc<dyn pa_core::session::manager::SessionEntryWriter>;

    /// Capture only the current owner and header, never a new lease or writer.
    fn capture(&self, expected_file: &Path) -> anyhow::Result<ContextSelectionSource>;

    /// Revalidate or commit the already chosen route through that same owner.
    fn commit(
        &self,
        source: ContextSelectionSource,
        selection: ContextSelection,
    ) -> ContextSelectionCommitFuture;

}

impl super::AgentSessionEngine {
    pub(crate) fn set_initial_context_selection_store(
        &self,
        store: std::sync::Arc<dyn InitialContextSelectionStore>,
    ) {
        *self
            .initial_context_selection_store
            .lock()
            .expect("initial context selection store lock") = Some(store);
    }

    /// Build the full canonical mirror before any session-scoped SDK service.
    /// The fileless path retains its existing in-memory semantics.
    pub(super) async fn build_session_mirror(
        &self,
        cwd: std::path::PathBuf,
        session_dir: Option<std::path::PathBuf>,
        session_file: Option<std::path::PathBuf>,
    ) -> anyhow::Result<(
        pa_core::session::manager::SessionManager,
        Option<pa_core::session_engine::engine::InitialContextSelectionCommit>,

    )> {
        let Some(path) = session_file else {
            let manager = session_dir.map_or_else(
                || pa_core::session::manager::SessionManager::in_memory(&cwd),
                |dir| {
                    pa_core::session::manager::SessionManager::in_memory_in_session_dir(&cwd, &dir)
                },
            );
            return Ok((manager, None));
        };
        let store = self
            .initial_context_selection_store
            .lock()
            .expect("initial context selection store lock")
            .clone()
            .ok_or_else(|| {
                anyhow::anyhow!("persistent session has no canonical context selection owner")
            })?;
        let seed = store.mirror_seed()?.ok_or_else(|| anyhow::anyhow!("session log fold is unavailable"))?;
        let source = store.capture(&path)?;
        let dir = session_dir.unwrap_or_else(|| {
            path.parent().map_or_else(|| std::path::PathBuf::from("."), Path::to_owned)
        });
        let mut manager = pa_core::session::manager::SessionManager::in_memory_from_header(
            &cwd, &dir, seed.header,
        ).map_err(anyhow::Error::msg)?;
        manager.adopt_entries(seed.entries);
        manager.set_entry_writer(store.entry_writer(source.clone()));
        let commit: pa_core::session_engine::engine::InitialContextSelectionCommit =
            std::sync::Arc::new(move |selection| store.commit(source.clone(), selection));
        Ok((manager, Some(commit)))
    }
}

/// Exact delivered-user IDs on the selected canonical suffix. This is identity
/// traversal, never content/timestamp deduplication. The SDK validates all roles
/// and the complete prefix before invoking any strategy or initial commit.
pub(super) fn admitted_user_ids(
    entries: &[pa_types::session::FileEntry],
    leaf: Option<&str>,
    boundary: &pa_core::session_engine::ContextStartupBoundary,
) -> anyhow::Result<Vec<String>> {
    use pa_core::session_engine::ContextStartupBoundary;
    use pa_types::session::{AgentMessage, FileEntry};
    if matches!(boundary, ContextStartupBoundary::Current) {
        return Ok(Vec::new());
    }
    let by_id: std::collections::HashMap<_, _> = entries
        .iter()
        .filter_map(|entry| entry.id().map(|id| (id, entry)))
        .collect();
    let mut current = leaf.map(str::to_owned);
    let mut seen = std::collections::HashSet::new();
    let mut ids = Vec::new();
    while let Some(id) = current {
        if matches!(boundary, ContextStartupBoundary::Leaf(target) if &id == target) {
            ids.reverse();
            return Ok(ids);
        }
        anyhow::ensure!(
            seen.insert(id.clone()),
            "canonical admission branch contains a cycle"
        );
        let entry = *by_id
            .get(id.as_str())
            .ok_or_else(|| anyhow::anyhow!("canonical admission branch is missing entry {id}"))?;
        if matches!(
            entry,
            FileEntry::Message {
                message: AgentMessage::User(_),
                ..
            }
        ) {
            ids.push(id);
        }
        current = entry.parent_id().map(str::to_owned);
    }
    anyhow::ensure!(
        matches!(boundary, ContextStartupBoundary::Empty),
        "prompt context boundary is absent from the retained transcript"
    );
    ids.reverse();
    Ok(ids)
}

#[cfg(test)]
#[path = "context_selection_tests.rs"]
mod tests;
