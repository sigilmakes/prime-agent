//! Factory activity request validation and explicit model-selector preflight.
//! Named factory specs and subagents resolve in the Python library. Their
//! children still pass through the `rlm.spawn` host model and allowlist checks.
//! This module never reads legacy harness stores.

use std::path::PathBuf;

use anyhow::anyhow;

use crate::kernel::shared::{FACTIVITY_WATCH_TIMEOUT_MS_CAP, FACTORY_ACTIVITY_ACTIONS};
use crate::models::registry::ModelRegistry;
use crate::models::resolver::find_exact_model_reference_match;
use pa_types::ai::Model as AiModel;

/// Explicit session facts used for model-selector preflight.
#[derive(Clone)]
pub struct FactoryHostConfig {
    /// The agent directory: the model registry and auth cache resolve
    /// against it (the `run` preflight's model resolution).
    pub agent_dir: PathBuf,
    /// The session model (the agent-side descriptor `create_session`
    /// requires): the declared-selector preflight's stale-provider gate and
    /// the equality fallback for a selector the catalog does not carry (the
    /// spawn path owns the full resolution). The descriptor is lossy by
    /// design, so the preflight crosses it back to the ai side field by
    /// field for the auth check (the #3184 field mapping).
    pub session_model: Option<pa_agent::types::Model>,
    /// The daemon `allowedModels` pin, enforced on every resolved model the
    /// preflight accepts (the per-spawn enforcement stays on the
    /// `rlm.spawn` host path).
    pub allowed_models: Option<Vec<String>>,
}

/// An explicit model-selector policy built from [`FactoryHostConfig`].
#[derive(Clone)]
pub struct FactoryHost {
    config: FactoryHostConfig,
}

impl FactoryHost {
    #[must_use]
    pub fn new(config: FactoryHostConfig) -> Self {
        Self { config }
    }

    /// Preflight explicit model selectors supplied by a factory caller.
    /// Named library specs and subagents stay kernel-owned and are checked per spawn.
    /// This never reads stored harness specs or resolves scoped harness references.
    ///
    /// # Errors
    ///
    /// Returns an error if an explicit selector is unavailable, unauthenticated,
    /// expired, or outside the daemon model allowlist.
    pub fn preflight_selectors(&self, selectors: &[String]) -> anyhow::Result<()> {
        if selectors.is_empty() {
            return Ok(());
        }
        let mut registry = ModelRegistry::create(
            crate::auth::AuthStorage::create(&self.config.agent_dir),
            self.config.agent_dir.join("models.json"),
        );
        registry.load_private_authorization_from_cache();
        let searchable: Vec<AiModel> = registry
            .get_rlm_searchable_models()
            .into_iter()
            .cloned()
            .collect();
        for selector in selectors {
            let model = resolve_declared_model(&searchable, selector)
                .or_else(|| self.session_model_fallback(&registry, selector))
                .ok_or_else(|| unavailable_error(selector))?;
            if let Some(allowlist) = &self.config.allowed_models {
                let pinned = format!("{}/{}", model.provider, model.id).to_lowercase();
                if !crate::models::model_allowed(&pinned, allowlist) {
                    anyhow::bail!(
                        "Requested factory model \"{pinned}\" is blocked by the model allowlist"
                    );
                }
            }
            let resolved = registry.get_api_key_and_headers(&model, model.headers.as_ref());
            if !resolved.ok {
                anyhow::bail!(
                    "Model \"{}/{}\" is not authenticated: {}",
                    model.provider,
                    model.id,
                    resolved.error.as_deref().unwrap_or("no credential found")
                );
            }
        }
        Ok(())
    }

    /// A declared selector naming the session's own model the catalog does
    /// not carry (a scripted or in-memory model): the spawn path resolves
    /// it, so the preflight gates only the stale/expired provider — the
    /// #3184 session-model fallback gate.
    fn session_model_fallback(&self, registry: &ModelRegistry, selector: &str) -> Option<AiModel> {
        let session_model = self.config.session_model.as_ref()?;
        let session_selector = format!("{}/{}", session_model.provider, session_model.id);
        let matches = selector.trim().eq_ignore_ascii_case(&session_selector)
            || is_short_form_selector(selector, &session_selector);
        if !matches {
            return None;
        }
        let status = registry.get_provider_auth_status(&session_model.provider);
        if status.source != Some(crate::auth::types::AuthSource::Stale)
            && status.label.as_deref() != Some("expired")
        {
            return Some(agent_model_to_ai_model(session_model));
        }
        None
    }
}

