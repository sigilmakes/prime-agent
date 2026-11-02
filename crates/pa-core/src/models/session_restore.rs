//! Session-model restore with a bounded catalog-readiness wait: a revived
//! session's saved model can be missing only because the boot's
//! catalog/auth refresh has not settled. Without the wait the restore
//! silently lands on the startup-chain default instead of the saved model.

use std::time::Duration;

use pa_types::ai::Model;

use super::registry::ModelRegistry;

/// The bounded window a session-model restore waits for in-flight catalog/auth refreshes to settle
/// before the lookup may fail.
pub const SESSION_MODEL_RESTORE_READINESS_TIMEOUT_MS: u64 = 5_000;

/// The model is registered and its provider has configured auth (TS `findRestorable`).
fn find_restorable(registry: &ModelRegistry, provider: &str, model_id: &str) -> Option<Model> {
    registry
        .get_all()
        .iter()
        .find(|model| model.provider == provider && model.id == model_id)
        .filter(|model| registry.has_configured_auth(model))
        .cloned()
}

/// Find a saved session model, giving in-flight catalog/auth refreshes a
/// bounded window to settle before the lookup may fail. A miss returns
/// `None`, leaving the caller to fall back (and to say so: the fallback
/// must never be silent).
pub async fn find_session_model_with_readiness_wait(
    registry: &mut ModelRegistry,
    provider: &str,
    model_id: &str,
    readiness_timeout_ms: u64,
) -> Option<Model> {
    if !registry.get_provider_auth_status(provider).configured {
        return None;
    }
    let direct = find_restorable(registry, provider, model_id);
    if direct.is_some() {
        return direct;
    }
    // The budget bounds only this caller. The owned publication task retains
    // successful discoveries even when HTTP or a sibling cache lock runs longer.
    let _ = tokio::time::timeout(
        Duration::from_millis(readiness_timeout_ms),
        registry.wait_for_catalog_publication(),
    )
    .await;
    registry.refresh();
    registry.load_private_authorization_from_cache();
    find_restorable(registry, provider, model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn environment_only_auth_restores_a_known_model_without_waiting() {
        struct Environment;
        impl crate::auth::manager::EnvCredentialSource for Environment {
            fn key_names(&self, provider: &str) -> Option<Vec<String>> {
                (provider == "openai").then(|| vec!["OPENAI_API_KEY".into()])
            }
            fn api_key(&self, provider: &str) -> Option<String> {
                (provider == "openai").then(|| "fixture-env-key".into())
            }
            fn prime_team_id(&self) -> Option<String> {
                None
            }
            fn ambient_identity_material(&self, provider: &str) -> String {
                provider.into()
            }
        }

        let auth = crate::auth::AuthStorage::in_memory_with_env(
            &crate::auth::AuthStorageData::default(),
            std::sync::Arc::new(crate::auth::NoOAuth),
            std::sync::Arc::new(Environment),
        );
        let mut registry = ModelRegistry::in_memory(auth);
        let model = registry
            .get_all()
            .iter()
            .find(|model| model.provider == "openai")
            .unwrap()
            .clone();
        let local_status = registry.auth.get_auth_status_local(&model.provider);
        assert!(!local_status.configured);
        let status = registry.get_provider_auth_status(&model.provider);
        assert!(status.configured);
        assert_eq!(status.source, local_status.source);
        assert_eq!(status.label, local_status.label);
        assert!(registry.get_available().contains(&&model));
        assert_eq!(
            find_session_model_with_readiness_wait(&mut registry, &model.provider, &model.id, 0)
                .await,
            Some(model),
        );
    }

    #[tokio::test]
    async fn restore_requires_a_registered_model_with_configured_auth() {
        let auth = crate::auth::AuthStorage::in_memory_without_env(
            &crate::auth::AuthStorageData::default(),
            std::sync::Arc::new(crate::auth::NoOAuth),
        );
        let mut registry = ModelRegistry::in_memory(auth);
        assert!(
            find_session_model_with_readiness_wait(
                &mut registry,
                "prime-inference",
                "z-ai/glm-5.3",
                0
            )
            .await
            .is_none(),
            "no provider auth configured: not restorable"
        );
        assert!(
            find_session_model_with_readiness_wait(&mut registry, "anthropic", "missing", 0)
                .await
                .is_none(),
            "not registered: not restorable"
        );
    }
}
