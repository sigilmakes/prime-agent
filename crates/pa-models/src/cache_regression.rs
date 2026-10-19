//! Regression tests use isolated files, loopback servers, and child test
//! processes. No credentials, external endpoints, or shared environment edits.

use super::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

fn cache(path: &Path, url: &str) -> CatalogCache<Value> {
    CatalogCache::new(
        url,
        Some(path.to_path_buf()),
        Arc::new(CatalogFetcher::new()),
        Arc::new(|payload, _| {
            let valid = payload.get("value").is_some_and(Value::is_string)
                || payload
                    .get("models")
                    .and_then(Value::as_array)
                    .is_some_and(|models| {
                        models
                            .iter()
                            .all(|model| model.get("id").is_some_and(Value::is_string))
                    });
            if valid {
                Ok(payload.clone())
            } else {
                Err("secret parser diagnostic".into())
            }
        }),
    )
}

fn force() -> RefreshOptions {
    RefreshOptions {
        force: true,
        ..Default::default()
    }
}

fn payload_options(payload: Value) -> RefreshOptions {
    RefreshOptions {
        force: true,
        fetch_payload: Some(Arc::new(move || {
            let payload = payload.clone();
            Box::pin(async move { Ok(payload) })
        })),
        ..Default::default()
    }
}

async fn seed(
    cache: &CatalogCache<Value>,
    scope: &str,
    value: &str,
) -> CatalogRefreshReport<Value> {
    cache
        .refresh_report(scope, payload_options(json!({"value": value})))
        .await
}

struct HeldServer {
    url: String,
    requested: oneshot::Receiver<String>,
    release: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl HeldServer {
    async fn start(response: Vec<u8>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/catalog", listener.local_addr().unwrap());
        let (seen_tx, requested) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut bytes = [0; 1024];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut bytes).await.unwrap();
                assert!(read > 0 && head.len() < 16_384);
                head.extend_from_slice(&bytes[..read]);
            }
            seen_tx.send(String::from_utf8(head).unwrap()).unwrap();
            if released.await.is_ok() {
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            }
        });
        Self {
            url,
            requested,
            release: Some(release),
            task,
        }
    }

    fn respond(&mut self) {
        self.release.take().unwrap().send(()).unwrap();
    }
}

impl Drop for HeldServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn response(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} Test\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

async fn request_started(
    server: &mut HeldServer,
    pending: &mut Pin<Box<impl Future<Output = CatalogRefreshReport<Value>>>>,
) -> String {
    tokio::select! {
        head = &mut server.requested => head.expect("request observed"),
        result = pending => panic!("request completed before release: {result:?}"),
        () = tokio::time::sleep(Duration::from_secs(5)) => panic!("request never started"),
    }
}

// Re-run only this helper in a separate OS process. The operation never uses
// the network; real HTTP requests are held by the parent fixture.
#[test]
fn process_worker() {
    let Ok(action) = std::env::var("PA_CATALOG_TEST_ACTION") else {
        return;
    };
    let path = PathBuf::from(std::env::var_os("PA_CATALOG_TEST_PATH").unwrap());
    let url = std::env::var("PA_CATALOG_TEST_URL").unwrap();
    let scope = std::env::var("PA_CATALOG_TEST_SCOPE").unwrap();
    let cache = cache(&path, &url);
    match action.as_str() {
        "revoke" => {
            cache.get(&scope);
            cache.clear(&scope);
        }
        "write" => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let report = runtime.block_on(seed(&cache, &scope, "external"));
            assert_eq!(report.outcome, CatalogRefreshOutcome::Updated);
        }
        "offline" => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let report = runtime.block_on(cache.refresh_report(&scope, force()));
            assert_eq!(
                report,
                CatalogRefreshReport {
                    models: Some(json!({"value":"external"})),
                    outcome: CatalogRefreshOutcome::Offline
                }
            );
        }
        _ => panic!("unknown test action"),
    }
}

