//! Provider-owned, authenticated discovery snapshots. Transport metadata stays local.

use std::collections::HashMap;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use pa_types::model_refresh::{
    ModelRefreshApplyReport, ModelRefreshReason, ModelRefreshSourceResult, ModelRefreshStatus,
};
use sha2::Sha256;

use crate::cache::{
    CatalogCache, CatalogRefreshOutcome, CatalogRefreshReport, RefreshOptions, PUBLIC_SCOPE,
};
use crate::fetch::CatalogFetcher;
use crate::provider_discovery::{
    discovery_headers, merge_provider_discovery_payload, parse_provider_discovery,
    DiscoverableProvider, DISCOVERABLE_PROVIDERS,
};
use crate::{ollama_cloud, Model};

pub const DEFAULT_MODEL_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/main/defaults.v1.json";

/// A resolved discovery credential and its publication authorization check.
#[derive(Clone)]
pub struct DiscoveryCredential {
    pub secret: String,
    pub is_current: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

/// The caller resolves credentials; the catalog never reads auth files or renews tokens.
pub type DiscoveryCredentials = HashMap<String, DiscoveryCredential>;

pub(crate) struct ProviderCatalogs {
    providers: HashMap<&'static str, CatalogCache<Vec<Model>>>,
    ollama: CatalogCache<Vec<Model>>,
    ollama_base_url: String,
    default: CatalogCache<String>,
}

pub(crate) fn selected(sources: Option<&[String]>, source: &str) -> bool {
    sources.is_none_or(|sources| sources.iter().any(|item| item == source))
}

pub(crate) fn discovery_scope(provider: &str, secret: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("prime-agent:discovery:{provider}:v1").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub(crate) fn source_result<T>(
    source: &str,
    report: &CatalogRefreshReport<T>,
    count: Option<usize>,
) -> ModelRefreshSourceResult {
    use crate::cache::CatalogRefreshError;
    let has_models = report.models.is_some();
    let (status, reason) = match &report.outcome {
        CatalogRefreshOutcome::Updated => (ModelRefreshStatus::Updated, None),
        CatalogRefreshOutcome::NotModified => (ModelRefreshStatus::Unchanged, None),
        CatalogRefreshOutcome::Cached => (ModelRefreshStatus::Stale, None),
        CatalogRefreshOutcome::Offline => {
            (ModelRefreshStatus::Stale, Some(ModelRefreshReason::Offline))
        }
        CatalogRefreshOutcome::MemoryOnly => (
            ModelRefreshStatus::Failed,
            Some(ModelRefreshReason::CacheWrite),
        ),
        CatalogRefreshOutcome::Revoked { .. } => (
            ModelRefreshStatus::Failed,
            Some(ModelRefreshReason::AuthRejected),
        ),
        CatalogRefreshOutcome::Cancelled => (
            ModelRefreshStatus::Stale,
            Some(ModelRefreshReason::Cancelled),
        ),
        CatalogRefreshOutcome::Error { error } => {
            let reason = match error {
                CatalogRefreshError::InvalidJson | CatalogRefreshError::InvalidPayload => {
                    ModelRefreshReason::InvalidCatalog
                }
                CatalogRefreshError::PersistenceFailed => ModelRefreshReason::CacheWrite,
                CatalogRefreshError::Http { status: 401 | 403 } => ModelRefreshReason::AuthRejected,
                CatalogRefreshError::Http { .. } | CatalogRefreshError::RequestFailed => {
                    ModelRefreshReason::Network
                }
            };
            (
                if has_models {
                    ModelRefreshStatus::Stale
                } else {
                    ModelRefreshStatus::Failed
                },
                Some(reason),
            )
        }
    };
    ModelRefreshSourceResult {
        source: source.to_string(),
        status,
        model_count: count,
        reason,
    }
}

pub(crate) fn unconfigured(source: &str) -> ModelRefreshSourceResult {
    ModelRefreshSourceResult {
        source: source.to_string(),
        status: ModelRefreshStatus::Skipped,
        model_count: None,
        reason: Some(ModelRefreshReason::Unconfigured),
    }
}

fn parse_default(payload: &serde_json::Value) -> Result<String, String> {
    if payload
        .get("schemaVersion")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
    {
        return Err("Unsupported default-model catalog version".into());
    }
    let selector = payload
        .get("defaultModel")
        .and_then(serde_json::Value::as_str)
        .ok_or("Invalid default-model catalog entry")?;
    let (provider, model) = selector
        .split_once('/')
        .ok_or("Invalid default-model catalog entry")?;
    if provider.is_empty()
        || provider.len() > 128
        || model.is_empty()
        || !provider
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !provider.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err("Invalid default-model catalog entry".into());
    }
    Ok(selector.to_string())
}

impl ProviderCatalogs {
    pub(crate) fn new(
        models_dir: Option<&std::path::Path>,
        urls: &HashMap<String, String>,
        ollama_base_url: &str,
    ) -> Self {
        let path = |name: &str| models_dir.as_ref().map(|dir| dir.join(name));
        let mut providers = HashMap::new();
        for provider in DISCOVERABLE_PROVIDERS {
            let id = provider.provider_id();
            let bundled = Arc::new(crate::transports::compiled_models().to_vec());
            providers.insert(
                id,
                CatalogCache::new(
                    urls.get(id).map_or(provider.url(), String::as_str),
                    path(&format!("{id}-models.v1.json")),
                    Arc::new(CatalogFetcher::new()),
                    Arc::new(move |payload, _scope| {
                        parse_provider_discovery(provider, payload, &bundled, &[])
                    }),
                ),
            );
        }
        let base = ollama_base_url.trim_end_matches('/').to_string();
        let parse_base = base.clone();
        Self {
            providers,
            ollama: CatalogCache::new(
                &format!("{base}/v1/models"),
                path("ollama-cloud-models.v1.json"),
                Arc::new(CatalogFetcher::new()),
                Arc::new(move |payload, _scope| ollama_cloud::parse_catalog(payload, &parse_base)),
            ),
            ollama_base_url: base,
            default: CatalogCache::new(
                urls.get("default")
                    .map_or(DEFAULT_MODEL_CATALOG_URL, String::as_str),
                path("default-model.v1.json"),
                Arc::new(CatalogFetcher::new()),
                Arc::new(|payload, _scope| parse_default(payload)),
            ),
        }
    }

