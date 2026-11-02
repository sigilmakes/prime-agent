//! `ModelRegistry`: composes built-in, custom (models.json), and Prime Inference
//! catalogs; resolves request auth per provider/model.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use pa_types::ai::{
    CompatKind, Model, ModelCompat, ModelThinkingLevel, OpenAiResponsesCompat, ThinkingLevelMap,
};

use crate::auth::manager::AuthStorage;
use crate::auth::types::AuthCredential;

mod catalog_refresh;
mod catalog_sources;

use super::catalog_chain;
use super::custom::{apply_model_override, load_custom_models, merge_compat, CustomModelsResult};
use super::prime_inference::is_private_prime_inference_model;

/// Request-auth bits a provider can configure in models.json.
#[derive(Debug, Clone, Default)]
pub struct ProviderRequestConfig {
    pub api_key: Option<String>,
    /// Ordered (`BTreeMap`): merged into request-header maps that
    /// providers iterate deterministically.
    pub headers: Option<BTreeMap<String, String>>,
    pub auth_header: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRequestAuth {
    pub ok: bool,
    pub api_key: Option<String>,
    /// Ordered (`BTreeMap`): unordered iteration would order the
    /// composed request headers randomly.
    pub headers: Option<BTreeMap<String, String>>,
    pub error: Option<String>,
}

/// Why a `set_model` selection failed to resolve: the provider is not
/// signed in (the client offers the sign-in flow and retries) or the model
/// is not in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetModelSelectionError {
    /// The model exists, but its provider has no credential and none is
    /// stale: sign in, then retry the switch.
    ProviderUnauthenticated { provider: String },
    /// No such model in the catalog.
    NotFound { provider: String, model_id: String },
}

impl SetModelSelectionError {
    /// The refused provider of the sign-in variant; `None` on not-found.
    #[must_use]
    pub fn unauthenticated_provider(&self) -> Option<&str> {
        match self {
            SetModelSelectionError::ProviderUnauthenticated { provider } => Some(provider),
            SetModelSelectionError::NotFound { .. } => None,
        }
    }
}

impl std::fmt::Display for SetModelSelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetModelSelectionError::ProviderUnauthenticated { provider } => write!(
                f,
                "Provider \"{provider}\" is not signed in. Sign in to the provider (the TUI's /login command), then set the model again."
            ),
            SetModelSelectionError::NotFound { provider, model_id } => {
                write!(f, "Model not found: {provider}/{model_id}")
            }
        }
    }
}

impl std::error::Error for SetModelSelectionError {}

/// Composed model catalog with auth-aware availability.
pub struct ModelRegistry {
    pub auth: AuthStorage,
    models_json_path: Option<PathBuf>,
    models: Vec<Model>,
    load_error: Option<String>,
    provider_request_configs: HashMap<String, ProviderRequestConfig>,
    model_request_headers: HashMap<String, BTreeMap<String, String>>,
    explicit_private_ids: HashSet<String>,
    authorized_private_ids: HashSet<String>,
    authorized_private_models: Vec<Model>,
    preferred_default: Option<String>,
    unresolved_catalog_sources: Vec<String>,
    /// The process-shared live catalog chain (`catalog_chain::catalog_for`);
    /// `resolve()` sources built-ins from it.
    catalog: std::sync::Arc<pa_models::ModelCatalog>,
}

/// Under a stored xAI subscription the provider's models switch onto the
/// openai-responses API with the subscription thinking maps and
/// `supportsLongCacheRetention: false` compat.
fn xai_subscription_model(model: &Model) -> Model {
    let mut adapted = model.clone();
    adapted.api = "openai-responses".to_string();
    adapted.base_url = "https://api.x.ai/v1".to_string();
    // TS fills only an absent map: a model that already declares its
    // levels keeps them.
    if adapted.thinking_level_map.is_none() {
        adapted.thinking_level_map = Some(xai_subscription_thinking_map(&model.id));
    }
    adapted.compat = Some(ModelCompat::from_kind(CompatKind::OpenAiResponses(
        OpenAiResponsesCompat {
            send_session_id_header: None,
            supports_long_cache_retention: Some(false),
        },
    )));
    adapted
}

