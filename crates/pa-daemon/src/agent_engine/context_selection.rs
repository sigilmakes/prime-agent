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

pub(crate) trait InitialContextSelectionStore: Send + Sync {
    /// Capture only the current owner and header, never a new lease or writer.
    fn capture(&self, expected_file: &Path) -> anyhow::Result<ContextSelectionSource>;

    /// Revalidate or commit the already chosen route through that same owner.
    fn commit(
        &self,
        source: ContextSelectionSource,
        selection: ContextSelection,
    ) -> ContextSelectionCommitFuture;

    /// Bind live receipts to the same immutable owner used to build this mirror.
    /// Read-only/test adapters may retain the standalone behavior.
    fn live_owner(
        &self,
        _source: ContextSelectionSource,
    ) -> Option<std::sync::Arc<dyn pa_core::session_engine::canonical_live::CanonicalLiveOwner>>
    {
        None
    }

    /// The typed refusal for a read under a poisoned epoch, if this owner has one.
    ///
    /// A live epoch that an uncertain write poisoned may not serve its raw tail:
    /// the bytes are unclassified, so a reader would be reading a row that may be
    /// partially present. Owners with no epoch (or no canonical path) return `None`.
    fn canonical_read_refusal(&self) -> Option<pa_core::session::canonical::CanonicalStoreError> {
        None
    }

    /// Certify and retire a blocked SDK mirror's actual owner so the next build
    /// attaches a successor epoch. Refusal keeps the cached runtime and its
    /// blocked view; history is never rewritten.
    fn retire_quarantined_owner(&self) -> anyhow::Result<()> {
        anyhow::bail!("this session has no canonical owner to reopen")
    }
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
        Option<std::sync::Arc<dyn pa_core::session_engine::canonical_live::CanonicalLiveOwner>>,
    )> {
        let Some(path) = session_file else {
            let manager = session_dir.map_or_else(
                || pa_core::session::manager::SessionManager::in_memory(&cwd),
                |dir| {
                    pa_core::session::manager::SessionManager::in_memory_in_session_dir(&cwd, &dir)
                },
            );
            return Ok((manager, None, None));
        };
        let store = self
            .initial_context_selection_store
            .lock()
            .expect("initial context selection store lock")
            .clone()
            .ok_or_else(|| {
                anyhow::anyhow!("persistent session has no canonical context selection owner")
            })?;
        // Refuse the read BEFORE building anything: a poisoned epoch means an uncertain
        // write, and serving its tail as if classified is exactly the lie the drain
        // claim exists to prevent.
        if let Some(refusal) = store.canonical_read_refusal() {
            return Err(anyhow::Error::new(refusal));
        }
        let capture_store = std::sync::Arc::clone(&store);
        let (manager, source) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let source = capture_store.capture(&path)?;
            let dir = session_dir
                .or_else(|| source.path.parent().map(Path::to_owned))
                .ok_or_else(|| anyhow::anyhow!("canonical session has no parent directory"))?;
            let mut manager = pa_core::session::manager::SessionManager::in_memory_from_header(
                &cwd,
                &dir,
                source.header.clone(),
            )
            .map_err(anyhow::Error::msg)?;
            if let Some(window) =
                pa_core::session::window::WindowedSessionStore::open(&source.path)?
            {
                let header = window
                    .entries()
                    .iter()
                    .find_map(|entry| match entry {
                        pa_types::session::FileEntry::Header { header } => Some(header),
                        _ => None,
                    })
                    .ok_or_else(|| anyhow::anyhow!("verified canonical window has no header"))?;
                anyhow::ensure!(
                    *header == source.header,
                    "canonical header changed while building its mirror"
                );
                manager.adopt_window(window);
            } else {
                // Existing read-only full fallback; never SDK open/repair/persist.
                let file = crate::session_store::SessionFile::open(&source.path)?;
                anyhow::ensure!(
                    canonical_mirror_header(&file.header)? == source.header,
                    "canonical header changed while building its mirror"
                );
                let entries = file
                    .entries()
                    .iter()
                    .map(|entry| serde_json::from_value(serde_json::to_value(entry)?))
                    .collect::<Result<Vec<pa_types::session::FileEntry>, serde_json::Error>>()?;
                manager.adopt_entries(entries);
            }
            let current = capture_store.capture(&path)?;
            anyhow::ensure!(
                current.header == source.header
                    && current.path == source.path
                    && Weak::ptr_eq(&current.lease, &source.lease),
                "canonical session owner changed while building its mirror"
            );
            Ok((manager, source))
        })
        .await??;
        let live_owner = store.live_owner(source.clone());
        let commit: pa_core::session_engine::engine::InitialContextSelectionCommit =
            std::sync::Arc::new(move |selection| store.commit(source.clone(), selection));
        Ok((manager, Some(commit), live_owner))
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
