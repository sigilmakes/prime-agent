//! Scope-keyed last-good catalogs with coalesced refresh and disk coherence.
//!
//! A source owns one path. Scope changes discard the active view. Persistent
//! locks serialize publishers, while durable scoped tombstones invalidate even
//! writers that fetched before a revocation. Readers observe external writes.

use std::fs::File;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fetch::{CatalogFetcher, FetchError, FetchOutcome, MAX_RESPONSE_BYTES};
use crate::offline::is_catalog_offline;
use crate::CATALOG_REFRESH_INTERVAL_MS;

/// The unauthenticated scope shared by the aggregate catalog URLs.
pub const PUBLIC_SCOPE: &str = "public";

/// Parser turning a fetched payload into the cache's value type.
pub type CatalogParse<T> = Arc<dyn Fn(&serde_json::Value, &str) -> Result<T, String> + Send + Sync>;

/// Merge validated partial metadata with a previous provider-owned payload.
/// The callback validates the input envelope and prunes absent IDs. The cache
/// validates the merged output and persists it, not just its derived models.
pub type CatalogMergePayload = Arc<
    dyn Fn(&serde_json::Value, Option<&serde_json::Value>) -> Result<serde_json::Value, String>
        + Send
        + Sync,
>;

/// An optional provider-owned fetch (for discovery protocols with multiple
/// endpoints). Its result uses the same validation and publication machinery.
pub type CatalogFetchPayload = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<serde_json::Value, FetchError>> + Send>>
        + Send
        + Sync,
>;

/// Credential-free failure categories. Never contain URLs, headers, parser
/// diagnostics, file paths, response bodies, or underlying transport errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum CatalogRefreshError {
    #[error("Invalid catalog JSON")]
    InvalidJson,
    #[error("Invalid catalog payload")]
    InvalidPayload,
    #[error("Catalog request failed with status {status}")]
    Http { status: u16 },
    #[error("Catalog refresh failed")]
    RequestFailed,
    #[error("Catalog cache persistence failed")]
    PersistenceFailed,
}

/// Result category of one refresh (including a shared/coalesced attempt).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CatalogRefreshOutcome {
    Updated,
    NotModified,
    Cached,
    Offline,
    Error { error: CatalogRefreshError },
    MemoryOnly,
    Revoked { status: u16 },
    Cancelled,
}

/// A refresh result with last-good models on recoverable failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRefreshReport<T> {
    pub models: Option<T>,
    pub outcome: CatalogRefreshOutcome,
}

impl<T> CatalogRefreshReport<T> {
    fn cancelled() -> Self {
        Self {
            models: None,
            outcome: CatalogRefreshOutcome::Cancelled,
        }
    }
}

/// The current state of a source's active scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogCacheState {
    Idle,
    Refreshing,
    Fresh,
    Stale,
    Error,
}

/// Where the active snapshot came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogCacheSource {
    None,
    Disk,
    Network,
    Memory,
}

/// Credential-free freshness information for one source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogCacheStatus {
    pub state: CatalogCacheState,
    pub source: CatalogCacheSource,
    pub fetched_at: Option<u64>,
    pub checked_at: Option<u64>,
    pub last_attempt: Option<u64>,
    pub error: Option<CatalogRefreshError>,
}

#[derive(Clone)]
struct Snapshot<T> {
    file: SnapshotFile,
    models: T,
}

/// Older snapshots have neither checkedAt nor revision. Both remain readable.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotFile {
    url: String,
    scope: String,
    fetched_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checked_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
    /// The tombstone epoch observed before this request began. Tombstones stay
    /// on disk so a writer with no prior snapshot cannot resurrect a revocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revocation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    payload: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
struct RevocationFile {
    url: String,
    scope: String,
    revision: String,
}

/// Compare content, not mtime or wall-clock ordering: independent processes can
/// complete in one millisecond, and revocation can happen without a snapshot.
#[derive(Clone, Default, PartialEq, Eq)]
struct DiskVersion {
    snapshot: Option<[u8; 32]>,
    revoked: Option<[u8; 32]>,
}

struct DiskView {
    version: DiskVersion,
    snapshot: Option<Vec<u8>>,
    revoked: Option<Vec<u8>>,
    unreadable_revocation: bool,
}

