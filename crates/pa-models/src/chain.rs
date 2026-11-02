//! Catalog facade: trusted baseline, current seeds, and scoped metadata publications.
use crate::bundled::{load_bundled_models, BundledAssets};
use crate::cache::{CatalogCache, RefreshOptions, PUBLIC_SCOPE};
use crate::fetch::{CatalogFetcher, MODEL_CATALOG_URL};
use crate::pinning::{parse_provider_model_catalog, PinnedTemplates};
use crate::{transports, Model};
use std::path::PathBuf;
use std::sync::Arc;

pub const PROVIDER_CATALOG_CACHE_FILE: &str = "provider-model-catalog.v1.json";

pub struct ModelCatalog {
    provider_cache: CatalogCache<Vec<Model>>,
    default: Option<CatalogCache<String>>,
    bundled: BundledAssets,
    templates: PinnedTemplates,
    compiled: Arc<Vec<Model>>,
    discovery: crate::discovery::ProviderCatalog,
    discovery_urls: std::collections::HashMap<String, String>,
    pi_base_url: Arc<str>,
    ollama_base_url: String,
}

impl ModelCatalog {
    #[must_use]
    pub fn new(models_dir: Option<PathBuf>) -> Self {
        Self::with_bundled_dir(models_dir, None)
    }
    #[must_use]
    pub fn with_bundled_dir(models_dir: Option<PathBuf>, bundled_dir: Option<PathBuf>) -> Self {
        Self::with_discovery_urls(
            models_dir,
            bundled_dir,
            MODEL_CATALOG_URL,
            crate::prime_inference::PRIME_INFERENCE_BASE_URL,
            &std::collections::HashMap::new(),
            &std::env::var("OLLAMA_API_BASE")
                .unwrap_or_else(|_| crate::ollama_cloud::BASE_URL.into()),
        )
    }
    #[must_use]
    pub fn with_urls(
        models_dir: Option<PathBuf>,
        bundled_dir: Option<PathBuf>,
        model_catalog_url: &str,
        pi_base_url: &str,
    ) -> Self {
        let mut catalog = Self::with_discovery_urls(
            models_dir,
            bundled_dir,
            model_catalog_url,
            pi_base_url,
            &std::collections::HashMap::new(),
            crate::ollama_cloud::BASE_URL,
        );
        catalog.default = None;
        catalog
    }
    #[must_use]
    pub fn with_discovery_urls(
        models_dir: Option<PathBuf>,
        bundled_dir: Option<PathBuf>,
        model_catalog_url: &str,
        pi_base_url: &str,
        discovery_urls: &std::collections::HashMap<String, String>,
        ollama_base_url: &str,
    ) -> Self {
        let templates = PinnedTemplates::from_compiled();
        let pin_templates = templates.clone();
        Self {
            provider_cache: CatalogCache::new(
                model_catalog_url,
                models_dir
                    .as_ref()
                    .map(|dir| dir.join(PROVIDER_CATALOG_CACHE_FILE)),
                Arc::new(CatalogFetcher::new()),
                Arc::new(move |payload, _| parse_provider_model_catalog(payload, &pin_templates)),
            ),
            default: Some(CatalogCache::new(
                discovery_urls
                    .get("default")
                    .map_or(crate::DEFAULT_MODEL_CATALOG_URL, String::as_str),
                models_dir
                    .as_ref()
                    .map(|dir| dir.join("default-model.v1.json")),
                Arc::new(CatalogFetcher::new()),
                Arc::new(|payload, _| crate::provider_catalogs::parse_default(payload)),
            )),
            bundled: bundled_dir
                .map_or_else(BundledAssets::at_package_root, BundledAssets::from_dir),
            compiled: Arc::new(transports::compiled_models().to_vec()),
            templates,
            discovery: crate::discovery::ProviderCatalog::new(models_dir),
            discovery_urls: discovery_urls.clone(),
            pi_base_url: Arc::from(pi_base_url),
            ollama_base_url: ollama_base_url.into(),
        }
    }
    #[must_use]
    pub fn discovery_url_override(&self, provider: &str) -> Option<&str> {
        self.discovery_urls.get(provider).map(String::as_str)
    }
    #[must_use]
    pub fn prime_inference_base_url(&self) -> &str {
        &self.pi_base_url
    }
    #[must_use]
    pub fn ollama_base_url(&self) -> &str {
        &self.ollama_base_url
    }
    fn baseline(&self) -> Vec<Model> {
        match self.provider_cache.get(PUBLIC_SCOPE) {
            Some(remote) => {
                // Step 1: validated last-good cache + the compiled offline
                // Prime Inference entries (private/live entries come from
                // the credentialed snapshot below).
                let mut models = remote;
                for model in transports::prime_inference_offline_entries() {
                    models.push(model);
                }
                models
            }
            // Step 2: packaged bundled snapshot (damaged assets are None).
            None => match self
                .bundled
                .read_models()
                .and_then(|asset| load_bundled_models(&asset, &self.templates))
            {
                Some(bundled) => bundled,
                // Step 3: compiled fallback.
                None => self.compiled.as_ref().clone(),
            },
        }
    }
    fn preferred_default(&self) -> Option<String> {
        self.default
            .as_ref()
            .and_then(|cache| cache.get(PUBLIC_SCOPE))
    }
    #[must_use]
    pub fn snapshot(&self, inputs: &crate::CatalogInputs) -> crate::CatalogSnapshot {
        let mut baseline = self.baseline();
        for seed in &inputs.seeds {
            if let Some(model) = baseline
                .iter_mut()
                .find(|model| model.provider == seed.provider && model.id == seed.id)
            {
                *model = seed.clone();
            } else {
                baseline.push(seed.clone());
            }
        }
        let (models, authorized_private_prime_models) =
            self.discovery.resolve(&inputs.sources, &baseline);
        crate::CatalogSnapshot {
            models,
            preferred_default: self.preferred_default(),
            authorized_private_prime_models,
        }
    }
    pub async fn refresh(
        &self,
        inputs: &crate::CatalogInputs,
        request: &crate::RefreshRequest,
    ) -> crate::RefreshPublication {
        let selected = request.sources.as_deref();
        let force = request.policy == crate::RefreshPolicy::Force;
        let discovery = self
            .discovery
            .refresh(&inputs.sources, request.policy, selected);
        let curated = async {
            if !crate::provider_catalogs::selected(selected, "curated") {
                return None;
            }
            let templates = self.templates.clone();
            let report = self
                .provider_cache
                .refresh_report(
                    PUBLIC_SCOPE,
                    RefreshOptions {
                        force,
                        merge_payload: Some(Arc::new(move |payload, _| {
                            crate::pinning::parse_trusted_provider_model_catalog(
                                payload, &templates,
                            )?;
                            Ok(payload.clone())
                        })),
                        ..Default::default()
                    },
                )
                .await;
            Some(crate::provider_catalogs::source_result(
                "curated",
                &report,
                report.models.as_ref().map(Vec::len),
            ))
        };
        let default = async {
            let cache = self.default.as_ref()?;
            if !crate::provider_catalogs::selected(selected, "default") {
                return None;
            }
            let report = cache
                .refresh_report(
                    PUBLIC_SCOPE,
                    RefreshOptions {
                        force,
                        ..Default::default()
                    },
                )
                .await;
            Some(crate::provider_catalogs::source_result(
                "default",
                &report,
                report.models.as_ref().map(|_| 1),
            ))
        };
        let (mut sources, curated, default) = tokio::join!(discovery, curated, default);
        sources.extend(curated);
        sources.extend(default);
        crate::RefreshPublication {
            snapshot: self.snapshot(inputs),
            sources,
        }
    }
    pub fn adopt(
        &self,
        inputs: &crate::CatalogInputs,
        requirement: crate::SnapshotRequirement,
    ) -> crate::ApplyPublication {
        let mut report = self.discovery.adopt(&inputs.sources, requirement.since);
        let fresh = |checked: Option<u64>| {
            requirement
                .since
                .is_none_or(|since| checked.is_some_and(|at| at >= since))
        };
        if self.provider_cache.reload_from_disk(PUBLIC_SCOPE).is_some()
            && fresh(self.provider_cache.status(PUBLIC_SCOPE).checked_at)
        {
            report.applied.push("curated".into());
        } else {
            report.missing.push("curated".into());
        }
        if let Some(cache) = &self.default {
            if cache.reload_from_disk(PUBLIC_SCOPE).is_some()
                && fresh(cache.status(PUBLIC_SCOPE).checked_at)
            {
                report.applied.push("default".into());
            } else {
                report.missing.push("default".into());
            }
        }
        crate::ApplyPublication {
            snapshot: self.snapshot(inputs),
            report,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn models_json(ids: &[&str]) -> String {
        let compiled = transports::compiled_models();
        let anthropic = compiled
            .iter()
            .find(|m| m.provider == "anthropic")
            .expect("compiled anthropic");
        let models: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                json!({
                    "id": id,
                    "name": id,
                    "api": anthropic.api,
                    "provider": anthropic.provider,
                    "baseUrl": anthropic.base_url,
                    "reasoning": false,
                    "input": ["text"],
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                    "contextWindow": 128_000,
                    "maxTokens": 4_096,
                })
            })
            .collect();
        serde_json::to_string(&json!({"schemaVersion": 1, "models": models})).unwrap()
    }

