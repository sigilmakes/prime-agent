//! Resolve locally authorized discovery requests. Wire adapters and caches live in pa-models.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pa_models::{CatalogFormat, CatalogInputs, ResolvedSource, UnknownModelPolicy};
use pa_types::ai::Model;

use super::{ModelRegistry, ProviderRequestConfig};
use crate::models::custom::{
    self, CustomModelsResult, DiscoveryConfig, DiscoveryFormat, ModelCostConfig, ModelDefinition,
    ProviderConfig, ProviderLimits,
};
use crate::plugins::ProviderFact;

fn builtin_format(provider: &str) -> Option<CatalogFormat> {
    Some(match provider {
        "openai" => CatalogFormat::OpenAi,
        "openrouter" => CatalogFormat::OpenRouter,
        "anthropic" => CatalogFormat::Anthropic,
        "google" => CatalogFormat::Google,
        "openai-codex" => CatalogFormat::Codex,
        "ollama-cloud" => CatalogFormat::Ollama,
        "prime-inference" => CatalogFormat::PrimeInference,
        _ => return None,
    })
}

/// The seven compiled provider ids may not be redefined by a plugin fact.
pub(super) fn is_builtin_provider(name: &str) -> bool {
    crate::models::BUILTIN_PROVIDER_IDS.contains(&name)
}

/// A fact protocol names both the wire API and the discovery shape.
fn protocol_route(protocol: &str) -> Option<(&'static str, DiscoveryFormat)> {
    Some(match protocol.to_ascii_lowercase().as_str() {
        "openai" | "openai-completions" => ("openai-completions", DiscoveryFormat::Openai),
        "openrouter" => ("openai-completions", DiscoveryFormat::Openrouter),
        "anthropic" | "anthropic-messages" => ("anthropic-messages", DiscoveryFormat::Anthropic),
        "google" | "google-generative-ai" => ("google-generative-ai", DiscoveryFormat::Google),
        "ollama" => ("openai-completions", DiscoveryFormat::Ollama),
        "openai-responses" => ("openai-responses", DiscoveryFormat::Openai),
        _ => return None,
    })
}

fn append_error(result: &mut CustomModelsResult, message: String) {
    match &mut result.error {
        Some(error) => {
            error.push('\n');
            error.push_str(&message);
        }
        None => result.error = Some(message),
    }
}

fn fact_limits(fact: &ProviderFact) -> Result<Option<ProviderLimits>, String> {
    fact.limits
        .as_ref()
        .map(|value| {
            serde_json::from_value::<ProviderLimits>(value.clone()).map_err(|error| {
                format!(
                    "Provider fact \"{}\" from plugin \"{}\": invalid limits: {error}",
                    fact.name, fact.plugin
                )
            })
        })
        .transpose()
}

fn fact_headers(fact: &ProviderFact) -> Result<Option<BTreeMap<String, String>>, String> {
    fact.headers
        .as_ref()
        .map(|value| {
            serde_json::from_value::<BTreeMap<String, String>>(value.clone()).map_err(|error| {
                format!(
                    "Provider fact \"{}\" from plugin \"{}\": invalid headers: {error}",
                    fact.name, fact.plugin
                )
            })
        })
        .transpose()
}

