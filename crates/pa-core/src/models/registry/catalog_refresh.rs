//! Registry publication boundaries and explicit refresh reports.

use std::sync::Arc;

use pa_models::{DiscoveryCredential, DiscoveryCredentials};
use pa_types::model_refresh::{
    ModelRefreshApplyReport, ModelRefreshReason, ModelRefreshReport, ModelRefreshSourceResult,
    ModelRefreshStatus,
};

use super::{now_millis, ModelRegistry};
use crate::auth::types::AuthCredential;
use crate::models::private_auth::{
    is_offline_mode_enabled, read_private_prime_authorization_cache,
};

impl ModelRegistry {
    pub(super) fn can_resolve_catalog_auth_locally(&self, provider: &str) -> bool {
        match self.auth.get_all().credential(provider) {
            Some(AuthCredential::Oauth { .. }) => return false,
            Some(AuthCredential::ApiKey { key, .. }) if key.starts_with('!') => return false,
            _ => {}
        }
        if self
            .provider_request_configs
            .get(provider)
            .and_then(|config| config.api_key.as_ref())
            .is_some_and(|key| key.starts_with('!'))
        {
            return false;
        }
        if provider == "ollama-cloud" {
            let configured = self
                .provider_request_configs
                .get(provider)
                .and_then(|config| config.headers.as_ref());
            if configured
                .into_iter()
                .flat_map(|headers| headers.values())
                .any(|value| value.starts_with('!'))
            {
                return false;
            }
            if self
                .model_request_headers
                .iter()
                .filter(|(key, _)| key.starts_with("ollama-cloud:"))
                .flat_map(|(_, headers)| headers.values())
                .any(|value| value.starts_with('!'))
            {
                return false;
            }
        }
        true
    }

    /// A file-backed auth/config mutation invalidates publication, even when another process made it.
    pub(super) fn catalog_publication_guard(&self) -> Option<Arc<dyn Fn() -> bool + Send + Sync>> {
        let path = self.models_json_path.as_ref()?;
        let auth_path = path.parent()?.join("auth.json");
        let paths = [path.clone(), auth_path];
        let previous = paths.each_ref().map(|path| std::fs::read(path).ok());
        Some(Arc::new(move || {
            paths
                .iter()
                .zip(&previous)
                .all(|(path, bytes)| std::fs::read(path).ok().as_ref() == bytes.as_ref())
        }))
    }

    pub(super) fn discovery_credentials(&mut self, local_only: bool) -> DiscoveryCredentials {
        let model = self
            .models
            .iter()
            .find(|model| model.provider == "ollama-cloud")
            .cloned()
            .or_else(|| {
                pa_models::ollama_cloud::pinned_models(self.catalog.ollama_base_url())
                    .into_iter()
                    .next()
            });
        self.discovery_credentials_for_ollama(local_only, model)
    }

    pub(super) fn discovery_credentials_for_ollama(
        &mut self,
        local_only: bool,
        model: Option<pa_types::ai::Model>,
    ) -> DiscoveryCredentials {
        let mut credentials = DiscoveryCredentials::new();
        for provider in pa_models::provider_discovery::DISCOVERABLE_PROVIDERS {
            let id = provider.provider_id();
            if local_only && !self.can_resolve_catalog_auth_locally(id) {
                continue;
            }
            let key = self
                .auth
                .get_api_key_with_source_token(id, false)
                .api_key
                .or_else(|| {
                    self.provider_request_configs
                        .get(id)
                        .and_then(|config| config.api_key.as_ref())
                        .and_then(|value| {
                            crate::auth::resolve_config_value::resolve_config_value(value)
                        })
                });
            if let Some(secret) = key {
                credentials.insert(
                    id.to_string(),
                    DiscoveryCredential {
                        secret,
                        is_current: self.catalog_publication_guard(),
                    },
                );
            }
        }
        let id = "ollama-cloud";
        if !local_only || self.can_resolve_catalog_auth_locally(id) {
            if let Some(model) = model {
                let same_origin = reqwest::Url::parse(&model.base_url)
                    .ok()
                    .zip(reqwest::Url::parse(self.catalog.ollama_base_url()).ok())
                    .is_some_and(|(model, catalog)| model.origin() == catalog.origin());
                if same_origin {
                    let auth = self.get_api_key_and_headers(&model, None);
                    if auth.ok {
                        let authorization = auth
                            .headers
                            .as_ref()
                            .and_then(|headers| {
                                headers
                                    .iter()
                                    .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                                    .and_then(|(_, value)| {
                                        crate::auth::resolve_config_value::resolve_config_value(
                                            value,
                                        )
                                    })
                            })
                            .or_else(|| auth.api_key.map(|key| format!("Bearer {key}")));
                        if let Some(secret) = authorization {
                            credentials.insert(
                                id.to_string(),
                                DiscoveryCredential {
                                    secret,
                                    is_current: self.catalog_publication_guard(),
                                },
                            );
                        }
                    }
                }
            }
        }
        credentials
    }

