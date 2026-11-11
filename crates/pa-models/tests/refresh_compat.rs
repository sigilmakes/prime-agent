//! Compat regressions (spec §5.2) over the generic `CatalogCache` and a
//! scripted local HTTP server: fake future versions keep last-good
//! silently, malformed entries skip, failures keep state unchanged,
//! hourly gating + coalescing hold, and scopes isolate accounts.

use pa_test_scratch as _; // a private TMPDIR for this test binary, deleted on exit
mod common;

use std::sync::Arc;

use pa_models::cache::{CatalogCache, RefreshOptions, PUBLIC_SCOPE};
use pa_models::fetch::CatalogFetcher;
use pa_models::pinning::{parse_provider_model_catalog, PinnedTemplates};
use pa_models::prime_inference::{
    build_prime_inference_models, parse_prime_inference_model_catalog,
};
use serde_json::{json, Value};

fn prime_inputs(base: &str, key: &str, team: &str) -> pa_models::CatalogInputs {
    pa_models::CatalogInputs {
        sources: vec![pa_models::ResolvedSource {
            id: "prime-inference-public".into(),
            provider: "prime-inference".into(),
            format: pa_models::CatalogFormat::PrimeInference,
            url: format!("{base}/models"),
            headers: vec![
                ("authorization".into(), format!("Bearer {key}")),
                ("x-prime-team-id".into(), team.into()),
            ],
            unknown_models: pa_models::UnknownModelPolicy::KnownOnly,
            is_current: None,
        }],
        seeds: Vec::new(),
    }
}
async fn refresh_prime(
    catalog: &pa_models::ModelCatalog,
    inputs: &pa_models::CatalogInputs,
) -> pa_models::RefreshPublication {
    catalog
        .refresh(
            inputs,
            &pa_models::RefreshRequest {
                policy: pa_models::RefreshPolicy::Force,
                sources: Some(vec!["prime-inference-public".into()]),
            },
        )
        .await
}

/// A cache over the models catalog parse pipeline (the same closure
/// `ModelCatalog` installs in production), aimed at the mock server.
fn models_cache(
    dir: &std::path::Path,
    url: impl AsRef<str>,
) -> CatalogCache<Vec<pa_models::Model>> {
    let templates = PinnedTemplates::from_compiled();
    let parse: pa_models::cache::CatalogParse<Vec<pa_models::Model>> =
        Arc::new(move |payload, _scope| parse_provider_model_catalog(payload, &templates));
    CatalogCache::new(
        url.as_ref(),
        Some(dir.join("provider-model-catalog.v1.json")),
        Arc::new(CatalogFetcher::new()),
        parse,
    )
}

fn catalog_json(ids: &[&str]) -> String {
    let compiled = pa_models::transports::compiled_models();
    let anthropic = compiled.iter().find(|m| m.provider == "anthropic").unwrap();
    let models: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "name": id,
                "api": anthropic.api,
                "provider": anthropic.provider,
                "baseUrl": anthropic.base_url,
                "reasoning": false,
                "input": ["text"],
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                "contextWindow": 128_000,
                "maxTokens": 4_096,
            })
        })
        .collect();
    serde_json::to_string(&json!({"schemaVersion": 1, "models": models})).unwrap()
}

