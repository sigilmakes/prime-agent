//! Facade-level source orchestration tests. All endpoints are loopback and state is temporary.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_types::model_refresh::{
    ModelRefreshApplyReport, ModelRefreshReason as Reason, ModelRefreshSourceResult,
    ModelRefreshStatus as Status,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{watch, Semaphore};

use crate::{DiscoveryCredential, DiscoveryCredentials, ModelCatalog};

#[derive(Clone)]
struct Request {
    path: String,
    authorization: Option<String>,
    body: Value,
}

struct Response {
    status: u16,
    body: Value,
    gate: Option<Arc<Semaphore>>,
}

impl Response {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            body,
            gate: None,
        }
    }
    fn status(status: u16) -> Self {
        Self {
            status,
            body: json!({}),
            gate: None,
        }
    }
}

struct Server {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    count: watch::Receiver<usize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn start(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handler = Arc::new(handler);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let records = Arc::clone(&requests);
        let (count_tx, count) = watch::channel(0);
        let task = tokio::spawn(async move {
            let mut sockets = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let handler = Arc::clone(&handler);
                let records = Arc::clone(&records);
                let count_tx = count_tx.clone();
                sockets.spawn(async move {
                    let mut bytes = Vec::new();
                    let mut chunk = [0u8;4096];
                    let head_end = loop {
                        let read = socket.read(&mut chunk).await.unwrap();
                        assert!(read > 0);
                        bytes.extend_from_slice(&chunk[..read]);
                        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {break index + 4;}
                        assert!(bytes.len() < 16_384);
                    };
                    let head = String::from_utf8_lossy(&bytes[..head_end]).to_lowercase();
                    let length = head.lines().find_map(|line|line.strip_prefix("content-length: "))
                        .map_or(0, |value|value.trim().parse::<usize>().unwrap());
                    while bytes.len() < head_end + length {
                        let read=socket.read(&mut chunk).await.unwrap();assert!(read>0);bytes.extend_from_slice(&chunk[..read]);
                    }
                    let request = Request {
                        path: head.lines().next().unwrap().split_whitespace().nth(1).unwrap().into(),
                        authorization: head.lines().find_map(|line|line.strip_prefix("authorization: ")).map(str::to_string),
                        body: if length == 0 {Value::Null} else {serde_json::from_slice(&bytes[head_end..head_end+length]).unwrap()},
                    };
                    let response = handler(&request);
                    let observed = {let mut records=records.lock().unwrap();records.push(request);records.len()};
                    count_tx.send_modify(|count|*count=observed);
                    if let Some(gate) = response.gate {gate.acquire_owned().await.unwrap().forget();}
                    let body=response.body.to_string();
                    let status=response.status;
                    let reply=format!("HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len());
                    socket.write_all(reply.as_bytes()).await.unwrap();
                });
            }
        });
        Self {
            base,
            requests,
            count,
            task,
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    async fn received(&self, count: usize) {
        let mut receiver = self.count.clone();
        tokio::time::timeout(
            Duration::from_secs(2),
            receiver.wait_for(|observed| *observed >= count),
        )
        .await
        .unwrap()
        .unwrap();
    }

    fn catalog(&self, dir: &std::path::Path) -> ModelCatalog {
        let urls: HashMap<String, String> =
            ["openrouter", "openai", "anthropic", "google", "default"]
                .into_iter()
                .map(|id| (id.into(), format!("{}/{id}", self.base)))
                .collect();
        ModelCatalog::with_discovery_urls(
            Some(dir.into()),
            Some(dir.into()),
            &format!("{}/curated", self.base),
            &format!("{}/prime", self.base),
            &urls,
            &self.base,
        )
    }
}

fn credentials(provider: &str, secret: &str) -> DiscoveryCredentials {
    [(
        provider.into(),
        DiscoveryCredential {
            secret: secret.into(),
            is_current: None,
        },
    )]
    .into_iter()
    .collect()
}

fn only(source: &str) -> Vec<String> {
    vec![source.into()]
}

fn rich(id: &str) -> Value {
    json!({"id":id,"context_length":32_000,"top_provider":{"max_completion_tokens":4096},"pricing":{"prompt":"0.000001","completion":"0.000002"},"architecture":{"input_modalities":["text"],"output_modalities":["text"]}})
}

fn result(
    source: &str,
    status: Status,
    reason: Option<Reason>,
    count: Option<usize>,
) -> ModelRefreshSourceResult {
    ModelRefreshSourceResult {
        source: source.into(),
        status,
        reason,
        model_count: count,
    }
}

#[tokio::test]
async fn scoped_provider_updates_discard_old_accounts_and_persist_partial_metadata() {
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server = Server::start(move |request| {
        if request.path != "/openrouter" {
            return Response::status(500);
        }
        if requests.fetch_add(1, Ordering::SeqCst) == 2 {
            return Response::json(json!({"data":[{
                "id":"test/account-b", "name":"Partial update",
                "headers":{"Authorization":"attacker"}, "baseUrl":"https://attacker.invalid"
            }]}));
        }
        let id = if request.authorization.as_deref() == Some("bearer secret-account-a") {
            "test/account-a"
        } else {
            "test/account-b"
        };
        Response::json(json!({"data":[rich(id)]}))
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let catalog = server.catalog(dir.path());
    let a = credentials("openrouter", "secret-account-a");
    let b = credentials("openrouter", "secret-account-b");
    let sources = only("openrouter");
    let expected_count = crate::transports::compiled_models()
        .iter()
        .filter(|model| model.provider == "openrouter")
        .count()
        + 1;
    assert_eq!(
        catalog
            .refresh_catalog_sources(true, None, &a, Some(&sources))
            .await,
        vec![result(
            "openrouter",
            Status::Updated,
            None,
            Some(expected_count)
        )]
    );
    assert!(catalog
        .resolve_with_discovery(None, &a)
        .iter()
        .any(|model| model.id == "test/account-a"));
    assert!(!catalog
        .resolve_with_discovery(None, &b)
        .iter()
        .any(|model| model.id == "test/account-a"));
    assert_eq!(
        catalog
            .refresh_catalog_sources(true, None, &b, Some(&sources))
            .await,
        vec![result(
            "openrouter",
            Status::Updated,
            None,
            Some(expected_count)
        )]
    );
    let before_partial = catalog.resolve_with_discovery(None, &b);
    assert_eq!(
        catalog
            .refresh_catalog_sources(true, None, &b, Some(&sources))
            .await,
        vec![result(
            "openrouter",
            Status::Updated,
            None,
            Some(expected_count)
        )]
    );
    let models = catalog.resolve_with_discovery(None, &b);
    let mut expected = before_partial;
    expected
        .iter_mut()
        .find(|model| model.id == "test/account-b")
        .unwrap()
        .name = "Partial update".into();
    assert_eq!(models, expected);
    assert!(models.iter().any(|model| model.id == "test/account-b"));
    assert!(!models.iter().any(|model| model.id == "test/account-a"));
    let fresh = server.catalog(dir.path());
    assert!(fresh
        .apply_catalog_snapshots(None, &b, None)
        .applied
        .contains(&"openrouter".into()));
    assert_eq!(fresh.resolve_with_discovery(None, &b), models);
    assert_eq!(server.count(), 3);
    let snapshot = std::fs::read_to_string(dir.path().join("openrouter-models.v1.json")).unwrap();
    assert!(!snapshot.contains("attacker"));
    assert!(!snapshot.contains("Bearer"));
    assert!(!snapshot.contains("secret-account-b"));
}

#[tokio::test]
async fn source_filtering_default_status_and_local_apply_are_credential_free_and_network_free() {
    let known = crate::transports::compiled_models()
        .iter()
        .find(|model| model.provider == "anthropic")
        .unwrap()
        .id
        .clone();
    let selected_id = known.clone();
    let server = Server::start(move |request| match request.path.as_str() {
        "/default" => Response::json(
            json!({"schemaVersion":1,"defaultModel":format!("anthropic/{selected_id}")}),
        ),
        "/anthropic" => {
            Response::json(json!({"data":[{"id":selected_id,"display_name":"Fresh name"}]}))
        }
        _ => Response::status(500),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let catalog = server.catalog(dir.path());
    let auth = credentials("anthropic", "never-persist-me");
    let sources = vec!["openai".into(), "anthropic".into(), "default".into()];
    let count = crate::transports::compiled_models()
        .iter()
        .filter(|model| model.provider == "anthropic")
        .count();
    let reports = catalog
        .refresh_catalog_sources(true, None, &auth, Some(&sources))
        .await;
    assert_eq!(
        reports,
        vec![
            result("openai", Status::Skipped, Some(Reason::Unconfigured), None),
            result("anthropic", Status::Updated, None, Some(count)),
            result("default", Status::Updated, None, Some(1))
        ]
    );
    assert!(!serde_json::to_string(&reports)
        .unwrap()
        .contains("never-persist-me"));
    assert_eq!(
        catalog.preferred_default(),
        Some(format!("anthropic/{known}"))
    );
    let reports = catalog
        .refresh_catalog_sources(false, None, &auth, Some(&sources))
        .await;
    assert_eq!(
        reports,
        vec![
            result("openai", Status::Skipped, Some(Reason::Unconfigured), None),
            result("anthropic", Status::Stale, None, Some(count)),
            result("default", Status::Stale, None, Some(1))
        ]
    );
    let fresh = server.catalog(dir.path());
    let applied = fresh.apply_catalog_snapshots(None, &auth, None);
    assert_eq!(
        applied,
        ModelRefreshApplyReport {
            applied: vec!["anthropic".into(), "default".into()],
            skipped: vec!["openai".into(), "google".into(), "ollama-cloud".into()],
            missing: vec![
                "openrouter".into(),
                "curated".into(),
                "prime-inference-public".into()
            ]
        }
    );
    assert_eq!(fresh.preferred_default(), catalog.preferred_default());
    assert!(fresh
        .apply_catalog_snapshots(None, &auth, Some(u64::MAX))
        .applied
        .is_empty());
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.path.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["/anthropic", "/default"])
    );
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn ollama_anonymous_fallback_live_multiendpoint_persistence_and_revocation() {
    let revoked = Arc::new(AtomicBool::new(false));
    let denial = Arc::clone(&revoked);
    let server=Server::start(move |request| {
        assert_eq!(request.authorization.as_deref(),Some("bearer ollama-test-key"));
        match request.path.as_str() {
            "/v1/models" if denial.load(Ordering::SeqCst)=>Response::status(403),
            "/v1/models"=>Response::json(json!({"data":[{"id":"cloud/private"}]})),
            "/api/show"=>{
                assert_eq!(request.body,json!({"model":"cloud/private"}));
                Response::json(json!({"capabilities":["tools","vision","thinking"],"model_info":{"test.context_length":256_000},"headers":{"Authorization":"attacker"},"baseUrl":"https://attacker.invalid"}))
            }
            _=>Response::status(500),
        }
    }).await;
    let dir = tempfile::tempdir().unwrap();
    let catalog = server.catalog(dir.path());
    let anonymous = DiscoveryCredentials::new();
    let fallback = crate::ollama_cloud::pinned_models(&server.base);
    let anonymous_models: Vec<_> = catalog
        .resolve_with_discovery(None, &anonymous)
        .into_iter()
        .filter(|model| model.provider == "ollama-cloud")
        .collect();
    assert_eq!(anonymous_models, fallback);
    let sources = only("ollama-cloud");
    assert_eq!(
        catalog
            .refresh_catalog_sources(true, None, &anonymous, Some(&sources))
            .await,
        vec![result(
            "ollama-cloud",
            Status::Skipped,
            Some(Reason::Unconfigured),
            None
        )]
    );
    assert_eq!(server.count(), 0);
    let auth = credentials("ollama-cloud", "Bearer ollama-test-key");
    assert_eq!(
        catalog
            .refresh_catalog_sources(true, None, &auth, Some(&sources))
            .await,
        vec![result("ollama-cloud", Status::Updated, None, Some(1))]
    );
    let expected=crate::ollama_cloud::parse_catalog(&json!({"models":{"cloud/private":{"capabilities":["tools","vision","thinking"],"model_info":{"test.context_length":256_000}}}}),&server.base).unwrap();
    let live = |catalog: &ModelCatalog, auth: &DiscoveryCredentials| {
        catalog
            .resolve_with_discovery(None, auth)
            .into_iter()
            .filter(|model| model.provider == "ollama-cloud")
            .collect::<Vec<_>>()
    };
    assert_eq!(live(&catalog, &auth), expected);
    let stored = std::fs::read_to_string(dir.path().join("ollama-cloud-models.v1.json")).unwrap();
    assert!(!stored.contains("ollama-test-key"));
    assert!(!stored.contains("attacker"));
    let fresh = server.catalog(dir.path());
    fresh.apply_catalog_snapshots(None, &auth, None);
    assert_eq!(live(&fresh, &auth), expected);
    assert_eq!(server.count(), 2);
    assert_eq!(
        live(
            &catalog,
            &credentials("ollama-cloud", "Bearer different-account")
        ),
        fallback
    );
    revoked.store(true, Ordering::SeqCst);
    assert_eq!(
        catalog
            .refresh_catalog_sources(true, None, &auth, Some(&sources))
            .await,
        vec![result(
            "ollama-cloud",
            Status::Failed,
            Some(Reason::AuthRejected),
            None
        )]
    );
    assert_eq!(live(&catalog, &auth), fallback);
    assert_eq!(live(&server.catalog(dir.path()), &auth), fallback);
    assert_eq!(server.count(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_refresh_never_blocks_synchronous_resolution_or_starts_more_network() {
    let gate = Arc::new(Semaphore::new(0));
    let response_gate = Arc::clone(&gate);
    let server = Server::start(move |_| Response {
        status: 200,
        body: json!({"data":[rich("test/after-held-refresh")]}),
        gate: Some(Arc::clone(&response_gate)),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let catalog = Arc::new(server.catalog(dir.path()));
    let auth = credentials("openrouter", "held-key");
    let refresh_catalog = Arc::clone(&catalog);
    let refresh_auth = auth.clone();
    let refresh = tokio::spawn(async move {
        refresh_catalog
            .refresh_catalog_sources(true, None, &refresh_auth, Some(&only("openrouter")))
            .await
    });
    server.received(1).await;
    let resolve_catalog = Arc::clone(&catalog);
    let resolve_auth = auth.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let resolve = std::thread::spawn(move || {
        tx.send(resolve_catalog.resolve_with_discovery(None, &resolve_auth))
            .unwrap();
    });
    let observed = rx.recv_timeout(Duration::from_millis(300));
    gate.add_permits(10);
    let report = tokio::time::timeout(Duration::from_secs(2), refresh)
        .await
        .unwrap()
        .unwrap();
    resolve.join().unwrap();
    let before = observed.expect("resolve must complete while the HTTP response remains held");
    assert!(!before
        .iter()
        .any(|model| model.id == "test/after-held-refresh"));
    assert_eq!(report[0].status, Status::Updated);
    assert!(catalog
        .resolve_with_discovery(None, &auth)
        .iter()
        .any(|model| model.id == "test/after-held-refresh"));
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn changed_credentials_cancel_inflight_publication_without_cross_account_models() {
    let gate = Arc::new(Semaphore::new(0));
    let response_gate = Arc::clone(&gate);
    let server = Server::start(move |_| Response {
        status: 200,
        body: json!({"data":[rich("test/stale-account-a")]}),
        gate: Some(Arc::clone(&response_gate)),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let catalog = Arc::new(server.catalog(dir.path()));
    let authorized = Arc::new(AtomicBool::new(true));
    let check = Arc::clone(&authorized);
    let mut a = credentials("openrouter", "secret-account-a");
    a.get_mut("openrouter").unwrap().is_current =
        Some(Arc::new(move || check.load(Ordering::SeqCst)));
    let refresh_catalog = Arc::clone(&catalog);
    let refresh_auth = a.clone();
    let refresh = tokio::spawn(async move {
        refresh_catalog
            .refresh_catalog_sources(true, None, &refresh_auth, Some(&only("openrouter")))
            .await
    });
    server.received(1).await;
    authorized.store(false, Ordering::SeqCst);
    let b = credentials("openrouter", "secret-account-b");
    assert!(!catalog
        .resolve_with_discovery(None, &b)
        .iter()
        .any(|model| model.id == "test/stale-account-a"));
    gate.add_permits(1);
    assert_eq!(
        refresh.await.unwrap(),
        vec![result(
            "openrouter",
            Status::Stale,
            Some(Reason::Cancelled),
            None
        )]
    );
    assert!(!server
        .catalog(dir.path())
        .resolve_with_discovery(None, &a)
        .iter()
        .any(|model| model.id == "test/stale-account-a"));
    assert!(!dir.path().join("openrouter-models.v1.json").exists());
    assert_eq!(server.count(), 1);
}