/// The TS flow's explicit thinking maps for the grok models it names,
/// and the all-levels-null default for every other xAI model (reasoning
/// stays; unverified effort controls never send).
fn xai_subscription_thinking_map(model_id: &str) -> ThinkingLevelMap {
    // The value mirrors the TS table: `None` for the null mapping
    // (unsupported), the wire string for a named level; an absent level
    // keeps its default support.
    let null = || None;
    let value = |text: &str| Some(text.to_string());
    let mut map = ThinkingLevelMap::new();
    match model_id {
        "grok-4.3" => {
            map.insert(ModelThinkingLevel::Off, value("none"));
            map.insert(ModelThinkingLevel::Minimal, null());
        }
        "grok-4.5" => {
            map.insert(ModelThinkingLevel::Off, null());
            map.insert(ModelThinkingLevel::Minimal, null());
        }
        "grok-4.6" | "grok-4.7" => {
            map.insert(ModelThinkingLevel::Off, null());
            map.insert(ModelThinkingLevel::Minimal, null());
            map.insert(ModelThinkingLevel::Xhigh, value("xhigh"));
        }
        _ => {
            for level in [
                ModelThinkingLevel::Off,
                ModelThinkingLevel::Minimal,
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
                ModelThinkingLevel::Xhigh,
                ModelThinkingLevel::Max,
            ] {
                map.insert(level, null());
            }
        }
    }
    map
}

impl ModelRegistry {
    pub fn create(auth: AuthStorage, models_json_path: impl Into<PathBuf>) -> Self {
        Self::new(auth, Some(models_json_path.into()))
    }

    pub fn in_memory(auth: AuthStorage) -> Self {
        Self::new(auth, None)
    }

    fn new(auth: AuthStorage, models_json_path: Option<PathBuf>) -> Self {
        let catalog = catalog_chain::catalog_for(models_json_path.as_deref());
        Self::with_catalog(auth, models_json_path, catalog)
    }

    /// Inject the catalog owner for hermetic endpoints and isolated tests.
    pub fn with_catalog(
        auth: AuthStorage,
        models_json_path: Option<PathBuf>,
        catalog: std::sync::Arc<pa_models::ModelCatalog>,
    ) -> Self {
        let mut registry = Self {
            auth,
            models_json_path,
            models: Vec::new(),
            load_error: None,
            provider_request_configs: HashMap::new(),
            model_request_headers: HashMap::new(),
            explicit_private_ids: HashSet::new(),
            authorized_private_ids: HashSet::new(),
            authorized_private_models: Vec::new(),
            preferred_default: None,
            unresolved_catalog_sources: Vec::new(),
            catalog,
        };
        registry.load_models();
        registry
    }

    pub fn get_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Built-in + custom models (auth not filtered).
    pub fn get_all(&self) -> &[Model] {
        &self.models
    }

    /// Models whose provider has configured auth, with unauthorized private
    /// Prime Inference models gated out. The per-model auth probe is answered
    /// once per provider: each probe rebuilds the auth-source candidates.
    pub fn get_available(&self) -> Vec<&Model> {
        let mut auth_by_provider: HashMap<&str, bool> = HashMap::new();
        self.models
            .iter()
            .filter(|model| {
                (!is_private_prime_inference_model(model)
                    || self.is_authorized_private_model(model))
                    && *auth_by_provider
                        .entry(model.provider.as_str())
                        .or_insert_with(|| self.has_configured_auth(model))
            })
            .collect()
    }

    /// Models `rlm.find_models` may search: auth-configured, and not on a
    /// stale or expired provider credential (probe answered once per provider).
    pub fn get_rlm_searchable_models(&self) -> Vec<&Model> {
        let mut status_by_provider: HashMap<&str, bool> = HashMap::new();
        self.get_available()
            .into_iter()
            .filter(|model| {
                *status_by_provider
                    .entry(model.provider.as_str())
                    .or_insert_with(|| {
                        let status = self.auth.get_auth_status_local(&model.provider);
                        status.source != Some(crate::auth::types::AuthSource::Stale)
                            && status.label.as_deref() != Some("expired")
                    })
            })
            .collect()
    }

    pub fn has_configured_auth(&self, model: &Model) -> bool {
        self.auth.has_auth(&model.provider)
            || self.has_configured_provider_request_auth(&model.provider)
    }

