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
    ensure!(!core.shutdown_requested, "session is shutting down");
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
    fn read_tail(
        &self,
        cursor: &mut pa_core::session_log::ReadCursor,
    ) -> std::io::Result<Vec<pa_types::session::FileEntry>> {
        let mut read = || -> Result<Vec<pa_types::session::FileEntry>> {
            let mut core = self
                .core
                .lock()
                .map_err(|_| anyhow::anyhow!("session core lock poisoned"))?;
            validate_source(&core, &self.source)?;
            core.store
                .as_mut()
                .context("session store unavailable")?
                .read_sdk_tail(cursor)
        };
        read().map_err(std::io::Error::other)
    }

    fn append(&self, entry: &mut pa_types::session::FileEntry) -> std::io::Result<()> {
        let mut write = || -> Result<()> {
            let mut core = self
                .core
                .lock()
                .map_err(|_| anyhow::anyhow!("session core lock poisoned"))?;
            validate_source(&core, &self.source)?;
            let store = core.store.as_mut().context("session store unavailable")?;
            store.persist_sdk_entry(entry)?;
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
        let mut core = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("session core lock poisoned"))?;
        let Some(store) = core.store.as_mut() else {
            return Ok(None);
        };
        if !store.is_log_served() {
            return Ok(None);
        }
        store.observe_log_tail()?;
        let entries = store
            .entries()
            .iter()
            .map(|entry| serde_json::from_value(serde_json::to_value(entry)?))
            .collect::<Result<Vec<pa_types::session::FileEntry>, serde_json::Error>>()?;
        Ok(Some(MirrorSeed {
            header: store.header.clone(),
            entries,
            leaf_id: store.leaf_id().map(str::to_owned),
            cursor: store.sdk_read_cursor(),
        }))
    }

    fn capture(&self, expected_file: &std::path::Path) -> Result<ContextSelectionSource> {
        let core = self.0.lock().expect("session core lock");
        ensure!(!core.shutdown_requested, "session is shutting down");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admitted_terminal_rows_persist_during_shutdown_until_execution_lock_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shutdown.jsonl");
        let mut store = SessionFile::create("/fixture", None, 0);
        store.set_path(path.clone());
        store.rewrite().unwrap();
        store.lease = Some(Arc::new(
            crate::lease::acquire_runtime_session_lease(&path, dir.path()).unwrap(),
        ));
        let core = Arc::new(Mutex::new(SessionCore::test_core(
            Some(store),
            "/fixture".into(),
        )));
        let adapter = CoreInitialContextSelectionStore(Arc::clone(&core));
        let source = adapter.capture(&path).unwrap();
        let writer = adapter.entry_writer(source);
        core.lock().unwrap().shutdown_requested = true;
        assert!(
            adapter.capture(&path).is_err(),
            "new startup must stop at shutdown"
        );
        let mut parent = None;
        for (id, message) in [
            (
                "terminal-tool",
                json!({"role":"toolResult", "toolCallId":"admitted-tool", "toolName":"fixture", "content":[{"type":"text","text":"interrupted"}], "isError":true, "timestamp":1}),
            ),
            (
                "terminal-assistant",
                json!({"role":"assistant", "api":"openai-completions", "provider":"fixture", "model":"fixture", "content":[], "stopReason":"aborted", "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}, "timestamp":2}),
            ),
        ] {
            let mut entry = serde_json::from_value::<pa_types::session::FileEntry>(json!({"type":"message","id":id,"parentId":parent,"timestamp":"2026-10-31T00:00:00.000Z","message":message})).unwrap();
            writer.append(&mut entry).unwrap();
            parent = Some(id);
        }
        let reopened = SessionFile::open(&path).unwrap();
        assert_eq!(
            reopened
                .entries()
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["terminal-tool", "terminal-assistant"]
        );
        core.lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .lease
            .as_ref()
            .unwrap()
            .release();
        let mut late = serde_json::from_value::<pa_types::session::FileEntry>(json!({"type":"session_info","id":"late","parentId":"terminal-assistant","timestamp":"2026-10-31T00:00:00.000Z","name":"late"})).unwrap();
        assert!(
            writer.append(&mut late).is_err(),
            "released execution owner cannot publish"
        );
        assert!(SessionFile::open(&path).unwrap().entry("late").is_none());
    }
}
