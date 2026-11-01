//! Metadata persistence through the session manager.
use pa_types::session::{EntryBase, FileEntry};
use std::sync::Arc;

#[must_use]
pub fn draft_base() -> EntryBase {
    EntryBase {
        id: None,
        parent_id: None,
        timestamp: None,
        rest: pa_types::JsonMap::new(),
    }
}

/// Append one envelope-free metadata row.
///
/// # Errors
/// Returns append or flush failures.
pub async fn persist_metadata_entry(
    session: &Arc<tokio::sync::Mutex<super::manager::SessionManager>>,
    entry: FileEntry,
) -> anyhow::Result<String> {
    let mut manager = session.lock().await;
    let id = manager.append_metadata_intent(entry)?;
    manager.flush_now()?;
    Ok(id)
}
