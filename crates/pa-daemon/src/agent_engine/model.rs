//! The agent-session engine's model concern: the startup/restore
//! model-resolution cluster (the TS `createAgentSession` chain), the
//! session's live-model and thinking-level surfaces, and the request
//! API-key seam.

use super::{AgentSessionEngine, EngineModelSelection, Model, RestoredSessionModel, SessionEngine};

impl AgentSessionEngine {
    /// Merge requested flags without resolving against the previous live target.
    pub(super) fn merge_model_selection(&self, selection: EngineModelSelection) {
        // Merge like the TS runtime config: explicit wire flags replace; absent fields keep.
        {
            let mut current = self.selection.write().expect("model selection lock");
            if selection.provider.is_some() {
                current.provider = selection.provider;
            }
            if selection.model.is_some() {
                current.model = selection.model;
            }
            if selection.api_key.is_some() {
                current.api_key = selection.api_key;
            }
            if selection.thinking.is_some() {
                current.thinking = selection.thinking;
            }
        }
    }

    /// Preserve a lazy session's already selected model before disk publication.
    /// This fills only the model slot: the eventual build resolves credentials
    /// normally, and explicit switches/replacements replace or clear the slot.
    pub(crate) fn capture_model_metadata_refresh_anchor(&self) {
        let _reload_serialized = self
            .reload_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self
            .provider_target
            .read()
            .expect("provider target lock")
            .is_some()
        {
            return;
        }
        // A session without any resolvable model may still refresh its catalog.
        // There is no selected identity to retain in that case.
        let Ok(model) = self.resolve_model() else {
            return;
        };
        let mut target = self.provider_target.write().expect("provider target lock");
        if target.is_none() {
            *target = Some(pa_core::session_engine::provider_adapter::ProviderTarget {
                model,
                api_key: None,
                headers: None,
                service_tier: *self.service_tier.read().expect("service tier lock"),
            });
        }
    }