    pub(crate) fn catalog_refresh_credentials(
        &mut self,
    ) -> (Option<pa_models::PrimeCredentials>, DiscoveryCredentials) {
        let local_only = is_offline_mode_enabled();
        let prime = if !local_only || self.can_resolve_catalog_auth_locally("prime-inference") {
            self.prime_credentials()
        } else {
            None
        };
        (prime, self.discovery_credentials(local_only))
    }

    /// Catalog readiness is a publication boundary, not a whole-startup timeout.
    pub async fn wait_for_catalog_publication(&mut self) {
        self.refresh();
        let offline = is_offline_mode_enabled();
        let credentials = self.discovery_credentials(offline);
        let prime = if !offline || self.can_resolve_catalog_auth_locally("prime-inference") {
            self.prime_credentials()
        } else {
            None
        };
        let private = prime.as_ref().and_then(|prime| {
            let team = prime.team_id.as_ref()?;
            let fingerprint = crate::models::private_auth::private_prime_authorization_fingerprint(
                &prime.api_key,
                team,
            );
            let cache = crate::models::private_auth::private_catalog_cache(
                self.models_json_path.as_deref(),
                self.catalog.prime_inference_base_url(),
            );
            let mut headers = vec![
                ("Authorization".into(), format!("Bearer {}", prime.api_key)),
                ("X-Prime-Team-ID".into(), team.clone()),
            ];
            if let Some(extra) = self.auth.get_provider_headers("prime-inference") {
                headers.extend(extra);
            }
            Some((
                cache,
                fingerprint,
                pa_models::cache::RefreshOptions {
                    headers,
                    is_current: self.catalog_publication_guard(),
                    ..Default::default()
                },
            ))
        });
        let catalog = Arc::clone(&self.catalog);
        let publication = tokio::spawn(async move {
            tokio::join!(
                catalog.refresh_catalog_sources(false, prime.as_ref(), &credentials, None),
                async {
                    if let Some((cache, fingerprint, options)) = private {
                        cache.refresh_report(&fingerprint, options).await;
                    }
                },
            );
        });
        // The monitor outlives a cancelled caller too: a worker panic is never
        // silently discarded with a dropped JoinHandle.
        let monitor = tokio::spawn(async move {
            if let Err(error) = publication.await {
                tracing::warn!(%error, "catalog readiness publication task failed");
            }
        });
        if let Err(error) = monitor.await {
            tracing::warn!(%error, "catalog readiness monitor failed");
        }
        self.load_models();
        self.load_private_authorization_from_cache();
    }

    /// Refresh every supported source now. Returns only after guarded cache publication.
    pub async fn refresh_catalogs(&mut self) -> ModelRefreshReport {
        self.refresh_catalog_sources(None).await
    }

    /// Source-filtered fallback for a resident worker missing the leader's local snapshots.
    pub async fn refresh_catalog_sources(
        &mut self,
        selected_sources: Option<&[String]>,
    ) -> ModelRefreshReport {
        let started_at = now_millis();
        self.refresh();
        let selected = |source: &str| {
            selected_sources.is_none_or(|sources| sources.iter().any(|selected| selected == source))
        };
        let offline = is_offline_mode_enabled();
        let credentials = self.discovery_credentials(offline);
        let prime = if !offline || self.can_resolve_catalog_auth_locally("prime-inference") {
            self.prime_credentials()
        } else {
            None
        };
        let mut sources = self
            .catalog
            .refresh_catalog_sources(true, prime.as_ref(), &credentials, selected_sources)
            .await;
        self.load_models();
        if selected("prime-inference-private") {
            let result = if offline {
                self.load_private_authorization_from_cache();
                ModelRefreshSourceResult {
                    source: "prime-inference-private".into(),
                    status: if self.get_provider_auth_status("prime-inference").configured {
                        ModelRefreshStatus::Stale
                    } else {
                        ModelRefreshStatus::Skipped
                    },
                    model_count: None,
                    reason: Some(
                        if self.get_provider_auth_status("prime-inference").configured {
                            ModelRefreshReason::Offline
                        } else {
                            ModelRefreshReason::Unconfigured
                        },
                    ),
                }
            } else {
                self.force_refresh_private_catalog().await
            };
            sources.push(result);
        }
        if selected("openai-codex") {
            sources.push(self.force_refresh_codex_catalog().await);
        }
        self.load_models();
        ModelRefreshReport {
            started_at,
            finished_at: now_millis(),
            sources,
            workers: None,
        }
    }