impl DiskView {
    fn read(path: &Path, url: &str, scope: &str) -> Self {
        let snapshot = std::fs::read(path).ok();
        // Read the tombstone AFTER the payload so revocation racing a read
        // hides a snapshot that was just read from disk.
        let (revoked, unreadable_revocation) =
            match std::fs::read(revocation_path(path, url, scope)) {
                Ok(bytes) => (Some(bytes), false),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, false),
                Err(_) => (None, true),
            };
        Self {
            version: DiskVersion {
                snapshot: snapshot
                    .as_deref()
                    .map(|bytes| Sha256::digest(bytes).into()),
                revoked: if unreadable_revocation {
                    Some([0; 32])
                } else {
                    revoked.as_deref().map(|bytes| Sha256::digest(bytes).into())
                },
            },
            snapshot,
            revoked,
            unreadable_revocation,
        }
    }

    fn revocation(&self, url: &str, scope: &str) -> Option<String> {
        let revoked: RevocationFile = serde_json::from_slice(self.revoked.as_deref()?).ok()?;
        (revoked.url == url && revoked.scope == scope).then_some(revoked.revision)
    }
}

struct InFlight<T> {
    result: OnceLock<CatalogRefreshReport<T>>,
    notify: tokio::sync::Notify,
}

impl<T> InFlight<T> {
    fn new() -> Self {
        Self {
            result: OnceLock::new(),
            notify: tokio::sync::Notify::new(),
        }
    }
}

struct CacheState<T> {
    scope: Option<String>,
    snapshot: Option<Snapshot<T>>,
    observed_disk: DiskVersion,
    pending: Option<Arc<InFlight<T>>>,
    last_attempt: Option<u64>,
    generation: u64,
    source: CatalogCacheSource,
    error: Option<CatalogRefreshError>,
    revoked: bool,
}

/// Options for [`CatalogCache::refresh`].
#[derive(Default)]
pub struct RefreshOptions {
    /// Skip the hourly gating window (startup, picker open, auth change).
    pub force: bool,
    /// Extra request headers. Never included in results or status.
    pub headers: Vec<(String, String)>,
    /// Additional staleness guard consulted before applying any result.
    pub is_current: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Provider-specific validation/normalization, before parsing the merged payload.
    pub merge_payload: Option<CatalogMergePayload>,
    /// Override the ordinary single-URL HTTP request with provider discovery.
    pub fetch_payload: Option<CatalogFetchPayload>,
}

/// The last-good cache for one catalog source and its active auth scope.
pub struct CatalogCache<T> {
    url: Arc<str>,
    cache_path: Option<PathBuf>,
    fetcher: Arc<CatalogFetcher>,
    parse: CatalogParse<T>,
    state: Mutex<CacheState<T>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
        .unwrap_or_default()
}

fn revision() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{nanos}-{count}", std::process::id())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn revocation_path(path: &Path, url: &str, scope: &str) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(url.as_bytes());
    digest.update([0]);
    digest.update(scope.as_bytes());
    sidecar(
        path,
        &format!(".revoked-{}", hex::encode(digest.finalize())),
    )
}

