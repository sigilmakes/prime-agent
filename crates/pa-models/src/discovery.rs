//! Authorized provider discovery. Cached metadata is projected onto current trusted models.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pa_types::ai::{ModelInput, ThinkingLevelMap};
use pa_types::model_refresh::{ModelRefreshApplyReport, ModelRefreshSourceResult};
use pa_types::JsNumber;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::{CatalogCache, RefreshOptions};
use crate::fetch::CatalogFetcher;
use crate::Model;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatalogFormat {
    OpenAi,
    OpenRouter,
    Anthropic,
    Google,
    Codex,
    Ollama,
    PrimeInference,
    PrimeInferencePrivate,
}

impl CatalogFormat {
    /// Derive a discovery endpoint from a trusted local inference URL.
    /// No remote catalog metadata participates in endpoint selection.
    #[must_use]
    pub fn discovery_url(self, inference_base: &str) -> Option<String> {
        let mut url = reqwest::Url::parse(inference_base).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
        let base = url.path().trim_end_matches('/');
        let path = match self {
            Self::Ollama => base.strip_suffix("/v1").unwrap_or(base).to_string(),
            Self::Anthropic if !base.ends_with("/v1") => format!("{base}/v1/models"),
            Self::Codex => {
                let base = base.strip_suffix("/responses").unwrap_or(base);
                if base.ends_with("/codex") {
                    format!("{base}/models")
                } else {
                    format!("{base}/codex/models")
                }
            }
            _ => format!("{base}/models"),
        };
        url.set_path(&path);
        match self {
            Self::Codex => {
                url.query_pairs_mut()
                    .append_pair("client_version", "0.159.0");
            }
            Self::Google => {
                url.query_pairs_mut().append_pair("pageSize", "1000");
            }
            _ => {}
        }
        Some(url.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshPolicy {
    Force,
    IfStale,
}

/// Authority for admitting IDs absent from the trusted baseline.
#[derive(Clone, Serialize)]
pub enum UnknownModelPolicy {
    KnownOnly,
    ProviderMetadata { template: Model },
    ConfiguredDefaults { model: Model },
}

/// Fully authorized request description. Credentials never enter snapshots or diagnostics.
#[derive(Clone)]
pub struct ResolvedSource {
    pub id: String,
    pub provider: String,
    pub format: CatalogFormat,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub unknown_models: UnknownModelPolicy,
    pub is_current: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

#[derive(Clone, Default)]
pub struct CatalogInputs {
    pub sources: Vec<ResolvedSource>,
    pub seeds: Vec<Model>,
}

pub struct RefreshRequest {
    pub policy: RefreshPolicy,
    pub sources: Option<Vec<String>>,
}

#[derive(Clone, Copy, Default)]
pub struct SnapshotRequirement {
    pub since: Option<u64>,
}

#[derive(Clone)]
pub struct CatalogSnapshot {
    pub models: Vec<Model>,
    pub preferred_default: Option<String>,
    /// Current private-source grants only; trusted templates are not authorization.
    pub authorized_private_prime_models: Vec<Model>,
}

pub struct RefreshPublication {
    pub snapshot: CatalogSnapshot,
    pub sources: Vec<ModelRefreshSourceResult>,
}

pub struct ApplyPublication {
    pub snapshot: CatalogSnapshot,
    pub report: ModelRefreshApplyReport,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Metadata {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input: Option<Vec<ModelInput>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingLevelMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_price: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_price: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prime: Option<crate::prime_inference::PrimeInferenceEntry>,
    #[serde(default)]
    denied: bool,
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;

type MetadataCache = Arc<CatalogCache<Vec<Metadata>>>;

pub(crate) struct ProviderCatalog {
    dir: Option<PathBuf>,
    caches: Mutex<HashMap<String, MetadataCache>>,
}

fn digest(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

impl ResolvedSource {
    fn identity(&self) -> String {
        digest(
            &serde_json::to_vec(&(&self.id, &self.provider, self.format, &self.url))
                .expect("source identity is serializable"),
        )
    }

    fn scope(&self) -> String {
        let mut headers: Vec<_> = self
            .headers
            .iter()
            .map(|(key, value)| (key.to_ascii_lowercase(), value))
            .collect();
        headers.sort_unstable();
        digest(
            &serde_json::to_vec(&(self.identity(), headers))
                .expect("request scope is serializable"),
        )
    }

    fn current(&self) -> bool {
        self.is_current.as_ref().is_none_or(|check| check())
    }
}

impl ProviderCatalog {
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            caches: Mutex::new(HashMap::new()),
        }
    }

    fn cache(&self, source: &ResolvedSource) -> MetadataCache {
        let identity = source.identity();
        let mut caches = self.caches.lock().expect("provider catalog lock");
        Arc::clone(caches.entry(identity.clone()).or_insert_with(|| {
            Arc::new(CatalogCache::new(
                &format!("discovery:{identity}"),
                self.dir
                    .as_ref()
                    .map(|dir| dir.join(format!("discovery-{identity}.v1.json"))),
                Arc::new(if source.format == CatalogFormat::PrimeInferencePrivate {
                    CatalogFetcher::with_limits(std::time::Duration::from_secs(10), 2 * 1024 * 1024)
                } else {
                    CatalogFetcher::new()
                }),
                Arc::new(|payload, _| {
                    let rows: Vec<Metadata> = serde_json::from_value(payload.clone())
                        .map_err(|_| "Invalid normalized discovery metadata".to_string())?;
                    if rows.iter().any(|row| !valid_id(&row.id)) {
                        return Err("Invalid normalized model identity".into());
                    }
                    Ok(rows)
                }),
            ))
        }))
    }

    pub(crate) fn resolve(
        &self,
        sources: &[ResolvedSource],
        baseline: &[Model],
    ) -> (Vec<Model>, Vec<Model>) {
        let mut models = baseline.to_vec();
        let mut private = Vec::new();
        for source in sources {
            if !source.current() {
                continue;
            }
            let Some(metadata) = self.cache(source).get(&source.scope()) else {
                continue;
            };
            for row in metadata {
                if row.denied {
                    continue;
                }
                let exact = models
                    .iter()
                    .position(|model| model.provider == source.provider && model.id == row.id);
                let template = exact.map(|index| &models[index]).or_else(|| {
                    match &source.unknown_models {
                        UnknownModelPolicy::KnownOnly => None,
                        UnknownModelPolicy::ConfiguredDefaults { model } => Some(model),
                        UnknownModelPolicy::ProviderMetadata { template } => {
                            // Until unknown costs have a representation, require every rate.
                            (row.context.is_some()
                                && row.max_tokens.is_some()
                                && row.input.is_some()
                                && row.reasoning.is_some()
                                && row.input_price.is_some()
                                && row.output_price.is_some()
                                && row.cache_read.is_some()
                                && row.cache_write.is_some())
                            .then_some(template)
                        }
                    }
                });
                let Some(template) = template else {
                    continue;
                };
                let mut model = project(source, &row, template);
                model.provider.clone_from(&source.provider);
                if source.format == CatalogFormat::PrimeInferencePrivate {
                    private.push(model.clone());
                }
                match exact {
                    Some(index) => models[index] = model,
                    None => models.push(model),
                }
            }
        }
        (models, private)
    }

    pub(crate) async fn refresh(
        &self,
        sources: &[ResolvedSource],
        policy: RefreshPolicy,
        selected: Option<&[String]>,
    ) -> Vec<ModelRefreshSourceResult> {
        let mut tasks = Vec::new();
        for (index, source) in sources
            .iter()
            .enumerate()
            .filter(|(_, source)| crate::provider_catalogs::selected(selected, &source.id))
        {
            let cache = self.cache(source);
            let source = source.clone();
            tasks.push(tokio::spawn(async move {
                let format = source.format;
                let url = source.url.clone();
                let headers = source.headers.clone();
                // The cache identity is opaque: endpoint queries may contain credentials.
                // This explicit fetch path intentionally does not reuse discovery ETags.
                let fetch_payload = Some(Arc::new(move || {
                    let url = url.clone();
                    let headers = headers.clone();
                    Box::pin(async move {
                        if format == CatalogFormat::Ollama {
                            return crate::ollama_cloud::fetch_catalog_with_headers(&url, &headers)
                                .await;
                        }
                        let fetcher = if format == CatalogFormat::PrimeInferencePrivate {
                            CatalogFetcher::with_limits(
                                std::time::Duration::from_secs(10),
                                2 * 1024 * 1024,
                            )
                        } else {
                            CatalogFetcher::new()
                        };
                        match fetcher.fetch_with(&url, None, &headers).await? {
                            crate::fetch::FetchOutcome::Fresh { body, .. } => {
                                serde_json::from_slice(&body).map_err(|_| {
                                    crate::fetch::FetchError::Transport {
                                        message: "Invalid discovery JSON".into(),
                                    }
                                })
                            }
                            crate::fetch::FetchOutcome::NotModified => {
                                Err(crate::fetch::FetchError::Transport {
                                    message: "Unexpected discovery response without body".into(),
                                })
                            }
                        }
                    })
                        as std::pin::Pin<
                            Box<
                                dyn std::future::Future<
                                        Output = Result<
                                            serde_json::Value,
                                            crate::fetch::FetchError,
                                        >,
                                    > + Send,
                            >,
                        >
                }) as crate::cache::CatalogFetchPayload);
                // Private authorization expires sooner than public metadata.
                let force = policy == RefreshPolicy::Force
                    || (format == CatalogFormat::PrimeInferencePrivate
                        && cache.status(&source.scope()).checked_at.is_none_or(|at| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis()
                                .saturating_sub(u128::from(at))
                                >= 5 * 60 * 1000
                        }));
                let report = cache
                    .refresh_report(
                        &source.scope(),
                        RefreshOptions {
                            force,
                            headers: source.headers.clone(),
                            is_current: source.is_current.clone(),
                            fetch_payload,
                            merge_payload: Some(Arc::new(move |payload, previous| {
                                normalize(format, payload, previous)
                            })),
                        },
                    )
                    .await;
                (
                    index,
                    crate::provider_catalogs::source_result(
                        &source.id,
                        &report,
                        report.models.as_ref().map(Vec::len),
                    ),
                )
            }));
        }
        let mut reports = Vec::new();
        for task in tasks {
            reports.push(task.await.expect("discovery refresh task panicked"));
        }
        reports.sort_by_key(|(index, _)| *index);
        reports.into_iter().map(|(_, report)| report).collect()
    }

    pub(crate) fn adopt(
        &self,
        sources: &[ResolvedSource],
        since: Option<u64>,
    ) -> ModelRefreshApplyReport {
        let mut report = ModelRefreshApplyReport::default();
        for source in sources {
            let cache = self.cache(source);
            let scope = source.scope();
            if source.current()
                && cache.reload_from_disk(&scope).is_some()
                && since.is_none_or(|since| {
                    cache
                        .status(&scope)
                        .checked_at
                        .is_some_and(|at| at >= since)
                })
            {
                report.applied.push(source.id.clone());
            } else {
                report.missing.push(source.id.clone());
            }
        }
        report
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.encode_utf16().count() <= 1024 && !id.chars().any(char::is_control)
}

fn positive(value: Option<&serde_json::Value>) -> Option<u64> {
    let value = value?;
    if let Some(integer) = value.as_u64() {
        return (integer > 0).then_some(integer);
    }
    let number = value.as_f64()?;
    if number.is_finite()
        && number > 0.0
        && number.fract() == 0.0
        && number < 18_446_744_073_709_551_616.0
    {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let integer = number as u64;
        Some(integer)
    } else {
        None
    }
}

fn price(value: Option<&serde_json::Value>) -> Option<f64> {
    let value = value?;
    let value = value.as_f64().or_else(|| value.as_str()?.parse().ok())? * 1_000_000.0;
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn string(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn normalize(
    format: CatalogFormat,
    payload: &serde_json::Value,
    previous: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let previous: Vec<Metadata> = previous
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default();
    let prior: BTreeMap<_, _> = previous
        .into_iter()
        .map(|row| (row.id.clone(), row))
        .collect();
    let next = match format {
        CatalogFormat::PrimeInference | CatalogFormat::PrimeInferencePrivate => {
            normalize_prime(format, payload, &prior)?
        }
        CatalogFormat::Ollama => normalize_ollama(payload, &prior)?,
        _ => normalize_rows(format, payload)?,
    };
    let mut result = Vec::new();
    for row in next {
        let mut value = prior
            .get(&row.id)
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| "Invalid retained metadata")?
            .unwrap_or_else(|| serde_json::json!({}));
        let new = serde_json::to_value(row).map_err(|_| "Invalid discovery metadata")?;
        value
            .as_object_mut()
            .expect("metadata object")
            .extend(new.as_object().expect("metadata object").clone());
        result.push(value);
    }
    Ok(serde_json::Value::Array(result))
}

fn normalize_prime(
    format: CatalogFormat,
    payload: &serde_json::Value,
    prior: &BTreeMap<String, Metadata>,
) -> Result<Vec<Metadata>, String> {
    let rows = payload
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or("Invalid Prime Inference model list")?;
    let mut result = BTreeMap::new();
    for raw in rows {
        let Some(id) = raw
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| valid_id(id))
        else {
            continue;
        };
        if crate::prime_inference::is_private_prime_inference_model_id(id)
            != (format == CatalogFormat::PrimeInferencePrivate)
        {
            continue;
        }
        let mut sanitized = raw.clone();
        let Some(fields) = sanitized.as_object_mut() else {
            continue;
        };
        let pricing = raw.get("pricing");
        let number = |key: &str| {
            pricing
                .and_then(|pricing| pricing.get(key))
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite() && *value >= 0.0)
        };
        // The existing parser derives routing controls; absent prices are not assertions of zero.
        fields.insert(
            "pricing".into(),
            serde_json::json!({
                "input_usd_per_mtok":number("input_usd_per_mtok").unwrap_or(0.0),
                "output_usd_per_mtok":number("output_usd_per_mtok").unwrap_or(0.0),
            }),
        );
        fields.remove("specs");
        let mut entries = crate::prime_inference::parse_prime_inference_model_catalog(
            &serde_json::json!({"data":[sanitized]}),
            true,
        )?;
        let Some(mut entry) = entries.pop() else {
            continue;
        };
        if let Some(old) = prior.get(id).and_then(|old| old.prime.as_ref()) {
            if entry.supported_parameters.is_none() {
                entry
                    .supported_parameters
                    .clone_from(&old.supported_parameters);
            }
            if entry.reasoning_efforts.is_none() {
                entry.reasoning_efforts.clone_from(&old.reasoning_efforts);
            }
            if entry.reasoning_mandatory.is_none() {
                entry.reasoning_mandatory = old.reasoning_mandatory;
            }
        }
        let specs = raw.get("specs");
        let input = specs
            .and_then(|specs| specs.get("modalities"))
            .and_then(|modes| modes.get("input"))
            .and_then(serde_json::Value::as_array)
            .and_then(|modes| {
                modes
                    .iter()
                    .any(|mode| mode.as_str() == Some("text"))
                    .then(|| {
                        let mut input = vec![ModelInput::Text];
                        if modes.iter().any(|mode| mode.as_str() == Some("image")) {
                            input.push(ModelInput::Image);
                        }
                        input
                    })
            });
        let row = Metadata {
            id: id.into(),
            name: entry.name.clone(),
            context: positive(specs.and_then(|specs| specs.get("context_window"))),
            max_tokens: positive(specs.and_then(|specs| specs.get("max_output_tokens"))),
            reasoning: specs
                .and_then(|specs| specs.get("supports_reasoning"))
                .and_then(serde_json::Value::as_bool),
            input,
            input_price: number("input_usd_per_mtok"),
            output_price: number("output_usd_per_mtok"),
            cache_read: number("cache_read_usd_per_mtok"),
            cache_write: number("cache_write_usd_per_mtok"),
            prime: Some(entry),
            ..Default::default()
        };
        if result.insert(id.to_string(), row).is_some() {
            return Err("Duplicate Prime Inference model identity".into());
        }
    }
    Ok(result.into_values().collect())
}

fn normalize_rows(
    format: CatalogFormat,
    payload: &serde_json::Value,
) -> Result<Vec<Metadata>, String> {
    let key = if matches!(format, CatalogFormat::Google | CatalogFormat::Codex) {
        "models"
    } else {
        "data"
    };
    let rows = payload
        .get(key)
        .and_then(serde_json::Value::as_array)
        .ok_or("Invalid provider model list")?;
    let mut result = BTreeMap::new();
    for row in rows {
        let id_key = match format {
            CatalogFormat::Google => "name",
            CatalogFormat::Codex => "slug",
            _ => "id",
        };
        let Some(raw_id) = row.get(id_key).and_then(serde_json::Value::as_str) else {
            continue;
        };
        let id = if format == CatalogFormat::Google {
            raw_id.strip_prefix("models/").unwrap_or(raw_id)
        } else {
            raw_id
        };
        if !valid_id(id) {
            continue;
        }
        let mut metadata = Metadata {
            id: id.into(),
            ..Default::default()
        };
        match format {
            CatalogFormat::Anthropic => metadata.name = string(row.get("display_name")),
            CatalogFormat::Codex => normalize_codex_row(row, &mut metadata),
            CatalogFormat::Google => normalize_google_row(row, &mut metadata),
            CatalogFormat::OpenRouter => normalize_openrouter_row(row, &mut metadata),
            _ => {}
        }
        result.insert(id.to_string(), metadata);
    }
    if !rows.is_empty() && result.is_empty() {
        return Err("Provider list has no valid metadata".into());
    }
    Ok(result.into_values().collect())
}

fn normalize_codex_row(row: &serde_json::Value, metadata: &mut Metadata) {
    metadata.name = string(row.get("display_name"));
    metadata.context = positive(row.get("context_window"));
    metadata.denied = row
        .get("visibility")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|visibility| visibility != "list");
    if let Some(modes) = row
        .get("input_modalities")
        .and_then(serde_json::Value::as_array)
    {
        if modes.iter().any(|mode| mode.as_str() == Some("text")) {
            let mut input = vec![ModelInput::Text];
            if modes.iter().any(|mode| mode.as_str() == Some("image")) {
                input.push(ModelInput::Image);
            }
            metadata.input = Some(input);
        }
    }
    if let Some(levels) = row
        .get("supported_reasoning_levels")
        .and_then(serde_json::Value::as_array)
    {
        let mut map = ThinkingLevelMap::new();
        for level in [
            pa_types::ai::ModelThinkingLevel::Off,
            pa_types::ai::ModelThinkingLevel::Minimal,
            pa_types::ai::ModelThinkingLevel::Low,
            pa_types::ai::ModelThinkingLevel::Medium,
            pa_types::ai::ModelThinkingLevel::High,
            pa_types::ai::ModelThinkingLevel::Xhigh,
            pa_types::ai::ModelThinkingLevel::Max,
        ] {
            let name = level.wire_name();
            let supported = levels.iter().any(|value| {
                value
                    .as_str()
                    .or_else(|| value.get("effort").and_then(serde_json::Value::as_str))
                    == Some(name)
            });
            map.insert(level, supported.then(|| name.to_string()));
        }
        if map.values().any(Option::is_some) {
            metadata.reasoning = Some(true);
            metadata.thinking = Some(map);
        }
    }
}

fn normalize_google_row(row: &serde_json::Value, metadata: &mut Metadata) {
    metadata.denied = row
        .get("supportedGenerationMethods")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|methods| {
            !methods
                .iter()
                .any(|method| method.as_str() == Some("generateContent"))
        });
    metadata.name = string(row.get("displayName"));
    metadata.context = positive(row.get("inputTokenLimit"));
    metadata.max_tokens = positive(row.get("outputTokenLimit"));
}

fn normalize_openrouter_row(row: &serde_json::Value, metadata: &mut Metadata) {
    metadata.reasoning = row
        .get("supported_parameters")
        .and_then(serde_json::Value::as_array)
        .map(|parameters| {
            parameters.iter().any(|parameter| {
                matches!(parameter.as_str(), Some("reasoning" | "reasoning_effort"))
            })
        });
    metadata.name = string(row.get("name"));
    metadata.context = positive(row.get("context_length"));
    metadata.max_tokens = positive(
        row.get("top_provider")
            .and_then(|value| value.get("max_completion_tokens")),
    );
    for direction in ["input_modalities", "output_modalities"] {
        if let Some(modes) = row
            .get("architecture")
            .and_then(|arch| arch.get(direction))
            .and_then(serde_json::Value::as_array)
        {
            if !modes.iter().any(|mode| mode.as_str() == Some("text")) {
                metadata.denied = true;
            }
            if direction == "input_modalities" && !metadata.denied {
                let mut input = vec![ModelInput::Text];
                if modes.iter().any(|mode| mode.as_str() == Some("image")) {
                    input.push(ModelInput::Image);
                }
                metadata.input = Some(input);
            }
        }
    }
    let pricing = row.get("pricing");
    metadata.input_price = price(pricing.and_then(|value| value.get("prompt")));
    metadata.output_price = price(pricing.and_then(|value| value.get("completion")));
    metadata.cache_read = price(pricing.and_then(|value| value.get("input_cache_read")));
    metadata.cache_write = price(pricing.and_then(|value| value.get("input_cache_write")));
}

fn normalize_ollama(
    payload: &serde_json::Value,
    prior: &BTreeMap<String, Metadata>,
) -> Result<Vec<Metadata>, String> {
    let models = payload
        .get("models")
        .and_then(serde_json::Value::as_object)
        .ok_or("Invalid Ollama metadata")?;
    let listed = payload
        .get("listedIds")
        .and_then(serde_json::Value::as_array)
        .ok_or("Invalid Ollama model list")?;
    let denied = payload
        .get("deniedIds")
        .and_then(serde_json::Value::as_array)
        .ok_or("Invalid Ollama denied list")?;
    let mut result = Vec::new();
    for id in listed
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|id| valid_id(id))
    {
        if denied.iter().any(|value| value.as_str() == Some(id)) {
            result.push(Metadata {
                id: id.into(),
                denied: true,
                ..Default::default()
            });
            continue;
        }
        let Some(raw) = models.get(id) else {
            if let Some(old) = prior.get(id) {
                result.push(old.clone());
            }
            continue;
        };
        let Some(caps) = raw
            .get("capabilities")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        let has = |name| caps.iter().any(|cap| cap.as_str() == Some(name));
        let mut row = Metadata {
            id: id.into(),
            denied: !has("tools"),
            reasoning: Some(has("thinking")),
            ..Default::default()
        };
        if !row.denied {
            row.thinking = crate::ollama_cloud::parse_catalog(
                &serde_json::json!({"models":{id:raw}}),
                crate::ollama_cloud::BASE_URL,
            )
            .ok()
            .and_then(|models| models.into_iter().next())
            .and_then(|model| model.thinking_level_map);
            row.input = Some(if has("vision") {
                vec![ModelInput::Text, ModelInput::Image]
            } else {
                vec![ModelInput::Text]
            });
            // Ollama Cloud bills a flat subscription, not per token. Its rows
            // therefore have known zero rates and the pinned output ceiling,
            // satisfying admission for IDs absent from the curated baseline.
            row.max_tokens = Some(crate::ollama_cloud::MAX_OUTPUT_TOKENS);
            row.input_price = Some(0.0);
            row.output_price = Some(0.0);
            row.cache_read = Some(0.0);
            row.cache_write = Some(0.0);
            row.context = raw
                .get("model_info")
                .and_then(serde_json::Value::as_object)
                .and_then(|info| {
                    info.iter()
                        .filter(|(key, _)| key.ends_with(".context_length"))
                        .find_map(|(_, value)| positive(Some(value)))
                });
        }
        result.push(row);
    }
    Ok(result)
}

fn project(source: &ResolvedSource, row: &Metadata, template: &Model) -> Model {
    let mut model = template.clone();
    model.id.clone_from(&row.id);
    if template.id != row.id {
        model.name.clone_from(&row.id);
    }
    if let Some(entry) = &row.prime {
        let mut entry = entry.clone();
        entry.input = row
            .input_price
            .unwrap_or_else(|| template.cost.input.as_f64());
        entry.output = row
            .output_price
            .unwrap_or_else(|| template.cost.output.as_f64());
        let mut seed = template.clone();
        seed.id.clone_from(&entry.id);
        if let Some(mut prime) = crate::prime_inference::build_prime_inference_models(
            &[seed],
            std::slice::from_ref(&entry),
            true,
            Some(0),
        )
        .and_then(|models| models.into_iter().next())
        {
            prime.provider.clone_from(&source.provider);
            prime.api.clone_from(&template.api);
            prime.base_url.clone_from(&template.base_url);
            prime.headers.clone_from(&template.headers);
            prime.max_tokens_explicit = template.max_tokens_explicit;
            model = prime;
        }
    }
    if let Some(name) = &row.name {
        model.name.clone_from(name);
    }
    if let Some(context) = row.context {
        model.context_window = context;
    }
    if let Some(tokens) = row.max_tokens {
        model.max_tokens = tokens;
    }
    if let Some(input) = &row.input {
        model.input.clone_from(input);
    }
    if let Some(reasoning) = row.reasoning {
        model.reasoning = reasoning;
    }
    if let Some(thinking) = &row.thinking {
        model.thinking_level_map = Some(thinking.clone());
    }
    for (target, value) in [
        (&mut model.cost.input, row.input_price),
        (&mut model.cost.output, row.output_price),
        (&mut model.cost.cache_read, row.cache_read),
        (&mut model.cost.cache_write, row.cache_write),
    ] {
        if let Some(value) = value {
            *target = JsNumber::from(value);
        }
    }

    model
}
