//! Production catalog startup runs in env-cleared child processes. A counted,
//! non-forwarding proxy fences every non-loopback destination, even under mutation.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pa_models::{
    CatalogFormat, CatalogInputs, ModelCatalog, RefreshPolicy, RefreshRequest, ResolvedSource,
};
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

fn fixture_catalog(dir: &Path, base: &str) -> ModelCatalog {
    let urls = [
        "openrouter",
        "openai",
        "anthropic",
        "google",
        "openai-codex",
        "prime-inference",
    ]
    .into_iter()
    .map(|provider| (provider.to_string(), format!("{base}/{provider}")))
    .collect();
    ModelCatalog::with_discovery_urls(
        Some(dir.into()),
        None,
        &format!("{base}/catalog"),
        pa_models::prime_inference::PRIME_INFERENCE_BASE_URL,
        &urls,
        base,
    )
}

fn fixture_inputs(base: &str, configured: bool) -> CatalogInputs {
    let mut sources = vec![ResolvedSource {
        id: "prime-inference-public".into(),
        provider: "prime-inference".into(),
        format: CatalogFormat::PrimeInference,
        url: format!("{base}/prime-inference"),
        headers: if configured {
            vec![
                ("authorization".into(), "Bearer fixture-key".into()),
                ("x-prime-team-id".into(), "fixture-team".into()),
            ]
        } else {
            Vec::new()
        },
        unknown_models: pa_models::UnknownModelPolicy::KnownOnly,
        is_current: None,
    }];
    if configured {
        for (provider, format) in [
            ("openai", CatalogFormat::OpenAi),
            ("openrouter", CatalogFormat::OpenRouter),
            ("anthropic", CatalogFormat::Anthropic),
            ("google", CatalogFormat::Google),
            ("openai-codex", CatalogFormat::Codex),
            ("ollama-cloud", CatalogFormat::Ollama),
        ] {
            sources.push(ResolvedSource {
                id: provider.into(),
                provider: provider.into(),
                format,
                url: format!("{base}/{provider}"),
                headers: vec![("authorization".into(), "Bearer fixture-key".into())],
                unknown_models: pa_models::UnknownModelPolicy::KnownOnly,
                is_current: None,
            });
        }
        let mut private = sources[0].clone();
        private.id = "prime-inference-private".into();
        private.format = CatalogFormat::PrimeInferencePrivate;
        sources.push(private);
    }
    CatalogInputs {
        sources,
        seeds: Vec::new(),
    }
}

fn catalog_entry(model: &pa_types::ai::Model) -> serde_json::Value {
    json!({
        "id": model.id, "name": model.name, "api": model.api,
        "provider": model.provider, "baseUrl": model.base_url,
        "reasoning": model.reasoning, "thinkingLevelMap": model.thinking_level_map,
        "input": model.input, "cost": model.cost,
        "contextWindow": model.context_window, "maxTokens": model.max_tokens,
        "featured": model.featured, "compat": model.compat,
    })
}

fn curated_payload(online: bool) -> serde_json::Value {
    let compiled = pa_models::transports::compiled_models();
    let mut models: Vec<_> = compiled
        .iter()
        .filter(|model| model.provider != "prime-inference")
        .map(catalog_entry)
        .collect();
    if online {
        let mut model = compiled
            .iter()
            .find(|model| model.provider == "anthropic")
            .unwrap()
            .clone();
        model.id = "offline-fixture-model".into();
        models.push(catalog_entry(&model));
    }
    json!({"schemaVersion": 1, "models": models})
}

