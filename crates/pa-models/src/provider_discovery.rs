//! Provider-owned model-list adapters. Discovery supplies metadata, never transport.

use std::collections::{HashMap, HashSet};

use pa_types::ai::{Model, ModelInput};
use pa_types::JsNumber;
use serde_json::{json, Map, Value};

/// Provider APIs with a supported model-list endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiscoverableProvider {
    OpenRouter,
    OpenAi,
    Anthropic,
    Google,
}

pub const DISCOVERABLE_PROVIDERS: [DiscoverableProvider; 4] = [
    DiscoverableProvider::OpenRouter,
    DiscoverableProvider::OpenAi,
    DiscoverableProvider::Anthropic,
    DiscoverableProvider::Google,
];

impl DiscoverableProvider {
    #[must_use]
    pub const fn provider_id(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
        }
    }

    #[must_use]
    pub const fn url(self) -> &'static str {
        match self {
            Self::OpenRouter => "https://openrouter.ai/api/v1/models",
            Self::OpenAi => "https://api.openai.com/v1/models",
            Self::Anthropic => "https://api.anthropic.com/v1/models?limit=1000",
            Self::Google => "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000",
        }
    }

    const fn rows_key(self) -> &'static str {
        match self {
            Self::Google => "models",
            Self::OpenRouter | Self::OpenAi | Self::Anthropic => "data",
        }
    }

    const fn id_key(self) -> &'static str {
        match self {
            Self::Google => "name",
            Self::OpenRouter | Self::OpenAi | Self::Anthropic => "id",
        }
    }

    fn normalized_id(self, raw: &str) -> &str {
        match self {
            Self::Google => raw.strip_prefix("models/").unwrap_or(raw),
            Self::OpenRouter | Self::OpenAi | Self::Anthropic => raw,
        }
    }
}

/// Request configuration for `CatalogCache` and `CatalogFetcher::fetch_with`.
/// Contains credentials; deliberately does not implement `Debug` or serialization.
#[derive(Clone)]
pub struct ProviderDiscoverySource {
    pub provider: DiscoverableProvider,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl ProviderDiscoverySource {
    /// Missing credentials disable authenticated sources. `OpenRouter` also supports anonymous lists.
    #[must_use]
    pub fn new(provider: DiscoverableProvider, api_key: Option<&str>) -> Option<Self> {
        Some(Self {
            provider,
            url: provider.url().into(),
            headers: discovery_headers(provider, api_key)?,
        })
    }
}

/// Build provider-specific credentials without putting a secret in the URL.
#[must_use]
pub fn discovery_headers(
    provider: DiscoverableProvider,
    api_key: Option<&str>,
) -> Option<Vec<(String, String)>> {
    let key = api_key.filter(|key| !key.is_empty());
    match provider {
        DiscoverableProvider::Google => Some(vec![("x-goog-api-key".into(), key?.into())]),
        DiscoverableProvider::Anthropic => Some(vec![
            ("x-api-key".into(), key?.into()),
            ("anthropic-version".into(), "2023-06-01".into()),
        ]),
        DiscoverableProvider::OpenAi => {
            Some(vec![("Authorization".into(), format!("Bearer {}", key?))])
        }
        DiscoverableProvider::OpenRouter => Some(key.map_or_else(Vec::new, |key| {
            vec![("Authorization".into(), format!("Bearer {key}"))]
        })),
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.encode_utf16().count() <= 200 && !id.chars().any(|ch| ch <= '\u{1f}')
}

fn positive(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(number) = value.as_u64() {
        return (number > 0).then_some(number);
    }
    let number = value.as_f64()?;
    if number > 0.0 && number <= 9_007_199_254_740_991.0 && number.fract() == 0.0 {
        // Only positive, integral, exactly representable values reach this cast.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        return Some(number as u64);
    }
    None
}

fn price(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    let parsed = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                0.0
            } else {
                text.parse::<f64>().ok()?
            }
        }
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => return None,
    };
    let per_million = parsed * 1_000_000.0;
    (parsed.is_finite() && parsed >= 0.0 && per_million.is_finite()).then_some(per_million)
}

fn includes(values: Option<&Value>, item: &str) -> bool {
    values
        .and_then(Value::as_array)
        .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(item)))
}