    /// Queue catalog metadata adoption without touching an active provider request.
    pub(crate) fn schedule_model_metadata_refresh(&self) {
        self.model_metadata_refresh_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Called only before a model turn starts, never from a refresh command.
    /// Refresh the serving identity, not the startup default; a catalog removal
    /// keeps the last-good model. Credentials stay owned by the auth/reload lane.
    pub(super) fn apply_scheduled_model_metadata_refresh(&self) -> anyhow::Result<()> {
        if !self
            .model_metadata_refresh_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        let _reload_serialized = self
            .reload_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = self
            .provider_target
            .read()
            .expect("provider target lock")
            .as_ref()
            .map(|target| target.model.clone());
        let Some(current) = current else {
            // A not-yet-built session resolves the refreshed disk view at build.
            return Ok(());
        };
        let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
        let mut registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        registry.load_private_authorization_from_cache();
        let refreshed = registry
            .get_all()
            .iter()
            .find(|model| model.provider == current.provider && model.id == current.id)
            .cloned()
            .unwrap_or_else(|| current.clone());
        let refreshed = self.configure_fresh_ollama_model(&refreshed);
        let wire = pa_core::session_engine::provider_adapter::json_round_trip(&refreshed)
            .ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
        let level = pa_ai::models::clamp_thinking_level(&refreshed, self.effective_thinking());
        let session = self.session.blocking_lock().clone();
        if let Some(core) = session {
            // Agent-only metadata update: no synthetic model_change row.
            self.runtime
                .block_on(core.session.agent().set_model_and_thinking_level(
                    wire,
                    pa_core::session_engine::provider_adapter::map_thinking_level(level),
                ));
            core.update_model_facts(&refreshed);
        }
        // Reload and model-switch credentials may have advanced while reading
        // the catalog. Replace only model metadata, and only for the same id.
        let mut target = self.provider_target.write().expect("provider target lock");
        if let Some(target) = target.as_mut().filter(|target| {
            target.model.provider == current.provider && target.model.id == current.id
        }) {
            target.model = refreshed;
            *self
                .effective_thinking
                .write()
                .expect("effective thinking lock") = Some(level);
        }
        Ok(())
    }

    pub(crate) fn configure_ollama_model(&self, model: &Model) -> Model {
        self.ollama_cloud
            .configure_model(&self.cwd(), &self.config.agent_dir, model)
    }

    pub(crate) fn configure_fresh_ollama_model(&self, model: &Model) -> Model {
        self.ollama_cloud
            .configure_fresh_model(&self.cwd(), &self.config.agent_dir, model)
    }

    /// The startup chain: the saved settings default, then the featured
    /// default, then the first available model.
    fn startup_chain_model(&self, registry: &pa_core::models::ModelRegistry) -> Option<Model> {
        let available: Vec<Model> = registry.get_available().into_iter().cloned().collect();
        let all: Vec<Model> = registry.get_all().to_vec();
        let settings =
            pa_core::settings::SettingsManager::create(self.cwd(), &self.config.agent_dir);
        // The create-time `--models` scope: a fresh session starts on the
        // saved default when in scope, else the first scoped model.
        let (scoped_models, is_continuing) = match self
            .startup_scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            Some(scope) => (scope.scoped_models, scope.is_continuing),
            None => (Vec::new(), false),
        };
        pa_core::models::find_initial_model(&pa_core::models::InitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: &scoped_models,
            is_continuing,
            default_provider: settings.get_default_provider(),
            default_model_id: settings.get_default_model(),
            all_models: &all,
            available_models: &available,
        })
        .or_else(|| all.first().cloned())
    }

    /// The runtime-config reset at every session restore: the create's own
    /// flags survive every replacement; the cached thinking level drops (the
    /// clamp follows the moved-to model).
    fn reset_selection_to_spawn_fallback(&self) {
        *self
            .effective_thinking
            .write()
            .expect("effective thinking lock") = None;
        {
            let initial = self
                .initial_selection
                .read()
                .expect("initial selection lock")
                .clone();
            let mut current = self.selection.write().expect("model selection lock");
            if current.provider == initial.provider
                && current.model == initial.model
                && current.api_key == initial.api_key
                && current.thinking == initial.thinking
            {
                return;
            }
            *current = initial;
        }
    }

    /// The create-time session-model restore (see
    /// [`SessionEngine::restore_session_model`]): reset the selection to the
    /// spawn-time fallback, read the saved model context, give the in-flight
    /// refreshes the bounded readiness window, and record the decision.
    /// Explicit spawn flags win; a miss records the fallback (never silent).
    pub(super) async fn restore_session_model_at(
        &self,
        session_path: &std::path::Path,
        pre_read: Option<crate::engine::SavedSessionContext>,
    ) {
        // An unpersisted session (an in-memory fork or a no-session worker's
        // replacement) has no file to read: the reset must not run with
        // nothing to restore.
        if session_path.as_os_str().is_empty() {
            return;
        }
        self.reset_selection_to_spawn_fallback();
        // The file pins the model and thinking level. A caller that already
        // read the context hands it in; otherwise the scan is plain file work on
        // a potentially large file — park it on a blocking thread.
        let saved = if let Some(saved) = pre_read {
            saved
        } else {
            let path = session_path.to_path_buf();
            let Ok(saved) = tokio::task::spawn_blocking(move || saved_session_context(&path)).await
            else {
                return;
            };
            let Some(saved) = saved else {
                return;
            };
            saved
        };
        // TS `createAgentSession` re-reads the saved thinking level at every
        // boot when the runtime config carries no explicit flag: the pinned
        // level wins over the settings/medium default.
        if self.current_selection().thinking.is_none() {
            if let Some(level) = saved.thinking {
                self.configure_model(EngineModelSelection {
                    thinking: Some(level),
                    ..Default::default()
                });
            }
        }
        // An explicit create flag wins for the MODEL — the saved thinking
        // above still applies, and the restore skips the model's readiness
        // window entirely.
        if self.current_selection().model.is_some() {
            return;
        }
        let Some((provider, model_id)) = saved.model else {
            return;
        };
        let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
        let mut registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        registry.load_private_authorization_from_cache();
        let restored = pa_core::models::find_session_model_with_readiness_wait(
            &mut registry,
            &provider,
            &model_id,
            pa_core::models::SESSION_MODEL_RESTORE_READINESS_TIMEOUT_MS,
        )
        .await;
        let (model, fallback_message) = if let Some(restored) = restored {
            (Some((restored.provider, restored.id)), None)
        } else {
            // The restore miss is on the record — the startup chain owns the
            // session.
            let fallback = self.startup_chain_model(&registry);
            let message = match &fallback {
                Some(fallback) => format!(
                    "Could not restore model {provider}/{model_id}. Using {}/{}",
                    fallback.provider, fallback.id
                ),
                None => format!("Could not restore model {provider}/{model_id}"),
            };
            eprintln!("{message}");
            (None, Some(message))
        };
        *self
            .restored_model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(RestoredSessionModel {
            session_file: session_path.to_path_buf(),
            model,
            fallback_message,
        });
        // The decision is on the record, so the level must resolve against the
        // model this session runs on; a concurrent read may have populated the
        // cache against the startup chain, so drop it once more.
        *self
            .effective_thinking
            .write()
            .expect("effective thinking lock") = None;
        let _ = self.effective_thinking();
    }

    /// The restored-from-session resolution for the engine's current session
    /// file: a catalog flap never silently drifts to the featured default; a
    /// decision for another file is ignored.
    fn restored_model_resolution(
        &self,
        registry: &pa_core::models::ModelRegistry,
    ) -> Option<Model> {
        let decision = self
            .restored_model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        let (provider, model_id) = decision.model?;
        let current = self
            .session_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        if decision.session_file != current {
            return None;
        }
        pa_core::models::resolve_cli_model(Some(&provider), &model_id, registry.get_all()).model
    }

    /// Resolve the model through the composed registry, then enforce the
    /// settings `allowedModels` allowlist: a resolution outside the allowlist
    /// fails loudly here (never a silent fallback).
    pub(super) fn resolve_registry_model(&self) -> anyhow::Result<Model> {
        let model = self.resolve_registry_model_unchecked()?;
        let selector = format!("{}/{}", model.provider, model.id);
        let allowlist = crate::model_allowlist::load(&self.cwd(), &self.config.agent_dir);
        crate::model_allowlist::assert_allowed(&allowlist, &selector)?;
        Ok(self.configure_fresh_ollama_model(&model))
    }

    /// The registry resolution before the allowlist gate: the flagged-model
    /// arm or the startup chain.
    fn resolve_registry_model_unchecked(&self) -> anyhow::Result<Model> {
        let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
        let mut registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        // A fresh registry gates private Prime Inference models out until the
        // async authorization refresh runs; adopt the on-disk cache so
        // create-time resolution can pick the session's private model.
        registry.load_private_authorization_from_cache();
        let selection = self.current_selection();
        let Some(model_name) = selection.model.as_deref() else {
            if let Some(model) = self.restored_model_resolution(&registry) {
                return Ok(model);
            }
            let Some(model) = self.startup_chain_model(&registry) else {
                anyhow::bail!(
                    "No models available. Check your installation or add models to models.json."
                );
            };
            return Ok(model);
        };
        // The full catalog, not the auth-configured list: a saved or switched
        // model keeps resolving when no credential is visible; run-start auth
        // validation reports it.
        let all: Vec<Model> = registry.get_all().to_vec();
        let resolved =
            pa_core::models::resolve_cli_model(selection.provider.as_deref(), model_name, &all);
        if let Some(error) = resolved.error {
            anyhow::bail!("{error}");
        }
        resolved
            .model
            .ok_or_else(|| anyhow::anyhow!("No matching model found."))
    }

    /// Test seam: a scripted faux provider (same script contract as pa-cli's
    /// print runtime) drives the engine without the network. The provider
    /// registers once per engine: its queued responses then span the whole
    /// session (multi-turn scripts), instead of replaying from the top on
    /// every model resolution.
    ///
    /// The script model serves the session while the selection is unset or
    /// names it; any other selection resolves through the registry (a
    /// models.json faux-api model streams through the same registered
    /// provider).
    pub(crate) fn resolve_model(&self) -> anyhow::Result<Model> {
        if let Some(script) = &self.config.faux_script {
            let model = if let Some(model) = self.faux_model.get() {
                model.clone()
            } else {
                let model = faux_model_from_script(script)?;
                let _ = self.faux_model.set(model.clone());
                model
            };
            let selection = self.current_selection();
            let names_script_model = selection
                .provider
                .as_deref()
                .is_none_or(|provider| provider == model.provider.as_str())
                && selection
                    .model
                    .as_deref()
                    .is_none_or(|model_id| model_id == model.id.as_str());
            if names_script_model {
                return Ok(model);
            }
            return self.resolve_registry_model();
        }
        self.resolve_registry_model()
    }

    /// The session's live model for summarization-side model calls
    /// (compaction, branch summaries, side questions): the provider target
    /// the stream reads per call — never a fresh resolution
    /// ([`Self::resolve_model`] can resolve differently). Falls back to it
    /// before the session's first build.
    pub(crate) fn session_model(&self) -> anyhow::Result<Model> {
        if let Some(target) = self
            .provider_target
            .read()
            .expect("provider target lock")
            .clone()
        {
            return Ok(self.configure_ollama_model(&target.model));
        }
        self.resolve_model()
    }

    /// The effective session thinking level: the create-config flag, then the
    /// settings default, then "medium" — clamped to what the model supports; an
    /// unresolvable model degrades to "off". Resolved once and cached.
    pub(crate) fn effective_thinking(&self) -> pa_types::ai::ModelThinkingLevel {
        if let Some(level) = *self
            .effective_thinking
            .read()
            .expect("effective thinking lock")
        {
            return level;
        }
        let model = self.session_model();
        let selection = self.current_selection();
        let requested = selection
            .thinking
            .or_else(|| {
                // An unflagged fresh session that starts on a scoped entry
                // takes that entry's `:thinking` (the explicit `--thinking`
                // above still wins).
                if selection.model.is_some() {
                    return None;
                }
                let scope = self
                    .startup_scope
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                scope
                    .filter(|scope| !scope.is_continuing)?
                    .scoped_models
                    .iter()
                    .find(|scoped| {
                        model.as_ref().is_ok_and(|model| {
                            scoped.model.provider == model.provider && scoped.model.id == model.id
                        })
                    })?
                    .thinking_level
                    .and_then(|level| {
                        let wire = serde_json::to_value(level).ok()?;
                        wire.as_str()
                            .and_then(pa_ai::models::thinking_level_from_str)
                    })
            })
            .or_else(|| {
                let settings =
                    pa_core::settings::SettingsManager::create(self.cwd(), &self.config.agent_dir);
                settings
                    .get_default_thinking_level()
                    .map(pa_core::settings::ThinkingLevelSetting::model_level)
            })
            // TS `DEFAULT_THINKING_LEVEL`.
            .unwrap_or(pa_types::ai::ModelThinkingLevel::Medium);
        let resolved = match model {
            Ok(model) => pa_ai::models::clamp_thinking_level(&model, requested),
            Err(_) => pa_types::ai::ModelThinkingLevel::Off,
        };
        *self
            .effective_thinking
            .write()
            .expect("effective thinking lock") = Some(resolved);
        resolved
    }

    /// The request-time api key AND its resolved provider headers (the
    /// selection's own headers lead; the registry resolves the model's
    /// otherwise): models needing custom or auth headers send them on every
    /// request.
    pub(crate) fn resolve_request_key_and_headers(
        &self,
        model: &Model,
    ) -> (
        Option<String>,
        Option<std::collections::BTreeMap<String, String>>,
    ) {
        let (api_key, headers, _store_healthy) =
            self.resolve_request_key_and_headers_and_store_health(model);
        (api_key, headers)
    }

    /// [`Self::resolve_request_key_and_headers`] plus whether the
    /// resolution serves a live credential for the model's provider on a
    /// healthy read — stored, or the ambient environment/runtime override
    /// (a logout that empties the store leaves the session on the env
    /// key, exactly like a fresh resolution). `false` means the resolved
    /// key is the configured `models.json` fallback — served when the
    /// store is unreadable OR resolves no credential from any live
    /// source — which is not a credential and must never replace a
    /// serving pair (the create-config override is source-independent;
    /// callers apply it themselves).
    pub(crate) fn resolve_request_key_and_headers_and_store_health(
        &self,
        model: &Model,
    ) -> (
        Option<String>,
        Option<std::collections::BTreeMap<String, String>>,
        bool,
    ) {
        let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
        let store_healthy = auth.load_error().is_none();
        let mut registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        let resolved = registry.get_api_key_and_headers(model, model.headers.as_ref());
        // The credential check runs AFTER the resolution: the registry's
        // lookup may OAuth-refresh and store a fresh token, so a check
        // before it would read the stale one and discard the fresh pair.
        let stored_credential = registry
            .auth
            .get_api_key_with_source_token(&model.provider, false)
            .api_key
            .is_some();
        let store_auth = store_healthy && stored_credential;
        if let Some(api_key) = &self.current_selection().api_key {
            // The create-config key override pins the key, never the headers:
            // the registry's merged headers still ship, exactly like the TS
            // `getApiKeyAndHeaders` override path. The flag stays the
            // store's: a caller pairing the override key with headers
            // keeps its last-good headers when the store cannot serve.
            return (Some(api_key.clone()), resolved.headers, store_auth);
        }
        (resolved.api_key, resolved.headers, store_auth)
    }

    pub(crate) fn resolve_request_api_key(&self, model: &Model) -> Option<String> {
        if let Some(api_key) = &self.current_selection().api_key {
            return Some(api_key.clone());
        }
        let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
        let mut registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        registry
            .get_api_key_and_headers(model, model.headers.as_ref())
            .api_key
    }

    /// The primary target a failover episode restores: the captured model
    /// with the request auth RE-RESOLVED from the store. The captured
    /// pair only backs the restore when the store cannot serve a live
    /// credential; the create-config key override stands in memory
    /// regardless, keeping the captured headers off a failed read.
    pub(crate) fn restored_primary_target(
        &self,
        primary: &Model,
        captured_api_key: Option<String>,
        captured_headers: Option<std::collections::BTreeMap<String, String>>,
    ) -> pa_core::session_engine::provider_adapter::ProviderTarget {
        let override_key = self.current_selection().api_key;
        let (api_key, headers, store_auth) =
            self.resolve_request_key_and_headers_and_store_health(primary);
        // The capture backs the restore whenever the store cannot serve a
        // live credential (a failed read, or a healthy store with nothing
        // stored — both resolve the configured `models.json` fallback key,
        // which is not a credential); a live stored credential REPLACES
        // the pair, headers included.
        let (api_key, headers) = if let Some(override_key) = override_key {
            if store_auth {
                (Some(override_key), headers)
            } else {
                (Some(override_key), captured_headers.or(headers))
            }
        } else {
            match api_key {
                Some(api_key) if store_auth => (Some(api_key), headers),
                _ => (captured_api_key, captured_headers),
            }
        };
        pa_core::session_engine::provider_adapter::ProviderTarget {
            service_tier: *self.service_tier.read().expect("service tier lock"),
            api_key,
            model: self.configure_ollama_model(primary),
            headers,
        }
    }
}

