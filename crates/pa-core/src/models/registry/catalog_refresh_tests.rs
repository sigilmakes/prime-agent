//! Core publication/auth regressions. Every endpoint is loopback, every auth
//! store excludes ambient credentials, and every cache directory is temporary.

use super::*;
use crate::auth::manager::{AuthStorage, NoOAuth, OAuthIntegration};
use crate::auth::types::AuthStorageData;
use crate::models::{catalog_chain, private_auth, session_restore};
use pa_types::ai::Model;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Notify};

#[derive(Clone)]
struct Reply {
    status: u16,
    body: Value,
    gate: Option<Arc<Notify>>,
}

impl Reply {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            body,
            gate: None,
        }
    }
}

struct Loopback {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    observed: mpsc::UnboundedReceiver<String>,
    task: tokio::task::JoinHandle<()>,
}

impl Loopback {
    async fn start(routes: HashMap<String, Reply>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let routes = Arc::new(routes);
        let (events, observed) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut workers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        let seen = Arc::clone(&seen);
                        let routes = Arc::clone(&routes);
                        let events = events.clone();
                        workers.spawn(async move {
                            let mut bytes = Vec::new();
                            let mut buffer = [0u8; 1024];
                            let header_end = loop {
                                let read = socket.read(&mut buffer).await.unwrap();
                                if read == 0 { return; }
                                bytes.extend_from_slice(&buffer[..read]);
                                assert!(bytes.len() < 65_536, "bounded fixture request");
                                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                                    break end + 4;
                                }
                            };
                            let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                            let body_length = head.lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                            }).unwrap_or_default();
                            while bytes.len() < header_end + body_length {
                                let read = socket.read(&mut buffer).await.unwrap();
                                assert!(read > 0);
                                bytes.extend_from_slice(&buffer[..read]);
                            }
                            let path = head.split_whitespace().nth(1).unwrap().to_string();
                            seen.lock().unwrap().push(String::from_utf8(bytes).unwrap());
                            events.send(path.clone()).unwrap();
                            let reply = routes.get(&path).cloned().unwrap_or(Reply { status: 500, body: json!({"unexpected":path}), gate: None });
                            if let Some(gate) = reply.gate { gate.notified().await; }
                            let body = reply.body.to_string();
                            let response = format!("HTTP/1.1 {} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", reply.status, body.len());
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                        });
                    }
                    _ = workers.join_next(), if !workers.is_empty() => {}
                }
            }
        });
        Self {
            base,
            requests,
            observed,
            task,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn auth(data: Value) -> AuthStorage {
    let Value::Object(data) = data else {
        panic!("auth fixture must be an object")
    };
    AuthStorage::in_memory_without_env(&AuthStorageData(data), Arc::new(NoOAuth))
}

fn install(path: &Path, server: &Loopback) {
    let urls = ["openrouter", "openai", "anthropic", "google", "default"]
        .into_iter()
        .map(|source| (source.to_string(), format!("{}/{}", server.base, source)))
        .collect();
    let catalog = pa_models::ModelCatalog::with_discovery_urls(
        Some(path.parent().unwrap().join("models")),
        Some(path.parent().unwrap().join("no-bundled-assets")),
        &format!("{}/curated", server.base),
        &format!("{}/prime/api/v1", server.base),
        &urls,
        &server.base,
    );
    catalog_chain::install_catalog(path, Arc::new(catalog));
}

fn write_config(path: &Path, providers: Value) {
    let document = Value::Object(serde_json::Map::from_iter([(
        "providers".to_string(),
        providers,
    )]));
    std::fs::write(path, document.to_string()).unwrap();
}

fn selected(source: &str) -> Vec<String> {
    vec![source.to_string()]
}

#[tokio::test]
async fn source_filtered_refresh_uses_models_json_key_then_local_apply_needs_no_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models.json");
    let model = pa_models::transports::compiled_models()
        .iter()
        .find(|model| model.provider == "openai")
        .unwrap()
        .clone();
    let server = Loopback::start(HashMap::from([(
        "/openai".into(),
        Reply::json(json!({"data":[{"id":model.id}]})),
    )]))
    .await;
    install(&path, &server);
    write_config(
        &path,
        json!({"openai":{"apiKey":"fixture-models-json-key", "headers":{}}}),
    );
    let mut registry = ModelRegistry::create(auth(json!({})), &path);
    assert!(
        registry.get_error().is_none(),
        "valid provider request config: {:?}",
        registry.get_error()
    );
    assert!(
        !registry.auth.has_auth("openai"),
        "no stored or ambient credential"
    );
    let report = registry
        .refresh_catalog_sources(Some(&selected("openai")))
        .await;
    assert_eq!(
        report.sources,
        vec![ModelRefreshSourceResult {
            source: "openai".into(),
            status: ModelRefreshStatus::Updated,
            model_count: Some(1),
            reason: None
        }]
    );
    assert!(report.finished_at >= report.started_at);
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        1,
        "source filter sends no unrelated catalog requests"
    );
    assert!(requests[0].starts_with("GET /openai "));
    assert!(requests[0].contains("authorization: Bearer fixture-models-json-key"));
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("fixture-models-json-key"));
    let mut fresh = ModelRegistry::create(auth(json!({})), &path);
    let applied = fresh.apply_catalog_refresh(Some(report.started_at)).await;
    assert!(applied.applied.iter().any(|source| source == "openai"));
    assert!(fresh
        .get_available()
        .iter()
        .any(|candidate| candidate.provider == "openai" && candidate.id == model.id));
    assert_eq!(server.requests(), requests, "local apply does not fetch");
}