fn child(path: &Path, url: &str, scope: &str, action: &str) {
    // Never inherit a live agent's internal worker/session/socket pointers or
    // provider credentials when re-executing this test binary.
    let environment = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env("HOME", environment.path())
        .env("XDG_CACHE_HOME", environment.path().join("cache"))
        .env("XDG_CONFIG_HOME", environment.path().join("config"))
        .env("XDG_DATA_HOME", environment.path().join("data"))
        .env("XDG_STATE_HOME", environment.path().join("state"))
        .env("XDG_RUNTIME_DIR", environment.path())
        .env("TMPDIR", environment.path())
        .args([
            "--exact",
            "cache::regression::process_worker",
            "--nocapture",
        ])
        .env("PA_CATALOG_TEST_ACTION", action)
        .env("PA_CATALOG_TEST_PATH", path)
        .env("PA_CATALOG_TEST_URL", url)
        .env("PA_CATALOG_TEST_SCOPE", scope)
        .env("PI_OFFLINE", if action == "offline" { "1" } else { "0" })
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn changed_disk_reloads_in_a_live_reader_and_a_fresh_offline_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let local = cache(&path, "http://catalog.invalid/");
    seed(&local, PUBLIC_SCOPE, "old").await;
    child(&path, local.url(), PUBLIC_SCOPE, "write");
    assert_eq!(local.get(PUBLIC_SCOPE), Some(json!({"value":"external"})));
    assert_eq!(local.status(PUBLIC_SCOPE).source, CatalogCacheSource::Disk);
    child(&path, local.url(), PUBLIC_SCOPE, "offline");
}

#[tokio::test]
async fn slower_process_result_never_overwrites_newer_disk_and_reports_cached() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let mut server = HeldServer::start(response(200, r#"{"value":"late"}"#)).await;
    let local = cache(&path, &server.url);
    let mut pending = Box::pin(local.refresh_report(PUBLIC_SCOPE, force()));
    request_started(&mut server, &mut pending).await;
    child(&path, &server.url, PUBLIC_SCOPE, "write");
    server.respond();
    assert_eq!(
        pending.await,
        CatalogRefreshReport {
            models: Some(json!({"value":"external"})),
            outcome: CatalogRefreshOutcome::Cached
        }
    );
    assert_eq!(
        cache(&path, &server.url).get(PUBLIC_SCOPE),
        Some(json!({"value":"external"}))
    );
}

#[tokio::test]
async fn external_revocation_blocks_late_success_with_or_without_a_prior_snapshot() {
    for prior in [false, true] {
        for status in [200, 304] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("catalog.json");
            let mut server = HeldServer::start(response(
                status,
                if status == 200 {
                    r#"{"value":"late"}"#
                } else {
                    ""
                },
            ))
            .await;
            let local = cache(&path, &server.url);
            if prior {
                seed(&local, "private", "old").await;
            }
            let mut pending = Box::pin(local.refresh_report("private", force()));
            request_started(&mut server, &mut pending).await;
            child(&path, &server.url, "private", "revoke");
            assert_eq!(cache(&path, &server.url).get("private"), None);
            server.respond();
            let report = pending.await;
            assert_eq!(report.models, None);
            assert!(matches!(
                report.outcome,
                CatalogRefreshOutcome::Cancelled | CatalogRefreshOutcome::Error { .. }
            ));
            assert_eq!(local.get("private"), None);
            assert!(!path.exists(), "late response did not recreate snapshot");
            assert!(revocation_path(&path, &server.url, "private").exists());
        }
    }
}