/// The saved model context of a session file: the pinned `(provider,
/// model)` and the saved thinking level, present only when the file carries
/// a `thinking_level_change` row. `None` when the file cannot be read (the
/// create flow owns that failure).
pub(crate) fn saved_session_context(
    path: &std::path::Path,
) -> Option<crate::engine::SavedSessionContext> {
    // Reads the saved (provider, model) + thinking level from the retained
    // window; unsupported files and malformed retained rows fall back to the
    // full open inside `open_windowed`.
    let store = crate::session_store::SessionFile::open_windowed(path).ok()?;
    let has_thinking_level = store.has_thinking_level();
    Some(saved_session_context_from_parts(
        &store.restored_settings(),
        has_thinking_level,
    ))
}

/// The saved context derived from an already-folded `restored_settings()`
/// value: the create path folds the context once, so a pre-read context and a
/// file-read context are identical by construction.
pub(crate) fn saved_session_context_from_parts(
    context: &pa_core::session::SessionContext,
    has_thinking_level: bool,
) -> crate::engine::SavedSessionContext {
    crate::engine::SavedSessionContext {
        model: context.model.clone(),
        thinking: has_thinking_level
            .then(|| pa_ai::models::thinking_level_from_str(&context.thinking_level))
            .flatten(),
    }
}

