//! Credential-free source reports and default-selector validation.
use crate::cache::{CatalogRefreshOutcome, CatalogRefreshReport};
use pa_types::model_refresh::{ModelRefreshReason, ModelRefreshSourceResult, ModelRefreshStatus};

pub const DEFAULT_MODEL_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/main/defaults.v1.json";

pub(crate) fn selected(sources: Option<&[String]>, source: &str) -> bool {
    sources.is_none_or(|sources| sources.iter().any(|item| item == source))
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

pub(crate) fn parse_default(payload: &serde_json::Value) -> Result<String, String> {
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