fn explicitly_non_chat(row: &Value) -> bool {
    ["input_modalities", "output_modalities"].iter().any(|key| {
        let modes = row.get("architecture").and_then(|arch| arch.get(*key));
        modes.is_some_and(Value::is_array) && !includes(modes, "text")
    })
}

/// Strip all fields except documented provider metadata, including nested objects.
fn sanitize(provider: DiscoverableProvider, value: &Value) -> Option<Value> {
    let row = value.as_object()?;
    let raw_id = row.get(provider.id_key())?.as_str()?;
    if !valid_id(raw_id) || !valid_id(provider.normalized_id(raw_id)) {
        return None;
    }
    let mut safe = Map::new();
    safe.insert(provider.id_key().into(), Value::String(raw_id.into()));
    match provider {
        DiscoverableProvider::OpenRouter => {
            if let Some(name) = row.get("name").filter(|name| name.is_string()) {
                safe.insert("name".into(), name.clone());
            }
            if let Some(limit) = positive(row.get("context_length")) {
                safe.insert("context_length".into(), limit.into());
            }
            if let Some(limit) = positive(
                row.get("top_provider")
                    .and_then(|top| top.get("max_completion_tokens")),
            ) {
                safe.insert(
                    "top_provider".into(),
                    json!({"max_completion_tokens": limit}),
                );
            }
            let pricing = row.get("pricing");
            let mut safe_prices = Map::new();
            for key in [
                "prompt",
                "completion",
                "input_cache_read",
                "input_cache_write",
            ] {
                let value = pricing.and_then(|prices| prices.get(key));
                if price(value).is_some() {
                    if let Some(value) = value {
                        safe_prices.insert(key.into(), value.clone());
                    }
                }
            }
            if !safe_prices.is_empty() {
                safe.insert("pricing".into(), Value::Object(safe_prices));
            }
            let architecture = row.get("architecture");
            let mut safe_architecture = Map::new();
            for key in ["input_modalities", "output_modalities"] {
                if let Some(values) = architecture
                    .and_then(|arch| arch.get(key))
                    .and_then(Value::as_array)
                {
                    let modes: Vec<Value> = values
                        .iter()
                        .filter(|mode| {
                            mode.as_str() == Some("text")
                                || (key == "input_modalities" && mode.as_str() == Some("image"))
                        })
                        .cloned()
                        .collect();
                    safe_architecture.insert(key.into(), Value::Array(modes));
                }
            }
            if !safe_architecture.is_empty() {
                safe.insert("architecture".into(), Value::Object(safe_architecture));
            }
        }
        DiscoverableProvider::Google => {
            if let Some(name) = row.get("displayName").filter(|name| name.is_string()) {
                safe.insert("displayName".into(), name.clone());
            }
            for key in ["inputTokenLimit", "outputTokenLimit"] {
                if let Some(limit) = positive(row.get(key)) {
                    safe.insert(key.into(), limit.into());
                }
            }
            if let Some(methods) = row
                .get("supportedGenerationMethods")
                .and_then(Value::as_array)
            {
                safe.insert(
                    "supportedGenerationMethods".into(),
                    Value::Array(
                        methods
                            .iter()
                            .filter(|method| method.as_str() == Some("generateContent"))
                            .cloned()
                            .collect(),
                    ),
                );
            }
        }
        DiscoverableProvider::Anthropic => {
            if let Some(name) = row.get("display_name").filter(|name| name.is_string()) {
                safe.insert("display_name".into(), name.clone());
            }
        }
        DiscoverableProvider::OpenAi => {}
    }
    Some(Value::Object(safe))
}

/// Restore compiled transport and model-specific controls on a retained record.
fn repin(model: &mut Model, exact: Option<&Model>, fallback: &Model) {
    let transport = exact.unwrap_or(fallback);
    model.api.clone_from(&transport.api);
    model.base_url.clone_from(&transport.base_url);
    model.headers = exact.and_then(|model| model.headers.clone());
    model.compat = exact.and_then(|model| model.compat.clone());
    model.reasoning = exact.is_some_and(|model| model.reasoning);
    model.thinking_level_map = exact.and_then(|model| model.thinking_level_map.clone());
    model.featured = exact.and_then(|model| model.featured);
    model.max_tokens_explicit = exact.is_some_and(|model| model.max_tokens_explicit);
    if let Some(exact) = exact {
        model.input.clone_from(&exact.input);
    }
}