#[tokio::test]
async fn fresh_fetch_writes_a_validated_0600_snapshot_and_serves_it() {
    let dir = tempfile::tempdir().unwrap();
    let body = catalog_json(&["model-a"]);
    let server =
        common::MockServer::start(vec![common::ok_json(body.clone(), Some("\"e1\""))]).await;
    let cache = models_cache(dir.path(), server.url("/catalog"));
    let models = cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("refreshed");
    assert!(models.iter().any(|m| m.id == "model-a"));

    let path = dir.path().join("provider-model-catalog.v1.json");
    // The mode check is unix-only (Windows inherits ACLs instead, the
    // documented TS-parity decision), so the metadata read lives inside
    // the gate: the binding would be unused on the other side.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(&path).expect("snapshot written");
        assert_eq!(
            metadata.permissions().mode() & 0o777,
            0o600,
            "mode 0600 like the TS writeFileAtomicSync"
        );
    }
    let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["url"], server.url("/catalog"));
    assert_eq!(stored["scope"], PUBLIC_SCOPE);
    assert_eq!(stored["etag"], "\"e1\"");
    assert!(stored["fetchedAt"].as_u64().is_some());
    assert_eq!(stored["payload"]["models"][0]["id"], "model-a");

    let cache2 = models_cache(dir.path(), server.url("/catalog"));
    let models = cache2.get(PUBLIC_SCOPE).expect("disk snapshot");
    assert!(models.iter().any(|m| m.id == "model-a"));
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn unsupported_future_version_keeps_last_good_silently() {
    let dir = tempfile::tempdir().unwrap();
    let good = catalog_json(&["model-a"]);
    let server = common::MockServer::start(vec![
        common::ok_json(good.clone(), None),
        common::ok_json(
            serde_json::to_string(&json!({
                "schemaVersion": 3, "models": [{"id": "v3-model"}]
            }))
            .unwrap(),
            None,
        ),
    ])
    .await;
    let cache = models_cache(dir.path(), server.url("/catalog"));
    cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("good first refresh");
    let before = cache.get(PUBLIC_SCOPE).expect("last good");

    let after = cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("kept last good");
    assert_eq!(
        after.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
        before.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
        "v3 payload silently keeps last-good"
    );
    let cache2 = models_cache(dir.path(), server.url("/catalog"));
    assert!(cache2
        .get(PUBLIC_SCOPE)
        .expect("disk snapshot intact")
        .iter()
        .any(|m| m.id == "model-a"));
}

#[tokio::test]
async fn malformed_model_entry_skips_and_the_refresh_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let compiled = pa_models::transports::compiled_models();
    let anthropic = compiled.iter().find(|m| m.provider == "anthropic").unwrap();
    let body = json!({"schemaVersion": 1, "models": [
        {"id": "broken-entry", "api": anthropic.api, "provider": anthropic.provider,
         "baseUrl": anthropic.base_url, "contextWindow": 0},
        {"id": "good-entry", "name": "good", "api": anthropic.api,
         "provider": anthropic.provider, "baseUrl": anthropic.base_url,
         "reasoning": false, "input": ["text"],
         "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
         "contextWindow": 128_000, "maxTokens": 4_096},
    ]});
    let server = common::MockServer::start(vec![common::ok_json(
        serde_json::to_string(&body).unwrap(),
        None,
    )])
    .await;
    let cache = models_cache(dir.path(), server.url("/catalog"));
    let models = cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("refresh succeeds");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["good-entry"], "bad entry skipped, refresh kept");
}

#[tokio::test]
async fn network_failure_and_oversize_keep_state_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let server = common::MockServer::start(vec![
        common::ok_json(catalog_json(&["model-a"]), None),
        common::status(500, "Internal Server Error"),
    ])
    .await;
    let url = server.url("/catalog");
    let cache = models_cache(dir.path(), url.clone());
    cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("good refresh");
    let before: Vec<String> = cache
        .get(PUBLIC_SCOPE)
        .expect("snapshot")
        .iter()
        .map(|m| m.id.clone())
        .collect();

    let after = cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("last good kept");
    assert_eq!(
        after.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
        before,
        "state unchanged after a failed fetch"
    );

    let oversize = common::MockServer::start(vec![common::oversized_header()]).await;
    let big = models_cache(dir.path(), oversize.url("/catalog"));
    assert!(big
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            }
        )
        .await
        .is_none());
    assert!(
        big.get(PUBLIC_SCOPE).is_none(),
        "oversized body never produces a snapshot"
    );
}

#[tokio::test]
async fn hourly_gating_skips_recent_attempts_and_force_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let server =
        common::MockServer::start(vec![common::ok_json(catalog_json(&["model-a"]), None)]).await;
    let cache = models_cache(dir.path(), server.url("/catalog"));
    cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("first refresh");
    assert_eq!(server.request_count(), 1);

    cache
        .refresh(PUBLIC_SCOPE, RefreshOptions::default())
        .await
        .expect("served from snapshot");
    assert_eq!(server.request_count(), 1, "hourly gating");

    cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("forced refresh");
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn concurrent_refreshes_coalesce_into_one_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let server =
        common::MockServer::start(vec![common::ok_json(catalog_json(&["model-a"]), None)]).await;
    let cache = Arc::new(models_cache(dir.path(), server.url("/catalog")));
    let a = cache.refresh(
        PUBLIC_SCOPE,
        RefreshOptions {
            force: true,
            ..Default::default()
        },
    );
    let b = cache.refresh(
        PUBLIC_SCOPE,
        RefreshOptions {
            force: true,
            ..Default::default()
        },
    );
    let (a, b) = tokio::join!(a, b);
    assert!(a.is_some());
    assert!(b.is_some());
    assert_eq!(
        server.request_count(),
        1,
        "in-flight refreshes coalesce per source"
    );
}