fn prime_payload() -> serde_json::Value {
    json!({"data": pa_models::transports::prime_inference_offline_entries()
        .iter().map(|model| json!({
            "id": model.id, "display_name": model.name,
            "pricing": {"input_usd_per_mtok": 0, "output_usd_per_mtok": 0},
            "specs": {"context_window": model.context_window, "max_output_tokens": model.max_tokens,
                "supports_reasoning": model.reasoning,
                "modalities": {"input": ["text"], "output": ["text"]}},
        })).collect::<Vec<_>>()})
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
            "models-warm" | "models" => {
                let base = std::env::var("PA_OFFLINE_LOOPBACK_URL").unwrap();
                let dir = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
                    .join("agent/models");
                std::fs::create_dir_all(&dir).unwrap();
                let catalog = Arc::new(fixture_catalog(&dir, &base));
                let inputs = fixture_inputs(&base, true);
                if action == "models-warm" {
                    let publication = catalog
                        .refresh(
                            &inputs,
                            &RefreshRequest {
                                policy: RefreshPolicy::Force,
                                sources: Some(vec![
                                    "curated".into(),
                                    "prime-inference-public".into(),
                                ]),
                            },
                        )
                        .await;
                    assert_eq!(publication.sources.len(), 2);
                    assert!(
                        publication.sources.iter().all(|report| report.status
                            == pa_types::model_refresh::ModelRefreshStatus::Updated),
                        "fixture warm reports: {:?}",
                        publication.sources
                    );
                    return;
                }
                let before = files(&dir);
                let resolved = catalog.snapshot(&inputs).models;
                let model = pa_models::transports::compiled_models()
                    .iter()
                    .find(|model| model.provider == "anthropic")
                    .unwrap()
                    .clone();
                assert!(resolved.iter().any(|item| item.id == model.id));
                for inputs in [fixture_inputs(&base, false), inputs] {
                    let expected: std::collections::BTreeSet<_> = inputs
                        .sources
                        .iter()
                        .map(|source| source.id.clone())
                        .chain(["curated".into(), "default".into()])
                        .collect();
                    let publication = catalog
                        .refresh(
                            &inputs,
                            &RefreshRequest {
                                policy: RefreshPolicy::Force,
                                sources: None,
                            },
                        )
                        .await;
                    assert_eq!(
                        publication
                            .sources
                            .iter()
                            .map(|report| report.source.clone())
                            .collect::<std::collections::BTreeSet<_>>(),
                        expected
                    );
                    let prime = publication
                        .sources
                        .iter()
                        .find(|report| report.source == "prime-inference-public")
                        .unwrap();
                    if inputs.sources[0].headers.is_empty() {
                        assert_eq!(
                            prime.model_count, None,
                            "public startup cannot use credential-scoped metadata"
                        );
                    } else {
                        assert_eq!(
                            prime.model_count,
                            Some(prime_payload()["data"].as_array().unwrap().len()),
                            "credential-scoped cache remains usable without renewing freshness"
                        );
                    }
                    assert!(
                        publication.sources.iter().all(|report| matches!(
                            report.reason,
                            Some(
                                pa_types::model_refresh::ModelRefreshReason::Offline
                                    | pa_types::model_refresh::ModelRefreshReason::Unconfigured
                            )
                        )),
                        "{:?}",
                        publication.sources
                    );
                }
                let path = dir.parent().unwrap().join("models.json");
                pa_core::models::install_catalog(&path, Arc::clone(&catalog));
                let _ = pa_core::models::startup_refresh(dir.parent().unwrap());
                tokio::time::timeout(Duration::from_secs(5), async {
                    while runtime.metrics().num_alive_tasks() != 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("background refresh settled");
                assert_eq!(
                    catalog.snapshot(&fixture_inputs(&base, true)).models,
                    resolved
                );
                assert_eq!(
                    files(&dir),
                    before,
                    "offline never renews freshness or publishes auth snapshots"
                );
                let payload = curated_payload(false);
                let bundled = tempfile::tempdir().unwrap();
                std::fs::write(
                    bundled.path().join("models.bundled.json"),
                    serde_json::to_vec(&payload).unwrap(),
                )
                .unwrap();
                let bundled = ModelCatalog::with_bundled_dir(None, Some(bundled.path().into()));
                assert!(bundled
                    .snapshot(&CatalogInputs::default())
                    .models
                    .iter()
                    .any(|item| item.id == model.id));
                let missing = tempfile::tempdir().unwrap();
                let fallback = ModelCatalog::with_bundled_dir(None, Some(missing.path().into()));
                assert_eq!(
                    fallback.snapshot(&CatalogInputs::default()).models,
                    pa_models::transports::compiled_models()
                );
            }
            "online" => {
                let base = std::env::var("PA_OFFLINE_LOOPBACK_URL").unwrap();
                let dir = tempfile::tempdir().unwrap();
                let catalog = ModelCatalog::with_urls(
                    Some(dir.path().into()),
                    None,
                    &format!("{base}/online"),
                    &base,
                );

                let publication = catalog
                    .refresh(
                        &CatalogInputs::default(),
                        &RefreshRequest {
                            policy: RefreshPolicy::Force,
                            sources: Some(vec!["curated".into()]),
                        },
                    )
                    .await;
                assert_eq!(
                    publication.sources[0].status,
                    pa_types::model_refresh::ModelRefreshStatus::Updated,
                    "online control reports: {:?}",
                    publication.sources
                );
                assert!(publication
                    .snapshot
                    .models
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

async fn run_child(action: &str, root: &Path, url: &str, endpoint: &str) -> std::process::Output {
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
    child
        .env_clear()
        .kill_on_drop(true)
        .args(["--exact", "startup_child", "--nocapture"])
        .env(CHILD, action)
        .env("HOME", root)
        .env("PI_PACKAGE_DIR", root)
        .env("PRIME_AGENT_CODING_AGENT_DIR", root.join("agent"))
        .env("PRIME_AGENT_KERNEL_VENV", root.join("kernel"))
        .env("RLM_SESSION_DIR", root.join("sessions"))
        .env(
            "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
            root.join("supervisor.sock"),
        )
        .env("TMPDIR", root)
        .env(
            "PI_OFFLINE",
            if matches!(action, "online" | "fence" | "models-warm") {
                "0"
            } else {
                "1"
            },
        )
        .env("PA_OFFLINE_LOOPBACK_URL", url);
    for name in [
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        child.env(name, root.join(name));
    }
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        child.env(name, endpoint);
    }
    for name in ["NO_PROXY", "no_proxy"] {
        child.env(name, "127.0.0.1,localhost,::1");
    }
    tokio::time::timeout(Duration::from_secs(15), child.output())
        .await
        .expect("finite child")
        .unwrap()
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
    let url = format!("http://{}", local.local_addr().unwrap());
    let counter = Arc::clone(&served);
    let local_task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = local.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let request = String::from_utf8(request).unwrap();
            let path = request.split_whitespace().nth(1).unwrap();
            let payload = match path {
                "/prime-inference" => prime_payload(),
                "/catalog" | "/online" => curated_payload(path == "/online"),
                _ => panic!("unexpected fixture request: {path}"),
            };
            let body = payload.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    for &action in actions {
        let root = tempfile::tempdir().unwrap();
        if action == "models" {
            let warm = run_child("models-warm", root.path(), &url, &endpoint).await;
            assert!(
                warm.status.success(),
                "online fixture warm: {}{}",
                String::from_utf8_lossy(&warm.stdout),
                String::from_utf8_lossy(&warm.stderr)
            );
            assert_eq!(
                denied.load(Ordering::SeqCst),
                0,
                "fixture warm stayed on loopback"
            );
        }
        let output = run_child(action, root.path(), &url, &endpoint).await;
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
        usize::from(actions.contains(&"online")) + 2 * usize::from(actions.contains(&"models")),
        "only explicit online fixture warms and online control reach loopback"
    );
    local_task.abort();
    proxy_task.abort();
}

#[tokio::test]
async fn production_default_catalog_makes_zero_offline_requests() {
    fenced_children(&["models"]).await;
}

#[tokio::test]
async fn online_loopback_and_rejecting_proxy_are_live() {
    fenced_children(&["online", "fence"]).await;
}
