//! One registry update flow for picker readiness, explicit refresh and local adoption.

use pa_models::{RefreshPolicy, RefreshRequest, SnapshotRequirement};
use pa_types::model_refresh::{
    ModelRefreshApplyReport, ModelRefreshReason, ModelRefreshReport, ModelRefreshSourceResult,
    ModelRefreshStatus,
};

use super::{now_millis, ModelRegistry};
use crate::models::private_auth::is_offline_mode_enabled;

impl ModelRegistry {
    pub(super) async fn update_catalog(
        &mut self,
        policy: RefreshPolicy,
        sources: Option<&[String]>,
    ) -> ModelRefreshReport {
        let started_at = now_millis();
        self.auth.reload();
        let (custom, inputs) = self.prepare_catalog(is_offline_mode_enabled());
        let mut publication = self
            .catalog
            .refresh(
                &inputs,
                &RefreshRequest {
                    policy,
                    sources: sources.map(<[String]>::to_vec),
                },
            )
            .await;
        for source in &self.unresolved_catalog_sources {
            if sources.is_none_or(|selected| selected.contains(source)) {
                publication.sources.push(ModelRefreshSourceResult {
                    source: source.clone(),
                    status: ModelRefreshStatus::Skipped,
                    reason: Some(ModelRefreshReason::Unconfigured),
                    model_count: None,
                });
            }
        }
        // A file/auth change can supersede a publication. Rebuild local configuration
        // too, rather than serving old explicit definitions with a new account.
        if inputs
            .sources
            .iter()
            .any(|source| source.is_current.as_ref().is_some_and(|current| !current()))
        {
            self.auth.reload();
            self.load_models();
        } else {
            self.accept_catalog_snapshot(custom, publication.snapshot);
        }
        ModelRefreshReport {
            started_at,
            finished_at: now_millis(),
            sources: publication.sources,
            workers: None,
        }
    }

    pub async fn wait_for_catalog_publication(&mut self) {
        self.update_catalog(RefreshPolicy::IfStale, None).await;
    }

    pub async fn refresh_catalogs(&mut self) -> ModelRefreshReport {
        self.update_catalog(RefreshPolicy::Force, None).await
    }

    pub async fn refresh_catalog_sources(
        &mut self,
        sources: Option<&[String]>,
    ) -> ModelRefreshReport {
        self.update_catalog(RefreshPolicy::Force, sources).await
    }

    /// Only local snapshots: no command keys, OAuth renewal or HTTP requests.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn apply_catalog_refresh(&mut self, since: Option<u64>) -> ModelRefreshApplyReport {
        self.auth.reload();
        let (custom, inputs) = self.prepare_catalog(true);
        let mut publication = self.catalog.adopt(&inputs, SnapshotRequirement { since });
        for source in &self.unresolved_catalog_sources {
            let provider = if source.starts_with("prime-inference-") {
                "prime-inference"
            } else {
                source
            };
            if self.get_provider_auth_status(provider).configured {
                publication.report.missing.push(source.clone());
            } else {
                publication.report.skipped.push(source.clone());
            }
        }
        self.accept_catalog_snapshot(custom, publication.snapshot);
        publication.report
    }

    pub fn preferred_default_model_id(&self) -> Option<String> {
        self.preferred_default.clone()
    }
}

#[cfg(test)]
#[path = "catalog_refresh_tests.rs"]
mod tests;
