//! Initial strategy selection through the worker's durable log writer.

use std::sync::{Arc, Mutex};

use anyhow::{bail, ensure, Context, Result};
use pa_types::session::{ContextSelection, CONTEXT_SELECTION_CUSTOM_TYPE};
use serde_json::json;

use crate::agent_engine::{
    canonical_mirror_header, ContextSelectionCommitFuture, ContextSelectionSource,
    InitialContextSelectionStore, MirrorSeed,
};
use crate::lease::{canonical_session_path, SessionLease};
use crate::session_store::SessionFile;

use super::SessionCore;

pub(super) struct CoreInitialContextSelectionStore(pub(super) Arc<Mutex<SessionCore>>);

fn current_lease(core: &SessionCore) -> Result<Arc<SessionLease>> {
    ensure!(!core.shutdown_requested, "session is shutting down");
    let store = core
        .store
        .as_ref()
        .context("session store is unavailable")?;
    ensure!(
        !store.path.as_os_str().is_empty(),
        "initial context selection requires a persistent canonical store"
    );
    let lease = store
        .lease
        .as_ref()
        .context("canonical session store has no runtime lease")?;
    lease.ensure_held(&store.path)?;
    Ok(Arc::clone(lease))
}

fn validate_source(core: &SessionCore, source: &ContextSelectionSource) -> Result<()> {
    let lease = current_lease(core)?;
    let expected = source
        .lease
        .upgrade()
        .context("canonical session owner was replaced")?;
    ensure!(
        Arc::ptr_eq(&lease, &expected),
        "canonical session owner changed during startup"
    );
    let store = core.store.as_ref().expect("current_lease proved the store");
    ensure!(
        canonical_session_path(&store.path) == source.path
            && canonical_mirror_header(&store.header)? == source.header,
        "canonical session identity changed during startup"
    );
    Ok(())
}

fn commit_with(
    core: &mut SessionCore,
    source: &ContextSelectionSource,
    selection: &ContextSelection,
    write: impl FnOnce(&mut SessionFile, &ContextSelection) -> Result<()>,
) -> Result<()> {
    validate_source(core, source)?;
    ensure!(
        selection.version == 1 && selection.strategy.split_once('/').is_some(),
        "unsupported session context selection schema"
    );
    let store = core.store.as_mut().expect("validated canonical store");
    match store.latest_context_selection()? {
        Some(current) if current == *selection => return Ok(()),
        Some(_) => bail!("context strategy selection changed during startup"),
        None => {}
    }
    write(store, selection)?;
    Ok(())
}

fn write_selection(store: &mut SessionFile, selection: &ContextSelection) -> Result<()> {
    store.persist_entry(
        "custom",
        json!({"customType": CONTEXT_SELECTION_CUSTOM_TYPE, "data": selection}),
    )?;
    Ok(())
}

struct CoreSessionEntryWriter {
    core: Arc<Mutex<SessionCore>>,
    source: ContextSelectionSource,
}

impl pa_core::session::manager::SessionEntryWriter for CoreSessionEntryWriter {
    fn append(&self, entry: &pa_types::session::FileEntry) -> std::io::Result<()> {
        let write = || -> Result<()> {
            let mut core = self
                .core
                .lock()
                .map_err(|_| anyhow::anyhow!("session core lock poisoned"))?;
            validate_source(&core, &self.source)?;
            let row: crate::session_store::SessionEntry =
                serde_json::from_value(serde_json::to_value(entry)?)?;
            let store = core.store.as_mut().context("session store unavailable")?;
            if let Some(existing) = store.entry(&row.id) {
                ensure!(
                    serde_json::to_value(existing)? == serde_json::to_value(&row)?,
                    "session row ID collision"
                );
                return Ok(());
            }
            store.persist_built_log_entry(row)?;
            Ok(())
        };
        write().map_err(std::io::Error::other)
    }
}

impl InitialContextSelectionStore for CoreInitialContextSelectionStore {
    fn entry_writer(
        &self,
        source: ContextSelectionSource,
    ) -> Arc<dyn pa_core::session::manager::SessionEntryWriter> {
        Arc::new(CoreSessionEntryWriter {
            core: Arc::clone(&self.0),
            source,
        })
    }

    fn mirror_seed(&self) -> Result<Option<MirrorSeed>> {
        let core = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("session core lock poisoned"))?;
        let Some(store) = core.store.as_ref() else {
            return Ok(None);
        };
        if !store.is_log_served() {
            return Ok(None);
        }
        let entries = store
            .entries()
            .iter()
            .map(|entry| serde_json::from_value(serde_json::to_value(entry)?))
            .collect::<Result<Vec<pa_types::session::FileEntry>, serde_json::Error>>()?;
        Ok(Some(MirrorSeed {
            header: store.header.clone(),
            entries,
            leaf_id: store.leaf_id().map(str::to_owned),
        }))
    }

    fn capture(&self, expected_file: &std::path::Path) -> Result<ContextSelectionSource> {
        let core = self.0.lock().expect("session core lock");
        let lease = current_lease(&core)?;
        let store = core.store.as_ref().expect("current_lease proved the store");
        let path = canonical_session_path(&store.path);
        ensure!(
            canonical_session_path(expected_file) == path,
            "canonical session path changed before startup"
        );
        let source = ContextSelectionSource {
            header: canonical_mirror_header(&store.header)?,
            path,
            lease: Arc::downgrade(&lease),
        };
        validate_source(&core, &source)?;
        Ok(source)
    }

    /// The typed read refusal: a poisoned epoch may not serve its raw tail.

    fn commit(
        &self,
        source: ContextSelectionSource,
        selection: ContextSelection,
    ) -> ContextSelectionCommitFuture {
        let core = Arc::clone(&self.0);
        Box::pin(async move {
            // Locking and fsync run off the SDK reactor. No core guard spans
            // this await, nor any awaited builder or strategy callback.
            tokio::task::spawn_blocking(move || {
                let mut core = core.lock().expect("session core lock");
                commit_with(&mut core, &source, &selection, write_selection)
            })
            .await
            .context("canonical context selection commit task failed")?
        })
    }
}