#[tokio::test]
async fn a_post_revocation_refresh_can_publish_but_a_pre_revocation_request_cannot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let mut server = HeldServer::start(response(200, r#"{"value":"late"}"#)).await;
    let local = cache(&path, &server.url);
    let mut pending = Box::pin(local.refresh_report("private", force()));
    request_started(&mut server, &mut pending).await;
    child(&path, &server.url, "private", "revoke");
    child(&path, &server.url, "private", "write");
    server.respond();
    assert_eq!(
        pending.await,
        CatalogRefreshReport {
            models: Some(json!({"value":"external"})),
            outcome: CatalogRefreshOutcome::Cached
        }
    );
    let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let marker: Value = serde_json::from_slice(
        &std::fs::read(revocation_path(&path, &server.url, "private")).unwrap(),
    )
    .unwrap();
    assert_eq!(stored["revocation"], marker["revision"]);
    assert_eq!(local.get("private"), Some(json!({"value":"external"})));
}

#[tokio::test]
async fn external_reload_and_explicit_clear_cancel_old_drivers() {
    for reload in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        let mut server = HeldServer::start(response(200, r#"{"value":"late"}"#)).await;
        let local = cache(&path, &server.url);
        let mut pending = Box::pin(local.refresh_report(PUBLIC_SCOPE, force()));
        request_started(&mut server, &mut pending).await;
        if reload {
            child(&path, &server.url, PUBLIC_SCOPE, "write");
            assert_eq!(
                local.reload_from_disk(PUBLIC_SCOPE),
                Some(json!({"value":"external"}))
            );
        } else {
            local.clear(PUBLIC_SCOPE);
        }
        server.respond();
        assert_eq!(pending.await, CatalogRefreshReport::cancelled());
        assert_eq!(
            local.get(PUBLIC_SCOPE),
            reload.then(|| json!({"value":"external"}))
        );
    }
}

#[tokio::test]
async fn revocation_is_visible_with_a_held_publisher_lock_and_keeps_scopes_separate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let local = cache(&path, "http://catalog.invalid/");
    seed(&local, "scope-a", "secret-a").await;
    let lock = open_lock(&path).unwrap();
    lock.lock().unwrap();
    child(&path, local.url(), "scope-a", "revoke");
    assert!(path.exists(), "lock prevented removal, not revocation");
    assert_eq!(local.get("scope-a"), None);
    // Revoking B must not erase A's marker while A's file still exists.
    child(&path, local.url(), "scope-b", "revoke");
    assert_eq!(cache(&path, local.url()).get("scope-a"), None);
    assert_eq!(cache(&path, local.url()).get("scope-b"), None);
    drop(lock);
    assert_eq!(
        seed(&local, "scope-a", "authorized-again").await.outcome,
        CatalogRefreshOutcome::Updated
    );
    local.get("scope-b");
    local.clear("scope-b");
    assert_eq!(
        cache(&path, local.url()).get("scope-a"),
        Some(json!({"value":"authorized-again"}))
    );
}

#[tokio::test]
async fn unauthorized_private_scope_revokes_while_public_auth_errors_keep_last_good() {
    for scope in [PUBLIC_SCOPE, "private"] {
        for status in [401, 403] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("catalog.json");
            let mut server = HeldServer::start(response(status, "")).await;
            let local = cache(&path, &server.url);
            seed(&local, scope, "good").await;
            let reader = cache(&path, &server.url);
            assert!(reader.get(scope).is_some());
            let mut pending = Box::pin(local.refresh_report(scope, force()));
            request_started(&mut server, &mut pending).await;
            server.respond();
            let report = pending.await;
            if scope == PUBLIC_SCOPE {
                assert_eq!(
                    report,
                    CatalogRefreshReport {
                        models: Some(json!({"value":"good"})),
                        outcome: CatalogRefreshOutcome::Error {
                            error: CatalogRefreshError::Http { status }
                        }
                    }
                );
            } else {
                assert_eq!(
                    report,
                    CatalogRefreshReport {
                        models: None,
                        outcome: CatalogRefreshOutcome::Revoked { status }
                    }
                );
                assert_eq!(reader.get(scope), None);
                assert_eq!(local.get(scope), None);
                assert!(!path.exists());
            }
            assert_eq!(
                local.status(scope).error,
                Some(CatalogRefreshError::Http { status })
            );
        }
    }
}