    /// The provider's auth status without credential values: callers
    /// classifying unauthenticated vs stale providers read this instead of
    /// probing for keys.
    pub fn get_provider_auth_status(&self, provider: &str) -> crate::auth::types::AuthStatus {
        use crate::auth::types::{AuthSource, AuthStatus};
        let status = self.auth.get_auth_status_local(provider);
        if status.configured && status.source != Some(AuthSource::Stale) {
            return status;
        }
        let Some(config) = self.provider_request_configs.get(provider) else {
            return status;
        };
        if !self.has_configured_provider_request_auth(provider) {
            return status;
        }
        let (source, label) = match config.api_key.as_deref() {
            Some(key) if key.starts_with('!') => (AuthSource::ModelsJsonCommand, None),
            Some(key) if std::env::var(key).is_ok_and(|value| !value.is_empty()) => {
                (AuthSource::Environment, Some(key.to_string()))
            }
            Some(_) | None => (AuthSource::ModelsJsonKey, None),
        };
        AuthStatus {
            configured: true,
            source: Some(source),
            label,
        }
    }

    /// `set_model`'s model resolution: an available model resolves directly; a
    /// model whose provider has no credential (none stale) is the typed sign-in
    /// refusal; stale-auth providers keep the switch (the lookup never mutates
    /// stale state).
    ///
    /// # Errors
    ///
    /// `ProviderUnauthenticated` when the provider has no credential;
    /// `NotFound` when no catalog model matches.
    pub fn resolve_set_model_selection(
        &self,
        provider: &str,
        model_id: &str,
    ) -> Result<&Model, SetModelSelectionError> {
        let matches = |model: &Model| model.provider == provider && model.id == model_id;
        if let Some(model) = self
            .get_available()
            .into_iter()
            .find(|model| matches(model))
        {
            return Ok(model);
        }
        // A model can exist while its provider is not signed in — the
        // picker's discovery path. No credential at all is the sign-in refusal;
        // the unauthorized private Prime Inference model keeps the not-found
        // refusal — never a switch to an unentitled model.
        let Some(model) = self.get_all().iter().find(|model| matches(model)) else {
            return Err(SetModelSelectionError::NotFound {
                provider: provider.to_string(),
                model_id: model_id.to_string(),
            });
        };
        if self.get_provider_auth_status(provider).source
            == Some(crate::auth::types::AuthSource::Stale)
        {
            return Ok(model);
        }
        if !self.has_configured_auth(model) {
            return Err(SetModelSelectionError::ProviderUnauthenticated {
                provider: provider.to_string(),
            });
        }
        Err(SetModelSelectionError::NotFound {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        })
    }

    fn has_configured_provider_request_auth(&self, provider: &str) -> bool {
        let Some(config) = self.provider_request_configs.get(provider) else {
            return false;
        };
        if config.headers.is_some() || config.auth_header.is_some() {
            return true;
        }
        config.api_key.as_deref().is_some_and(|key| !key.is_empty())
    }

    /// Reload local validated metadata only. Never renews OAuth or runs config commands.
    pub fn load_private_authorization_from_cache(&mut self) {
        self.load_models();
    }

    pub fn refresh(&mut self) {
        self.auth.reload();
        self.load_models();
    }

    fn prepare_catalog(
        &mut self,
        local_only: bool,
    ) -> (CustomModelsResult, pa_models::CatalogInputs) {
        self.provider_request_configs.clear();
        self.model_request_headers.clear();
        self.load_error = None;
        let result = self
            .models_json_path
            .clone()
            .map_or_else(CustomModelsResult::default, |path| {
                self.load_custom_models_file(&path)
            });
        self.load_error.clone_from(&result.error);
        let inputs = self.catalog_inputs(&result, local_only);
        (result, inputs)
    }

    fn load_models(&mut self) {
        let (custom, inputs) = self.prepare_catalog(true);
        let snapshot = self.catalog.snapshot(&inputs);
        self.accept_catalog_snapshot(custom, snapshot);
    }