    pub(crate) fn ollama_base_url(&self) -> &str {
        &self.ollama_base_url
    }

    pub(crate) fn resolve(
        &self,
        mut models: Vec<Model>,
        credentials: &DiscoveryCredentials,
    ) -> Vec<Model> {
        for provider in DISCOVERABLE_PROVIDERS {
            let id = provider.provider_id();
            let credential = credentials.get(id);
            if discovery_headers(
                provider,
                credential.map(|credential| credential.secret.as_str()),
            )
            .is_none()
            {
                self.providers[id].get(PUBLIC_SCOPE);
                continue;
            }
            let scope = credential.map_or_else(
                || PUBLIC_SCOPE.to_string(),
                |credential| discovery_scope(id, &credential.secret),
            );
            if let Some(discovered) = self.providers[id].get(&scope) {
                models.retain(|model| model.provider != id);
                models.extend(discovered);
            }
        }
        models.retain(|model| model.provider != ollama_cloud::PROVIDER_ID);
        let discovered = if let Some(credential) = credentials.get(ollama_cloud::PROVIDER_ID) {
            self.ollama.get(&discovery_scope(
                ollama_cloud::PROVIDER_ID,
                &credential.secret,
            ))
        } else {
            self.ollama.get(PUBLIC_SCOPE);
            None
        };
        models.extend(
            discovered.unwrap_or_else(|| ollama_cloud::pinned_models(&self.ollama_base_url)),
        );
        models
    }

    async fn refresh_provider(
        &self,
        provider: DiscoverableProvider,
        credentials: &DiscoveryCredentials,
        force: bool,
    ) -> ModelRefreshSourceResult {
        let id = provider.provider_id();
        let credential = credentials.get(id);
        let Some(headers) = discovery_headers(
            provider,
            credential.map(|credential| credential.secret.as_str()),
        ) else {
            self.providers[id].get(PUBLIC_SCOPE);
            return unconfigured(id);
        };
        let scope = credential.map_or_else(
            || PUBLIC_SCOPE.to_string(),
            |credential| discovery_scope(id, &credential.secret),
        );
        let bundled = crate::transports::compiled_models().to_vec();
        let report = self.providers[id]
            .refresh_report(
                &scope,
                RefreshOptions {
                    force,
                    headers,
                    is_current: credential.and_then(|credential| credential.is_current.clone()),
                    merge_payload: Some(Arc::new(move |payload, previous| {
                        merge_provider_discovery_payload(
                            provider,
                            payload,
                            previous.unwrap_or(&serde_json::Value::Null),
                            &bundled,
                        )
                    })),
                    ..Default::default()
                },
            )
            .await;
        source_result(id, &report, report.models.as_ref().map(Vec::len))
    }