impl<T: Clone + Send + Sync + 'static> CatalogCache<T> {
    pub fn new(
        url: &str,
        cache_path: Option<PathBuf>,
        fetcher: Arc<CatalogFetcher>,
        parse: CatalogParse<T>,
    ) -> Self {
        Self {
            url: Arc::from(url),
            cache_path,
            fetcher,
            parse,
            state: Mutex::new(CacheState {
                scope: None,
                snapshot: None,
                observed_disk: DiskVersion::default(),
                pending: None,
                last_attempt: None,
                generation: 0,
                source: CatalogCacheSource::None,
                error: None,
                revoked: false,
            }),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn cache_path(&self) -> Option<&Path> {
        self.cache_path.as_deref()
    }

    /// Serve validated last-good data, reloading changed disk state even when
    /// the scope is unchanged. Unchanged disk state preserves memory-only data.
    /// A changed scope or disk version invalidates old in-flight publications.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    pub fn get(&self, scope: &str) -> Option<T> {
        let mut state = self.state.lock().unwrap();
        let disk = self
            .cache_path
            .as_deref()
            .map(|path| DiskView::read(path, &self.url, scope));
        let changed = state.scope.as_deref() != Some(scope)
            || disk
                .as_ref()
                .is_some_and(|disk| disk.version != state.observed_disk);
        if changed {
            state.scope = Some(scope.to_string());
            state.generation += 1;
            state.pending = None;
            state.last_attempt = None;
            state.error = None;
            state.snapshot = disk
                .as_ref()
                .and_then(|disk| self.read_snapshot(disk, scope));
            state.revoked = state.snapshot.is_none()
                && disk
                    .as_ref()
                    .is_some_and(|disk| disk.unreadable_revocation || disk.revoked.is_some());
            state.observed_disk = disk.map_or_else(DiskVersion::default, |disk| disk.version);
            state.source = if state.snapshot.is_some() {
                CatalogCacheSource::Disk
            } else {
                CatalogCacheSource::None
            };
        }
        state
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.models.clone())
    }

    /// Force a disk reload and invalidate any prior in-flight publication.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    pub fn reload_from_disk(&self, scope: &str) -> Option<T> {
        self.state.lock().unwrap().scope = None;
        self.get(scope)
    }

    /// The stored scope for this URL, including older-format snapshots. Used
    /// only to seed a fresh process's auth-change comparison.
    pub fn stored_scope(&self) -> Option<String> {
        let bytes = std::fs::read(self.cache_path.as_deref()?).ok()?;
        let stored: SnapshotFile = serde_json::from_slice(&bytes).ok()?;
        (stored.url == self.url.as_ref()).then_some(stored.scope)
    }

    /// Whether this scope was revoked and has no validated newer snapshot.
    /// Use this to prevent an older private-cache format from restoring access
    /// after a rejection. A failed local tombstone write still returns true.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    pub fn has_revocation(&self, scope: &str) -> bool {
        self.get(scope);
        self.state.lock().unwrap().revoked
    }

    /// Drop this scope and invalidate old requests across processes. Other
    /// scopes' snapshots are not removed. The durable tombstone is written
    /// before trying the publisher lock, so a held lock cannot hide revocation.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    pub fn clear(&self, scope: &str) {
        self.revoke(scope);
    }

    fn revoke(&self, scope: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.scope.as_deref() != Some(scope) {
            return true;
        }
        state.generation += 1;
        state.pending = None;
        state.snapshot = None;
        state.revoked = true;
        state.source = CatalogCacheSource::None;
        state.error = None;
        state.last_attempt = None;
        let Some(path) = &self.cache_path else {
            return true;
        };
        let revocation = RevocationFile {
            url: self.url.to_string(),
            scope: scope.to_string(),
            revision: revision(),
        };
        let persisted_marker = serde_json::to_vec(&revocation).is_ok_and(|bytes| {
            write_atomic(&revocation_path(path, &self.url, scope), &bytes).is_ok()
        });
        // Never unlink the lock file: its inode must stay shared by all users.
        // A failed try-lock is safe because the tombstone hides the old payload
        // and any publisher bound to the previous revocation epoch.
        if let Some(_lock) = open_lock(path).ok().filter(|lock| lock.try_lock().is_ok()) {
            let disk = DiskView::read(path, &self.url, scope);
            let matches = disk.snapshot.as_deref().is_some_and(|bytes| {
                serde_json::from_slice::<SnapshotFile>(bytes)
                    .is_ok_and(|stored| stored.url == self.url.as_ref() && stored.scope == scope)
            });
            if matches {
                if let Err(error) = std::fs::remove_file(path) {
                    tracing::debug!(%error, "catalog cache removal failed");
                }
            }
        }
        state.observed_disk = DiskView::read(path, &self.url, scope).version;
        // Deletion alone is insufficient: an old fetch with no previous
        // snapshot also needs a durable epoch to compare before publication.
        if !persisted_marker {
            state.error = Some(CatalogRefreshError::PersistenceFailed);
        }
        persisted_marker
    }

    /// Freshness and source information without credentials or raw errors.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    pub fn status(&self, scope: &str) -> CatalogCacheStatus {
        self.get(scope);
        let state = self.state.lock().unwrap();
        let fetched_at = state
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.file.fetched_at);
        let checked_at = state
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.file.checked_at.unwrap_or(snapshot.file.fetched_at));
        let status = if state.pending.is_some() {
            CatalogCacheState::Refreshing
        } else if state.error.is_some() {
            CatalogCacheState::Error
        } else if state.snapshot.is_none() {
            CatalogCacheState::Idle
        } else if checked_at
            .is_some_and(|at| now_ms().saturating_sub(at) < CATALOG_REFRESH_INTERVAL_MS)
        {
            CatalogCacheState::Fresh
        } else {
            CatalogCacheState::Stale
        };
        CatalogCacheStatus {
            state: status,
            source: state.source,
            fetched_at,
            checked_at,
            last_attempt: state.last_attempt,
            error: state.error,
        }
    }

    /// Legacy models-only API. See [`Self::refresh_report`] for attempt status.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    pub async fn refresh(&self, scope: &str, opts: RefreshOptions) -> Option<T> {
        self.refresh_report(scope, opts).await.models
    }

    /// Refresh, coalescing one scope's pending request. Hourly gating, offline
    /// behavior and last-good fallback remain identical to `refresh`.
    /// Dropping a coalesced waiter does not cancel the driving request. Dropping
    /// the driver resolves all its waiters as cancelled and releases its gate.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned or an internal gate settles twice.
    pub async fn refresh_report(
        &self,
        scope: &str,
        opts: RefreshOptions,
    ) -> CatalogRefreshReport<T> {
        enum Gate<T> {
            Coalesced(Arc<InFlight<T>>),
            Gated,
            Start(Arc<InFlight<T>>, Option<Box<Snapshot<T>>>, u64, DiskVersion),
        }
        let cached = self.get(scope);
        if opts.is_current.as_ref().is_some_and(|check| !check()) {
            return CatalogRefreshReport::cancelled();
        }
        if is_catalog_offline() {
            return CatalogRefreshReport {
                models: cached,
                outcome: CatalogRefreshOutcome::Offline,
            };
        }
        let gate = {
            let mut state = self.state.lock().unwrap();
            if let Some(pending) = state.pending.clone() {
                Gate::Coalesced(pending)
            } else {
                let checked_at = state.last_attempt.or_else(|| {
                    state.snapshot.as_ref().map(|snapshot| {
                        snapshot.file.checked_at.unwrap_or(snapshot.file.fetched_at)
                    })
                });
                let now = now_ms();
                if !opts.force
                    && checked_at
                        .is_some_and(|at| now >= at && now - at < CATALOG_REFRESH_INTERVAL_MS)
                {
                    Gate::Gated
                } else {
                    state.last_attempt = Some(now);
                    state.error = None;
                    let inflight = Arc::new(InFlight::new());
                    state.pending = Some(Arc::clone(&inflight));
                    Gate::Start(
                        inflight,
                        state.snapshot.clone().map(Box::new),
                        state.generation,
                        state.observed_disk.clone(),
                    )
                }
            }
        };
        let (inflight, previous, generation, disk_version) = match gate {
            Gate::Coalesced(pending) => return await_inflight(pending).await,
            Gate::Gated => {
                return CatalogRefreshReport {
                    models: cached,
                    outcome: CatalogRefreshOutcome::Cached,
                }
            }
            Gate::Start(inflight, previous, generation, version) => (
                inflight,
                previous.map(|snapshot| *snapshot),
                generation,
                version,
            ),
        };
        let _driver = SettleOnDrop {
            cache: self,
            inflight: Arc::clone(&inflight),
        };
        let result = self
            .drive_refresh(scope, &opts, previous, generation, &disk_version)
            .await;
        {
            let mut state = self.state.lock().unwrap();
            if state
                .pending
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(pending, &inflight))
            {
                state.pending = None;
            }
        }
        if inflight.result.set(result.clone()).is_err() {
            debug_assert!(false, "catalog refresh settled twice");
        }
        inflight.notify.notify_waiters();
        result
    }

    fn is_current(&self, scope: &str, generation: u64, opts: &RefreshOptions) -> bool {
        if opts.is_current.as_ref().is_some_and(|check| !check()) {
            return false;
        }
        let state = self.state.lock().unwrap();
        state.scope.as_deref() == Some(scope) && state.generation == generation
    }

    // Keep HTTP, 304, merge validation, and revocation transitions together;
    // splitting this protocol into single-use helpers obscures its guards.
    #[allow(clippy::too_many_lines)]
    async fn drive_refresh(
        &self,
        scope: &str,
        opts: &RefreshOptions,
        previous: Option<Snapshot<T>>,
        generation: u64,
        version: &DiskVersion,
    ) -> CatalogRefreshReport<T> {
        let etag = previous
            .as_ref()
            .and_then(|snapshot| snapshot.file.etag.as_deref());
        let result = if let Some(fetch) = &opts.fetch_payload {
            match fetch().await {
                Ok(payload) => serde_json::to_vec(&payload)
                    .map_err(|_| FetchError::Transport {
                        message: "Invalid catalog payload".into(),
                    })
                    .and_then(|body| {
                        if body.len() > MAX_RESPONSE_BYTES {
                            Err(FetchError::Transport {
                                message: "Catalog is too large".into(),
                            })
                        } else {
                            Ok(FetchOutcome::Fresh { body, etag: None })
                        }
                    }),
                Err(error) => Err(error),
            }
        } else {
            self.fetcher
                .fetch_with(&self.url, etag, &opts.headers)
                .await
        };
        if !self.is_current(scope, generation, opts) {
            return CatalogRefreshReport::cancelled();
        }
        let (mut snapshot, outcome) = match result {
            Ok(FetchOutcome::NotModified) => {
                let Some(mut snapshot) = previous else {
                    return self.keep_last_good(
                        scope,
                        generation,
                        opts,
                        CatalogRefreshError::InvalidPayload,
                    );
                };
                snapshot.file.checked_at = Some(now_ms());
                (snapshot, CatalogRefreshOutcome::NotModified)
            }
            Ok(FetchOutcome::Fresh { body, etag }) => {
                if body.len() > MAX_RESPONSE_BYTES {
                    return self.keep_last_good(
                        scope,
                        generation,
                        opts,
                        CatalogRefreshError::InvalidPayload,
                    );
                }
                let received: serde_json::Value = match serde_json::from_slice(&body) {
                    Ok(payload) => payload,
                    Err(_) => {
                        return self.keep_last_good(
                            scope,
                            generation,
                            opts,
                            CatalogRefreshError::InvalidJson,
                        )
                    }
                };
                // The provider merge validates/normalizes its own envelope.
                // Partial records may not parse alone (for example an unknown
                // OpenRouter ID whose capabilities exist only in last-good).
                let payload = if let Some(merge) = &opts.merge_payload {
                    let merged = merge(
                        &received,
                        previous.as_ref().map(|snapshot| &snapshot.file.payload),
                    );
                    let Ok(merged) = merged else {
                        return self.keep_last_good(
                            scope,
                            generation,
                            opts,
                            CatalogRefreshError::InvalidPayload,
                        );
                    };
                    if serde_json::to_vec(&merged)
                        .map_or(true, |bytes| bytes.len() > MAX_RESPONSE_BYTES)
                    {
                        return self.keep_last_good(
                            scope,
                            generation,
                            opts,
                            CatalogRefreshError::InvalidPayload,
                        );
                    }
                    merged
                } else {
                    received
                };
                let Ok(models) = (self.parse)(&payload, scope) else {
                    return self.keep_last_good(
                        scope,
                        generation,
                        opts,
                        CatalogRefreshError::InvalidPayload,
                    );
                };
                (
                    Snapshot {
                        file: SnapshotFile {
                            url: self.url.to_string(),
                            scope: scope.to_string(),
                            fetched_at: now_ms(),
                            checked_at: Some(now_ms()),
                            revision: None,
                            revocation: None,
                            etag,
                            payload,
                        },
                        models,
                    },
                    CatalogRefreshOutcome::Updated,
                )
            }
            Err(error) => {
                if let Some(status) = error
                    .status()
                    .filter(|status| scope != PUBLIC_SCOPE && matches!(status, 401 | 403))
                {
                    self.revoke(scope);
                    let mut state = self.state.lock().unwrap();
                    // Keep the attempt gate after rejection; non-forced calls
                    // must not hammer an unauthorized endpoint every frame.
                    state.last_attempt = Some(now_ms());
                    if state.error.is_none() {
                        state.error = Some(CatalogRefreshError::Http { status });
                    }
                    return CatalogRefreshReport {
                        models: None,
                        outcome: CatalogRefreshOutcome::Revoked { status },
                    };
                }
                let error = error
                    .status()
                    .map_or(CatalogRefreshError::RequestFailed, |status| {
                        CatalogRefreshError::Http { status }
                    });
                return self.keep_last_good(scope, generation, opts, error);
            }
        };
        snapshot.file.revision = Some(revision());
        // Lock acquisition is bounded and asynchronous. The render-side get()
        // never waits on the publisher lock, and no mutex lives across await.
        let lock = if let Some(path) = &self.cache_path {
            acquire_lock(path).await.ok()
        } else {
            None
        };
        if !self.is_current(scope, generation, opts) {
            return CatalogRefreshReport::cancelled();
        }
        self.store_snapshot(scope, generation, version, snapshot, outcome, lock.as_ref())
    }

    fn keep_last_good(
        &self,
        scope: &str,
        generation: u64,
        opts: &RefreshOptions,
        error: CatalogRefreshError,
    ) -> CatalogRefreshReport<T> {
        // Observe revocation/external updates even on an HTTP or parse failure.
        self.get(scope);
        if !self.is_current(scope, generation, opts) {
            return CatalogRefreshReport::cancelled();
        }
        let mut state = self.state.lock().unwrap();
        state.error = Some(error);
        CatalogRefreshReport {
            models: state
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.models.clone()),
            outcome: CatalogRefreshOutcome::Error { error },
        }
    }

    fn store_snapshot(
        &self,
        scope: &str,
        generation: u64,
        version: &DiskVersion,
        mut snapshot: Snapshot<T>,
        outcome: CatalogRefreshOutcome,
        lock: Option<&File>,
    ) -> CatalogRefreshReport<T> {
        let mut state = self.state.lock().unwrap();
        if state.scope.as_deref() != Some(scope) || state.generation != generation {
            return CatalogRefreshReport::cancelled();
        }
        let disk = self
            .cache_path
            .as_deref()
            .map(|path| DiskView::read(path, &self.url, scope));
        if let Some(disk) = &disk {
            if &disk.version != version {
                // A newer publication or revocation always wins, irrespective
                // of wall-clock granularity and even when it deleted the file.
                state.snapshot = self.read_snapshot(disk, scope);
                state.revoked = state.snapshot.is_none()
                    && (disk.unreadable_revocation || disk.revoked.is_some());
                state.observed_disk = disk.version.clone();
                state.source = if state.snapshot.is_some() {
                    CatalogCacheSource::Disk
                } else {
                    CatalogCacheSource::None
                };
                return CatalogRefreshReport {
                    models: state
                        .snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.models.clone()),
                    outcome: if state.snapshot.is_some() {
                        CatalogRefreshOutcome::Cached
                    } else {
                        CatalogRefreshOutcome::Cancelled
                    },
                };
            }
            snapshot.file.revocation = disk.revocation(&self.url, scope);
        }
        let persisted = if let (Some(path), Some(_lock)) = (&self.cache_path, lock) {
            serde_json::to_vec(&snapshot.file).is_ok_and(|bytes| write_atomic(path, &bytes).is_ok())
        } else {
            false
        };
        if let Some(path) = self.cache_path.as_ref().filter(|_| persisted) {
            let disk = DiskView::read(path, &self.url, scope);
            // Tombstones do not wait for the lock. Detect revocation that
            // raced the atomic rename, rather than serving a rejected result.
            state.snapshot = self.read_snapshot(&disk, scope);
            state.revoked =
                state.snapshot.is_none() && (disk.unreadable_revocation || disk.revoked.is_some());
            state.observed_disk = disk.version;
            if state.snapshot.is_none() {
                state.source = CatalogCacheSource::None;
                return CatalogRefreshReport::cancelled();
            }
        } else {
            state.snapshot = Some(snapshot);
            state.revoked = false;
        }
        state.source = if persisted {
            CatalogCacheSource::Network
        } else {
            CatalogCacheSource::Memory
        };
        state.error = (!persisted).then_some(CatalogRefreshError::PersistenceFailed);
        CatalogRefreshReport {
            models: state
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.models.clone()),
            outcome: if persisted {
                outcome
            } else {
                CatalogRefreshOutcome::MemoryOnly
            },
        }
    }

    fn read_snapshot(&self, disk: &DiskView, scope: &str) -> Option<Snapshot<T>> {
        if disk.unreadable_revocation {
            return None;
        }
        let bytes = disk.snapshot.as_deref()?;
        let stored: SnapshotFile = serde_json::from_slice(bytes).ok()?;
        if stored.url != self.url.as_ref() || stored.scope != scope {
            return None;
        }
        if let Some(bytes) = &disk.revoked {
            // A malformed existing tombstone fails closed, not back to an
            // otherwise readable private snapshot.
            let revoked: RevocationFile = serde_json::from_slice(bytes).ok()?;
            if revoked.url == self.url.as_ref()
                && revoked.scope == scope
                && stored.revocation.as_deref() != Some(revoked.revision.as_str())
            {
                return None;
            }
        }
        let models = (self.parse)(&stored.payload, scope).ok()?;
        Some(Snapshot {
            file: stored,
            models,
        })
    }
}

