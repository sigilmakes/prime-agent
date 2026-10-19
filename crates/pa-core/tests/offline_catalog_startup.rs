//! Production catalog startup runs in env-cleared child processes. A counted,
//! non-forwarding proxy fences every non-loopback destination, even under mutation.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pa_models::{DiscoveryCredential, DiscoveryCredentials, ModelCatalog, PrimeCredentials};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CHILD: &str = "PA_OFFLINE_STARTUP_CHILD";

fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(path).unwrap(),
            )
        })
        .collect()
}

// Keep the isolated child actions together so the env-cleared entry point is explicit.
#[allow(clippy::too_many_lines)]
#[test]
fn startup_child() {
    let Ok(action) = std::env::var(CHILD) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        match action.as_str() {
            "models" => {
                let dir = tempfile::tempdir().unwrap();
                let model = pa_models::transports::compiled_models()
                    .iter()
                    .find(|model| model.provider == "anthropic")
                    .unwrap()
                    .clone();
                let payload = json!({"schemaVersion": 1, "models": [model]});
                let cache = dir.path().join("provider-model-catalog.v1.json");
                std::fs::write(
                    &cache,
                    serde_json::to_vec(&json!({
                        "url": pa_models::fetch::MODEL_CATALOG_URL,
                        "scope": "public", "fetchedAt": 1, "checkedAt": 2,
                        "payload": payload,
                    }))
                    .unwrap(),
                )
                .unwrap();
                let prime_payload: Vec<_> = pa_models::transports::prime_inference_offline_entries()
                    .iter()
                    .map(|model| json!({
                        "id": model.id, "display_name": model.name,
                        "pricing": {"input_usd_per_mtok": 0, "output_usd_per_mtok": 0},
                        "specs": {"context_window": model.context_window,
                            "max_output_tokens": model.max_tokens,
                            "supports_reasoning": model.reasoning,
                            "modalities": {"input": ["text"], "output": ["text"]}},
                    }))
                    .collect();
                std::fs::write(dir.path().join("prime-inference-models-cache.json"),
                    serde_json::to_vec(&json!({
                        "url": format!("{}/models", pa_models::prime_inference::PRIME_INFERENCE_BASE_URL),
                        "scope": pa_models::prime_inference::scope_key("fixture-key", "fixture-team"),
                        "fetchedAt": 1, "checkedAt": 2, "payload": {"data": prime_payload},
                    })).unwrap()).unwrap();
                let before = files(dir.path());
                let catalog = Arc::new(ModelCatalog::new(Some(dir.path().into())));
                let resolved = catalog.resolve(None);
                assert!(resolved.iter().any(|item| item.id == model.id));
                let prime = PrimeCredentials {
                    api_key: "fixture-key".into(),
                    team_id: Some("fixture-team".into()),
                };
                let credentials: DiscoveryCredentials =
                    ["openai", "anthropic", "google", "ollama-cloud"]
                        .into_iter()
                        .map(|id| {
                            (
                                id.into(),
                                DiscoveryCredential {
                                    secret: "fixture-key".into(),
                                    is_current: None,
                                },
                            )
                        })
                        .collect();
                // Both public/no-auth startup and configured discovery are covered.
                for credentials in [&DiscoveryCredentials::new(), &credentials] {
                    let reports = catalog
                        .refresh_catalog_sources(true, Some(&prime), credentials, None)
                        .await;
                    assert_eq!(reports.len(), 8, "production defaults include every source");
                    assert_eq!(
                        reports.iter().find(|report| report.source == "prime-inference-public")
                            .unwrap().model_count,
                        Some(prime_payload.len()),
                        "credential-scoped cache remains usable without renewing freshness"
                    );
                    assert!(
                        reports.iter().all(|report| matches!(
                            report.reason,
                            Some(
                                pa_types::model_refresh::ModelRefreshReason::Offline
                                    | pa_types::model_refresh::ModelRefreshReason::Unconfigured
                            )
                        )),
                        "{reports:?}"
                    );
                }
                catalog.trigger_refresh_with_credentials(
                    pa_models::RefreshTrigger::Startup,
                    Some(prime),
                );
                tokio::time::timeout(Duration::from_secs(5), async {
                    while runtime.metrics().num_alive_tasks() != 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("background refresh settled");
                assert_eq!(catalog.resolve(None), resolved);
                assert_eq!(
                    files(dir.path()),
                    before,
                    "offline never renews freshness or publishes auth snapshots"
                );
                // The same offline chain still serves bundled and compiled data.
                let bundled = tempfile::tempdir().unwrap();
                std::fs::write(
                    bundled.path().join("models.bundled.json"),
                    serde_json::to_vec(&payload).unwrap(),
                )
                .unwrap();
                let bundled = ModelCatalog::with_bundled_dir(None, Some(bundled.path().into()));
                assert!(bundled.resolve(None).iter().any(|item| item.id == model.id));
                let missing = tempfile::tempdir().unwrap();
                let fallback = ModelCatalog::with_bundled_dir(None, Some(missing.path().into()));
                assert_eq!(
                    fallback.resolve(None),
                    pa_models::transports::compiled_models()
                );
            }
            "mcp" => {
                let dir = tempfile::tempdir().unwrap();
                pa_core::mcp::startup_plugins_refresh(dir.path());
                tokio::time::timeout(Duration::from_secs(5), async {
                    while runtime.metrics().num_alive_tasks() != 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("MCP startup refresh settled");
                assert!(files(dir.path()).is_empty());
            }
            "online" => {
                let base = std::env::var("PA_OFFLINE_LOOPBACK_URL").unwrap();
                let catalog = ModelCatalog::with_urls(None, None, &base, &base);
                let models = catalog
                    .refresh(true)
                    .await
                    .expect("explicit online loopback response");
                assert!(models
                    .iter()
                    .any(|model| model.id == "offline-fixture-model"));
            }
            "fence" => {
                // Validate that the proxy sees HTTPS attempts without DNS/forwarding.
                assert!(pa_models::fetch::CatalogFetcher::new()
                    .fetch("https://offline-startup.invalid/catalog", None)
                    .await
                    .is_err());
            }
            _ => panic!("unknown child action"),
        }
    });
}

// The fixture owns both listeners and all child lifetimes in one place.
#[allow(clippy::too_many_lines)]
async fn fenced_children(actions: &[&str]) {
    let denied = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(AtomicUsize::new(0));
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", proxy.local_addr().unwrap());
    let counter = Arc::clone(&denied);
    let proxy_task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = proxy.accept().await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buffer = [0; 4096];
                let _ =
                    tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buffer)).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 503 Fenced\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
            });
        }
    });
    let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/catalog", local.local_addr().unwrap());
    let counter = Arc::clone(&served);
    let local_task = tokio::spawn(async move {
        let (mut socket, _) = local.accept().await.unwrap();
        let mut buffer = [0; 4096];
        tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        counter.fetch_add(1, Ordering::SeqCst);
        let mut model = pa_models::transports::compiled_models()
            .iter()
            .find(|model| model.provider == "anthropic")
            .unwrap()
            .clone();
        model.id = "offline-fixture-model".into();
        let body = json!({"schemaVersion": 1, "models": [model]}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    for &action in actions {
        let root = tempfile::tempdir().unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
        child
            .env_clear()
            .kill_on_drop(true)
            .args(["--exact", "startup_child", "--nocapture"])
            .env(CHILD, action)
            .env("HOME", root.path())
            .env("PI_PACKAGE_DIR", root.path())
            .env("PRIME_AGENT_CODING_AGENT_DIR", root.path().join("agent"))
            .env("PRIME_AGENT_KERNEL_VENV", root.path().join("kernel"))
            .env("RLM_SESSION_DIR", root.path().join("sessions"))
            .env("RLM_HARNESS_STATE_DIR", root.path().join("harness"))
            .env(
                "RLM_GLOBAL_HARNESS_STATE_DIR",
                root.path().join("global-harness"),
            )
            .env(
                "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
                root.path().join("supervisor.sock"),
            )
            .env("TMPDIR", root.path())
            .env(
                "PI_OFFLINE",
                if action == "online" || action == "fence" {
                    "0"
                } else {
                    "1"
                },
            )
            .env("PA_OFFLINE_LOOPBACK_URL", &url);
        for name in [
            "XDG_CACHE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
        ] {
            child.env(name, root.path().join(name));
        }
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            child.env(name, &endpoint);
        }
        for name in ["NO_PROXY", "no_proxy"] {
            child.env(name, "127.0.0.1,localhost,::1");
        }
        let output = tokio::time::timeout(Duration::from_secs(15), child.output())
            .await
            .expect("finite child")
            .unwrap();
        assert_eq!(
            denied.load(Ordering::SeqCst),
            usize::from(action == "fence"),
            "{action}: forbidden non-loopback requests; child output: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "{action}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        served.load(Ordering::SeqCst),
        usize::from(actions.contains(&"online")),
        "online opt-in bypasses the proxy only for loopback"
    );
    if actions.contains(&"online") {
        local_task.await.unwrap();
    } else {
        local_task.abort();
    }
    proxy_task.abort();
}

#[tokio::test]
async fn production_default_catalog_makes_zero_offline_requests() {
    fenced_children(&["models"]).await;
}

#[tokio::test]
async fn production_mcp_startup_makes_zero_offline_requests() {
    fenced_children(&["mcp"]).await;
}

#[tokio::test]
async fn online_loopback_and_rejecting_proxy_are_live() {
    fenced_children(&["online", "fence"]).await;
}
