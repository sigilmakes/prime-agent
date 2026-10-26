//! Strict standalone compact response validation and raw JSON span extraction.
//! This is deliberately not the inference stream's JSON-repair path.

use std::collections::HashMap;

use serde_json::Value;

use super::{CompactionError, CompactionLimits, CompactionRoute, CompactionUsage, HttpFailureKind};

pub(super) struct ParsedResponse {
    pub output_json: String,
    pub usage_json: String,
    pub usage: CompactionUsage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ItemWindow {
    Input,
    Compacted,
    CodexCompacted,
}

pub(super) fn validate_input(
    raw: &str,
    limits: CompactionLimits,
    window: ItemWindow,
) -> Result<(), CompactionError> {
    if raw.len() > limits.max_request_bytes {
        return Err(CompactionError::LimitExceeded);
    }
    let input: Value = serde_json::from_str(raw)
        .map_err(|_| CompactionError::InvalidData("invalid input JSON"))?;
    validate_items(&input, limits, window)
}

fn validate_items(
    value: &Value,
    limits: CompactionLimits,
    window: ItemWindow,
) -> Result<(), CompactionError> {
    let items =
        value
            .as_array()
            .filter(|items| !items.is_empty())
            .ok_or(CompactionError::InvalidData(
                "expected nonempty ordered item array",
            ))?;
    if items.len() > limits.max_items {
        return Err(CompactionError::LimitExceeded);
    }
    let mut has_compaction = false;
    for item in items {
        let object = item.as_object().ok_or(CompactionError::InvalidData(
            "expected Responses item object",
        ))?;
        let item_type = object.get("type").and_then(Value::as_str);
        let role = object.get("role").and_then(Value::as_str);
        let nonempty = |key| {
            object
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        };
        let string = |key| object.get(key).is_some_and(Value::is_string);
        let valid = match item_type {
            Some("message") | None => {
                let valid_role =
                    matches!(role, Some("user" | "assistant" | "system" | "developer"))
                        || (window != ItemWindow::Input
                            && matches!(
                                role,
                                Some("unknown" | "critic" | "discriminator" | "tool")
                            ));
                valid_role
                    && object.get("content").is_some_and(|content| {
                        content.is_string()
                            || content.as_array().is_some_and(|parts| {
                                parts.iter().all(|part| {
                                    match part.get("type").and_then(Value::as_str) {
                                        Some("input_text" | "output_text") => {
                                            part.get("text").is_some_and(Value::is_string)
                                        }
                                        Some("refusal") => {
                                            part.get("refusal").is_some_and(Value::is_string)
                                        }
                                        Some(kind) => !kind.is_empty() && part.is_object(),
                                        None => false,
                                    }
                                })
                            })
                    })
            }
            Some("function_call") => nonempty("call_id") && nonempty("name") && string("arguments"),
            Some("function_call_output") => {
                (window != ItemWindow::Input || nonempty("call_id"))
                    && object
                        .get("output")
                        .is_some_and(|output| output.is_string() || output.is_array())
            }
            Some("reasoning") => {
                nonempty("id") && object.get("summary").is_some_and(Value::is_array)
            }
            Some("compaction") => {
                has_compaction = true;
                object
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| !content.trim().is_empty())
                    && (window != ItemWindow::Compacted || nonempty("id"))
            }
            // Unknown future native variants are opaque. Never prune them or
            // confuse a new type with a known, malformed retained message.
            Some(kind) => !kind.is_empty(),
        };
        if !valid {
            return Err(CompactionError::InvalidData("unusable Responses item"));
        }
    }
    if window != ItemWindow::Input && !has_compaction {
        return Err(CompactionError::InvalidData(
            "response lacks compaction item",
        ));
    }
    Ok(())
}