#[tokio::test]
async fn ollama_forwards_custom_authorization_only_on_the_configured_origin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models.json");
    let server = Loopback::start(HashMap::from([
        ("/v1/models".into(), Reply::json(json!({"data":[{"id":"fixture-cloud:2099"}]}))),
        ("/api/show".into(), Reply::json(json!({"capabilities":["completion","tools","thinking"], "model_info":{"fixture.context_length":65_536}}))),
    ])).await;
    install(&path, &server);
    write_config(
        &path,
        json!({"ollama-cloud":{
            "baseUrl":format!("{}/v1",server.base),
            "headers":{"Authorization":"Basic fixture-custom-authorization", "X-Do-Not-Forward":"private-header"}
        }}),
    );
    let mut registry = ModelRegistry::create(auth(json!({})), &path);
    let report = registry
        .refresh_catalog_sources(Some(&selected("ollama-cloud")))
        .await;
    assert_eq!(
        report.sources,
        vec![ModelRefreshSourceResult {
            source: "ollama-cloud".into(),
            status: ModelRefreshStatus::Updated,
            model_count: Some(1),
            reason: None
        }]
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(request.contains("authorization: Basic fixture-custom-authorization"));
        assert!(!request.to_ascii_lowercase().contains("x-do-not-forward"));
    }
    assert!(requests
        .iter()
        .any(|request| request.starts_with("POST /api/show ")
            && request.contains(r#"{"model":"fixture-cloud:2099"}"#)));
    assert!(registry
        .get_available()
        .iter()
        .any(|model| model.provider == "ollama-cloud" && model.id == "fixture-cloud:2099"));
}

#[tokio::test]
async fn ollama_cross_origin_override_never_sends_custom_authorization() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models.json");
    let server = Loopback::start(HashMap::new()).await;
    let different_origin = Loopback::start(HashMap::new()).await;
    install(&path, &server);
    write_config(
        &path,
        json!({"ollama-cloud":{
            "baseUrl":format!("{}/v1",different_origin.base),
            "headers":{"Authorization":"Bearer fixture-must-not-leak"}
        }}),
    );
    let mut registry = ModelRegistry::create(auth(json!({})), &path);
    let report = registry
        .refresh_catalog_sources(Some(&selected("ollama-cloud")))
        .await;
    assert_eq!(
        report.sources,
        vec![ModelRefreshSourceResult {
            source: "ollama-cloud".into(),
            status: ModelRefreshStatus::Skipped,
            model_count: None,
            reason: Some(ModelRefreshReason::Unconfigured)
        }]
    );
    assert!(server.requests().is_empty());
    assert!(different_origin.requests().is_empty());
}

#[derive(Default)]
struct OAuthProbe {
    keys: AtomicUsize,
    refreshes: AtomicUsize,
}

impl OAuthIntegration for OAuthProbe {
    fn api_key_for(&self, _: &str, _: &AuthCredential) -> Option<String> {
        self.keys.fetch_add(1, Ordering::SeqCst);
        Some("fixture-oauth-access".into())
    }
    fn refresh(&self, _: &str, _: &AuthStorageData) -> Option<AuthCredential> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        None
    }
}