/// The ai-side view of the session's agent-side model descriptor, for the
/// preflight's allowlist and auth checks only. The two `Model`s do not
/// share a wire shape (the agent side serializes `base_url`, never carries
/// `input`), and the descriptor is lossy by design (no input modalities,
/// thinking-level map, featured flag, request headers, or compat
/// overrides), so full resolution stays with the catalog and the spawn
/// path (the #3184 `provider_adapter::agent_model_to_ai_model` field
/// mapping).
fn agent_model_to_ai_model(model: &pa_agent::types::Model) -> AiModel {
    AiModel {
        id: model.id.clone(),
        name: model.name.clone(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        base_url: model.base_url.clone(),
        reasoning: model.reasoning,
        thinking_level_map: None,
        input: Vec::new(),
        cost: pa_types::ai::ModelCost {
            input: model.cost.input.into(),
            output: model.cost.output.into(),
            cache_read: model.cost.cache_read.into(),
            cache_write: model.cost.cache_write.into(),
        },
        context_window: model.context_window,
        max_tokens: model.max_tokens,
        max_tokens_explicit: model.max_tokens_explicit,
        featured: None,
        headers: None,
        compat: None,
    }
}

/// The exact catalog match over the searchable set, then the TS short form
/// (`_resolveRlmSubagentModel`'s unique-suffix rule): a bare id resolves
/// only while it names exactly one model.
fn resolve_declared_model(searchable: &[AiModel], selector: &str) -> Option<AiModel> {
    if let Some(model) = find_exact_model_reference_match(selector, searchable) {
        return Some(model.clone());
    }
    let short_form: Vec<&AiModel> = searchable
        .iter()
        .filter(|model| {
            is_short_form_selector(selector, &format!("{}/{}", model.provider, model.id))
        })
        .collect();
    match short_form.len() {
        1 => Some(short_form[0].clone()),
        _ => None,
    }
}

/// The TS short form: a reference names a model when the full selector ends
/// with `"/<reference>"`, so a bare id like "glm-5.3" also matches
/// "prime-inference/z-ai/glm-5.3".
fn is_short_form_selector(reference: &str, selector: &str) -> bool {
    let normalized = reference.trim().to_lowercase();
    !normalized.is_empty() && selector.to_lowercase().ends_with(&format!("/{normalized}"))
}

/// The unavailable-model refusal (the TS `formatRlmModelUnavailableError`
/// base wording).
fn unavailable_error(reference: &str) -> anyhow::Error {
    anyhow!(
        "Requested factory model \"{reference}\" is unavailable, unauthenticated, or expired; selectors use the form \"provider/model-id\" (e.g. \"prime-inference/internal/glm-5.3-fast\")"
    )
}

/// The out-of-band request the bridge sends into the kernel, validated
/// host-side before the frame: one action with its target and the watch
/// bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactoryActivityRequest {
    pub action: &'static str,
    pub run_id: Option<String>,
    pub spec_id: Option<String>,
    pub timeout_ms: Option<u64>,
}

impl FactoryActivityRequest {
    /// Parse and validate one daemon command payload. Mirrors the kernel
    /// frame's own validation (the kernel re-validates): a known action,
    /// string targets when present, and a bounded watch timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when the action is unknown, a required target is
    /// missing, or the timeout is outside its bounds.
    pub fn parse(
        action: &str,
        run_id: Option<&str>,
        spec_id: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<Self> {
        let Some(action) = FACTORY_ACTIVITY_ACTIONS
            .iter()
            .find(|known| **known == action)
            .copied()
        else {
            return Err(anyhow!("unknown factory activity action"));
        };
        if action == "run" && spec_id.is_none_or(|id| id.trim().is_empty()) {
            return Err(anyhow!("factory activity run requires specId"));
        }
        if action != "graph" && action != "run" && run_id.is_none_or(|id| id.trim().is_empty()) {
            return Err(anyhow!("factory activity {action} requires runId"));
        }
        if let Some(timeout_ms) = timeout_ms {
            if timeout_ms > FACTIVITY_WATCH_TIMEOUT_MS_CAP {
                return Err(anyhow!(
                    "factory activity timeoutMs must be at most {FACTIVITY_WATCH_TIMEOUT_MS_CAP}"
                ));
            }
        }
        Ok(Self {
            action,
            run_id: run_id
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string),
            spec_id: spec_id
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string),
            timeout_ms,
        })
    }
}

// The unit battery lives in the child module (factory_host::tests).
#[cfg(test)]
mod tests;