/// Merge a provider model list into the last-good models, keeping omitted IDs.
/// `bundled` must be the client's trusted installed templates, never remote model records.
/// ID-only sources (and Google, which cannot describe capabilities) augment exact templates only.
/// New `OpenRouter` models require text input/output, token limits, and both prices.
///
/// # Errors
/// Returns an error for a malformed list or one containing no supported records.
#[allow(clippy::too_many_lines)] // One pass keeps per-row admission and metadata updates together.
pub fn parse_provider_discovery(
    provider: DiscoverableProvider,
    payload: &Value,
    bundled: &[Model],
    previous: &[Model],
) -> Result<Vec<Model>, String> {
    let rows = payload
        .get(provider.rows_key())
        .and_then(Value::as_array)
        .ok_or_else(|| "Invalid provider model list".to_string())?;
    let templates: Vec<&Model> = bundled
        .iter()
        .filter(|model| model.provider == provider.provider_id())
        .collect();
    let Some(fallback) = templates.first().copied() else {
        return Ok(Vec::new());
    };
    let known: HashMap<&str, &Model> = templates
        .iter()
        .map(|model| (model.id.as_str(), *model))
        .collect();
    let mut models = Vec::<Option<Model>>::new();
    let mut index = HashMap::<String, usize>::new();
    for prior in previous
        .iter()
        .filter(|model| model.provider == provider.provider_id())
    {
        let exact = known.get(prior.id.as_str()).copied();
        if !valid_id(&prior.id) || (exact.is_none() && provider != DiscoverableProvider::OpenRouter)
        {
            continue;
        }
        let mut model = prior.clone();
        repin(&mut model, exact, fallback);
        if let Some(slot) = index.get(&model.id).copied() {
            models[slot] = Some(model);
        } else {
            index.insert(model.id.clone(), models.len());
            models.push(Some(model));
        }
    }
    let mut valid = 0;
    for row in rows {
        let Some(raw_id) = row.get(provider.id_key()).and_then(Value::as_str) else {
            continue;
        };
        let id = provider.normalized_id(raw_id);
        if !valid_id(id) {
            continue;
        }
        if provider == DiscoverableProvider::Google
            && !includes(row.get("supportedGenerationMethods"), "generateContent")
        {
            continue;
        }
        let exact = known.get(id).copied();
        let slot = index.get(id).copied();
        if provider == DiscoverableProvider::OpenRouter
            && exact.is_none()
            && explicitly_non_chat(row)
        {
            if let Some(slot) = slot {
                models[slot] = None;
            }
            valid += 1;
            continue;
        }
        let old = slot.and_then(|slot| models[slot].as_ref());
        let context = match provider {
            DiscoverableProvider::OpenRouter => positive(row.get("context_length")),
            DiscoverableProvider::Google => positive(row.get("inputTokenLimit")),
            DiscoverableProvider::OpenAi | DiscoverableProvider::Anthropic => None,
        };
        let max_tokens = match provider {
            DiscoverableProvider::OpenRouter => positive(
                row.get("top_provider")
                    .and_then(|top| top.get("max_completion_tokens")),
            ),
            DiscoverableProvider::Google => positive(row.get("outputTokenLimit")),
            DiscoverableProvider::OpenAi | DiscoverableProvider::Anthropic => None,
        };
        let pricing = row.get("pricing");
        let prompt = price(pricing.and_then(|prices| prices.get("prompt")));
        let completion = price(pricing.and_then(|prices| prices.get("completion")));
        let architecture = row.get("architecture");
        if exact.is_none()
            && old.is_none()
            && (provider != DiscoverableProvider::OpenRouter
                || context.is_none()
                || max_tokens.is_none()
                || prompt.is_none()
                || completion.is_none()
                || !includes(
                    architecture.and_then(|arch| arch.get("input_modalities")),
                    "text",
                )
                || !includes(
                    architecture.and_then(|arch| arch.get("output_modalities")),
                    "text",
                ))
        {
            continue;
        }
        valid += 1;
        let mut model = old.or(exact).unwrap_or(fallback).clone();
        model.id = id.into();
        model.provider = provider.provider_id().into();
        repin(&mut model, exact, fallback);
        if exact.is_none() && old.is_none() {
            model.input = vec![ModelInput::Text];
            if includes(
                architecture.and_then(|arch| arch.get("input_modalities")),
                "image",
            ) {
                model.input.push(ModelInput::Image);
            }
            model.cost = pa_ai::types::zero_model_cost();
        }
        let name = match provider {
            DiscoverableProvider::Anthropic => row.get("display_name"),
            DiscoverableProvider::Google => row.get("displayName"),
            DiscoverableProvider::OpenRouter | DiscoverableProvider::OpenAi => row.get("name"),
        };
        model.name = name
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map_or_else(
                || exact.map_or(id, |model| model.name.as_str()).to_string(),
                str::to_string,
            );
        model.context_window = context.unwrap_or(model.context_window);
        model.max_tokens = max_tokens.unwrap_or(model.max_tokens);
        if provider == DiscoverableProvider::OpenRouter {
            if let Some(price) = prompt {
                model.cost.input = JsNumber::from(price);
            }
            if let Some(price) = completion {
                model.cost.output = JsNumber::from(price);
            }
            if let Some(price) = price(pricing.and_then(|prices| prices.get("input_cache_read"))) {
                model.cost.cache_read = JsNumber::from(price);
            }
            if let Some(price) = price(pricing.and_then(|prices| prices.get("input_cache_write"))) {
                model.cost.cache_write = JsNumber::from(price);
            }
        }
        if let Some(slot) = slot {
            models[slot] = Some(model);
        } else {
            index.insert(id.into(), models.len());
            models.push(Some(model));
        }
    }
    if valid == 0 {
        return Err("Provider model list contains no supported models".into());
    }
    for template in templates {
        if !index.contains_key(&template.id) {
            index.insert(template.id.clone(), models.len());
            models.push(Some(template.clone()));
        }
    }
    Ok(models.into_iter().flatten().collect())
}

