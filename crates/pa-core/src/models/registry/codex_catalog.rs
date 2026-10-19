//! Credential-scoped Codex backend availability (metadata-only; no transport discovery).

use std::collections::HashSet;
use std::sync::Arc;

use base64::Engine as _;
use pa_models::cache::{CatalogCache, CatalogRefreshError, CatalogRefreshOutcome, RefreshOptions};
use pa_types::model_refresh::{ModelRefreshReason, ModelRefreshSourceResult, ModelRefreshStatus};

use super::ModelRegistry;

fn models_url(base_url: &str) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    let path = if base.ends_with("/codex/responses") {
        format!("{}/models", base.trim_end_matches("/responses"))
    } else if base.ends_with("/codex") {
        format!("{base}/models")
    } else {
        format!("{base}/codex/models")
    };
    let mut url = reqwest::Url::parse(&path).ok()?;
    url.query_pairs_mut()
        .append_pair("client_version", "0.159.0");
    Some(url.to_string())
}

fn account_id(token: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.split('.').nth(1)?)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

impl ModelRegistry {
    pub(super) async fn force_refresh_codex_catalog(&mut self) -> ModelRefreshSourceResult {
        let source = "openai-codex";
        let skipped = || ModelRefreshSourceResult {
            source: source.into(),
            status: ModelRefreshStatus::Skipped,
            model_count: None,
            reason: Some(ModelRefreshReason::Unconfigured),
        };
        let Some(model) = self
            .get_available()
            .into_iter()
            .find(|model| model.provider == source)
            .cloned()
        else {
            return skipped();
        };
        if crate::models::private_auth::is_offline_mode_enabled() {
            return ModelRefreshSourceResult {
                source: source.into(),
                status: ModelRefreshStatus::Stale,
                model_count: None,
                reason: Some(ModelRefreshReason::Offline),
            };
        }
        let auth = self.get_api_key_and_headers(&model, None);
        let Some((key, account)) = auth
            .api_key
            .and_then(|key| account_id(&key).map(|account| (key, account)))
        else {
            return skipped();
        };
        let Some(url) = models_url(&model.base_url) else {
            return ModelRefreshSourceResult {
                source: source.into(),
                status: ModelRefreshStatus::Failed,
                reason: Some(ModelRefreshReason::InvalidCatalog),
                model_count: None,
            };
        };
        let scope =
            pa_models::prime_inference::scope_key(&key, "prime-agent:openai-codex-models:v1");
        let cache = match &self.codex_catalog {
            Some(cache) if cache.url() == url => Arc::clone(cache),
            _ => {
                let cache = Arc::new(CatalogCache::new(
                    &url,
                    None,
                    Arc::new(pa_models::fetch::CatalogFetcher::new()),
                    Arc::new(|payload, _scope| {
                        let models = payload
                            .get("models")
                            .and_then(serde_json::Value::as_array)
                            .ok_or("Invalid Codex catalog")?;
                        Ok(models
                            .iter()
                            .filter_map(|model| {
                                model
                                    .get("slug")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string)
                            })
                            .collect::<HashSet<_>>())
                    }),
                ));
                self.codex_catalog = Some(Arc::clone(&cache));
                cache
            }
        };
        let mut headers: Vec<_> = auth.headers.unwrap_or_default().into_iter().collect();
        headers.retain(|(name, _)| {
            !name.eq_ignore_ascii_case("authorization")
                && !name.eq_ignore_ascii_case("chatgpt-account-id")
                && !name.eq_ignore_ascii_case("originator")
        });
        headers.extend([
            ("Authorization".into(), format!("Bearer {key}")),
            ("chatgpt-account-id".into(), account),
            ("originator".into(), "pi".into()),
        ]);
        let report = cache
            .refresh_report(
                &scope,
                RefreshOptions {
                    force: true,
                    headers,
                    is_current: self.catalog_publication_guard(),
                    ..Default::default()
                },
            )
            .await;
        let (status, reason) = match report.outcome {
            CatalogRefreshOutcome::Updated | CatalogRefreshOutcome::MemoryOnly => {
                (ModelRefreshStatus::Updated, None)
            }
            CatalogRefreshOutcome::NotModified => (ModelRefreshStatus::Unchanged, None),
            CatalogRefreshOutcome::Cached => (ModelRefreshStatus::Stale, None),
            CatalogRefreshOutcome::Offline => {
                (ModelRefreshStatus::Stale, Some(ModelRefreshReason::Offline))
            }
            CatalogRefreshOutcome::Cancelled => (
                ModelRefreshStatus::Stale,
                Some(ModelRefreshReason::Superseded),
            ),
            CatalogRefreshOutcome::Revoked { .. } => (
                ModelRefreshStatus::Failed,
                Some(ModelRefreshReason::AuthRejected),
            ),
            CatalogRefreshOutcome::Error { error } => (
                ModelRefreshStatus::Failed,
                Some(match error {
                    CatalogRefreshError::InvalidJson | CatalogRefreshError::InvalidPayload => {
                        ModelRefreshReason::InvalidCatalog
                    }
                    CatalogRefreshError::Http { status: 401 | 403 } => {
                        ModelRefreshReason::AuthRejected
                    }
                    CatalogRefreshError::PersistenceFailed => ModelRefreshReason::CacheWrite,
                    CatalogRefreshError::Http { .. } | CatalogRefreshError::RequestFailed => {
                        ModelRefreshReason::Network
                    }
                }),
            ),
        };
        self.codex_scope = Some(scope);
        ModelRefreshSourceResult {
            source: source.into(),
            status,
            reason,
            model_count: report.models.as_ref().map(HashSet::len),
        }
    }
}