    fn accept_catalog_snapshot(
        &mut self,
        custom: CustomModelsResult,
        snapshot: pa_models::CatalogSnapshot,
    ) {
        self.explicit_private_ids = custom
            .models
            .iter()
            .filter(|model| is_private_prime_inference_model(model))
            .map(|model| model.id.clone())
            .collect();
        self.authorized_private_models = snapshot.authorized_private_prime_models;
        self.authorized_private_ids = self
            .authorized_private_models
            .iter()
            .map(|model| model.id.clone())
            .collect();
        self.preferred_default = snapshot.preferred_default;
        let mut models = snapshot.models;
        for private in super::private_auth::get_private_prime_inference_models() {
            if !models
                .iter()
                .any(|model| model.provider == private.provider && model.id == private.id)
            {
                models.push(private);
            }
        }
        // Explicit local definitions and overrides are the final authority, never remote metadata.
        for model in &mut models {
            if let Some(over) = custom.provider_overrides.get(&model.provider) {
                if let Some(base_url) = &over.base_url {
                    model.base_url.clone_from(base_url);
                }
                if over.compat.is_some() {
                    model.compat = merge_compat(model.compat.as_ref(), over.compat.clone());
                }
            }
            if let Some(over) = custom
                .model_overrides
                .get(&model.provider)
                .and_then(|over| over.get(&model.id))
            {
                *model = apply_model_override(model, over);
            }
        }
        self.models = Self::merge_custom_models(models, custom.models);
        self.apply_subscription_model_adaptations();
    }

    /// Per-model base-URL/API overrides plus the xAI subscription switch,
    /// applied at load: stored credentials rewrite the affected models, and
    /// the registry reloads after every credential change.
    fn apply_subscription_model_adaptations(&mut self) {
        let copilot_credential = self
            .auth
            .get_all()
            .credential(crate::auth::GITHUB_COPILOT_PROVIDER_ID);
        if let Some(AuthCredential::Oauth {
            access,
            enterprise_url,
            ..
        }) = copilot_credential
        {
            let base_url =
                pa_ai::oauth::get_github_copilot_base_url(Some(&access), enterprise_url.as_deref());
            for model in &mut self.models {
                if model.provider == crate::auth::GITHUB_COPILOT_PROVIDER_ID {
                    model.base_url.clone_from(&base_url);
                }
            }
        }
        // The stored credential is OAuth and the store is the winning
        // source (a stale credential does not serve it).
        let xai_stored = matches!(
            self.auth.get_all().credential(crate::auth::XAI_PROVIDER_ID),
            Some(AuthCredential::Oauth { .. })
        ) && self
            .auth
            .get_auth_status_local(crate::auth::XAI_PROVIDER_ID)
            .source
            == Some(crate::auth::types::AuthSource::Stored);
        if xai_stored {
            for model in &mut self.models {
                if model.provider == crate::auth::XAI_PROVIDER_ID {
                    *model = xai_subscription_model(model);
                }
            }
        }
    }

    fn load_custom_models_file(&mut self, path: &Path) -> CustomModelsResult {
        if !path.exists() {
            return CustomModelsResult::default();
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return CustomModelsResult {
                error: Some(format!(
                    "Failed to load models.json\n\nFile: {}",
                    path.display()
                )),
                ..Default::default()
            };
        };
        let mut result = load_custom_models(
            &content,
            &|provider| {
                provider == "ollama-cloud"
                    || !pa_ai::models_generated::get_models(provider).is_empty()
            },
            &|provider| {
                if provider == "ollama-cloud" {
                    Some((
                        "openai-completions".to_string(),
                        format!("{}/v1", self.catalog.ollama_base_url()),
                    ))
                } else {
                    pa_ai::models_generated::get_models(provider)
                        .first()
                        .map(|model| (model.api.clone(), model.base_url.clone()))
                }
            },
        );
        for (provider, provider_config) in &result.providers {
            self.provider_request_configs.insert(
                provider.clone(),
                ProviderRequestConfig {
                    api_key: provider_config.api_key.clone(),
                    headers: provider_config.headers.clone(),
                    auth_header: provider_config.auth_header,
                },
            );
            if let Some(overrides) = &provider_config.model_overrides {
                for (id, over) in overrides {
                    self.store_model_headers(provider, id, over.headers.clone());
                }
            }
            if let Some(models) = &provider_config.models {
                for model in models {
                    self.store_model_headers(provider, &model.id, model.headers.clone());
                }
            }
        }
        if let Some(error) = &result.error {
            // Keep built-ins only; message mirrors the TS load failure format.
            if !error.contains("models.json") {
                result.error = Some(format!("{error}\n\nFile: {}", path.display()));
            }
        }
        result
    }

    fn store_model_headers(
        &mut self,
        provider: &str,
        model_id: &str,
        headers: Option<BTreeMap<String, String>>,
    ) {
        let key = format!("{provider}:{model_id}");
        match headers {
            Some(headers) if !headers.is_empty() => {
                self.model_request_headers.insert(key, headers);
            }
            _ => {
                self.model_request_headers.remove(&key);
            }
        }
    }