/// Merge partial lists and persist only admitted metadata, stripping transport/auth fields.
/// Explicit non-chat `OpenRouter` rows remain as revocation tombstones across restarts.
///
/// # Errors
/// Returns the parser error if the merged list has no supported records.
pub fn merge_provider_discovery_payload(
    provider: DiscoverableProvider,
    payload: &Value,
    previous: &Value,
    bundled: &[Model],
) -> Result<Value, String> {
    let key = provider.rows_key();
    if payload.get(key).and_then(Value::as_array).is_none() {
        return Err("Invalid provider model list".into());
    }
    let mut rows = Map::<String, Value>::new();
    for source in [previous, payload] {
        let Some(entries) = source.get(key).and_then(Value::as_array) else {
            continue;
        };
        for safe in entries.iter().filter_map(|row| sanitize(provider, row)) {
            let id = safe[provider.id_key()]
                .as_str()
                .unwrap_or_default()
                .to_string();
            // Normalize Google's prefixed IDs so alternate spellings do not duplicate models.
            let id = provider.normalized_id(&id).to_string();
            if let Some(old) = rows.get_mut(&id).and_then(Value::as_object_mut) {
                for (field, value) in safe.as_object().into_iter().flatten() {
                    if provider == DiscoverableProvider::OpenRouter
                        && matches!(field.as_str(), "pricing" | "top_provider" | "architecture")
                    {
                        if let (Some(old), Some(new)) = (
                            old.get_mut(field).and_then(Value::as_object_mut),
                            value.as_object(),
                        ) {
                            old.extend(new.clone());
                            continue;
                        }
                    }
                    old.insert(field.clone(), value.clone());
                }
            } else {
                rows.insert(id, safe);
            }
        }
    }
    let merged = json!({key: rows.values().collect::<Vec<_>>()});
    let accepted: HashSet<String> = parse_provider_discovery(provider, &merged, bundled, &[])?
        .into_iter()
        .map(|model| model.id)
        .collect();
    let rows: Vec<Value> = rows
        .into_iter()
        .filter_map(|(id, row)| {
            (accepted.contains(&id)
                || (provider == DiscoverableProvider::OpenRouter && explicitly_non_chat(&row)))
            .then_some(row)
        })
        .collect();
    Ok(json!({key: rows}))
}
