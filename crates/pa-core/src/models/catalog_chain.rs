//! The process-shared live catalog chain: one [`ModelCatalog`] per models
//! dir, shared by every registry the process constructs. TS parity: the TS
//! daemon has a private catalog layer per session in one process; the Rust
//! daemon runs one worker per session, so the disk caches carry the state.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use pa_models::ModelCatalog;

use crate::auth::AuthStorage;

/// The core scheduler's refresh reasons; the catalog only receives freshness policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshTrigger {
    Startup,
    Hourly,
    PickerOpen,
    AuthChange,
}

impl RefreshTrigger {
    #[must_use]
    pub fn forced(self) -> bool {
        matches!(self, Self::Startup | Self::AuthChange)
    }
}

/// The models cache dir for a `models.json` path: `<agent-dir>/models` — both catalog caches live
/// beside `models.json`.
fn models_dir(models_json_path: &Path) -> PathBuf {
    models_json_path
        .parent()
        .unwrap_or(models_json_path)
        .join("models")
}

fn shared() -> &'static Mutex<HashMap<Option<PathBuf>, Arc<ModelCatalog>>> {
    static SHARED: OnceLock<Mutex<HashMap<Option<PathBuf>, Arc<ModelCatalog>>>> = OnceLock::new();
    SHARED.get_or_init(Default::default)
}

/// The process-shared catalog for `models_json_path` (`None` = an
/// in-memory registry, no disk caches).
///
/// # Panics
///
/// Panics if the shared catalog registry mutex is poisoned.
pub fn catalog_for(models_json_path: Option<&Path>) -> Arc<ModelCatalog> {
    let key = models_json_path.map(models_dir);
    let mut shared = shared().lock().unwrap();
    Arc::clone(
        shared
            .entry(key.clone())
            .or_insert_with(move || Arc::new(ModelCatalog::new(key))),
    )
}

/// Install `catalog` as the process-shared catalog for `models_json_path`
/// (hermetic tests: a catalog whose fetch URLs point at a local server).
///
/// # Panics
///
/// Panics if the shared catalog registry mutex is poisoned.
pub fn install_catalog(models_json_path: &Path, catalog: Arc<ModelCatalog>) {
    shared()
        .lock()
        .unwrap()
        .insert(Some(models_dir(models_json_path)), catalog);
}

/// The daemon's warm-up: the shared catalog for `agent_dir` with a forced
/// [`RefreshTrigger::Startup`] refresh fired (fire-and-forget: every
/// registry resolves the last-good chain immediately while the caches
/// fill in the background).
#[must_use]
pub fn startup_refresh(agent_dir: &Path) -> Arc<ModelCatalog> {
    let catalog = catalog_for(Some(&agent_dir.join("models.json")));
    let dir = agent_dir.to_path_buf();
    tokio::spawn(async move {
        let mut registry = super::registry::ModelRegistry::create(
            AuthStorage::create(&dir),
            dir.join("models.json"),
        );
        registry
            .refresh_available_models_with_trigger(crate::models::RefreshTrigger::Startup)
            .await;
    });
    catalog
}

/// The daemon's hourly refresh loop (one per process — the shared
/// catalog's own guard coalesces repeated calls). The credentials closure
/// re-reads auth from disk on every tick, so a login or logout between
/// ticks changes the next refresh's scope.
pub fn spawn_hourly_refresh(agent_dir: &Path) {
    static STARTED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    if !STARTED
        .get_or_init(Default::default)
        .lock()
        .expect("catalog scheduler lock")
        .insert(agent_dir.to_path_buf())
    {
        return;
    }
    let dir = agent_dir.to_path_buf();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        interval.tick().await;
        loop {
            interval.tick().await;
            let mut registry = super::registry::ModelRegistry::create(
                AuthStorage::create(&dir),
                dir.join("models.json"),
            );
            registry
                .refresh_available_models_with_trigger(crate::models::RefreshTrigger::Hourly)
                .await;
        }
    });
}