#[tokio::test]
async fn local_apply_never_runs_command_keys_or_oauth_including_prime_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models.json");
    let marker = dir.path().join("command-ran");
    let server = Loopback::start(HashMap::new()).await;
    install(&path, &server);
    let mut registry = ModelRegistry::create(auth(json!({})), &path);
    let command = format!(
        "!printf ran >> '{}'; printf fixture-command-key",
        marker.display()
    );
    write_config(
        &path,
        json!({
            "anthropic":{"apiKey":command, "headers":{}},
            "ollama-cloud":{"headers":{"Authorization":command}}
        }),
    );
    for prime in [
        json!({"type":"api_key", "key":command}),
        json!({"type":"oauth", "access":"expired", "refresh":"fixture-refresh", "expires":0}),
    ] {
        let probe = Arc::new(OAuthProbe::default());
        let data = AuthStorageData(json!({
            "prime-inference":prime,
            "openai":{"type":"api_key", "key":command},
            "google":{"type":"oauth", "access":"expired", "refresh":"fixture-refresh", "expires":0}
        }).as_object().unwrap().clone());
        registry.auth = AuthStorage::in_memory_without_env(&data, probe.clone());
        let report = registry.apply_catalog_refresh(None).await;
        assert!(
            !marker.exists(),
            "local disk apply executed a command-backed key"
        );
        assert_eq!(
            probe.refreshes.load(Ordering::SeqCst),
            0,
            "local apply renewed OAuth"
        );
        assert_eq!(
            probe.keys.load(Ordering::SeqCst),
            0,
            "local apply resolved OAuth"
        );
        for source in [
            "openai",
            "anthropic",
            "google",
            "ollama-cloud",
            "prime-inference-public",
        ] {
            assert!(
                report.missing.iter().any(|missing| missing == source),
                "configured unsafe source is reported missing: {source}"
            );
        }
    }
    assert!(
        server.requests().is_empty(),
        "local apply made an HTTP request"
    );
}

fn private_wire(model: &Model) -> Value {
    json!({"id":model.id, "display_name":model.name,
        "pricing":{"input_usd_per_mtok":0.0,"output_usd_per_mtok":0.0},
        "specs":{"context_window":model.context_window,"max_output_tokens":model.max_tokens,
            "supports_reasoning":model.reasoning,"modalities":{"input":["text"],"output":["text"]}}
    })
}

fn prime_auth() -> AuthStorage {
    auth(
        json!({"prime-inference":{"type":"api_key", "key":"fixture-prime-key",
        "primeTeam":{"teamId":"fixture-team", "name":"Fixture team"}}}),
    )
}

#[tokio::test]
async fn private_prime_401_tombstone_prevents_fresh_registry_from_loading_valid_legacy_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models.json");
    let server = Loopback::start(HashMap::from([(
        "/prime/api/v1/models".into(),
        Reply {
            status: 401,
            body: json!({}),
            gate: None,
        },
    )]))
    .await;
    install(&path, &server);
    let model = private_auth::get_private_prime_inference_models()
        .into_iter()
        .next()
        .unwrap();
    let fingerprint =
        private_auth::private_prime_authorization_fingerprint("fixture-prime-key", "fixture-team");
    private_auth::write_private_prime_authorization_cache(
        &path,
        &private_auth::PrivatePrimeAuthorizationCache {
            fingerprint: fingerprint.clone(),
            items: vec![private_wire(&model)],
            models: vec![model.clone()],
            refreshed_at: now_millis(),
        },
    );
    let mut registry = ModelRegistry::create(prime_auth(), &path);
    registry.load_private_authorization_from_cache();
    assert!(registry
        .get_available()
        .iter()
        .any(|candidate| candidate.provider == "prime-inference" && candidate.id == model.id));
    let report = registry
        .refresh_catalog_sources(Some(&selected("prime-inference-private")))
        .await;
    assert_eq!(
        report.sources,
        vec![ModelRefreshSourceResult {
            source: "prime-inference-private".into(),
            status: ModelRefreshStatus::Failed,
            reason: Some(ModelRefreshReason::AuthRejected),
            model_count: None
        }]
    );
    let cached = private_auth::read_private_prime_authorization_cache(&path)
        .expect("valid legacy file intentionally remains");
    assert_eq!(cached.fingerprint, fingerprint);
    assert!(cached
        .models
        .iter()
        .any(|candidate| candidate.id == model.id));
    let revisioned =
        private_auth::private_catalog_cache(Some(&path), &format!("{}/prime/api/v1", server.base));
    revisioned.reload_from_disk(&fingerprint);
    assert!(
        revisioned.has_revocation(&fingerprint),
        "revocation is on disk, not only stale auth state"
    );
    let mut fresh = ModelRegistry::create(prime_auth(), &path);
    fresh.load_private_authorization_from_cache();
    assert!(!fresh
        .get_available()
        .iter()
        .any(|candidate| candidate.provider == "prime-inference" && candidate.id == model.id));
    let applied = fresh.apply_catalog_refresh(None).await;
    assert!(applied
        .missing
        .iter()
        .any(|source| source == "prime-inference-private"));
    assert!(fresh.authorized_private_ids.is_empty());
    assert_eq!(
        server.requests().len(),
        1,
        "local reload/apply never retries authorization"
    );
}