    #[test]
    fn two_url_constructor_has_exactly_two_remote_domains() {
        let catalog = ModelCatalog::with_urls(
            None,
            None,
            "http://127.0.0.1:1/catalog",
            "http://127.0.0.1:1/api/v1",
        );
        assert!(
            catalog.default.is_none(),
            "no implicit provider/default endpoints"
        );
        assert!(catalog.preferred_default().is_none());
        let production = ModelCatalog::new(None);
        assert!(
            production.default.is_some(),
            "production explicitly enables discovery"
        );
    }

    #[test]
    fn compiled_fallback_when_no_cache_and_no_bundled_asset() {
        let dir = tempfile::tempdir().unwrap();
        let catalog =
            ModelCatalog::with_bundled_dir(Some(dir.path().into()), Some(dir.path().into()));
        let models = catalog.snapshot(&crate::CatalogInputs::default()).models;
        assert_eq!(
            models.len(),
            transports::compiled_models().len(),
            "compiled fallback"
        );
    }

    #[test]
    fn bundled_asset_beats_compiled_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("models.bundled.json"),
            models_json(&["bundled-a"]),
        )
        .unwrap();
        let catalog = ModelCatalog::with_bundled_dir(None, Some(dir.path().into()));
        let models = catalog.snapshot(&crate::CatalogInputs::default()).models;
        assert_eq!(
            models.len(),
            111,
            "1 pinned bundled entry + 110 offline prime-inference entries"
        );
        assert!(models.iter().any(|m| m.id == "bundled-a"));
    }

    #[test]
    fn disk_cache_beats_bundled_asset() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("models.bundled.json"),
            models_json(&["bundled-a"]),
        )
        .unwrap();
        // A validated last-good disk snapshot (the shape CatalogCache writes).
        let snapshot = json!({
            "url": MODEL_CATALOG_URL,
            "scope": PUBLIC_SCOPE,
            "fetchedAt": 1,
            "payload": serde_json::from_str::<serde_json::Value>(&models_json(&["cached-a"])).unwrap(),
        });
        std::fs::write(
            dir.path().join(PROVIDER_CATALOG_CACHE_FILE),
            serde_json::to_string(&snapshot).unwrap(),
        )
        .unwrap();
        let catalog =
            ModelCatalog::with_bundled_dir(Some(dir.path().into()), Some(dir.path().into()));
        let models = catalog.snapshot(&crate::CatalogInputs::default()).models;
        assert!(models.iter().any(|m| m.id == "cached-a"));
        assert!(!models.iter().any(|m| m.id == "bundled-a"), "cache wins");
    }

    #[test]
    fn damaged_cache_file_falls_back_to_bundled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("models.bundled.json"),
            models_json(&["bundled-a"]),
        )
        .unwrap();
        std::fs::write(dir.path().join(PROVIDER_CATALOG_CACHE_FILE), "{broken").unwrap();
        let catalog =
            ModelCatalog::with_bundled_dir(Some(dir.path().into()), Some(dir.path().into()));
        let models = catalog.snapshot(&crate::CatalogInputs::default()).models;
        assert!(models.iter().any(|m| m.id == "bundled-a"));
    }

    #[test]
    fn mid_session_refresh_never_retargets_the_active_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("models.bundled.json"),
            models_json(&["bundled-a"]),
        )
        .unwrap();
        let catalog =
            ModelCatalog::with_bundled_dir(Some(dir.path().into()), Some(dir.path().into()));
        let before = catalog.snapshot(&crate::CatalogInputs::default()).models;
        let active = before
            .iter()
            .find(|m| m.id == "bundled-a")
            .expect("active model")
            .clone();
        let snapshot = json!({
            "url": MODEL_CATALOG_URL,
            "scope": PUBLIC_SCOPE,
            "fetchedAt": 2,
            "payload": serde_json::from_str::<serde_json::Value>(&models_json(&["new-model"])).unwrap(),
        });
        std::fs::write(
            dir.path().join(PROVIDER_CATALOG_CACHE_FILE),
            serde_json::to_string(&snapshot).unwrap(),
        )
        .unwrap();
        assert_eq!(active.id, "bundled-a");
        assert_eq!(
            active.base_url,
            transports::compiled_models()
                .iter()
                .find(|m| m.provider == "anthropic")
                .unwrap()
                .base_url
        );
    }
}