/// Fill an unknown-id definition from the fact, keeping every value the fact
/// declared. The defaults are the model shape every discovered id inherits;
/// limits cap them after projection.
fn fact_default_definition(
    fact: &ProviderFact,
    limits: Option<ProviderLimits>,
) -> Result<ModelDefinition, String> {
    let mut definition = fact
        .defaults
        .as_ref()
        .map(|value| serde_json::from_value::<ModelDefinition>(value.clone()))
        .transpose()
        .map_err(|error| {
            format!(
                "Provider fact \"{}\" from plugin \"{}\": invalid defaults: {error}",
                fact.name, fact.plugin
            )
        })?
        .unwrap_or_default();
    definition.id = String::new();
    definition.context_window = Some(
        definition
            .context_window
            .or_else(|| limits.and_then(|limits| limits.context))
            .unwrap_or(32_768),
    );
    definition.max_tokens = Some(
        definition
            .max_tokens
            .or_else(|| limits.and_then(|limits| limits.max_tokens))
            .unwrap_or(8_192),
    );
    definition
        .input
        .get_or_insert_with(|| vec!["text".to_string()]);
    definition.reasoning.get_or_insert(false);
    definition.cost.get_or_insert_with(ModelCostConfig::default);
    // Discovery defaults describe model limits, never a transport.
    definition.api = None;
    definition.base_url = None;
    definition.headers = None;
    Ok(definition)
}

fn fact_models(
    fact: &ProviderFact,
    defaults: &ModelDefinition,
) -> Result<Option<Vec<ModelDefinition>>, String> {
    let Some(value) = fact.extra.get("models") else {
        return Ok(None);
    };
    let models =
        serde_json::from_value::<Vec<ModelDefinition>>(value.clone()).map_err(|error| {
            format!(
                "Provider fact \"{}\" from plugin \"{}\": invalid models: {error}",
                fact.name, fact.plugin
            )
        })?;
    if models.is_empty() {
        return Ok(None);
    }
    let mut filled = Vec::with_capacity(models.len());
    for mut model in models {
        if model.id.is_empty() {
            return Err(format!(
                "Provider fact \"{}\" from plugin \"{}\": a model needs an id",
                fact.name, fact.plugin
            ));
        }
        model.context_window = model.context_window.or(defaults.context_window);
        model.max_tokens = model.max_tokens.or(defaults.max_tokens);
        model.reasoning = model.reasoning.or(defaults.reasoning);
        model.input = model.input.or_else(|| defaults.input.clone());
        model.cost = model.cost.or_else(|| defaults.cost.clone());
        filled.push(model);
    }
    Ok(Some(filled))
}

/// One pure fact as the registry's provider shape: url is the transport base,
/// protocol names api plus discovery format, and the fact limits ride along so
/// the catalogue projection can clamp them.
fn provider_config_from_fact(fact: &ProviderFact) -> Result<ProviderConfig, String> {
    let url = fact
        .url
        .as_deref()
        .filter(|url| !url.is_empty())
        .ok_or_else(|| {
            format!(
                "Provider fact \"{}\" from plugin \"{}\": url is required",
                fact.name, fact.plugin
            )
        })?;
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        format!(
            "Provider fact \"{}\" from plugin \"{}\": invalid url: {error}",
            fact.name, fact.plugin
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(format!(
            "Provider fact \"{}\" from plugin \"{}\": url must be an http(s) base without credentials or a fragment",
            fact.name, fact.plugin
        ));
    }
    let protocol = fact.protocol.as_deref().ok_or_else(|| {
        format!(
            "Provider fact \"{}\" from plugin \"{}\": protocol is required",
            fact.name, fact.plugin
        )
    })?;
    let (api, format) = protocol_route(protocol).ok_or_else(|| {
        format!(
            "Provider fact \"{}\" from plugin \"{}\": unsupported protocol \"{protocol}\"",
            fact.name, fact.plugin
        )
    })?;
    let (keyless, api_key) = match &fact.auth {
        None => (false, None),
        Some(serde_json::Value::Null) => (true, None),
        Some(serde_json::Value::String(value)) if value.eq_ignore_ascii_case("none") => {
            (true, None)
        }
        Some(serde_json::Value::String(value)) if !value.is_empty() => (false, Some(value.clone())),
        Some(serde_json::Value::String(_)) => {
            return Err(format!(
                "Provider fact \"{}\" from plugin \"{}\": auth must not be empty",
                fact.name, fact.plugin
            ))
        }
        Some(_) => {
            return Err(format!(
                "Provider fact \"{}\" from plugin \"{}\": auth must be a string or null",
                fact.name, fact.plugin
            ))
        }
    };
    let limits = fact_limits(fact)?;
    let headers = fact_headers(fact)?;
    let defaults = fact_default_definition(fact, limits)?;
    let models = fact_models(fact, &defaults)?;
    Ok(ProviderConfig {
        name: Some(fact.name.clone()),
        base_url: Some(url.to_string()),
        api_key,
        api: Some(api.to_string()),
        headers,
        compat: keyless.then(|| serde_json::json!({"sendAuthorization": false})),
        auth_header: None,
        models,
        model_overrides: None,
        discovery: Some(DiscoveryConfig {
            format,
            url: None,
            api_key: None,
            headers: None,
            defaults: Some(defaults),
        }),
        keyless,
        limits,
    })
}