#[tokio::test]
async fn not_modified_preserves_fetched_time_and_advances_checked_time_on_legacy_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let mut server = HeldServer::start(response(304, "")).await;
    write_atomic(&path, &serde_json::to_vec(&json!({ "url":server.url, "scope":PUBLIC_SCOPE, "fetchedAt":10, "etag":"old-etag", "payload":{"value":"good"} })).unwrap()).unwrap();
    let local = cache(&path, &server.url);
    assert_eq!(local.status(PUBLIC_SCOPE).checked_at, Some(10));
    let mut pending = Box::pin(local.refresh_report(PUBLIC_SCOPE, force()));
    let head = request_started(&mut server, &mut pending).await;
    assert!(head.contains("if-none-match: old-etag"));
    assert_eq!(
        local.status(PUBLIC_SCOPE).state,
        CatalogCacheState::Refreshing
    );
    server.respond();
    assert_eq!(
        pending.await,
        CatalogRefreshReport {
            models: Some(json!({"value":"good"})),
            outcome: CatalogRefreshOutcome::NotModified
        }
    );
    let status = local.status(PUBLIC_SCOPE);
    assert_eq!(status.fetched_at, Some(10));
    assert!(status.checked_at.unwrap() > 10);
    assert_eq!(status.state, CatalogCacheState::Fresh);
    let gated = local
        .refresh_report(PUBLIC_SCOPE, RefreshOptions::default())
        .await;
    assert_eq!(gated.outcome, CatalogRefreshOutcome::Cached);
}

#[tokio::test]
async fn coalesced_waiter_drop_is_local_and_driver_drop_settles_remaining_waiters() {
    for drop_driver in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        let mut server = HeldServer::start(response(200, r#"{"value":"good"}"#)).await;
        let local = cache(&path, &server.url);
        let mut driver = Box::pin(local.refresh_report(PUBLIC_SCOPE, force()));
        request_started(&mut server, &mut driver).await;
        let mut waiter = Box::pin(local.refresh_report(PUBLIC_SCOPE, force()));
        // Poll the waiter once to join the already-observed driver.
        tokio::select! {
            biased;
            result = &mut waiter => panic!("waiter settled early: {result:?}"),
            () = std::future::ready(()) => {},
        }
        if drop_driver {
            drop(driver);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), waiter)
                    .await
                    .unwrap(),
                CatalogRefreshReport::cancelled()
            );
            assert_eq!(
                seed(&local, PUBLIC_SCOPE, "next").await.outcome,
                CatalogRefreshOutcome::Updated
            );
        } else {
            drop(waiter);
            server.respond();
            assert_eq!(driver.await.outcome, CatalogRefreshOutcome::Updated);
        }
    }
}

#[tokio::test]
async fn scope_change_and_guard_change_while_lock_is_held_prevent_publication() {
    for change_scope in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        let local = cache(&path, "http://catalog.invalid/");
        let lock = open_lock(&path).unwrap();
        lock.lock().unwrap();
        let current = Arc::new(AtomicBool::new(true));
        let received = Arc::new(tokio::sync::Notify::new());
        let mut opts = payload_options(json!({"value":"late"}));
        let fetch = opts.fetch_payload.take().unwrap();
        let received_copy = Arc::clone(&received);
        opts.fetch_payload = Some(Arc::new(move || {
            received_copy.notify_one();
            fetch()
        }));
        let guard = Arc::clone(&current);
        opts.is_current = Some(Arc::new(move || guard.load(Ordering::SeqCst)));
        let mut pending = Box::pin(local.refresh_report("old", opts));
        tokio::select! {
            result = &mut pending => panic!("published with lock held: {result:?}"),
            () = received.notified() => {},
        }
        if change_scope {
            local.get("new");
        } else {
            current.store(false, Ordering::SeqCst);
        }
        drop(lock);
        assert_eq!(pending.await, CatalogRefreshReport::cancelled());
        assert!(!path.exists());
    }
}

