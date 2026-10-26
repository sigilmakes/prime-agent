//! Codex V2 compaction uses the normal OAuth Responses transport, not the
//! retired standalone endpoint. Protocol reference: openai/codex 83a9600acb,
//! core/src/compact_remote_v2{,_attempt}.rs and protocol/src/models.rs.
use super::*;
use crate::providers::openai_codex_responses::request::{build_sse_headers, extract_account_id};
use crate::utils_inner::sse::SseDecoder;
use serde_json::Value;

// Upstream retains genuine user messages with a 64k token budget. We cannot
// reproduce its tokenizer/annotated-message truncation here. A byte upper bound
// keeps whole Prime user messages without destructive truncation; oversized
// retained input declines before any request, letting the host choose fallback.
const MAX_RETAINED_USER_BYTES: usize = 64_000;

fn retained_users(raw: &str) -> Result<Vec<&str>, CompactionError> {
    let mut pos = raw.find('[').ok_or(CompactionError::InvalidData("invalid input"))? + 1;
    let mut users = Vec::new();
    let mut bytes = 0usize;
    loop {
        while raw.as_bytes().get(pos).is_some_and(|b| b.is_ascii_whitespace() || *b == b',') {
            pos += 1;
        }
        if raw.as_bytes().get(pos) == Some(&b']') { break; }
        let end = protocol::value_end(raw.as_bytes(), pos);
        let item = raw.get(pos..end).ok_or(CompactionError::InvalidData("invalid input item"))?;
        let value: Value = serde_json::from_str(item).map_err(|_| CompactionError::InvalidData("invalid input item"))?;
        if value.get("role").and_then(Value::as_str) == Some("user") {
            bytes = bytes.saturating_add(item.len());
            if bytes > MAX_RETAINED_USER_BYTES { return Err(CompactionError::InputTooLarge); }
            users.push(item);
        }
        if end <= pos { return Err(CompactionError::InvalidData("invalid input span")); }
        pos = end;
    }
    Ok(users)
}

pub(super) async fn compact(
    adapter: &ResponsesCompactor,
    input: &ResponsesWindow,
    options: CompactOptions<'_>,
    cancel: &CancellationToken,
) -> Result<CompactionCandidate, CompactionError> {
    let users = retained_users(input.input_json())?;
    let account = extract_account_id(options.bearer_token)
        .map_err(|_| CompactionError::InvalidData("Codex compaction requires a resolved OAuth account token"))?;
    let additional = options.headers.map(|headers| headers.iter().map(|(k,v)| (k.clone(), v.clone())).collect());
    let headers = build_sse_headers(None, additional.as_ref(), &account, options.bearer_token, None);
    let close = input.input_json().rfind(']').ok_or(CompactionError::InvalidData("invalid input"))?;
    let mut triggered = input.input_json()[..close].to_owned();
    triggered.push_str(",{\"type\":\"compaction_trigger\"}]");
    let model = serde_json::to_string(&adapter.route.model).map_err(|_| CompactionError::InvalidRoute)?;
    let instructions = serde_json::to_string(options.instructions.unwrap_or_default())
        .map_err(|_| CompactionError::InvalidData("invalid instructions"))?;
    let body = format!("{{\"model\":{model},\"input\":{triggered},\"instructions\":{instructions},\"store\":false,\"stream\":true,\"include\":[\"reasoning.encrypted_content\"]}}");
    if body.len() > adapter.limits.max_request_bytes { return Err(CompactionError::LimitExceeded); }
    let work = async {
        let mut response = send(RequestOptions {
            headers, body: Some(body), signal: Some(cancel.clone()),
            ..RequestOptions::new(reqwest::Method::POST, adapter.route.endpoint.clone())
        }).await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.next_bytes().await? {
            if chunk.len() > adapter.limits.max_response_bytes.saturating_sub(bytes.len()) {
                return Err(CompactionError::LimitExceeded);
            }
            bytes.extend_from_slice(&chunk);
        }
        if response.status != 200 {
            return Err(protocol::http_error(response.status, &bytes, &response.headers));
        }
        let text = String::from_utf8(bytes).map_err(|_| CompactionError::InvalidData("Codex SSE is not UTF-8"))?;
        let mut decoder = SseDecoder::new();
        let mut events = decoder.push_text(&text);
        events.extend(decoder.finish());
        let mut compaction = None;
        let mut completed = None;
        for event in &events {
            if event.data.trim() == "[DONE]" { continue; }
            let value: Value = serde_json::from_str(&event.data)
                .map_err(|_| CompactionError::InvalidData("invalid Codex SSE event"))?;
            let kind = value.get("type").and_then(Value::as_str)
                .ok_or(CompactionError::InvalidData("Codex event lacks type"))?;
            if completed.is_some() {
                return Err(CompactionError::InvalidData("Codex event after completion"));
            }
            match kind {
                "response.output_item.done" if value["item"]["type"] == "compaction" => {
                    if compaction.is_some() { return Err(CompactionError::InvalidData("multiple Codex compaction items")); }
                    compaction = Some(*protocol::root_fields(&event.data)?.get("item")
                        .ok_or(CompactionError::InvalidData("missing Codex compaction item"))?);
                }
                "response.completed" | "response.done" => {
                    if value["response"]["status"] != "completed" {
                        return Err(CompactionError::InvalidData("Codex compaction did not complete"));
                    }
                    completed = Some(*protocol::root_fields(&event.data)?.get("response")
                        .ok_or(CompactionError::InvalidData("missing Codex completed response"))?);
                }
                "error" | "response.failed" | "response.incomplete" | "response.cancelled" => {
                    return Err(CompactionError::InvalidData("Codex compaction stream failed"));
                }
                _ => {}
            }
        }
        let compaction = compaction.ok_or(CompactionError::InvalidData("Codex response lacks compaction item"))?;
        let completed = completed.ok_or(CompactionError::InvalidData("Codex stream closed before completion"))?;
        let fields = protocol::root_fields(completed)?;
        let required = |name| fields.get(name).copied().ok_or(CompactionError::InvalidData("incomplete Codex response metadata"));
        let output = users.iter().copied().chain(std::iter::once(compaction)).collect::<Vec<_>>().join(",");
        let id = required("id")?;
        let created_at = required("created_at")?;
        let usage = required("usage")?;
        let response_json = format!("{{\"id\":{id},\"object\":\"response.compaction\",\"created_at\":{created_at},\"output\":[{output}],\"usage\":{usage},\"model\":{model}}}");
        if let Some(actual_model) = fields.get("model") {
            let actual: Value = serde_json::from_str(actual_model).map_err(|_| CompactionError::InvalidData("invalid Codex response model"))?;
            if actual.as_str() != Some(&adapter.route.model) { return Err(CompactionError::RouteMismatch); }
        }
        let parsed = parse_response(&response_json, &adapter.route, adapter.limits, adapter.window_kind())?;
        Ok(CompactionCandidate {
            archive: Archive { version: VERSION, protocol: adapter.protocol().to_owned(), route: adapter.route.clone(), response_json, usage: parsed.usage },
            output_json: parsed.output_json, usage_json: parsed.usage_json,
        })
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(CompactionError::Cancelled),
        result = tokio::time::timeout(adapter.limits.timeout, work) => result.unwrap_or(Err(CompactionError::Timeout)),
    }
}