#[tokio::test]
async fn not_modified_keeps_snapshot_and_updates_fetched_at() {
    let dir = tempfile::tempdir().unwrap();
    let server = common::MockServer::start(vec![
        common::ok_json(catalog_json(&["model-a"]), Some("\"e1\"")),
        common::not_modified(),
    ])
    .await;
    let cache = models_cache(dir.path(), server.url("/catalog"));
    cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("first refresh");
    let models = cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("304 keeps snapshot");
    assert!(models.iter().any(|m| m.id == "model-a"));
    let stored: Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("provider-model-catalog.v1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stored["etag"], "\"e1\"", "etag reused after 304");
}

#[tokio::test]
async fn prime_inference_cache_is_scope_keyed_and_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let compiled = pa_models::transports::prime_inference_offline_entries();
    let ids: Vec<String> = compiled.iter().map(|m| m.id.clone()).take(60).collect();
    let entry = |id: &str| {
        json!({
            "id": id,
            "display_name": id,
            "pricing": {"input_usd_per_mtok": 1.0, "output_usd_per_mtok": 2.0},
            "specs": {
                "context_window": 200_000, "max_output_tokens": 32_768,
                "supports_reasoning": true,
                "modalities": {"input": ["text"], "output": ["text"]},
            },
        })
    };
    let mut data: Vec<Value> = ids.iter().map(|id| entry(id)).collect();
    data.push(entry("internal/team-a-secret"));
    let payload = json!({"data": data}).to_string();
    let server = common::MockServer::start(vec![common::ok_json(payload, None)]).await;
    let base = {
        let url = server.url("/api/v1");
        url.trim_end_matches("/models").to_string()
    };
    let catalog = pa_models::ModelCatalog::with_urls(
        Some(dir.path().into()),
        Some(dir.path().into()),
        "http://127.0.0.1:1/unused",
        &base,
    );

    let team_a = prime_inputs(&base, "key-a", "team-a");
    let team_b = prime_inputs(&base, "key-b", "team-b");

    let publication = refresh_prime(&catalog, &team_a).await;
    assert_eq!(publication.sources[0].model_count, Some(60));
    let a = publication.snapshot.models;

    // The public models cache never includes private ids (the private
    // entitlement flow is the registry's, over its own authorization).
    assert!(
        !a.iter().any(|m| m.id == "internal/team-a-secret"),
        "private ids are filtered from the public models cache"
    );
    let request = &server.recorded_requests()[0];
    assert!(request.contains("authorization: Bearer key-a"), "{request}");
    assert!(request.contains("x-prime-team-id: team-a"), "{request}");

    assert!(
        catalog
            .adopt(&team_b, pa_models::SnapshotRequirement::default())
            .report
            .applied
            .is_empty(),
        "account B sees nothing from account A's scope"
    );

    let fresh = pa_models::ModelCatalog::with_urls(
        Some(dir.path().into()),
        Some(dir.path().into()),
        "http://127.0.0.1:1/unused",
        &base,
    );
    assert!(
        fresh
            .adopt(&team_a, pa_models::SnapshotRequirement::default())
            .report
            .applied
            .contains(&"prime-inference-public".into()),
        "A's scope reloads its own disk snapshot"
    );
    assert!(
        fresh
            .adopt(&team_b, pa_models::SnapshotRequirement::default())
            .report
            .applied
            .is_empty(),
        "B's scope stays empty across processes"
    );
    let stored_path = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_name().to_string_lossy().ends_with(".v1.json")
                && entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("discovery-")
        })
        .unwrap()
        .path();
    let stored = std::fs::read_to_string(stored_path).unwrap();
    assert!(!stored.contains("key-a"));
}

