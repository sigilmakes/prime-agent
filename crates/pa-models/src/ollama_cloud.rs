//! Ollama Cloud's provider-owned metadata adapter. Transport and auth are never
//! accepted from catalog data; callers own scoped caching and credential lookup.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::fetch::{FetchError, FETCH_TIMEOUT};
use crate::Model;

pub const PROVIDER_ID: &str = "ollama-cloud";
pub const BASE_URL: &str = "https://ollama.com";
const MAX_LIST_BYTES: usize = 2 * 1024 * 1024;
const MAX_SHOW_BYTES: usize = 512 * 1024;
const MAX_CATALOG_BYTES: usize = 5 * 1024 * 1024;

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.encode_utf16().count() <= 200 && !id.chars().any(|c| c <= '\u{1f}')
}

fn details(value: &Value) -> Option<Value> {
    let caps = value.get("capabilities")?.as_array()?;
    if !caps.iter().all(Value::is_string) {
        return None;
    }
    Some(
        json!({"capabilities": caps, "model_info": value.get("model_info").and_then(Value::as_object).cloned().unwrap_or_default()}),
    )
}

fn rows(payload: &Value) -> Result<&Map<String, Value>, String> {
    payload
        .get("models")
        .and_then(Value::as_object)
        .ok_or_else(|| "Invalid Ollama Cloud catalog".to_string())
}