/// Persistent advisory locks are released by the OS on close/process exit.
/// Never remove the lock path (that would split publishers across two inodes).
fn open_lock(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(sidecar(path, ".lock"))
}

async fn acquire_lock(path: &Path) -> std::io::Result<File> {
    let file = open_lock(path)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "catalog cache lock busy",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// A dropped driver must not strand coalesced waiters or the pending gate.
struct SettleOnDrop<'a, T: Clone + Send + Sync + 'static> {
    cache: &'a CatalogCache<T>,
    inflight: Arc<InFlight<T>>,
}

impl<T: Clone + Send + Sync + 'static> Drop for SettleOnDrop<'_, T> {
    fn drop(&mut self) {
        if self.inflight.result.get().is_some() {
            return;
        }
        {
            let mut state = self.cache.state.lock().unwrap();
            if state
                .pending
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.inflight))
            {
                state.pending = None;
            }
        }
        let _ = self.inflight.result.set(CatalogRefreshReport::cancelled());
        self.inflight.notify.notify_waiters();
    }
}

async fn await_inflight<T: Clone>(inflight: Arc<InFlight<T>>) -> CatalogRefreshReport<T> {
    loop {
        let notified = inflight.notify.notified();
        if let Some(result) = inflight.result.get() {
            return result.clone();
        }
        notified.await;
    }
}