#[tokio::test]
async fn lock_failure_is_memory_only_and_external_changes_supersede_that_memory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let local = cache(&path, "http://catalog.invalid/");
    seed(&local, "private", "disk").await;
    let lock = open_lock(&path).unwrap();
    lock.lock().unwrap();
    let report = seed(&local, "private", "memory").await;
    assert_eq!(
        report,
        CatalogRefreshReport {
            models: Some(json!({"value":"memory"})),
            outcome: CatalogRefreshOutcome::MemoryOnly
        }
    );
    assert_eq!(local.get("private"), Some(json!({"value":"memory"})));
    assert_eq!(
        cache(&path, local.url()).get("private"),
        Some(json!({"value":"disk"}))
    );
    assert_eq!(
        local.status("private").error,
        Some(CatalogRefreshError::PersistenceFailed)
    );
    drop(lock);
    child(&path, local.url(), "private", "revoke");
    assert_eq!(local.get("private"), None);
}

#[tokio::test]
async fn failed_tombstone_is_reported_and_never_restores_the_local_private_view() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let local = cache(&path, "http://catalog.invalid/");
    seed(&local, "private", "old").await;
    let lock = open_lock(&path).unwrap();
    lock.lock().unwrap();
    std::fs::create_dir(revocation_path(&path, local.url(), "private")).unwrap();
    let mut opts = force();
    opts.fetch_payload = Some(Arc::new(|| {
        Box::pin(async { Err(FetchError::Status { status: 401 }) })
    }));
    assert_eq!(
        local.refresh_report("private", opts).await.outcome,
        CatalogRefreshOutcome::Revoked { status: 401 }
    );
    assert_eq!(local.get("private"), None);
    assert_eq!(
        local.status("private").error,
        Some(CatalogRefreshError::PersistenceFailed)
    );
}

#[tokio::test]
async fn merged_payload_preserves_known_capabilities_but_prunes_missing_ids_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let local = cache(&path, "http://catalog.invalid/");
    let first = json!({"models":[{"id":"keep", "reasoning":true, "context":12345},{"id":"gone", "reasoning":true}]});
    local.refresh_report("scope", payload_options(first)).await;
    let mut opts =
        payload_options(json!({"models":[{"id":"keep", "name":"new name"},{"id":"new"}]}));
    opts.merge_payload = Some(Arc::new(|received, previous| {
        let models = received["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|fresh| {
                let old = previous
                    .and_then(|payload| payload["models"].as_array())
                    .and_then(|models| models.iter().find(|old| old["id"] == fresh["id"]));
                let mut merged = old.cloned().unwrap_or_else(|| json!({}));
                merged
                    .as_object_mut()
                    .unwrap()
                    .extend(fresh.as_object().unwrap().clone());
                merged
            })
            .collect::<Vec<_>>();
        Ok(json!({"models":models}))
    }));
    let expected = json!({"models":[{"id":"keep", "reasoning":true, "context":12345, "name":"new name"},{"id":"new"}]});
    assert_eq!(
        local.refresh_report("scope", opts).await.models,
        Some(expected.clone())
    );
    assert_eq!(cache(&path, local.url()).get("scope"), Some(expected));
}

