//! Resolve locally authorized discovery requests. Wire adapters and caches live in pa-models.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pa_models::{CatalogFormat, CatalogInputs, ResolvedSource, UnknownModelPolicy};
use pa_types::ai::Model;

use super::ModelRegistry;
use crate::models::custom::{self, CustomModelsResult, ProviderConfig};

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
    pub(super) fn catalog_inputs(
        &mut self,
        custom: &CustomModelsResult,
        local_only: bool,
    ) -> CatalogInputs {
        let mut providers: BTreeSet<String> = [
            "openai",
            "openrouter",
            "anthropic",
            "google",
            "openai-codex",
            "ollama-cloud",
            "prime-inference",
        ]
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
            let key = if explicit_auth_header {
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
            if inherit
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
                if let Some(headers) = self.auth.get_provider_headers(&provider) {
                    merge_headers(&headers.into_iter().collect());
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
            let guard = self.source_publication_guard(&provider, &config);
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
    ) -> Option<Arc<dyn Fn() -> bool + Send + Sync>> {
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
        Some(Arc::new(move || {
            if !paths
                .iter()
                .zip(&previous)
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
        }))
    }
}