/// Parse safe metadata into a pinned OpenAI-compatible request model.
///
/// # Errors
/// Returns an error for a malformed or entirely non-tool-capable catalog.
pub fn parse_catalog(payload: &Value, base_url: &str) -> Result<Vec<Model>, String> {
    let mut models = Vec::new();
    for (id, raw) in rows(payload)? {
        if !valid_id(id) {
            continue;
        }
        let Some(data) = details(raw) else {
            continue;
        };
        let Some(caps) = data["capabilities"].as_array() else {
            continue;
        };
        let has = |cap: &str| caps.iter().any(|v| v.as_str() == Some(cap));
        if !has("tools") {
            continue;
        }
        let context = data["model_info"]
            .as_object()
            .and_then(|info| {
                info.iter().find_map(|(key, value)| {
                    let n = value.as_f64()?;
                    if key.ends_with(".context_length") && n.is_finite() && n > 0.0 {
                        // Provider context metadata is numeric; the shared model uses
                        // an integer count. Saturation/truncation matches that boundary.
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let context = n as u64;
                        Some(context)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(128_000);
        let mut thinking = json!({"off":"none", "minimal":null, "low":"low", "medium":"medium", "high":"high", "xhigh":"max"});
        if id.starts_with("gpt-oss") {
            thinking["off"] = Value::Null;
            thinking["xhigh"] = Value::Null;
        } else if id.starts_with("qwen3-vl")
            || id == "kimi-k2-thinking"
            || id.starts_with("minimax")
        {
            thinking["off"] = Value::Null;
        } else if id.starts_with("qwen3") {
            thinking["low"] = Value::Null;
            thinking["high"] = Value::Null;
            thinking["xhigh"] = Value::Null;
        }
        let mut value = json!({
            "id": id, "name": id, "provider": PROVIDER_ID,
            "api": "openai-completions", "baseUrl": format!("{}/v1", base_url.trim_end_matches('/')),
            "reasoning": has("thinking"),
            "input": if has("vision") { vec!["text", "image"] } else { vec!["text"] },
            "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
            "contextWindow": context, "maxTokens":32_768,
            "compat": {
                "supportsDeveloperRole":false, "supportsReasoningEffort":true,
                "supportsStore":false, "maxTokensField":"max_tokens", "supportsUsageInStreaming":true,
                "requiresToolResultName":false, "requiresAssistantAfterToolResult":false,
                "requiresThinkingAsText":false, "requiresReasoningContentOnAssistantMessages":false,
                "thinkingFormat":"openai", "supportsStrictMode":false, "sendSessionAffinityHeaders":false,
                "supportsLongCacheRetention":false, "zaiToolStream":false,
                "openRouterRouting":{}, "vercelGatewayRouting":{}
            }
        });
        if has("thinking") {
            value["thinkingLevelMap"] = thinking;
        }
        models.push(
            serde_json::from_value(value)
                .map_err(|error| format!("Invalid Ollama Cloud metadata: {error}"))?,
        );
    }
    if models.is_empty() {
        return Err("Ollama Cloud catalog contains no tool-capable models".into());
    }
    Ok(models)
}

/// Merge partial show responses, retaining only listed, non-denied last-good rows.
///
/// # Errors
/// Returns an error when listing or catalog metadata is invalid.
pub fn merge_catalog(payload: &Value, previous: Option<&Value>) -> Result<Value, String> {
    let next = rows(payload)?;
    let prior = previous.map(rows).transpose()?;
    let ids = |field| -> Result<BTreeSet<String>, String> {
        payload
            .get(field)
            .and_then(Value::as_array)
            .and_then(|values| {
                values
                    .iter()
                    .map(|v| v.as_str().filter(|id| valid_id(id)).map(str::to_string))
                    .collect()
            })
            .ok_or_else(|| "Invalid Ollama Cloud model listing metadata".to_string())
    };
    let listed = ids("listedIds")?;
    let denied = ids("deniedIds")?;
    let mut merged = Map::new();
    for source in prior.into_iter().chain(std::iter::once(next)) {
        for (id, value) in source {
            if listed.contains(id) && !denied.contains(id) {
                if let Some(safe) = details(value) {
                    merged.insert(id.clone(), safe);
                }
            }
        }
    }
    Ok(json!({"models":merged, "listedIds":payload["listedIds"], "deniedIds":payload["deniedIds"]}))
}

fn transport(message: &str) -> FetchError {
    FetchError::Transport {
        message: message.into(),
    }
}

async fn bounded_json(mut response: reqwest::Response, limit: usize) -> Result<Value, FetchError> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(transport("Ollama Cloud response is too large"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| transport("Ollama Cloud response read failed"))?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(transport("Ollama Cloud response is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| transport("Invalid Ollama Cloud JSON response"))
}

/// Fetch both provider-owned endpoints with one five-second budget and eight
/// bounded detail workers. Only Authorization is forwarded, never arbitrary headers.
///
/// # Errors
/// Returns an error on list failure, timeout, invalid data or no usable details.
#[tracing::instrument(skip_all, fields(provider = PROVIDER_ID))]
pub async fn fetch_catalog(base_url: &str, authorization: &str) -> Result<Value, FetchError> {
    if crate::offline::is_catalog_offline() {
        return Err(transport("Catalog is offline"));
    }
    fetch_catalog_online(base_url, authorization).await
}

enum Detail {
    Available { id: String, metadata: Value },
    Denied { id: String },
}

async fn detail_worker(
    client: reqwest::Client,
    queue: Arc<Mutex<VecDeque<String>>>,
    retained_bytes: Arc<std::sync::atomic::AtomicUsize>,
    base: String,
    authorization: String,
) -> Vec<Detail> {
    let mut result = Vec::new();
    loop {
        let id = queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        let Some(id) = id else {
            break;
        };
        let response = client
            .post(format!("{base}/api/show"))
            .header("authorization", &authorization)
            .header("content-type", "application/json")
            .body(json!({"model":id}).to_string())
            .send()
            .await;
        let Ok(response) = response else {
            continue;
        };
        match response.status().as_u16() {
            401 | 403 => {
                result.push(Detail::Denied { id });
                continue;
            }
            200..=299 => {}
            _ => continue,
        }
        let Ok(value) = bounded_json(response, MAX_SHOW_BYTES).await else {
            continue;
        };
        let Some(metadata) = details(&value) else {
            continue;
        };
        let size = metadata.to_string().len() + id.len() + 8;
        if retained_bytes
            .try_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |bytes| {
                    bytes
                        .checked_add(size)
                        .filter(|total| *total <= MAX_CATALOG_BYTES)
                },
            )
            .is_ok()
        {
            result.push(Detail::Available { id, metadata });
        }
    }
    result
}

async fn fetch_catalog_online(base_url: &str, authorization: &str) -> Result<Value, FetchError> {
    let base = base_url.trim_end_matches('/');
    let client = pa_ai::utils::tls::http_client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| transport("Ollama Cloud HTTP client failed"))?;
    tokio::time::timeout(FETCH_TIMEOUT, async {
        let response = client
            .get(format!("{base}/v1/models"))
            .header("authorization", authorization)
            .send()
            .await
            .map_err(|_| transport("Ollama Cloud model listing failed"))?;
        if !response.status().is_success() {
            return Err(FetchError::Status {
                status: response.status().as_u16(),
            });
        }
        let body = bounded_json(response, MAX_LIST_BYTES).await?;
        let data = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| transport("Invalid Ollama Cloud model list"))?;
        let ids: BTreeSet<String> = data
            .iter()
            .filter_map(|row| row.get("id").and_then(Value::as_str))
            .filter(|id| valid_id(id))
            .map(str::to_string)
            .collect();
        if ids.is_empty() {
            return Err(transport("Empty Ollama Cloud model list"));
        }
        let queue = Arc::new(Mutex::new(ids.iter().cloned().collect::<VecDeque<_>>()));
        let retained_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut workers = tokio::task::JoinSet::new();
        for _ in 0..ids.len().min(8) {
            workers.spawn(detail_worker(
                client.clone(),
                Arc::clone(&queue),
                Arc::clone(&retained_bytes),
                base.to_string(),
                authorization.to_string(),
            ));
        }
        let mut models = Map::new();
        let mut denied = Vec::new();
        while let Some(result) = workers.join_next().await {
            for detail in result.map_err(|_| transport("Ollama Cloud detail worker failed"))? {
                match detail {
                    Detail::Denied { id } => denied.push(id),
                    Detail::Available { id, metadata } => {
                        models.insert(id, metadata);
                    }
                }
            }
        }
        if models.is_empty() {
            return Err(if denied.is_empty() {
                transport("Ollama Cloud model details unavailable")
            } else {
                FetchError::Status { status: 401 }
            });
        }
        Ok(json!({"models":models,"listedIds":ids,"deniedIds":denied}))
    })
    .await
    .map_err(|_| transport("Ollama Cloud catalog timed out"))?
}

/// Pinned first-use tool-capable fallback (2026-09-14), never remote transport data.
///
/// # Panics
/// Panics only if the compiled metadata ceases to satisfy the model schema.
#[must_use]
pub fn pinned_models(base_url: &str) -> Vec<Model> {
    const PINNED: &[(&str, u64, bool, bool)] = &[
        ("deepseek-v4-flash:0731", 1_048_576, false, true),
        ("deepseek-v4-pro:0813", 1_048_576, false, true),
        ("deepseek-v4.1-flash", 1_048_576, true, true),
        ("gemma4:31b", 262_144, true, true),
        ("glm-5.1", 202_752, false, true),
        ("glm-5.2", 1_048_576, false, true),
        ("glm-5.3", 1_048_576, false, true),
        ("glm-5.3-flash", 1_048_576, true, true),
        ("gpt-oss:120b", 131_072, false, true),
        ("gpt-oss:20b", 131_072, false, true),
        ("kimi-k2.6", 262_144, true, true),
        ("kimi-k2.7-code", 262_144, true, true),
        ("kimi-k3", 1_048_576, true, true),
        ("minimax-m2.7", 196_608, false, true),
        ("minimax-m3", 512_000, true, true),
        ("mistral-large-3:675b", 262_144, true, false),
        ("nemotron-3-nano:30b", 262_144, false, true),
        ("nemotron-3-super", 262_144, false, true),
        ("nemotron-3-ultra", 262_144, false, true),
        ("qwen3.5:397b", 262_144, true, true),
    ];
    let models: Map<String, Value> = PINNED
        .iter()
        .map(|(id, context, vision, reasoning)| {
            let mut caps = vec!["tools"];
            if *vision {
                caps.push("vision");
            }
            if *reasoning {
                caps.push("thinking");
            }
            (
                id.to_string(),
                json!({"capabilities":caps, "model_info":{"pinned.context_length":context}}),
            )
        })
        .collect();
    parse_catalog(&json!({"models":models}), base_url).expect("pinned Ollama Cloud metadata")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn metadata_pins_transport_and_maps_each_thinking_family() {
        let mut models = Map::new();
        for id in [
            "gpt-oss:120b",
            "qwen3-vl:32b",
            "kimi-k2-thinking",
            "minimax-m3",
            "qwen3.5:397b",
            "glm-5.3",
        ] {
            models.insert(id.into(), json!({"capabilities":["tools","vision","thinking"], "model_info":{"x.context_length":1_048_576}, "baseUrl":"https://attacker.invalid", "headers":{"authorization":"bad"}}));
        }
        models.insert("no-tools".into(), json!({"capabilities":["vision"]}));
        let parsed = parse_catalog(&json!({"models":models}), "https://ollama.com").unwrap();
        assert_eq!(parsed.len(), 6);
        for model in &parsed {
            assert_eq!(
                (
                    &*model.provider,
                    &*model.api,
                    &*model.base_url,
                    model.context_window,
                    model.max_tokens
                ),
                (
                    PROVIDER_ID,
                    "openai-completions",
                    "https://ollama.com/v1",
                    1_048_576,
                    32_768
                )
            );
            assert!(model.headers.is_none());
        }
        let map = |id| {
            serde_json::to_value(parsed.iter().find(|m| m.id == id).unwrap()).unwrap()
                ["thinkingLevelMap"]
                .clone()
        };
        assert_eq!(
            map("gpt-oss:120b"),
            json!({"off":null,"minimal":null,"low":"low","medium":"medium","high":"high","xhigh":null})
        );
        for id in ["qwen3-vl:32b", "kimi-k2-thinking", "minimax-m3"] {
            assert_eq!(
                map(id),
                json!({"off":null,"minimal":null,"low":"low","medium":"medium","high":"high","xhigh":"max"})
            );
        }
        assert_eq!(
            map("qwen3.5:397b"),
            json!({"off":"none","minimal":null,"low":null,"medium":"medium","high":null,"xhigh":null})
        );
        assert_eq!(
            map("glm-5.3"),
            json!({"off":"none","minimal":null,"low":"low","medium":"medium","high":"high","xhigh":"max"})
        );
    }

    #[test]
    fn merge_retains_transient_details_but_removes_unlisted_or_denied_ids() {
        let detail = json!({"capabilities":["tools"],"model_info":{}});
        let previous =
            json!({"models":{"gone":detail,"denied":detail,"transient":detail,"good":detail}});
        let payload = json!({"models":{"good":{"capabilities":["tools","thinking"],"model_info":{},"api":"malicious"}},"listedIds":["denied","transient","good"],"deniedIds":["denied"]});
        assert_eq!(
            merge_catalog(&payload, Some(&previous)).unwrap(),
            json!({"models":{"transient":detail,"good":{"capabilities":["tools","thinking"],"model_info":{}}},"listedIds":["denied","transient","good"],"deniedIds":["denied"]})
        );
    }

    /// Serve one listing and three detail requests, validating the actual HTTP
    /// auth and body boundary rather than a mock of the adapter itself.
    #[tokio::test]
    async fn both_endpoints_use_same_bearer_and_partial_detail_failures_merge_safely() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = tokio::task::JoinSet::new();
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.spawn(async move {
                    let mut input=Vec::new(); let mut chunk=[0;4096];
                    let header_end = loop {
                        let n=socket.read(&mut chunk).await.unwrap(); assert!(n>0);
                        input.extend_from_slice(&chunk[..n]);
                        if let Some(i)=input.windows(4).position(|w|w==b"\r\n\r\n") { break i+4; }
                    };
                    let head=String::from_utf8_lossy(&input[..header_end]).to_lowercase();
                    assert!(head.contains("authorization: bearer fixture-key\r\n"));
                    let len=head.lines().find_map(|line|line.strip_prefix("content-length: ")).map(|n|n.trim().parse::<usize>().unwrap()).unwrap_or_default();
                    while input.len()<header_end+len {
                        let n=socket.read(&mut chunk).await.unwrap(); assert!(n>0);input.extend_from_slice(&chunk[..n]);
                    }
                    let (status,body)=if head.starts_with("get /v1/models ") {
                        (200,json!({"data":[{"id":"good"},{"id":"denied"},{"id":"transient"}]}).to_string())
                    } else {
                        assert!(head.starts_with("post /api/show "));
                        let body:Value=serde_json::from_slice(&input[header_end..header_end+len]).unwrap();
                        match body["model"].as_str().unwrap() {
                            "good" => (200,json!({"capabilities":["tools","thinking"],"model_info":{"x.context_length":262_144},"headers":{"x-secret":"bad"}}).to_string()),
                            "denied" => (403,"{}".into()),
                            "transient" => (503,"{}".into()),
                            _ => panic!("unexpected model")
                        }
                    };
                    let response=format!("HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len());
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
            }
            while let Some(result) = requests.join_next().await {
                result.unwrap();
            }
        });
        let payload = fetch_catalog_online(&format!("http://{addr}"), "Bearer fixture-key")
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(payload["deniedIds"], json!(["denied"]));
        let previous = json!({"models":{"transient":{"capabilities":["tools"],"model_info":{}},"denied":{"capabilities":["tools"],"model_info":{}}}});
        let merged = merge_catalog(&payload, Some(&previous)).unwrap();
        let parsed = parse_catalog(&merged, &format!("http://{addr}")).unwrap();
        assert_eq!(
            parsed
                .iter()
                .map(|m| m.id.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["good", "transient"])
        );
        assert_eq!(
            parsed
                .iter()
                .find(|m| m.id == "good")
                .unwrap()
                .context_window,
            262_144
        );
        assert!(payload["models"]["good"].get("headers").is_none());
    }

    #[tokio::test]
    async fn list_redirect_is_not_followed_or_given_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            assert_ne!(socket.read(&mut buffer).await.unwrap(), 0);
            socket.write_all(b"HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:1/leak\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await.unwrap();
        });
        let result = fetch_catalog_online(&format!("http://{addr}"), "Bearer fixture-key").await;
        server.await.unwrap();
        assert!(matches!(result, Err(FetchError::Status { status: 302 })));
    }
    #[tokio::test]
    async fn oversized_listing_is_rejected_before_reading_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            assert_ne!(socket.read(&mut buffer).await.unwrap(), 0);
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        MAX_LIST_BYTES + 1
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let result = fetch_catalog_online(&format!("http://{addr}"), "Bearer fixture-key").await;
        server.await.unwrap();
        assert!(
            matches!(result, Err(FetchError::Transport { message }) if message == "Ollama Cloud response is too large")
        );
    }
    #[tokio::test]
    async fn streamed_listing_without_content_length_still_obeys_byte_limit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            assert_ne!(socket.read(&mut buffer).await.unwrap(), 0);
            let response = format!("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n", MAX_LIST_BYTES + 1, " ".repeat(MAX_LIST_BYTES + 1));
            // The reader may close immediately on the first over-limit chunk.
            let _ = socket.write_all(response.as_bytes()).await;
        });
        let result = fetch_catalog_online(&format!("http://{addr}"), "Bearer fixture-key").await;
        server.await.unwrap();
        assert!(
            matches!(result, Err(FetchError::Transport { message }) if message == "Ollama Cloud response is too large")
        );
    }
    #[test]
    fn pinned_fallback_matches_legacy_ts_wire_metadata() {
        let expected: Vec<Model> =
            serde_json::from_str(include_str!("../tests/fixtures/ollama-cloud-pinned.json"))
                .unwrap();
        assert_eq!(pinned_models(BASE_URL), expected);
    }
}