pub(super) fn parse_response(
    raw: &str,
    route: &CompactionRoute,
    limits: CompactionLimits,
    window: ItemWindow,
) -> Result<ParsedResponse, CompactionError> {
    if raw.len() > limits.max_response_bytes {
        return Err(CompactionError::LimitExceeded);
    }
    let response: Value = serde_json::from_str(raw)
        .map_err(|_| CompactionError::InvalidData("invalid response JSON"))?;
    if response.get("object").and_then(Value::as_str) != Some("response.compaction")
        || response
            .get("id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || response.get("created_at").and_then(Value::as_u64).is_none()
    {
        return Err(CompactionError::InvalidData(
            "expected response.compaction envelope",
        ));
    }
    if let Some(model) = response.get("model") {
        if model.as_str() != Some(&route.model) {
            return Err(CompactionError::RouteMismatch);
        }
    }
    let output = response
        .get("output")
        .ok_or(CompactionError::InvalidData("missing output"))?;
    validate_items(output, limits, window)?;
    let usage = response
        .get("usage")
        .ok_or(CompactionError::InvalidData("missing usage"))?;
    let count = |key: &str| {
        usage
            .get(key)
            .and_then(Value::as_u64)
            .ok_or(CompactionError::InvalidData("invalid usage token count"))
    };
    let input_tokens = count("input_tokens")?;
    let output_tokens = count("output_tokens")?;
    let total_tokens = count("total_tokens")?;
    if input_tokens.checked_add(output_tokens) != Some(total_tokens) {
        return Err(CompactionError::InvalidData("inconsistent usage total"));
    }
    // Absent details remain compatible with older gateways. Present containers
    // and documented counters must be well-formed, never silently treated absent.
    for (key, counters) in [
        (
            "input_tokens_details",
            &["cached_tokens", "cache_write_tokens"][..],
        ),
        ("output_tokens_details", &["reasoning_tokens"][..]),
    ] {
        if let Some(details) = usage.get(key) {
            let details = details
                .as_object()
                .ok_or(CompactionError::InvalidData("invalid usage details"))?;
            for counter in counters {
                if details
                    .get(*counter)
                    .is_some_and(|count| count.as_u64().is_none())
                {
                    return Err(CompactionError::InvalidData("invalid usage detail count"));
                }
            }
        }
    }
    let optional_count = |key: &str| {
        usage
            .get("input_tokens_details")
            .and_then(|details| details.get(key))
            .and_then(Value::as_u64)
    };
    if optional_count("cached_tokens").is_some_and(|cached| cached > input_tokens) {
        return Err(CompactionError::InvalidData(
            "cached tokens exceed input tokens",
        ));
    }
    let usage = CompactionUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        cached_input_tokens: optional_count("cached_tokens"),
        cache_write_tokens: optional_count("cache_write_tokens"),
    };
    // The full JSON was validated above. The scanner only locates byte spans; it
    // never interprets/re-encodes native output or assumes known item variants.
    let fields = root_fields(raw)?;
    Ok(ParsedResponse {
        output_json: fields
            .get("output")
            .ok_or(CompactionError::InvalidData("missing output"))?
            .to_string(),
        usage_json: fields
            .get("usage")
            .ok_or(CompactionError::InvalidData("missing usage"))?
            .to_string(),
        usage,
    })
}

// Locate a value's end in already-valid JSON. Strings consume their escapes;
// containers count nested delimiters. Scalars end at the next structural comma.
pub(super) fn value_end(bytes: &[u8], start: usize) -> usize {
    let mut pos = start;
    let mut depth = 0usize;
    let mut quoted = false;
    while pos < bytes.len() {
        let byte = bytes[pos];
        if quoted {
            if byte == b'\\' {
                pos += 2;
                continue;
            }
            if byte == b'"' {
                quoted = false;
                if depth == 0 {
                    return pos + 1;
                }
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'[' | b'{' => depth += 1,
                b']' | b'}' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        return pos + 1;
                    }
                }
                b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t' if depth == 0 => return pos,
                _ => {}
            }
        }
        pos += 1;
    }
    pos
}

pub(super) fn root_fields(raw: &str) -> Result<HashMap<String, &str>, CompactionError> {
    let bytes = raw.as_bytes();
    let mut pos = 0;
    let mut fields = HashMap::new();
    while bytes.get(pos).is_some_and(u8::is_ascii_whitespace) {
        pos += 1;
    }
    if bytes.get(pos) != Some(&b'{') {
        return Err(CompactionError::InvalidData("expected response object"));
    }
    pos += 1;
    loop {
        while bytes
            .get(pos)
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b',')
        {
            pos += 1;
        }
        if bytes.get(pos) == Some(&b'}') {
            break;
        }
        let end = value_end(bytes, pos);
        let key: String = serde_json::from_str(&raw[pos..end])
            .map_err(|_| CompactionError::InvalidData("invalid envelope key"))?;
        pos = end;
        while bytes
            .get(pos)
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b':')
        {
            pos += 1;
        }
        let end = value_end(bytes, pos);
        if fields.insert(key, &raw[pos..end]).is_some() {
            return Err(CompactionError::InvalidData("duplicate envelope field"));
        }
        pos = end;
    }
    Ok(fields)
}

pub(super) fn http_error(
    status: u16,
    body: &[u8],
    headers: &HashMap<String, String>,
) -> CompactionError {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let code = parsed
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str);
    let kind = match (status, code) {
        (401, _) => HttpFailureKind::Authentication,
        (403, _) => HttpFailureKind::Permission,
        (429, Some("insufficient_quota" | "billing_hard_limit_reached")) => HttpFailureKind::Quota,
        (429, _) => HttpFailureKind::RateLimit,
        (408 | 500..=599, _) => HttpFailureKind::Transient,
        (
            400 | 404 | 422,
            Some(
                "unsupported_model"
                | "model_not_supported"
                | "unsupported_endpoint"
                | "unsupported_operation",
            ),
        ) => HttpFailureKind::Capability,
        (400 | 422, _) => HttpFailureKind::MalformedRequest,
        (404, _) => HttpFailureKind::Route,
        (_, _) => HttpFailureKind::Other,
    };
    CompactionError::Http {
        status,
        kind,
        code: code.map(str::to_owned),
        request_id: headers.get("x-request-id").cloned(),
        headers: headers.clone(),
    }
}