    /// Apply validated local snapshots only. Does not renew OAuth or run command-backed config.
    // Keep the awaitable publication interface: callers share the refresh/apply command path,
    // while this local-only operation deliberately performs no asynchronous external work.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn apply_catalog_refresh(&mut self, since: Option<u64>) -> ModelRefreshApplyReport {
        self.refresh();
        self.codex_catalog = None;
        self.codex_scope = None;
        let credentials = self.discovery_credentials(true);
        let prime = if self.can_resolve_catalog_auth_locally("prime-inference") {
            self.prime_credentials()
        } else {
            None
        };
        let mut report = self
            .catalog
            .apply_catalog_snapshots(prime.as_ref(), &credentials, since);
        for id in [
            "openai",
            "anthropic",
            "google",
            "ollama-cloud",
            "prime-inference",
        ] {
            if !self.can_resolve_catalog_auth_locally(id)
                && self.get_provider_auth_status(id).configured
            {
                let source = if id == "prime-inference" {
                    "prime-inference-public"
                } else {
                    id
                };
                report.skipped.retain(|item| item != source);
                if !report.missing.iter().any(|item| item == source) {
                    report.missing.push(source.to_string());
                }
            }
        }
        if let Some(prime) = prime {
            if let Some(team) = prime.team_id {
                let fingerprint =
                    crate::models::private_auth::private_prime_authorization_fingerprint(
                        &prime.api_key,
                        &team,
                    );
                let revisioned = crate::models::private_auth::private_catalog_cache(
                    self.models_json_path.as_deref(),
                    self.catalog.prime_inference_base_url(),
                );
                let cached = if revisioned.has_revocation(&fingerprint) {
                    None
                } else {
                    revisioned
                        .get(&fingerprint)
                        .map(|authorized| {
                            crate::models::private_auth::PrivatePrimeAuthorizationCache {
                                fingerprint: fingerprint.clone(),
                                items: authorized.items,
                                models: authorized.models,
                                refreshed_at: revisioned
                                    .status(&fingerprint)
                                    .checked_at
                                    .unwrap_or_default(),
                            }
                        })
                        .or_else(|| {
                            self.models_json_path
                                .as_deref()
                                .and_then(read_private_prime_authorization_cache)
                                .filter(|cache| cache.fingerprint == fingerprint)
                        })
                };
                if let Some(cache) = cached {
                    self.authorized_private_ids =
                        cache.models.iter().map(|model| model.id.clone()).collect();
                    self.authorized_private_models = cache.models;
                    self.authorized_team_id = Some(team);
                    if since.is_none_or(|since| cache.refreshed_at >= since) {
                        report.applied.push("prime-inference-private".into());
                    } else {
                        report.missing.push("prime-inference-private".into());
                    }
                } else {
                    report.missing.push("prime-inference-private".into());
                }
            } else {
                report.skipped.push("prime-inference-private".into());
            }
        } else {
            report.skipped.push("prime-inference-private".into());
        }
        if self.get_provider_auth_status("openai-codex").configured {
            report.missing.push("openai-codex".into());
        } else {
            report.skipped.push("openai-codex".into());
        }
        self.load_models();
        report
    }

    async fn force_refresh_private_catalog(&mut self) -> ModelRefreshSourceResult {
        use crate::models::private_auth::{
            private_catalog_cache, private_prime_authorization_fingerprint,
            write_private_prime_authorization_cache, PrivatePrimeAuthorizationCache,
        };
        use pa_models::cache::{CatalogRefreshError, CatalogRefreshOutcome, RefreshOptions};
        let source = "prime-inference-private";
        let Some(prime) = self.prime_credentials() else {
            self.clear_private_authorization();
            return ModelRefreshSourceResult {
                source: source.into(),
                status: ModelRefreshStatus::Skipped,
                reason: Some(ModelRefreshReason::Unconfigured),
                model_count: None,
            };
        };
        let Some(team) = prime.team_id else {
            self.clear_private_authorization();
            return ModelRefreshSourceResult {
                source: source.into(),
                status: ModelRefreshStatus::Skipped,
                reason: Some(ModelRefreshReason::Unconfigured),
                model_count: None,
            };
        };
        let fingerprint = private_prime_authorization_fingerprint(&prime.api_key, &team);
        let cache = private_catalog_cache(
            self.models_json_path.as_deref(),
            self.catalog.prime_inference_base_url(),
        );
        let mut headers = vec![
            ("Authorization".into(), format!("Bearer {}", prime.api_key)),
            ("X-Prime-Team-ID".into(), team.clone()),
        ];
        if let Some(extra) = self.auth.get_provider_headers("prime-inference") {
            headers.extend(extra);
        }
        let report = cache
            .refresh_report(
                &fingerprint,
                RefreshOptions {
                    force: true,
                    headers,
                    is_current: self.catalog_publication_guard(),
                    ..Default::default()
                },
            )
            .await;
        let (status, reason) = match report.outcome {
            CatalogRefreshOutcome::Updated => (ModelRefreshStatus::Updated, None),
            CatalogRefreshOutcome::NotModified => (ModelRefreshStatus::Unchanged, None),
            CatalogRefreshOutcome::Revoked { .. } => {
                self.clear_private_authorization();
                self.auth.mark_auth_stale("prime-inference");
                (
                    ModelRefreshStatus::Failed,
                    Some(ModelRefreshReason::AuthRejected),
                )
            }
            CatalogRefreshOutcome::Offline => {
                (ModelRefreshStatus::Stale, Some(ModelRefreshReason::Offline))
            }
            CatalogRefreshOutcome::Cached => (ModelRefreshStatus::Stale, None),
            CatalogRefreshOutcome::Cancelled => (
                ModelRefreshStatus::Stale,
                Some(ModelRefreshReason::Superseded),
            ),
            CatalogRefreshOutcome::MemoryOnly => (
                ModelRefreshStatus::Failed,
                Some(ModelRefreshReason::CacheWrite),
            ),
            CatalogRefreshOutcome::Error { error } => (
                if report.models.is_some() {
                    ModelRefreshStatus::Stale
                } else {
                    ModelRefreshStatus::Failed
                },
                Some(match error {
                    CatalogRefreshError::InvalidJson | CatalogRefreshError::InvalidPayload => {
                        ModelRefreshReason::InvalidCatalog
                    }
                    CatalogRefreshError::PersistenceFailed => ModelRefreshReason::CacheWrite,
                    CatalogRefreshError::Http { .. } | CatalogRefreshError::RequestFailed => {
                        ModelRefreshReason::Network
                    }
                }),
            ),
        };
        let model_count = report
            .models
            .as_ref()
            .map(|authorized| authorized.models.len());
        if let Some(authorized) = report.models {
            self.authorized_private_ids = authorized
                .models
                .iter()
                .map(|model| model.id.clone())
                .collect();
            self.authorized_private_models
                .clone_from(&authorized.models);
            self.authorized_team_id = Some(team);
            if matches!(
                status,
                ModelRefreshStatus::Updated | ModelRefreshStatus::Unchanged
            ) {
                if let Some(path) = &self.models_json_path {
                    write_private_prime_authorization_cache(
                        path,
                        &PrivatePrimeAuthorizationCache {
                            fingerprint,
                            models: authorized.models,
                            items: authorized.items,
                            refreshed_at: now_millis(),
                        },
                    );
                }
            }
        }
        ModelRefreshSourceResult {
            source: source.into(),
            status,
            reason,
            model_count,
        }
    }

    pub fn preferred_default_model_id(&self) -> Option<String> {
        self.catalog.preferred_default()
    }
}

#[cfg(test)]
#[path = "catalog_refresh_tests.rs"]
mod tests;