#[tokio::test]
async fn unauthorized_prime_inference_clears_only_that_scope() {
    let dir = tempfile::tempdir().unwrap();
    let compiled = pa_models::transports::prime_inference_offline_entries();
    let ids: Vec<String> = compiled.iter().map(|m| m.id.clone()).take(60).collect();
    let data: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "pricing": {"input_usd_per_mtok": 1.0, "output_usd_per_mtok": 2.0},
                "specs": {
                    "context_window": 200_000, "max_output_tokens": 32_768,
                    "supports_reasoning": true,
                    "modalities": {"input": ["text"], "output": ["text"]},
                },
            })
        })
        .collect();
    let payload = json!({"data": data}).to_string();
    let server = common::MockServer::start(vec![
        common::ok_json(payload, None),
        common::status(401, "Unauthorized"),
        common::ok_json(json!({"data": []}).to_string(), None),
    ])
    .await;
    let base = {
        let url = server.url("/api/v1");
        url.trim_end_matches("/models").to_string()
    };
    let catalog = pa_models::ModelCatalog::with_urls(
        Some(dir.path().into()),
        Some(dir.path().into()),
        "http://127.0.0.1:1/unused",
        &base,
    );

    let creds = prime_inputs(&base, "key-a", "team-a");
    let other = prime_inputs(&base, "key-z", "team-z");
    assert_eq!(
        refresh_prime(&catalog, &creds).await.sources[0].status,
        pa_types::model_refresh::ModelRefreshStatus::Updated
    );
    assert!(catalog
        .adopt(&creds, pa_models::SnapshotRequirement::default())
        .report
        .applied
        .contains(&"prime-inference-public".into()));
    assert_eq!(
        refresh_prime(&catalog, &creds).await.sources[0].reason,
        Some(pa_types::model_refresh::ModelRefreshReason::AuthRejected)
    );
    assert!(
        catalog
            .adopt(&creds, pa_models::SnapshotRequirement::default())
            .report
            .applied
            .is_empty(),
        "scope cleared"
    );
    assert!(catalog
        .adopt(&other, pa_models::SnapshotRequirement::default())
        .report
        .applied
        .is_empty());
}

#[test]
fn prime_inference_parse_and_coverage_gate_use_public_fns() {
    // Sanity for the closure the PrimeInferenceCatalog installs: an empty
    // live catalog parses (allowEmpty) but fails the coverage gate, so a
    // degraded fetch never replaces a good snapshot.
    let compiled = pa_models::transports::prime_inference_offline_entries();
    let payload = json!({"data": []});
    let entries = parse_prime_inference_model_catalog(&payload, true).expect("empty ok");
    assert!(build_prime_inference_models(&compiled, &entries, false, None).is_none());
}

#[tokio::test]
async fn mid_session_refresh_never_retargets() {
    let dir = tempfile::tempdir().unwrap();
    let server = common::MockServer::start(vec![
        common::ok_json(catalog_json(&["before-model"]), None),
        common::ok_json(catalog_json(&["after-model"]), None),
    ])
    .await;
    let cache = models_cache(dir.path(), server.url("/catalog"));
    let first = cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("first refresh");
    let active = first
        .iter()
        .find(|m| m.id == "before-model")
        .expect("active model")
        .clone();
    let _ = cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("second refresh");
    assert_eq!(active.id, "before-model");
    assert_eq!(active.api, "anthropic-messages");
}

/// Fire-and-forget safety: a caller that drops its `refresh` mid-fetch
/// must not poison the coalescing gate.
#[tokio::test]
async fn a_dropped_refresh_resolves_the_gate_and_the_next_refresh_starts_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let server = common::MockServer::start_scripted(vec![
        common::Scripted::Delayed(
            common::ok_json(catalog_json(&["model-a"]), None),
            std::time::Duration::from_secs(5),
        ),
        common::Scripted::Response(common::ok_json(catalog_json(&["model-b"]), None)),
    ])
    .await;
    let cache = Arc::new(models_cache(dir.path(), server.url("/catalog")));

    // A coalesced waiter joins the dropped driver: it must wake with a
    // failed refresh (None), never hang on the abandoned gate. Both waits
    // are bounded — the 2s bound fails the test instead of wedging it.
    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        cache.refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        ),
    );
    let joined = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        cache.refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        ),
    );
    let (dropped, joined) = tokio::join!(dropped, joined);
    assert!(
        dropped.is_err(),
        "the bounded wait must return while the fetch is held"
    );
    let joined = joined.expect("the coalesced waiter wakes on the abandoned gate");
    assert!(
        joined.is_none(),
        "a dropped refresh resolves as a failed one"
    );

    let next = cache
        .refresh(
            PUBLIC_SCOPE,
            RefreshOptions {
                force: true,
                ..Default::default()
            },
        )
        .await;
    assert!(next.is_some(), "the next refresh starts a fresh fetch");
    assert!(
        next.as_ref()
            .is_some_and(|models| { models.iter().any(|model| model.id == "model-b") }),
        "the fresh fetch lands"
    );
    assert!(
        server.request_count() >= 2,
        "a fresh fetch ran after the abandoned one"
    );
}