    /// Custom models win on provider+id conflicts.
    fn merge_custom_models(mut built_in: Vec<Model>, custom: Vec<Model>) -> Vec<Model> {
        for custom_model in custom {
            match built_in.iter().position(|model| {
                model.provider == custom_model.provider && model.id == custom_model.id
            }) {
                Some(index) => built_in[index] = custom_model,
                None => built_in.push(custom_model),
            }
        }
        built_in
    }

    /// Reload local state and refresh entitlements (live catalog + private
    /// auth). Both fetches are awaited, so the resolved catalog reflects
    /// them when the call returns.
    pub async fn refresh_available_models(&mut self) -> Vec<Model> {
        self.refresh_available_models_forced(false).await
    }

    /// [`ModelRegistry::refresh_available_models`] with the refresh trigger's
    /// gating: forced triggers (startup, auth change) skip the hourly catalog
    /// gate, picker-open keeps it; private authorization keeps its own gating.
    pub async fn refresh_available_models_with_trigger(
        &mut self,
        trigger: pa_models::RefreshTrigger,
    ) -> Vec<Model> {
        self.refresh_available_models_forced(trigger.forced()).await
    }

    async fn refresh_available_models_forced(&mut self, force: bool) -> Vec<Model> {
        let policy = if force {
            pa_models::RefreshPolicy::Force
        } else {
            pa_models::RefreshPolicy::IfStale
        };
        self.update_catalog(policy, None).await;
        self.get_available().into_iter().cloned().collect()
    }

    fn is_authorized_private_model(&self, model: &Model) -> bool {
        self.explicit_private_ids.contains(&model.id)
            || self.authorized_private_ids.contains(&model.id)
    }

    /// `assumeAuthConfigured` validates an explicit stale-provider selection.
    pub async fn can_use_model(&mut self, model: &Model, assume_auth_configured: bool) -> bool {
        if assume_auth_configured {
            return !is_private_prime_inference_model(model)
                || self.is_authorized_private_model(model);
        }
        if !self.has_configured_auth(model) {
            return false;
        }
        if !is_private_prime_inference_model(model) {
            return true;
        }
        let available = self.refresh_available_models().await;
        available
            .iter()
            .any(|candidate| candidate.provider == model.provider && candidate.id == model.id)
    }

    /// Resolve request auth: API key + merged headers for a model.
    pub fn get_api_key_and_headers(
        &mut self,
        model: &Model,
        request_headers: Option<&BTreeMap<String, String>>,
    ) -> ResolvedRequestAuth {
        let stored = self
            .auth
            .get_api_key_with_source_token(&model.provider, false);
        let mut api_key = stored.api_key;
        let provider_config = self.provider_request_configs.get(&model.provider).cloned();
        if api_key.is_none() {
            if let Some(config) = &provider_config {
                if let Some(configured) = &config.api_key {
                    if let Some(resolved) =
                        crate::auth::resolve_config_value::resolve_config_value(configured)
                    {
                        api_key = Some(resolved);
                    }
                }
            }
        }
        let provider_headers = provider_config
            .as_ref()
            .and_then(|config| config.headers.clone());
        let auth_storage_headers = self.auth.get_provider_headers(&model.provider);
        let model_request_key = format!("{}:{}", model.provider, model.id);
        let model_request_headers = self.model_request_headers.get(&model_request_key).cloned();

        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(model_headers) = &model.headers {
            headers.extend(model_headers.clone());
        }
        if let Some(auth_storage_headers) = auth_storage_headers {
            headers.extend(auth_storage_headers);
        }
        if let Some(provider_headers) = provider_headers {
            headers.extend(provider_headers);
        }
        if let Some(model_request_headers) = model_request_headers {
            headers.extend(model_request_headers);
        }
        if provider_config
            .as_ref()
            .and_then(|config| config.auth_header)
            .unwrap_or(false)
        {
            let Some(api_key) = &api_key else {
                return ResolvedRequestAuth {
                    ok: false,
                    error: Some(format!("No API key found for \"{}\"", model.provider)),
                    ..Default::default()
                };
            };
            headers.insert("Authorization".to_string(), format!("Bearer {api_key}"));
        }
        if let Some(request_headers) = request_headers {
            headers.extend(request_headers.clone());
        }
        ResolvedRequestAuth {
            ok: true,
            api_key,
            headers: (!headers.is_empty()).then_some(headers),
            error: None,
        }
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests;