    async fn refresh_ollama(
        &self,
        credentials: &DiscoveryCredentials,
        force: bool,
    ) -> ModelRefreshSourceResult {
        let id = ollama_cloud::PROVIDER_ID;
        let Some(credential) = credentials.get(id) else {
            self.ollama.get(PUBLIC_SCOPE);
            return unconfigured(id);
        };
        let base = self.ollama_base_url.clone();
        let authorization = credential.secret.clone();
        let report = self
            .ollama
            .refresh_report(
                &discovery_scope(id, &credential.secret),
                RefreshOptions {
                    force,
                    is_current: credential.is_current.clone(),
                    merge_payload: Some(Arc::new(ollama_cloud::merge_catalog)),
                    fetch_payload: Some(Arc::new(move || {
                        let base = base.clone();
                        let authorization = authorization.clone();
                        Box::pin(
                            async move { ollama_cloud::fetch_catalog(&base, &authorization).await },
                        )
                    })),
                    ..Default::default()
                },
            )
            .await;
        source_result(id, &report, report.models.as_ref().map(Vec::len))
    }

    pub(crate) async fn refresh(
        &self,
        credentials: &DiscoveryCredentials,
        force: bool,
        sources: Option<&[String]>,
    ) -> Vec<ModelRefreshSourceResult> {
        let task = async |provider: DiscoverableProvider| {
            if selected(sources, provider.provider_id()) {
                Some(self.refresh_provider(provider, credentials, force).await)
            } else {
                None
            }
        };
        let (openrouter, openai, anthropic, google, ollama, default) = tokio::join!(
            task(DiscoverableProvider::OpenRouter),
            task(DiscoverableProvider::OpenAi),
            task(DiscoverableProvider::Anthropic),
            task(DiscoverableProvider::Google),
            async {
                if selected(sources, ollama_cloud::PROVIDER_ID) {
                    Some(self.refresh_ollama(credentials, force).await)
                } else {
                    None
                }
            },
            async {
                if selected(sources, "default") {
                    let report = self
                        .default
                        .refresh_report(
                            PUBLIC_SCOPE,
                            RefreshOptions {
                                force,
                                ..Default::default()
                            },
                        )
                        .await;
                    Some(source_result(
                        "default",
                        &report,
                        report.models.as_ref().map(|_| 1),
                    ))
                } else {
                    None
                }
            }
        );
        [openrouter, openai, anthropic, google, ollama, default]
            .into_iter()
            .flatten()
            .collect()
    }

    pub(crate) fn apply(
        &self,
        credentials: &DiscoveryCredentials,
        since: Option<u64>,
    ) -> ModelRefreshApplyReport {
        let mut report = ModelRefreshApplyReport::default();
        let mut record = |source: &str, present: bool, checked_at: Option<u64>| {
            if present && since.is_none_or(|since| checked_at.is_some_and(|at| at >= since)) {
                report.applied.push(source.to_string());
            } else {
                report.missing.push(source.to_string());
            }
        };
        for provider in DISCOVERABLE_PROVIDERS {
            let id = provider.provider_id();
            let credential = credentials.get(id);
            if discovery_headers(
                provider,
                credential.map(|credential| credential.secret.as_str()),
            )
            .is_none()
            {
                report.skipped.push(id.to_string());
                continue;
            }
            let scope = credential.map_or_else(
                || PUBLIC_SCOPE.to_string(),
                |credential| discovery_scope(id, &credential.secret),
            );
            let cache = &self.providers[id];
            record(
                id,
                cache.reload_from_disk(&scope).is_some(),
                cache.status(&scope).checked_at,
            );
        }
        if let Some(credential) = credentials.get(ollama_cloud::PROVIDER_ID) {
            let scope = discovery_scope(ollama_cloud::PROVIDER_ID, &credential.secret);
            record(
                ollama_cloud::PROVIDER_ID,
                self.ollama.reload_from_disk(&scope).is_some(),
                self.ollama.status(&scope).checked_at,
            );
        } else {
            report.skipped.push(ollama_cloud::PROVIDER_ID.to_string());
        }
        record(
            "default",
            self.default.reload_from_disk(PUBLIC_SCOPE).is_some(),
            self.default.status(PUBLIC_SCOPE).checked_at,
        );
        report
    }

    pub(crate) fn preferred_default(&self) -> Option<String> {
        self.default.get(PUBLIC_SCOPE)
    }
}

#[cfg(test)]
#[path = "provider_catalogs_tests.rs"]
mod tests;