/// Atomic snapshot write: temp file + rename at mode 0600. The temp file
/// is unique per writer (`pid` + an in-process counter): one agent dir is
/// shared by many processes, and a shared temp path would let concurrent
/// refreshes truncate each other's temp file and publish a malformed snapshot.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    #[cfg(unix)]
    let write = || {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
    };
    #[cfg(not(unix))]
    let write = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
    };
    let mut file = write()?;
    file.write_all(bytes)?;
    file.flush()?;
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The multi-process snapshot-write invariant (Macroscope on #2576):
    /// a reader racing concurrent `write_atomic` writers always parses a
    /// full document.
    #[test]
    fn concurrent_writers_never_publish_a_malformed_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        write_atomic(&path, br#"{"writer":0}"#).unwrap();
        std::thread::scope(|scope| {
            for writer in 0..4u8 {
                let path = &path;
                scope.spawn(move || {
                    for round in 0..200usize {
                        let payload = format!(
                            r#"{{"writer":{writer},"round":{round},"padding":"{}"}}"#,
                            "x".repeat(round % 7 * 512)
                        );
                        write_atomic(path, payload.as_bytes()).unwrap();
                    }
                });
            }
            scope.spawn(|| {
                for _ in 0..500usize {
                    let bytes = std::fs::read(&path).unwrap();
                    let parsed: serde_json::Value = serde_json::from_slice(&bytes)
                        .expect("the published snapshot always parses");
                    assert!(parsed.get("writer").is_some());
                    std::hint::spin_loop();
                }
            });
        });
        // Every rename consumed its temp: no writer litter remains.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }
}

#[cfg(test)]
#[path = "cache_regression.rs"]
mod regression;