/// Register the faux provider from a script and return its model. Scripts
/// carry plain-text responses or content-block arrays (thinking, text, tool
/// calls) so harnesses can script full turns. Verification harness only.
fn faux_model_from_script(script: &str) -> anyhow::Result<Model> {
    let script: serde_json::Value = serde_json::from_str(script)?;
    let parsed = pa_ai::faux::script::parse_faux_script(&script).map_err(anyhow::Error::msg)?;
    let registration = pa_ai::faux::script::register_faux_provider_from_script(&parsed);
    Ok(registration.get_model())
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use pa_core::session_engine::provider_adapter::ProviderTarget;
    use serde_json::json;

    fn bare_engine(dir: &std::path::Path) -> AgentSessionEngine {
        let agent_dir = dir.join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        AgentSessionEngine::new(crate::agent_engine::AgentEngineConfig {
            cwd: dir.to_path_buf(),
            agent_dir,
            provider: None,
            model: None,
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
            telemetry_disabled: Some(true),
            cron_store: None,
            queued_steering_probe: None,
        })
        .unwrap()
    }

    fn write_models(engine: &AgentSessionEngine, ids: &[&str], window: u64) {
        let models: Vec<_> = ids
            .iter()
            .map(|id| {
                json!({
                    "id": id, "name": format!("Refreshed {id}"), "api": "openai-completions",
                    "contextWindow": window, "maxTokens": 4096,
                })
            })
            .collect();
        std::fs::write(engine.config.agent_dir.join("models.json"), json!({
            "providers": {"refresh-test": {"api": "openai-completions",
                "baseUrl": "http://127.0.0.1:9/v1", "apiKey": "configured-fallback", "models": models}}
        }).to_string()).unwrap();
    }

    #[test]
    fn refresh_metadata_defers_then_adopts_same_id_without_replacing_credentials() {
        let dir = tempfile::TempDir::new().unwrap();
        let engine = bare_engine(dir.path());
        write_models(&engine, &["selected"], 128_000);
        engine.configure_model(EngineModelSelection {
            provider: Some("refresh-test".into()),
            model: Some("selected".into()),
            ..Default::default()
        });
        let original = engine.resolve_model().unwrap();
        engine.ensure_core_session(&original).unwrap();
        let mut expected = ProviderTarget {
            model: original.clone(),
            api_key: Some("live-key".into()),
            headers: Some(std::collections::BTreeMap::from([(
                "X-Live".into(),
                "retained".into(),
            )])),
            service_tier: None,
        };
        *engine.provider_target.write().unwrap() = Some(expected.clone());
        write_models(&engine, &["other", "selected"], 256_000);
        engine.schedule_model_metadata_refresh();
        // Scheduling is safe during an active stream: the serving target has
        // not changed until the model-turn boundary explicitly applies it.
        assert_eq!(
            serde_json::to_value(engine.session_model().unwrap()).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
        engine.apply_scheduled_model_metadata_refresh().unwrap();
        expected.model = engine.resolve_model().unwrap();
        let core = engine.session.blocking_lock().clone().unwrap();
        let state = engine.runtime.block_on(core.session.agent().state());
        let expected_wire: pa_agent::types::Model =
            pa_core::session_engine::provider_adapter::json_round_trip(&expected.model).unwrap();
        assert_eq!(state.model, expected_wire);
        let actual = engine.provider_target.read().unwrap().clone().unwrap();
        assert_eq!(
            (
                actual.model,
                actual.api_key,
                actual.headers,
                actual.service_tier
            ),
            (
                expected.model,
                expected.api_key,
                expected.headers,
                expected.service_tier
            )
        );
        assert!(!engine
            .model_metadata_refresh_pending
            .load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn refresh_metadata_lazy_anchor_survives_removal_and_default_change_at_build() {
        let dir = tempfile::TempDir::new().unwrap();
        let engine = bare_engine(dir.path());
        write_models(&engine, &["selected", "replacement"], 128_000);
        let mut settings =
            pa_core::settings::SettingsManager::create(engine.cwd(), &engine.config.agent_dir);
        settings
            .set_default_model_and_provider("refresh-test", "selected")
            .unwrap();
        let original = engine.resolve_model().unwrap();
        engine.capture_model_metadata_refresh_anchor();
        assert_eq!(engine.model_metadata().unwrap()["id"], "selected");
        assert_eq!(engine.model_context_window(), Some(128_000));
        assert!(
            engine.session.blocking_lock().is_none(),
            "anchor must not build a core"
        );
        let anchor = engine.provider_target.read().unwrap().clone().unwrap();
        assert_eq!(
            (anchor.model, anchor.api_key, anchor.headers),
            (original.clone(), None, None)
        );
        write_models(&engine, &["replacement"], 256_000);
        settings
            .set_default_model_and_provider("refresh-test", "replacement")
            .unwrap();
        engine.schedule_model_metadata_refresh();
        engine.apply_scheduled_model_metadata_refresh().unwrap();
        let model = engine.session_model().unwrap();
        assert_eq!(model, original);
        assert_eq!(engine.model_metadata().unwrap()["id"], "selected");
        assert_eq!(engine.model_context_window(), Some(original.context_window));
        // This is the same lazy build seam used by run_model_turn.
        engine.ensure_core_session(&model).unwrap();
        let core = engine.session.blocking_lock().clone().unwrap();
        let state = engine.runtime.block_on(core.session.agent().state());
        let expected: pa_agent::types::Model =
            pa_core::session_engine::provider_adapter::json_round_trip(&original).unwrap();
        assert_eq!(state.model, expected);
        assert_eq!(
            engine
                .provider_target
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .api_key
                .as_deref(),
            Some("configured-fallback")
        );
    }

    #[test]
    fn refresh_metadata_lazy_anchor_yields_to_explicit_switch_and_replacement() {
        let dir = tempfile::TempDir::new().unwrap();
        let engine = bare_engine(dir.path());
        write_models(&engine, &["selected", "replacement"], 128_000);
        engine.configure_model(EngineModelSelection {
            provider: Some("refresh-test".into()),
            model: Some("selected".into()),
            ..Default::default()
        });
        engine.capture_model_metadata_refresh_anchor();
        assert!(engine.switch_model(EngineModelSelection {
            provider: Some("refresh-test".into()),
            model: Some("replacement".into()),
            ..Default::default()
        }));
        assert_eq!(engine.session_model().unwrap().id, "replacement");
        engine.runtime.block_on(engine.teardown_for_replacement());
        assert!(engine.provider_target.read().unwrap().is_none());
        assert!(engine.session.blocking_lock().is_none());
    }

    #[test]
    fn refresh_metadata_keeps_a_removed_selected_model() {
        let dir = tempfile::TempDir::new().unwrap();
        let engine = bare_engine(dir.path());
        write_models(&engine, &["selected"], 128_000);
        engine.configure_model(EngineModelSelection {
            provider: Some("refresh-test".into()),
            model: Some("selected".into()),
            ..Default::default()
        });
        let original = engine.resolve_model().unwrap();
        *engine.provider_target.write().unwrap() = Some(ProviderTarget {
            model: original.clone(),
            api_key: None,
            headers: None,
            service_tier: None,
        });
        write_models(&engine, &["replacement"], 256_000);
        engine.schedule_model_metadata_refresh();
        engine.apply_scheduled_model_metadata_refresh().unwrap();
        assert_eq!(engine.session_model().unwrap(), original);
    }
    #[test]
    fn lazy_refresh_retains_selected_identity_on_the_first_public_prompt() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::TempDir::new().unwrap();
        let engine = bare_engine(dir.path());
        let listener = engine
            .runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        engine.runtime.spawn(async move {
            let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let (header_end, content_len) = loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let len: usize = head.lines().find_map(|line| line.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                        break (end + 4, len);
                    }
                };
                while bytes.len() < header_end + content_len {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let request: serde_json::Value = serde_json::from_slice(&bytes[header_end..header_end + content_len]).unwrap();
                let body = concat!(
                    "data: {\"id\":\"refresh-test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ack\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"refresh-test\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                );
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                request
            }).await;
            let _ = request_tx.send(request);
        });
        write_models(&engine, &["selected", "replacement"], 128_000);
        let file = engine.config.agent_dir.join("models.json");
        std::fs::write(
            &file,
            std::fs::read_to_string(&file)
                .unwrap()
                .replace("http://127.0.0.1:9/v1", &base_url),
        )
        .unwrap();
        let mut settings =
            pa_core::settings::SettingsManager::create(engine.cwd(), &engine.config.agent_dir);
        settings
            .set_default_model_and_provider("refresh-test", "selected")
            .unwrap();
        engine.capture_model_metadata_refresh_anchor();
        write_models(&engine, &["replacement"], 256_000);
        std::fs::write(
            &file,
            std::fs::read_to_string(&file)
                .unwrap()
                .replace("http://127.0.0.1:9/v1", &base_url),
        )
        .unwrap();
        settings
            .set_default_model_and_provider("refresh-test", "replacement")
            .unwrap();
        engine.schedule_model_metadata_refresh();
        let mut events = Vec::new();
        engine.run_prompt(
            0,
            crate::engine::PromptRequest {
                message: "hello".into(),
                images: Vec::new(),
                source: "user".into(),
                agent_message_id: None,
                custom_message: None,
                batch: Vec::new(),
            },
            &|| false,
            &mut |event| {
                events.push(event);
                true
            },
        );
        let request = engine
            .runtime
            .block_on(request_rx)
            .unwrap()
            .expect("local provider request");
        assert_eq!(request["model"], "selected");
        assert_eq!(engine.model_metadata().unwrap()["id"], "selected");
        assert!(events
            .iter()
            .any(|event| matches!(event, crate::engine::EngineEvent::Done(Ok(())))));
    }
}
