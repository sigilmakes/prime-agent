//! Model subsystem: resolver and Prime Inference private models.

/// Providers whose catalogue templates and auth flows are compiled in. Plugin
/// facts may not take these ids.
pub(crate) const BUILTIN_PROVIDER_IDS: [&str; 7] = [
    "openai",
    "openrouter",
    "anthropic",
    "google",
    "openai-codex",
    "ollama-cloud",
    "prime-inference",
];

pub use allowlist::{model_allowed, ModelAllowlistRefusal};
pub use private_auth::get_private_prime_inference_models;
pub use registry::{
    ModelRegistry, ProviderRequestConfig, ResolvedRequestAuth, SetModelSelectionError,
};

/// Scheduler policy belongs to core, not the metadata/cache owner.
pub use catalog_chain::RefreshTrigger;
pub use catalog_chain::{catalog_for, install_catalog, spawn_hourly_refresh, startup_refresh};

pub mod allowlist;
pub(crate) mod catalog_chain;
pub(crate) mod custom;
mod ollama_cloud_config;
pub use ollama_cloud_config::OllamaCloudConfig;
pub mod image_model_routing;
pub(crate) mod prime_inference;
pub(crate) mod private_auth;
pub(crate) mod registry;
pub(crate) mod resolver;
pub(crate) mod session_restore;

pub use custom::{
    apply_model_override, load_custom_models, merge_compat, parse_models_config,
    strip_json_comments, validate_config, CustomModelsResult, ModelOverride, ModelsConfig,
    ProviderOverride,
};
pub use image_model_routing::{
    resolve_image_model_override, ImageModelRoutingInputs, ResolvedImageModel,
};
pub use prime_inference::{
    is_private_prime_inference_model, is_private_prime_inference_model_id,
    private_prime_inference_models, PRIME_INFERENCE_BASE_URL,
};
pub use resolver::{
    build_fallback_model, failover_candidates, find_exact_model_reference_match,
    find_initial_model, find_preferred_default_model, resolve_cli_model,
    resolve_model_scope_from_models, InitialModelOptions, ResolveCliModelResult, ScopedModel,
    PRIME_INFERENCE_DEFAULT_MODEL_ID,
};
pub use session_restore::{
    find_session_model_with_readiness_wait, SESSION_MODEL_RESTORE_READINESS_TIMEOUT_MS,
};