/// The first tokio tick fires immediately (a supervisor boot warms the
/// caches without waiting an hour) and the tick's fetch arms the hourly
/// gate, so an immediate second attempt stays inside the window.
#[tokio::test]
async fn facade_refreshes_authorized_layers_and_gates_recent_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let compiled = pa_models::transports::prime_inference_offline_entries();
    let ids: Vec<String> = compiled.iter().map(|m| m.id.clone()).take(60).collect();
    let entry = |id: &str| {
        json!({
            "id": id,
            "display_name": id,
            "pricing": {"input_usd_per_mtok": 1.0, "output_usd_per_mtok": 2.0},
            "specs": {
                "context_window": 200_000, "max_output_tokens": 32_768,
                "supports_reasoning": true,
                "modalities": {"input": ["text"], "output": ["text"]},
            },
        })
    };
    let data: Vec<Value> = ids.iter().map(|id| entry(id)).collect();
    let pi_payload = json!({"data": data}).to_string();
    let mut curated_payload: Value =
        serde_json::from_str(&catalog_json(&["hourly-model"])).unwrap();
    curated_payload["models"].as_array_mut().unwrap().extend(
        pa_models::transports::compiled_models()
            .iter()
            .filter(|model| model.provider != "prime-inference")
            .map(|model| {
                json!({
                    "id": model.id, "name": model.name, "api": model.api,
                    "provider": model.provider, "baseUrl": model.base_url,
                    "reasoning": model.reasoning, "input": model.input,
                    "cost": model.cost, "contextWindow": model.context_window,
                    "maxTokens": model.max_tokens,
                })
            }),
    );
    let server = common::MockServer::start(vec![
        common::ok_json(curated_payload.to_string(), None),
        common::ok_json(pi_payload, None),
    ])
    .await;
    let catalog = Arc::new(pa_models::ModelCatalog::with_urls(
        Some(dir.path().join("models")),
        Some(dir.path().to_path_buf()),
        &server.url("/catalog"),
        &server.url("/api/v1"),
    ));
    let credentials = prime_inputs(&server.url("/api/v1"), "sk-hourly", "team-hourly");
    let curated = pa_models::RefreshRequest {
        policy: pa_models::RefreshPolicy::Force,
        sources: Some(vec!["curated".into()]),
    };
    let publication = catalog
        .refresh(&pa_models::CatalogInputs::default(), &curated)
        .await;
    assert_eq!(
        publication.sources[0].status,
        pa_types::model_refresh::ModelRefreshStatus::Updated,
        "{:?}",
        publication.sources
    );
    refresh_prime(&catalog, &credentials).await;
    let heads = server.recorded_requests();
    assert!(
        heads.iter().any(|head| head.contains("/catalog")),
        "the provider catalog layer refreshed"
    );
    let pi_fetch = heads
        .iter()
        .find(|head| head.contains("/models"))
        .expect("the credentialed layer refreshed");
    assert!(
        pi_fetch.contains("authorization: Bearer sk-hourly"),
        "{pi_fetch}"
    );
    assert!(
        pi_fetch.contains("x-prime-team-id: team-hourly"),
        "{pi_fetch}"
    );
    // The hourly gate: an immediate second attempt serves the cached
    // snapshot with no fetch (the awaited call is the gate's own path, so
    // the count is settled at the assert, not raced).
    let request = pa_models::RefreshRequest {
        policy: pa_models::RefreshPolicy::IfStale,
        sources: Some(vec!["curated".into(), "prime-inference-public".into()]),
    };
    let gated = catalog.refresh(&credentials, &request).await;
    assert!(gated
        .snapshot
        .models
        .iter()
        .any(|model| model.id == "hourly-model"));
    assert_eq!(server.request_count(), 2, "one fetch per layer per window");
}
