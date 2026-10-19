//! Session-local Ollama Cloud caps and the native request-payload overlay.
//! Baselines belong to a session, never to the shared model catalog.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_agent::stream::StreamFn;
use pa_types::ai::Model;

use crate::models::OllamaCloudConfig;

/// Keeps uncapped model snapshots across selection, routing, and reload.
/// Hosts share one instance with their core session so passing a configured
/// model back into the session does not turn its caps into a new baseline.
/// Fresh model descriptors supersede old catalog metadata, while exact old
/// configured snapshots can still restore their original explicit transport.
#[derive(Default)]
pub struct SessionOllamaCloud {
    snapshots: Mutex<Vec<(Model, Model)>>,
}

impl SessionOllamaCloud {
    /// Reconfigure a captured target or a previously configured snapshot.
    /// Fresh registry selections must use `configure_fresh_model` instead;
    /// otherwise a catalog value equal to an old cap could restore stale data.
    /// Only Ollama Cloud models participate; unrelated models are unchanged.
    pub fn configure_model(&self, cwd: &Path, agent_dir: &Path, model: &Model) -> Model {
        let original = self
            .snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|(_, configured)| configured == model)
            .map_or_else(|| model.clone(), |(original, _)| original.clone());
        self.configure_fresh_model(cwd, agent_dir, &original)
    }

    /// Apply caps at a fresh registry ingress. Fresh catalog metadata wins even
    /// when it has the same values as an older configured snapshot.
    pub fn configure_fresh_model(&self, cwd: &Path, agent_dir: &Path, model: &Model) -> Model {
        if model.provider != "ollama-cloud" {
            return model.clone();
        }
        let configured = OllamaCloudConfig::load(cwd, agent_dir).configure_model(model);
        let mut snapshots = self
            .snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pair = (model.clone(), configured.clone());
        if let Some(index) = snapshots.iter().position(|existing| existing == &pair) {
            snapshots.remove(index);
        }
        // Record unchanged models too: an authoritative fresh baseline must
        // supersede an older cap with identical values.
        snapshots.push(pair);
        configured
    }

    /// Apply caps to the loop's smaller model descriptor without losing its
    /// uncapped snapshot when the full host model already carries those caps.
    pub fn configure_agent_model(
        &self,
        cwd: &Path,
        agent_dir: &Path,
        model: &pa_agent::types::Model,
    ) -> pa_agent::types::Model {
        if model.provider != "ollama-cloud" {
            return model.clone();
        }
        // The agent descriptor is deliberately smaller than the registry model
        // (and uses base_url, not baseUrl). Do not round-trip it into pa-types.
        let full = agent_model(model);
        let original = self
            .snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|(_, configured)| {
                configured.id == full.id
                    && configured.provider == full.provider
                    && configured.api == full.api
                    && configured.base_url == full.base_url
                    && configured.context_window == full.context_window
                    && configured.max_tokens == full.max_tokens
                    && configured.max_tokens_explicit == full.max_tokens_explicit
            })
            .map(|(original, _)| original.clone());
        // Provenance was resolved above. Do not interpret that baseline as
        // another configured snapshot a second time.
        let configured =
            self.configure_fresh_model(cwd, agent_dir, original.as_ref().unwrap_or(&full));
        let mut model = model.clone();
        model.context_window = configured.context_window;
        model.max_tokens = configured.max_tokens;
        model.max_tokens_explicit = configured.max_tokens_explicit;
        model
    }
}