#[tokio::test]
async fn invalid_input_invalid_merge_and_oversized_merge_keep_last_good_without_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let local = cache(&path, "http://catalog.invalid/secret-url");
    seed(&local, "private", "good").await;
    let merge_calls = Arc::new(AtomicUsize::new(0));
    let mut invalid = payload_options(json!({"invalid":true}));
    let calls = Arc::clone(&merge_calls);
    invalid.merge_payload = Some(Arc::new(move |_, _| {
        calls.fetch_add(1, Ordering::SeqCst);
        Err("secret envelope validation diagnostic".into())
    }));
    let mut bad_merge = payload_options(json!({"value":"new"}));
    bad_merge.merge_payload = Some(Arc::new(|_, _| Ok(json!({"value":null}))));
    let mut oversized = payload_options(json!({"value":"new"}));
    oversized.merge_payload = Some(Arc::new(|_, _| {
        Ok(json!({"value":"x".repeat(MAX_RESPONSE_BYTES)}))
    }));
    for opts in [invalid, bad_merge, oversized] {
        assert_eq!(
            local.refresh_report("private", opts).await,
            CatalogRefreshReport {
                models: Some(json!({"value":"good"})),
                outcome: CatalogRefreshOutcome::Error {
                    error: CatalogRefreshError::InvalidPayload
                }
            }
        );
    }
    assert_eq!(merge_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        cache(&path, local.url()).get("private"),
        Some(json!({"value":"good"}))
    );
    let mut error = force();
    error.fetch_payload = Some(Arc::new(|| {
        Box::pin(async {
            Err(FetchError::Transport {
                message: "secret-token-and-path".into(),
            })
        })
    }));
    assert_eq!(
        local.refresh_report("private", error).await.outcome,
        CatalogRefreshOutcome::Error {
            error: CatalogRefreshError::RequestFailed
        }
    );
    let status = serde_json::to_string(&local.status("private")).unwrap();
    assert!(!status.contains("secret"));
    assert!(!status.contains("catalog.invalid"));
    assert!(!status.contains(&path.display().to_string()));
}

#[tokio::test]
async fn malformed_json_reports_typed_failure_and_keeps_last_good() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let mut server = HeldServer::start(response(200, "not-json-secret")).await;
    let local = cache(&path, &server.url);
    seed(&local, PUBLIC_SCOPE, "good").await;
    let mut pending = Box::pin(local.refresh_report(PUBLIC_SCOPE, force()));
    request_started(&mut server, &mut pending).await;
    server.respond();
    assert_eq!(
        pending.await,
        CatalogRefreshReport {
            models: Some(json!({"value":"good"})),
            outcome: CatalogRefreshOutcome::Error {
                error: CatalogRefreshError::InvalidJson
            }
        }
    );
}

#[tokio::test]
async fn revocation_query_blocks_legacy_fallback_until_a_valid_new_snapshot_exists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let local = cache(&path, "http://catalog.invalid/");
    assert!(!local.has_revocation("private"));
    seed(&local, "private", "old").await;
    assert!(!local.has_revocation("private"));
    local.clear("private");
    assert!(local.has_revocation("private"));
    assert!(cache(&path, local.url()).has_revocation("private"));
    assert!(!cache(&path, local.url()).has_revocation("other-scope"));
    assert!(!cache(&path, "http://other.invalid/").has_revocation("private"));
    assert_eq!(
        seed(&local, "private", "new").await.outcome,
        CatalogRefreshOutcome::Updated
    );
    assert!(!local.has_revocation("private"));
    assert!(!cache(&path, local.url()).has_revocation("private"));
}

#[tokio::test]
async fn partial_payload_is_normalized_before_the_cache_parser_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let local = cache(&path, "http://catalog.invalid/");
    seed(&local, "scope", "retained capabilities").await;
    let mut opts = payload_options(json!({"partial":true}));
    opts.merge_payload = Some(Arc::new(|received, previous| {
        if received.get("partial") != Some(&Value::Bool(true)) {
            return Err("Invalid partial envelope".into());
        }
        Ok(previous.unwrap().clone())
    }));
    assert_eq!(
        local.refresh_report("scope", opts).await,
        CatalogRefreshReport {
            models: Some(json!({"value":"retained capabilities"})),
            outcome: CatalogRefreshOutcome::Updated,
        }
    );
    assert_eq!(
        cache(&path, local.url()).get("scope"),
        Some(json!({"value":"retained capabilities"}))
    );
}
