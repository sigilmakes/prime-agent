//! The first-run onboarding concern: the startup-model probe, the settings sink
//! behind the pa-tui onboarding trait, and the startup task assembly.

use super::{PathBuf, Result, RunOptions};

/// The startup-model resolution inputs, captured at task construction: the flow
/// re-resolves the model state at its own boundaries (its sign-in can change it).
#[derive(Clone)]
pub(super) struct StartupModelProbe {
    pub(super) cwd: PathBuf,
    pub(super) agent_dir: PathBuf,
    pub(super) cli_provider: Option<String>,
    pub(super) cli_model: Option<String>,
    /// The `--models` scope pattern list, resolved against the fresh
    /// catalog on every probe.
    pub(super) models: Option<Vec<String>>,
    pub(super) is_continuing: bool,
    /// An explicit `--api-key` counts as configured auth (it rides the
    /// resolved model's provider as a runtime key).
    pub(super) api_key: Option<String>,
}

impl StartupModelProbe {
    /// The startup model, and whether it carries configured auth.
    fn resolve(&self) -> (Option<pa_types::ai::Model>, bool) {
        let settings = pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir);
        let auth = pa_core::auth::AuthStorage::create(&self.agent_dir);
        let mut registry =
            pa_core::models::ModelRegistry::create(auth, self.agent_dir.join("models.json"));
        // Sync resolution on a fresh registry must adopt the on-disk private
        // authorization cache before `get_available`.
        registry.load_private_authorization_from_cache();
        let all: Vec<pa_types::ai::Model> = registry.get_all().to_vec();
        let available: Vec<pa_types::ai::Model> =
            registry.get_available().into_iter().cloned().collect();
        let scoped = self
            .models
            .as_deref()
            .map(|patterns| pa_core::models::resolve_model_scope_from_models(patterns, &available))
            .unwrap_or_default();
        let startup_model =
            pa_core::models::find_initial_model(&pa_core::models::InitialModelOptions {
                cli_provider: self.cli_provider.as_deref(),
                cli_model: self.cli_model.as_deref(),
                scoped_models: &scoped,
                is_continuing: self.is_continuing,
                default_provider: settings.get_default_provider(),
                default_model_id: settings.get_default_model(),
                all_models: &all,
                available_models: &available,
            });
        let ready = startup_model.as_ref().is_some_and(|model| {
            if let Some(key) = self.api_key.as_deref() {
                !key.trim().is_empty()
            } else {
                // Catalog availability describes configuration, not whether a
                // command actually produced a credential for this request.
                // Transport-only model headers are not an auth declaration.
                registry.has_configured_auth(model) && {
                    let auth = registry.get_api_key_and_headers(model, None);
                    auth.ok
                        && (auth
                            .api_key
                            .as_deref()
                            .is_some_and(|key| !key.trim().is_empty())
                            || auth.headers.as_ref().is_some_and(|headers| {
                                headers.values().any(|value| !value.trim().is_empty())
                            }))
                }
            }
        });
        (startup_model, ready)
    }
}

/// Persistence for first-run provider setup in the global settings file.
pub(super) struct SettingsOnboardingSink {
    pub(super) cwd: PathBuf,
    pub(super) agent_dir: PathBuf,
}

impl pa_tui::interactive::OnboardingSink for SettingsOnboardingSink {
    fn onboarding_shown(&self) -> bool {
        pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir)
            .get_onboarding_shown()
    }

    fn mark_onboarding_complete(&self) -> Result<()> {
        pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir)
            .set_onboarding_shown(true)
    }
}

/// Build the first-run provider setup task with a live model-readiness probe.
pub(super) fn onboarding_task(
    options: &RunOptions,
    provider_auth: Option<pa_tui::provider_auth::ProviderAuthCommandsHandle>,
) -> Option<pa_tui::interactive::OnboardingTask> {
    let config = &options.config;
    let settings = pa_core::settings::SettingsManager::create(&config.cwd, &config.agent_dir);
    if settings.get_onboarding_shown() {
        return None;
    }
    let probe = StartupModelProbe {
        cwd: config.cwd.clone(),
        agent_dir: config.agent_dir.clone(),
        cli_provider: config.provider.clone(),
        cli_model: config.model.clone(),
        models: config.models.clone(),
        // A fork launch resolves the startup model as a continuation
        // (#2806): the copy holds the source's rows.
        is_continuing: options.session.resume.is_some()
            || options.session.continue_recent
            || options.session.fork.is_some(),
        api_key: config.api_key.clone(),
    };
    let (current_model, _) = probe.resolve();
    Some(pa_tui::interactive::OnboardingTask {
        sink: std::sync::Arc::new(SettingsOnboardingSink {
            cwd: config.cwd.clone(),
            agent_dir: config.agent_dir.clone(),
        }),
        model_ready: std::sync::Arc::new(move || probe.resolve().1),
        current_model,
        provider_auth,
    })
}