fn resolve_value(value: &str, local_only: bool) -> Option<String> {
    if local_only && value.starts_with('!') {
        return None;
    }
    crate::auth::resolve_config_value::resolve_config_value(value)
}

fn same_origin(left: &str, right: &str) -> bool {
    reqwest::Url::parse(left)
        .ok()
        .zip(reqwest::Url::parse(right).ok())
        .is_some_and(|(left, right)| left.origin() == right.origin())
}

fn account_id(token: &str) -> Option<String> {
    use base64::Engine as _;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.split('.').nth(1)?)
        .ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    payload
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

impl ModelRegistry {
    /// Fold the session-free plugin facts into the models.json result. Facts
    /// and models.json providers share one map, so request configs, auth
    /// predicates, discovery, and model projection all read the same provider.
    pub(super) fn merge_provider_facts(&self, result: &mut CustomModelsResult) -> BTreeSet<String> {
        let mut materialized = BTreeSet::new();
        for fact in &self.provider_facts {
            if is_builtin_provider(&fact.name) {
                append_error(
                    result,
                    format!(
                        "Provider fact \"{}\" from plugin \"{}\" collides with a builtin provider id",
                        fact.name, fact.plugin
                    ),
                );
                continue;
            }
            let config = match provider_config_from_fact(fact) {
                Ok(config) => config,
                Err(error) => {
                    append_error(result, error);
                    continue;
                }
            };
            let models = config.models.clone().unwrap_or_default();
            result.providers.insert(fact.name.clone(), config.clone());
            materialized.insert(fact.name.clone());
            for definition in &models {
                if let Some(model) =
                    custom::model_from_definition(&fact.name, &config, definition, None)
                {
                    match result.models.iter_mut().find(|existing| {
                        existing.provider == model.provider && existing.id == model.id
                    }) {
                        Some(existing) => *existing = model,
                        None => result.models.push(model),
                    }
                }
            }
        }
        materialized
    }

    /// Install request-auth facts for every provider in the composed result.
    pub(super) fn install_provider_request_configs(
        &mut self,
        providers: &BTreeMap<String, ProviderConfig>,
    ) {
        self.provider_request_configs.clear();
        for (provider, config) in providers {
            self.provider_request_configs.insert(
                provider.clone(),
                ProviderRequestConfig {
                    api_key: config.api_key.clone(),
                    headers: config.headers.clone(),
                    auth_header: config.auth_header,
                    keyless: config.keyless,
                },
            );
            if let Some(overrides) = &config.model_overrides {
                for (id, over) in overrides {
                    self.store_model_headers(provider, id, over.headers.clone());
                }
            }
            if let Some(models) = &config.models {
                for model in models {
                    self.store_model_headers(provider, &model.id, model.headers.clone());
                }
            }
        }
    }