fn agent_model(model: &pa_agent::types::Model) -> Model {
    Model {
        id: model.id.clone(),
        name: model.name.clone(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        base_url: model.base_url.clone(),
        reasoning: model.reasoning,
        thinking_level_map: None,
        input: Vec::new(),
        cost: pa_types::ai::ModelCost {
            input: model.cost.input.into(),
            output: model.cost.output.into(),
            cache_read: model.cost.cache_read.into(),
            cache_write: model.cost.cache_write.into(),
        },
        context_window: model.context_window,
        max_tokens: model.max_tokens,
        max_tokens_explicit: model.max_tokens_explicit,
        featured: None,
        headers: None,
        compat: None,
    }
}

pub(crate) fn wrap_stream_fn(
    cwd: PathBuf,
    agent_dir: PathBuf,
    models: Arc<SessionOllamaCloud>,
    inner: StreamFn,
) -> StreamFn {
    Arc::new(move |model, context, mut options| {
        let model = models.configure_agent_model(&cwd, &agent_dir, &model);
        // Pinned host adapters can retain the uncapped full model. Carry the
        // explicit output cap through the existing request options as well.
        // A caller's explicit one-off budget still wins, like TS streamSimple.
        if model.provider == "ollama-cloud" {
            let budget = pa_ai::effective_request_max_tokens(
                &agent_model(&model),
                super::provider_adapter::model_thinking_level(options.reasoning),
            );
            options.max_tokens = options.max_tokens.or(Some(budget));
        }
        let user_hook = options.on_payload.take();
        let cwd = cwd.clone();
        let agent_dir = agent_dir.clone();
        // Inspect the actual provider model passed to the hook, not the session
        // model: image routing and provider failover may have changed it.
        options.on_payload = Some(Arc::new(move |payload, request_model| {
            let payload = if request_model.provider == "ollama-cloud" {
                OllamaCloudConfig::load(&cwd, &agent_dir)
                    .overlay_request(payload, &request_model.id)
            } else {
                payload
            };
            // None means no replacement, not loss of the native overlay.
            user_hook
                .as_ref()
                .and_then(|hook| hook(payload.clone(), request_model))
                .or(Some(payload))
        }));
        inner(model, context, options)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model() -> Model {
        serde_json::from_value(json!({
            "id": "glm-5", "name": "Gateway model", "api": "openai-completions",
            "provider": "ollama-cloud", "baseUrl": "http://127.0.0.1:9/gateway/v1",
            "reasoning": false, "input": ["text", "image"],
            "contextWindow": 200_000, "maxTokens": 32000,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "headers": {"X-Gateway": "preserve"}, "featured": true,
            "compat": {"supportsDeveloperRole": false, "maxTokensField": "max_tokens"}
        }))
        .unwrap()
    }

    fn write_caps(dir: &Path, context: u64, output: u64) {
        std::fs::write(
            dir.join("ollama-cloud.json"),
            json!({
                "modelOverrides": {"glm-5": {"contextWindow": context, "maxTokens": output}}
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn cap_changes_and_removal_restore_the_exact_gateway_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let original = model();
        let models = SessionOllamaCloud::default();
        write_caps(dir.path(), 100_000, 12000);
        let first = models.configure_model(dir.path(), dir.path(), &original);
        let mut expected = original.clone();
        expected.context_window = 100_000;
        expected.max_tokens = 12000;
        expected.max_tokens_explicit = true;
        assert_eq!(first, expected);
        write_caps(dir.path(), 50000, 8000);
        let second = models.configure_model(dir.path(), dir.path(), &first);
        expected.context_window = 50000;
        expected.max_tokens = 8000;
        assert_eq!(second, expected);
        std::fs::write(dir.path().join("ollama-cloud.json"), "{}").unwrap();
        assert_eq!(
            models.configure_model(dir.path(), dir.path(), &second),
            original
        );
        assert_eq!(
            models.configure_model(dir.path(), dir.path(), &first),
            original
        );
        // This is a fresh catalog baseline, not a previously configured clone.
        let mut fresh = original.clone();
        fresh.context_window = 400_000;
        fresh.max_tokens = 64000;
        write_caps(dir.path(), 80000, 16000);
        let capped = models.configure_model(dir.path(), dir.path(), &fresh);
        std::fs::write(dir.path().join("ollama-cloud.json"), "{}").unwrap();
        assert_eq!(
            models.configure_model(dir.path(), dir.path(), &capped),
            fresh
        );
        assert_eq!(original, model());
    }

    #[tokio::test]
    async fn native_overlay_precedes_user_hook_and_uses_actual_request_model() {
        use pa_agent::stream::{LlmContext, StreamRequestOptions};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ollama-cloud.json"),
            json!({
                "inferenceParams": {"temperature": 0.2},
                "models": {"glm-5": {"temperature": 0.4}}
            })
            .to_string(),
        )
        .unwrap();
        let actual: pa_agent::types::Model =
            super::super::provider_adapter::json_round_trip(&model()).unwrap();
        let actual_for_inner = actual.clone();
        let received = Arc::new(Mutex::new(None));
        let captured = received.clone();
        let inner: StreamFn = Arc::new(move |_requested, _context, options| {
            let actual = actual_for_inner.clone();
            let payload =
                options.on_payload.unwrap()(json!({"temperature": 1, "keep": true}), &actual);
            *captured.lock().unwrap() = payload;
            Box::pin(async { anyhow::bail!("captured without network") })
        });
        let stream = wrap_stream_fn(dir.path().into(), dir.path().into(), Arc::default(), inner);
        let seen = Arc::new(Mutex::new(None));
        let hook_seen = seen.clone();
        let options = StreamRequestOptions {
            on_payload: Some(Arc::new(move |mut payload, _| {
                *hook_seen.lock().unwrap() = Some(payload.clone());
                payload["temperature"] = json!(0.8);
                Some(payload)
            })),
            ..Default::default()
        };
        // A non-Ollama session descriptor must not suppress the actual routed
        // Ollama request's native overlay.
        let _ = stream(
            pa_agent::types::Model::unknown(),
            LlmContext::default(),
            options,
        )
        .await;
        assert_eq!(
            *seen.lock().unwrap(),
            Some(json!({"temperature": 0.4, "keep": true}))
        );
        assert_eq!(
            *received.lock().unwrap(),
            Some(json!({"temperature": 0.8, "keep": true}))
        );
    }

    #[tokio::test]
    async fn user_hook_none_keeps_overlay_and_non_ollama_requests_stay_unchanged() {
        use pa_agent::stream::{LlmContext, StreamRequestOptions};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ollama-cloud.json"),
            json!({"inferenceParams": {"temperature": 0.2}}).to_string(),
        )
        .unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let captured = received.clone();
        let inner: StreamFn = Arc::new(move |model, _context, options| {
            captured
                .lock()
                .unwrap()
                .push(options.on_payload.unwrap()(json!({"keep": true}), &model));
            Box::pin(async { anyhow::bail!("captured without network") })
        });
        let stream = wrap_stream_fn(dir.path().into(), dir.path().into(), Arc::default(), inner);
        let actual: pa_agent::types::Model =
            super::super::provider_adapter::json_round_trip(&model()).unwrap();
        for model in [actual, pa_agent::types::Model::unknown()] {
            let options = StreamRequestOptions {
                on_payload: Some(Arc::new(|_, _| None)),
                ..Default::default()
            };
            let _ = stream(model, LlmContext::default(), options).await;
        }
        assert_eq!(
            *received.lock().unwrap(),
            vec![
                Some(json!({"temperature": 0.2, "keep": true})),
                Some(json!({"keep": true}))
            ]
        );
    }

    #[tokio::test]
    async fn explicit_caps_reload_and_removal_reach_the_real_provider_body() {
        use pa_agent::stream::{LlmContext, StreamRequestOptions};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let captured = received.clone();
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                assert!(headers.starts_with("POST /gateway/v1/chat/completions "));
                let content_length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                while bytes.len() < header_end + content_length {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let payload: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + content_length])
                        .unwrap();
                captured.lock().unwrap().push(json!({"max_tokens": payload["max_tokens"], "temperature": payload["temperature"]}));
                let body = "data: {\"id\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let mut original = model();
        original.base_url = format!("http://{address}/gateway/v1");
        let models = Arc::new(SessionOllamaCloud::default());
        write_caps(dir.path(), 100_000, 12000);
        let configured = models.configure_model(dir.path(), dir.path(), &original);
        let requested: pa_agent::types::Model =
            super::super::provider_adapter::json_round_trip(&configured).unwrap();
        // Deliberately pin an ALREADY CAPPED full target. Removing the config
        // must still restore the original budget without recreating the adapter.
        let inner =
            super::super::provider_adapter::real_stream_fn(Some("offline".into()), configured);
        let stream = wrap_stream_fn(dir.path().into(), dir.path().into(), models, inner);
        for index in 0..4 {
            let config = match index {
                0 => {
                    json!({"modelOverrides": {"glm-5": {"maxTokens": 12000}}, "inferenceParams": {"temperature": 0.2}})
                }
                1 => {
                    json!({"modelOverrides": {"glm-5": {"maxTokens": 8000}}, "inferenceParams": {"temperature": 0.3, "max_tokens": 700}})
                }
                2 => json!({}),
                3 => {
                    json!({"modelOverrides": {"glm-5": {"maxTokens": 12000}}, "inferenceParams": {"temperature": 0.4}})
                }
                _ => unreachable!(),
            };
            std::fs::write(dir.path().join("ollama-cloud.json"), config.to_string()).unwrap();
            let mut options = StreamRequestOptions::default();
            if index == 3 {
                options.max_tokens = Some(1234);
                options.on_payload = Some(Arc::new(|mut body, _| {
                    assert_eq!(body["max_tokens"], json!(1234));
                    body["max_tokens"] = json!(2345);
                    Some(body)
                }));
            }
            let mut response = stream(requested.clone(), LlmContext::default(), options)
                .await
                .unwrap();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), response.result())
                .await
                .unwrap()
                .unwrap();
            assert_ne!(
                result.stop_reason,
                pa_agent::types::StopReason::Error,
                "{result:?}"
            );
        }
        server.await.unwrap();
        assert_eq!(
            *received.lock().unwrap(),
            vec![
                json!({"max_tokens": 12000, "temperature": 0.2}),
                json!({"max_tokens": 700, "temperature": 0.3}),
                json!({"max_tokens": 32000, "temperature": null}),
                json!({"max_tokens": 2345, "temperature": 0.4}),
            ]
        );
        assert_eq!(original.context_window, 200_000);
        assert_eq!(original.max_tokens, 32000);
    }

    #[test]
    fn full_model_projection_keeps_gateway_and_existing_agent_wire_spelling() {
        let full = model();
        let agent: pa_agent::types::Model =
            super::super::provider_adapter::json_round_trip(&full).unwrap();
        assert_eq!(agent.base_url, full.base_url);
        let mut wire = serde_json::to_value(&agent).unwrap();
        assert_eq!(wire["base_url"], json!(full.base_url));
        assert!(wire.get("baseUrl").is_none());
        wire["baseUrl"] = json!("http://other.invalid/v1");
        assert!(serde_json::from_value::<pa_agent::types::Model>(wire).is_err());
    }

    #[test]
    fn fresh_registry_baseline_wins_when_equal_to_an_old_cap() {
        let dir = tempfile::tempdir().unwrap();
        let models = SessionOllamaCloud::default();
        let original = model();
        write_caps(dir.path(), 100_000, 12000);
        let old_cap = models.configure_fresh_model(dir.path(), dir.path(), &original);
        // A refreshed explicit models.json descriptor can be byte-for-byte
        // equal to the old session cap. Its new baseline must still win.
        let fresh = old_cap.clone();
        assert_eq!(
            models.configure_fresh_model(dir.path(), dir.path(), &fresh),
            old_cap
        );
        std::fs::write(dir.path().join("ollama-cloud.json"), "{}").unwrap();
        assert_eq!(
            models.configure_model(dir.path(), dir.path(), &old_cap),
            fresh
        );
        let mut different_gateway = original;
        different_gateway.base_url = "https://other.invalid/v1".into();
        write_caps(dir.path(), 50000, 8000);
        let other_cap = models.configure_fresh_model(dir.path(), dir.path(), &different_gateway);
        std::fs::write(dir.path().join("ollama-cloud.json"), "{}").unwrap();
        assert_eq!(
            models.configure_model(dir.path(), dir.path(), &other_cap),
            different_gateway
        );
        assert_eq!(
            models.configure_model(dir.path(), dir.path(), &old_cap),
            fresh
        );
    }
}
