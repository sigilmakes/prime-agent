//! Initial strategy acknowledgement through the worker's one canonical writer.

use std::sync::{Arc, Mutex};

use anyhow::{bail, ensure, Context, Result};
use pa_types::session::{ContextSelection, CONTEXT_SELECTION_CUSTOM_TYPE};
use serde_json::json;

use crate::agent_engine::{
    canonical_mirror_header, ContextSelectionCommitFuture, ContextSelectionSource,
    InitialContextSelectionStore,
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
    ensure!(
        store.path.is_file(),
        "canonical session file is unavailable"
    );
    let lease = store
        .lease
        .as_ref()
        .context("canonical session store has no runtime lease")?;
    ensure!(
        !core.context_selection_uncertain.as_ref().is_some_and(|uncertain| {
            std::sync::Weak::ptr_eq(uncertain, &Arc::downgrade(lease))
        }),
        "context selection write outcome is uncertain; reopen the canonical session before inference"
    );
    lease.with_owned_mutation(&store.path, || Ok(()))?;
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
    let header = crate::session_store::read_session_header(&store.path)
        .context("canonical session header is unavailable")?;
    ensure!(
        canonical_mirror_header(&header)? == source.header,
        "canonical session header changed during startup"
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
    // A write failure can follow a partial or complete durable append. Keep
    // uncertainty on the worker owner so a new build/adapter cannot retry it.
    if let Err(error) = write(store, selection) {
        core.context_selection_uncertain = Some(source.lease.clone());
        return Err(error.context("canonical context selection write outcome is uncertain; reopen the session before inference"));
    }
    Ok(())
}

fn write_selection(store: &mut SessionFile, selection: &ContextSelection) -> Result<()> {
    store.persist_entry(
        "custom",
        json!({"customType": CONTEXT_SELECTION_CUSTOM_TYPE, "data": selection}),
    )?;
    Ok(())
}

impl InitialContextSelectionStore for CoreInitialContextSelectionStore {
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
    fn canonical_read_refusal(&self) -> Option<pa_core::session::canonical::CanonicalStoreError> {
        let core = self.0.lock().expect("session core lock");
        let epoch = core.store.as_ref().and_then(|store| store.epoch.as_ref());
        crate::session_store::canonical_adapter::epoch_is_poisoned(epoch)
            .then(crate::session_store::canonical_adapter::refused_poisoned_read)
    }

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

    fn fixture() -> (
        tempfile::TempDir,
        Arc<Mutex<SessionCore>>,
        ContextSelectionSource,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionFile::create("/synthetic", None, 0);
        let path = dir.path().join("canonical.jsonl");
        store.set_path(path.clone());
        store.rewrite().unwrap();
        store.lease = Some(Arc::new(
            crate::lease::acquire_runtime_session_lease(&path, dir.path()).unwrap(),
        ));
        let core = Arc::new(Mutex::new(SessionCore::test_core(
            Some(store),
            "/synthetic".into(),
        )));
        let source = CoreInitialContextSelectionStore(Arc::clone(&core))
            .capture(&path)
            .unwrap();
        (dir, core, source)
    }

    fn read_epoch() -> Arc<pa_core::session_engine::canonical_coordinator::CanonicalEpoch> {
        use pa_core::session::canonical::{
            CanonicalCursor, CanonicalFrontier, CanonicalOwnerScope,
        };
        Arc::new(
            pa_core::session_engine::canonical_coordinator::CanonicalEpoch::genesis(
                &CanonicalCursor {
                    owner: CanonicalOwnerScope::new(),
                    session_id: "read-refusal-session".to_owned(),
                    source_path: std::path::PathBuf::from("/private/read-refusal.jsonl"),
                    raw_source_generation: [3; 32],
                    header_certificate: [4; 32],
                    canonical_revision: 0,
                    branch_revision: 1,
                    selected_leaf: None,
                    frontier: CanonicalFrontier {
                        bytes: 5,
                        rows: 1,
                        prefix_certificate: [6; 32],
                    },
                },
                Arc::new(String::from("run")),
                Arc::new(String::from("model")),
            )
            .expect("epoch"),
        )
    }

    fn choice(route: &str) -> ContextSelection {
        ContextSelection {
            version: 1,
            strategy: route.into(),
        }
    }

    fn choices(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|row| {
                row["type"] == "custom" && row["customType"] == CONTEXT_SELECTION_CUSTOM_TYPE
            })
            .collect()
    }

    /// The read path's one question: a poisoned live epoch refuses, a clean one and
    /// an absent one do not. `build_session_mirror` asks exactly this before it builds
    /// anything, so the refusal cannot be skipped by a later branch.
    #[test]
    fn only_a_poisoned_epoch_refuses_the_read() {
        let (_dir, core, _source) = fixture();
        let store = CoreInitialContextSelectionStore(Arc::clone(&core));
        assert!(
            store.canonical_read_refusal().is_none(),
            "no epoch attached is not a refusal: the pre-genesis window still reads"
        );

        let epoch = read_epoch();
        {
            let mut core = core.lock().unwrap();
            core.store.as_mut().unwrap().epoch = Some(
                crate::session_store::canonical_adapter::EpochHandle(Arc::clone(&epoch)),
            );
        }
        assert!(
            store.canonical_read_refusal().is_none(),
            "a clean live epoch reads normally"
        );

        // An unclassified flight drop is the uncertain write.
        drop(
            epoch
                .begin_prepare_append(
                    pa_core::session::canonical::CanonicalIoStage::PreparePreflight,
                )
                .expect("flight"),
        );
        let refusal = store
            .canonical_read_refusal()
            .expect("a poisoned epoch refuses the read");
        assert_eq!(
            refusal.source,
            pa_core::session::canonical::CanonicalSourceDisposition::NeedsReconcile
        );
        assert_eq!(
            refusal.detail,
            "canonical epoch is poisoned by an uncertain write"
        );
    }

    #[tokio::test]
    async fn fresh_ack_is_canonical_and_same_choice_appends_nothing() {
        let (_dir, core, source) = fixture();
        let store = CoreInitialContextSelectionStore(Arc::clone(&core));
        store
            .commit(source.clone(), choice("policy/a"))
            .await
            .unwrap();
        let bytes = std::fs::read(&source.path).unwrap();
        store
            .commit(source.clone(), choice("policy/a"))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&source.path).unwrap(), bytes);
        let rows = choices(&source.path);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["data"],
            json!({"version": 1, "strategy": "policy/a"})
        );
        assert!(rows[0]["id"].as_str().is_some_and(|id| !id.is_empty()));
        assert_eq!(
            core.lock()
                .unwrap()
                .store
                .as_ref()
                .unwrap()
                .latest_context_selection()
                .unwrap(),
            Some(choice("policy/a"))
        );
        let reopened = SessionFile::open(&source.path).unwrap();
        assert_eq!(
            reopened.latest_context_selection().unwrap(),
            Some(choice("policy/a"))
        );
    }

    #[tokio::test]
    async fn conflicting_or_invalid_latest_choice_is_not_replaced() {
        let (_dir, core, source) = fixture();
        let store = CoreInitialContextSelectionStore(Arc::clone(&core));
        store
            .commit(source.clone(), choice("policy/a"))
            .await
            .unwrap();
        let before = std::fs::read(&source.path).unwrap();
        assert!(store
            .commit(source.clone(), choice("policy/b"))
            .await
            .is_err());
        assert_eq!(std::fs::read(&source.path).unwrap(), before);
        core.lock().unwrap().store.as_mut().unwrap().persist_entry("custom", json!({
            "customType": CONTEXT_SELECTION_CUSTOM_TYPE, "data": {"version": 9, "strategy": "policy/a"}
        })).unwrap();
        let invalid = std::fs::read(&source.path).unwrap();
        assert!(store
            .commit(source.clone(), choice("policy/a"))
            .await
            .is_err());
        assert_eq!(std::fs::read(&source.path).unwrap(), invalid);
    }

    #[test]
    fn write_error_before_or_after_append_poison_survives_new_adapter() {
        for after_append in [false, true] {
            let (_dir, core, source) = fixture();
            let error = commit_with(
                &mut core.lock().unwrap(),
                &source,
                &choice("policy/a"),
                |store, selection| {
                    if after_append {
                        write_selection(store, selection)?;
                    }
                    bail!("injected canonical acknowledgement failure")
                },
            )
            .unwrap_err();
            assert!(error.to_string().contains("write outcome is uncertain"));
            let rows_before_retry = choices(&source.path).len();
            assert_eq!(rows_before_retry, usize::from(after_append));
            let new_adapter = CoreInitialContextSelectionStore(Arc::clone(&core));
            assert!(new_adapter
                .capture(&source.path)
                .err()
                .unwrap()
                .to_string()
                .contains("uncertain"));
            assert!(commit_with(
                &mut core.lock().unwrap(),
                &source,
                &choice("policy/a"),
                write_selection
            )
            .is_err());
            assert_eq!(choices(&source.path).len(), rows_before_retry);
        }
    }

    #[tokio::test]
    async fn stale_epoch_refuses_and_reopened_owner_revalidates_actual_row() {
        let (dir, core, source) = fixture();
        commit_with(
            &mut core.lock().unwrap(),
            &source,
            &choice("policy/a"),
            |store, selection| {
                write_selection(store, selection)?;
                bail!("acknowledgement lost after complete canonical append")
            },
        )
        .unwrap_err();
        let path = source.path.clone();
        let previous = {
            let mut core = core.lock().unwrap();
            let lease = core.store.as_mut().unwrap().lease.take().unwrap();
            lease.release();
            let mut reopened = SessionFile::open(&path).unwrap();
            reopened.lease = Some(Arc::new(
                crate::lease::acquire_runtime_session_lease(&path, dir.path()).unwrap(),
            ));
            core.store.replace(reopened)
        };
        drop(previous);
        let adapter = CoreInitialContextSelectionStore(Arc::clone(&core));
        let bytes = std::fs::read(&path).unwrap();
        assert!(adapter.commit(source, choice("policy/a")).await.is_err());
        let fresh = adapter.capture(&path).unwrap();
        adapter.commit(fresh, choice("policy/a")).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[tokio::test]
    async fn changed_header_path_shutdown_and_released_lease_refuse() {
        let (_dir, core, source) = fixture();
        let adapter = CoreInitialContextSelectionStore(Arc::clone(&core));
        let mut wrong = source.clone();
        wrong.header.id = "foreign-session".into();
        assert!(adapter.commit(wrong, choice("policy/a")).await.is_err());
        let mut wrong = source.clone();
        wrong.path = wrong.path.with_file_name("foreign.jsonl");
        assert!(adapter.commit(wrong, choice("policy/a")).await.is_err());
        core.lock().unwrap().shutdown_requested = true;
        assert!(adapter
            .commit(source.clone(), choice("policy/a"))
            .await
            .is_err());
        core.lock().unwrap().shutdown_requested = false;
        core.lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .lease
            .as_ref()
            .unwrap()
            .release();
        assert!(adapter
            .commit(source.clone(), choice("policy/a"))
            .await
            .is_err());
        assert!(choices(&source.path).is_empty());
    }
}