    pub(super) fn catalog_inputs(
        &mut self,
        custom: &CustomModelsResult,
        local_only: bool,
    ) -> CatalogInputs {
        let mut providers: BTreeSet<String> = crate::models::BUILTIN_PROVIDER_IDS
            .into_iter()
            .map(str::to_string)
            .collect();
        providers.extend(custom.providers.keys().cloned());
        let mut seeds = crate::models::private_auth::get_private_prime_inference_models();
        seeds.extend(custom.models.clone());
        let mut inputs = CatalogInputs {
            sources: Vec::new(),
            seeds,
        };
        let requested: Vec<_> = providers
            .iter()
            .flat_map(|provider| {
                if provider == "prime-inference" {
                    vec![
                        "prime-inference-public".to_string(),
                        "prime-inference-private".to_string(),
                    ]
                } else if builtin_format(provider).is_some()
                    || custom
                        .providers
                        .get(provider)
                        .is_some_and(|config| config.discovery.is_some())
                {
                    vec![provider.clone()]
                } else {
                    Vec::new()
                }
            })
            .collect();
        for provider in providers {
            let config = custom.providers.get(&provider).cloned().unwrap_or_default();
            let Some(format) = config
                .discovery
                .as_ref()
                .map(|discovery| discovery.format.catalog_format())
                .or_else(|| builtin_format(&provider))
            else {
                continue;
            };
            let template = self.source_template(&provider, &config, format);
            let base = config
                .base_url
                .clone()
                .or_else(|| template.as_ref().map(|model| model.base_url.clone()));
            let Some(base) = base else { continue };
            let explicit_url = config
                .discovery
                .as_ref()
                .and_then(|discovery| discovery.url.clone());
            let endpoint = explicit_url
                .clone()
                .or_else(|| {
                    if config.base_url.is_none() {
                        self.catalog
                            .discovery_url_override(&provider)
                            .map(str::to_string)
                    } else {
                        None
                    }
                })
                .or_else(|| format.discovery_url(&base));
            let Some(url) = endpoint else { continue };
            let cross_origin = !same_origin(&url, &base);
            // An explicitly injected hermetic URL is already locally authorized. In normal
            // configuration a different origin must have its own explicit discovery auth.
            let injected = explicit_url.is_none()
                && config.base_url.is_none()
                && self.catalog.discovery_url_override(&provider).is_some();
            let discovery = config.discovery.as_ref();
            let explicit_auth = discovery.is_some_and(|discovery| {
                discovery.api_key.is_some() || discovery.headers.is_some()
            });
            if cross_origin && !injected && !explicit_auth {
                self.load_error = Some(format!(
                    "Provider {provider}: cross-origin discovery requires explicit discovery auth"
                ));
                continue;
            }
            let inherit = !cross_origin || injected;
            let key_config = discovery.and_then(|discovery| discovery.api_key.as_deref());
            let protocol_auth_header = match format {
                CatalogFormat::Anthropic => "x-api-key",
                CatalogFormat::Google => "x-goog-api-key",
                _ => "authorization",
            };
            let explicit_auth_header = discovery
                .and_then(|discovery| discovery.headers.as_ref())
                .is_some_and(|headers| {
                    headers
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case(protocol_auth_header))
                });
            let key = if config.keyless || explicit_auth_header {
                None
            } else if let Some(key) = key_config {
                resolve_value(key, local_only)
            } else if inherit {
                let stored = if local_only {
                    self.auth.get_api_key_local(&provider)
                } else {
                    self.auth
                        .get_api_key_with_source_token(&provider, false)
                        .api_key
                };
                if stored.is_some() || self.auth.get_auth_status_local(&provider).source.is_some() {
                    stored
                } else {
                    config
                        .api_key
                        .as_deref()
                        .and_then(|key| resolve_value(key, local_only))
                }
            } else {
                None
            };
            if !config.keyless
                && inherit
                && key_config.is_none()
                && !explicit_auth_header
                && key.is_none()
                && self.auth.get_auth_status_local(&provider).source.is_some()
            {
                continue;
            }
            let mut configured_headers = BTreeMap::new();
            let mut merge_headers = |values: &BTreeMap<String, String>| {
                for (name, value) in values {
                    configured_headers.insert(name.to_ascii_lowercase(), value.clone());
                }
            };
            if inherit {
                if !config.keyless {
                    if let Some(headers) = self.auth.get_provider_headers(&provider) {
                        merge_headers(&headers.into_iter().collect());
                    }
                }
                if let Some(headers) = &config.headers {
                    merge_headers(headers);
                }
            }
            let auth_header = match format {
                CatalogFormat::Anthropic => "x-api-key",
                CatalogFormat::Google => "x-goog-api-key",
                _ => "authorization",
            };
            if key_config.is_some() {
                configured_headers.remove(auth_header);
            }
            if let Some(values) = discovery.and_then(|discovery| discovery.headers.as_ref()) {
                for (name, value) in values {
                    configured_headers.insert(name.to_ascii_lowercase(), value.clone());
                }
            }
            let mut headers = BTreeMap::new();
            let mut unresolved = false;
            for (name, value) in configured_headers {
                if let Some(value) = resolve_value(&value, local_only) {
                    headers.insert(name, value);
                } else {
                    unresolved = true;
                }
            }
            if unresolved || (key_config.is_some() && key.is_none() && !explicit_auth_header) {
                continue;
            }
            if let Some(key) = &key {
                let value = if matches!(format, CatalogFormat::Anthropic | CatalogFormat::Google) {
                    key.clone()
                } else {
                    format!("Bearer {key}")
                };
                // Explicit discovery headers have the final word over a generated API key.
                headers.entry(auth_header.into()).or_insert(value);
            }
            if headers.is_empty()
                && !matches!(
                    format,
                    CatalogFormat::OpenRouter | CatalogFormat::PrimeInference
                )
                && config.discovery.is_none()
            {
                continue;
            }
            if format == CatalogFormat::Anthropic {
                headers
                    .entry("anthropic-version".into())
                    .or_insert_with(|| "2023-06-01".into());
            }
            if format == CatalogFormat::Codex {
                if let Some(account) = key.as_deref().and_then(account_id) {
                    headers
                        .entry("chatgpt-account-id".into())
                        .or_insert(account);
                }
            }
            let guard = Some(self.source_publication_guard(&provider, &config));
            let unknown_models = if let Some(defaults) =
                discovery.and_then(|discovery| discovery.defaults.as_ref())
            {
                custom::model_from_definition(
                    &provider,
                    &config,
                    defaults,
                    template
                        .as_ref()
                        .map(|model| (model.api.clone(), model.base_url.clone())),
                )
                .map_or(UnknownModelPolicy::KnownOnly, |model| {
                    UnknownModelPolicy::ConfiguredDefaults { model }
                })
            } else if builtin_format(&provider) == Some(format)
                && matches!(
                    format,
                    CatalogFormat::OpenRouter
                        | CatalogFormat::Ollama
                        | CatalogFormat::PrimeInference
                )
            {
                template.map_or(UnknownModelPolicy::KnownOnly, |template| {
                    UnknownModelPolicy::ProviderMetadata { template }
                })
            } else {
                UnknownModelPolicy::KnownOnly
            };
            let source = ResolvedSource {
                id: provider.clone(),
                provider: provider.clone(),
                format,
                url,
                headers: headers.into_iter().collect(),
                unknown_models,
                is_current: guard,
            };
            if format == CatalogFormat::PrimeInference {
                let mut public = source.clone();
                public.id = "prime-inference-public".into();
                inputs.sources.push(public);
                if source
                    .headers
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case("x-prime-team-id"))
                    && source
                        .headers
                        .iter()
                        .any(|(key, _)| key.eq_ignore_ascii_case("authorization"))
                {
                    let mut private = source;
                    private.id = "prime-inference-private".into();
                    private.format = CatalogFormat::PrimeInferencePrivate;
                    // Private routes are arbitrary internal deployments: the public
                    // provider template's reasoning ladder and request shape do not
                    // describe them. Their template is the bundled private route.
                    if let UnknownModelPolicy::ProviderMetadata { template } =
                        &mut private.unknown_models
                    {
                        if let Some(mut bundled) =
                            crate::models::private_auth::get_private_prime_inference_models()
                                .into_iter()
                                .next()
                        {
                            bundled.base_url = self.catalog.prime_inference_base_url().to_string();
                            *template = bundled;
                        }
                    }
                    inputs.sources.push(private);
                }
            } else {
                inputs.sources.push(source);
            }
        }
        self.unresolved_catalog_sources = requested
            .into_iter()
            .filter(|id| !inputs.sources.iter().any(|source| source.id == *id))
            .collect();
        inputs
    }

    fn source_template(
        &self,
        provider: &str,
        config: &ProviderConfig,
        _format: CatalogFormat,
    ) -> Option<Model> {
        let mut template = if provider == "ollama-cloud" {
            pa_models::ollama_cloud::pinned_models(self.catalog.ollama_base_url())
                .into_iter()
                .next()
        } else {
            pa_ai::models_generated::get_models(provider)
                .into_iter()
                .next()
                .cloned()
        }?;
        if provider == "prime-inference" {
            template.base_url = self.catalog.prime_inference_base_url().to_string();
        }
        if let Some(base) = &config.base_url {
            template.base_url.clone_from(base);
        }
        if let Some(api) = &config.api {
            template.api.clone_from(api);
        }
        Some(template)
    }

    fn source_publication_guard(
        &self,
        provider: &str,
        config: &ProviderConfig,
    ) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let paths: Vec<_> = self
            .models_json_path
            .iter()
            .flat_map(|path| {
                [
                    Some(path.clone()),
                    path.parent().map(|parent| parent.join("auth.json")),
                ]
            })
            .flatten()
            .collect();
        let previous: Vec<_> = paths.iter().map(|path| std::fs::read(path).ok()).collect();
        let plugin_paths: Vec<_> = self
            .provider_facts
            .iter()
            .filter(|fact| fact.name == provider)
            .filter_map(|fact| fact.source_path.clone())
            .collect();
        let plugin_previous: Vec<_> = plugin_paths
            .iter()
            .map(|path| std::fs::read(path).ok())
            .collect();
        let auth_guard = self.auth.local_catalog_guard(provider);
        let inherited_headers = self.auth.get_provider_headers(provider);
        let mut values: Vec<String> = config.api_key.iter().cloned().collect();
        values.extend(
            inherited_headers
                .iter()
                .flat_map(|headers| headers.values().cloned()),
        );
        values.extend(
            config
                .headers
                .iter()
                .flat_map(|headers| headers.values().cloned()),
        );
        if let Some(discovery) = &config.discovery {
            values.extend(discovery.api_key.iter().cloned());
            values.extend(
                discovery
                    .headers
                    .iter()
                    .flat_map(|headers| headers.values().cloned()),
            );
        }
        let environment: Vec<_> = values
            .into_iter()
            .filter(|value| !value.starts_with('!'))
            .map(|name| {
                let value = std::env::var(&name).ok();
                (name, value)
            })
            .collect();
        Arc::new(move || {
            if !paths
                .iter()
                .zip(&previous)
                .all(|(path, bytes)| std::fs::read(path).ok().as_ref() == bytes.as_ref())
            {
                return false;
            }
            if !plugin_paths
                .iter()
                .zip(&plugin_previous)
                .all(|(path, bytes)| std::fs::read(path).ok().as_ref() == bytes.as_ref())
            {
                return false;
            }
            if !environment
                .iter()
                .all(|(name, value)| std::env::var(name).ok().as_ref() == value.as_ref())
            {
                return false;
            }
            auth_guard()
        })
    }
}
