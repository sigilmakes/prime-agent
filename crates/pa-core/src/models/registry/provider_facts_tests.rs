//! Session-free plugin provider facts in the registry: availability, keyless
//! request auth, limits, and builtin collisions.
use super::*;
use crate::auth::manager::{AuthStorage, NoOAuth};
use crate::auth::types::AuthStorageData;
use serde_json::json;
use std::sync::Arc;

fn auth() -> AuthStorage {
    AuthStorage::in_memory_without_env(&AuthStorageData::default(), Arc::new(NoOAuth))
}

fn agent_dir(source: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.json"),
        json!({"plugins":{"local":{"enabled":true,"path":"local.lua"}}}).to_string(),
    )
    .unwrap();
    std::fs::write(dir.path().join("local.lua"), source).unwrap();
    dir
}

const LOCAL_FACT: &str = r#"
return {providers = {
  {name = "local-gateway", url = "http://127.0.0.1:8090/v1", protocol = "openai",
   auth = "none",
   headers = {["x-local"] = "1"},
   defaults = {reasoning = false},
   limits = {context = 32768, maxTokens = 4096},
   models = {{id = "local-model", name = "Local", contextWindow = 131072, maxTokens = 65536}}}
}}
"#;

#[test]
fn a_plugin_fact_exposes_its_provider_and_models_without_a_session() {
    let dir = agent_dir(LOCAL_FACT);
    let registry = ModelRegistry::create_for_agent_dir(auth(), dir.path());
    assert_eq!(registry.get_error(), None);
    let model = registry
        .get_all()
        .iter()
        .find(|model| model.provider == "local-gateway" && model.id == "local-model")
        .expect("the fact model is registered");
    assert_eq!(model.name, "Local");
    assert_eq!(model.base_url, "http://127.0.0.1:8090/v1");
    assert_eq!(model.api, "openai-completions");
    assert_eq!(model.context_window, 32_768);
    assert_eq!(model.max_tokens, 4_096);
    assert!(model.max_tokens_explicit);
    assert!(!model.reasoning);
    assert!(registry
        .get_available()
        .iter()
        .any(|model| model.provider == "local-gateway" && model.id == "local-model"));
    assert!(registry.is_keyless_provider("local-gateway"));
    let status = registry.get_provider_auth_status("local-gateway");
    assert!(status.configured);
    assert_eq!(status.source, None);
}

#[test]
fn a_keyless_provider_resolves_set_model_and_sends_no_authorization() {
    let dir = agent_dir(LOCAL_FACT);
    let mut registry = ModelRegistry::create_for_agent_dir(auth(), dir.path());
    let model = registry
        .resolve_set_model_selection("local-gateway", "local-model")
        .expect("a keyless selection resolves")
        .clone();
    assert!(registry.has_configured_auth(&model));
    let resolved = registry.get_api_key_and_headers(&model, model.headers.as_ref());
    assert!(resolved.ok, "{:?}", resolved.error);
    assert_eq!(resolved.api_key, None);
    let headers = resolved.headers.unwrap_or_default();
    assert_eq!(headers.get("x-local").map(String::as_str), Some("1"));
    assert!(
        !headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("authorization")),
        "a keyless request carries no Authorization: {headers:?}"
    );
    assert!(
        !model
            .compat
            .as_ref()
            .expect("the keyless provider marks its compat")
            .raw
            .get("sendAuthorization")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        "the model tells pa-ai to omit the bearer header"
    );
}

fn remote_model(id: &str, context: u64, max_tokens: u64) -> Model {
    serde_json::from_value(json!({
        "id": id, "name": id, "api": "openai-completions", "provider": "local-gateway",
        "baseUrl": "http://127.0.0.1:8090/v1", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": context, "maxTokens": max_tokens
    }))
    .unwrap()
}

#[test]
fn fact_limits_clamp_endpoint_metadata_and_mark_the_ceiling_explicit() {
    let dir = agent_dir(LOCAL_FACT);
    let mut registry = ModelRegistry::create_for_agent_dir(auth(), dir.path());
    let (custom, _) = registry.prepare_catalog(true);
    registry.accept_catalog_snapshot(
        custom,
        pa_models::CatalogSnapshot {
            models: vec![remote_model("remote-model", 200_000, 131_072)],
            preferred_default: None,
            authorized_private_prime_models: Vec::new(),
        },
    );
    let model = registry
        .get_all()
        .iter()
        .find(|model| model.id == "remote-model")
        .expect("the projected endpoint model is registered");
    assert_eq!(model.context_window, 32_768, "the fact context cap wins");
    assert_eq!(model.max_tokens, 4_096, "the fact output cap wins");
    assert!(
        model.max_tokens_explicit,
        "the fact ceiling is configuration"
    );
}

#[test]
fn a_fact_named_after_a_builtin_provider_is_rejected() {
    let dir = agent_dir(
        r#"return {providers = {
          {name = "openai", url = "http://127.0.0.1:8090/v1", protocol = "openai",
           models = {{id = "plugin-only"}}}
        }}"#,
    );
    let registry = ModelRegistry::create_for_agent_dir(auth(), dir.path());
    let error = registry
        .get_error()
        .expect("the collision is reported instead of silently applied");
    assert!(
        error.contains("collides with a builtin provider id"),
        "{error}"
    );
    assert!(!registry
        .get_all()
        .iter()
        .any(|model| model.id == "plugin-only"));
    assert!(
        registry
            .get_all()
            .iter()
            .filter(|model| model.provider == "openai")
            .all(|model| !model.max_tokens_explicit),
        "a rejected fact must not mark the builtin provider's models"
    );
}