#[tokio::test]
async fn timed_out_restore_keeps_late_publication_and_zero_budget_cached_fast_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models.json");
    let id = "fixture/published-only-2099-12-31";
    let gate = Arc::new(Notify::new());
    let mut server = Loopback::start(HashMap::from([
        ("/openrouter".into(), Reply { status:200, gate:Some(Arc::clone(&gate)), body:json!({"data":[{
            "id":id,"name":"Published dated model","context_length":200_000,
            "top_provider":{"max_completion_tokens":16_384},
            "pricing":{"prompt":"0.000001","completion":"0.000002"},
            "architecture":{"input_modalities":["text","image"],"output_modalities":["text"]}
        }]}) }),
        ("/default".into(), Reply::json(json!({"schemaVersion":1,"defaultModel":format!("openrouter/{id}")}))),
        // Invalid aggregate intentionally leaves the trusted bundled/compiled
        // baseline alone; provider-owned discovery is the readiness oracle.
        ("/curated".into(), Reply::json(json!({"schemaVersion":1,"models":[]}))),
    ])).await;
    install(&path, &server);
    // A caller deadline must cover both a held HTTP response and a sibling's
    // advisory publication lock. Neither deadline may cancel the owned job.
    std::fs::create_dir_all(dir.path().join("models")).unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.path().join("models/openrouter-models.v1.json.lock"))
        .unwrap();
    lock.lock().unwrap();
    let mut registry = ModelRegistry::create(
        auth(json!({"openrouter":{"type":"api_key","key":"fixture-openrouter-key"}})),
        &path,
    );
    assert!(!registry
        .get_all()
        .iter()
        .any(|model| model.provider == "openrouter" && model.id == id));
    let zero = tokio::time::timeout(
        Duration::from_millis(50),
        session_restore::find_session_model_with_readiness_wait(&mut registry, "openrouter", id, 0),
    )
    .await
    .expect("an uncached zero-budget call cannot wait for network");
    assert!(zero.is_none());
    let mut restore = Box::pin(session_restore::find_session_model_with_readiness_wait(
        &mut registry,
        "openrouter",
        id,
        200,
    ));
    loop {
        tokio::select! {
            result = &mut restore => panic!("restore returned before discovery started: {result:?}"),
            request = server.observed.recv() => {
                if request.as_deref() == Some("/openrouter") { break; }
                assert!(request.is_some(), "fixture listener stopped");
            }
            () = tokio::time::sleep(Duration::from_secs(2)) => panic!("provider discovery never started"),
        }
    }
    let result = tokio::time::timeout(Duration::from_secs(1), &mut restore)
        .await
        .expect("caller deadline covers held HTTP");
    assert!(result.is_none(), "held HTTP cannot have published the ID");
    drop(restore);
    gate.notify_one();
    // Give the response time to reach the held publication lock. This caller
    // also has a finite budget; a lock cannot turn readiness into an infinite wait.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        session_restore::find_session_model_with_readiness_wait(
            &mut registry,
            "openrouter",
            id,
            200,
        ),
    )
    .await
    .expect("caller deadline covers held publication lock");
    assert!(result.is_none(), "the lock still prevents publication");
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.starts_with("GET /openrouter "))
            .count(),
        1,
        "a second caller coalesces with the retained discovery"
    );
    drop(lock);
    let model = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            registry.refresh();
            if let Some(model) = registry
                .get_all()
                .iter()
                .find(|model| model.provider == "openrouter" && model.id == id)
                .cloned()
            {
                break model;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the owned job publishes after the lock releases");
    let cached = tokio::time::timeout(
        Duration::from_millis(50),
        session_restore::find_session_model_with_readiness_wait(&mut registry, "openrouter", id, 0),
    )
    .await
    .expect("cached zero-budget lookup stays immediate")
    .expect("the exact discovered ID remains restorable");
    assert_eq!(cached.id, id);
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.starts_with("GET /openrouter "))
            .count(),
        1,
        "late recovery and cached lookup do not refetch"
    );
    assert_eq!(
        (
            model.provider.as_str(),
            model.id.as_str(),
            model.context_window,
            model.max_tokens
        ),
        ("openrouter", id, 200_000, 16_384)
    );
    assert_eq!(model.cost.input.as_f64(), 1.0);
    assert_eq!(model.cost.output.as_f64(), 2.0);
    assert!(server
        .requests()
        .iter()
        .filter(|request| request.starts_with("GET /openrouter "))
        .all(|request| request.contains("authorization: Bearer fixture-openrouter-key")));
}
