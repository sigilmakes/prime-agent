//! Factory request validation and explicit model-selector policy.
//! Legacy harness files are passive data and are never a factory lookup source.

use std::path::Path;

use serde_json::json;

use super::{FactoryActivityRequest, FactoryHost, FactoryHostConfig};

const MODELS_JSON: &str = r#"{
  "providers": {
    "testprov": {
      "baseUrl": "http://localhost:9",
      "apiKey": "bridge-key",
      "api": "openai-completions",
      "models": [
        { "id": "declared-model", "name": "Declared Model", "contextWindow": 128000 },
        { "id": "shared-id", "name": "Shared One", "contextWindow": 128000 },
        { "id": "other-model", "name": "Other Model", "contextWindow": 128000 }
      ]
    },
    "otherprov": {
      "baseUrl": "http://localhost:10",
      "apiKey": "other-key",
      "api": "openai-completions",
      "models": [
        { "id": "shared-id", "name": "Shared Two", "contextWindow": 128000 }
      ]
    },
  }
}"#;

fn write_catalog(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("models.json"), MODELS_JSON).unwrap();
}

fn session_model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "session-model", "name": "Session Model", "api": "openai-completions",
        "provider": "testprov", "base_url": "http://localhost:9", "reasoning": false,
        "cost": { "input": 1.5, "output": 2.5, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 128_000, "maxTokens": 4096
    }))
    .unwrap()
}

fn host(
    dir: &Path,
    session_model: Option<pa_agent::types::Model>,
    allowed_models: Option<Vec<String>>,
) -> FactoryHost {
    FactoryHost::new(FactoryHostConfig {
        agent_dir: dir.to_path_buf(),
        session_model,
        allowed_models,
    })
}

#[test]
fn preflight_resolves_exact_and_short_form_selectors() {
    let dir = tempfile::tempdir().unwrap();
    write_catalog(dir.path());
    let bridge = host(dir.path(), None, None);
    bridge
        .preflight_selectors(&["testprov/declared-model".to_string()])
        .unwrap();
    bridge
        .preflight_selectors(&["declared-model".to_string()])
        .unwrap();
    let error = bridge
        .preflight_selectors(&["shared-id".to_string()])
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("Requested factory model \"shared-id\" is unavailable"),
        "{error}"
    );
}

#[test]
fn the_allowlist_pin_refuses_a_resolved_declared_model() {
    let dir = tempfile::tempdir().unwrap();
    write_catalog(dir.path());
    let bridge = host(dir.path(), None, Some(vec!["other/*".to_string()]));
    let selectors = vec!["testprov/declared-model".to_string()];
    let error = bridge
        .preflight_selectors(&selectors)
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "Requested factory model \"testprov/declared-model\" is blocked by the model allowlist"
    );
    let bridge = host(dir.path(), None, Some(vec!["testprov/*".to_string()]));
    bridge.preflight_selectors(&selectors).unwrap();
}

#[test]
fn session_model_fallback_and_request_auth_remain_enforced() {
    let dir = tempfile::tempdir().unwrap();
    write_catalog(dir.path());
    let selectors = vec!["testprov/session-model".to_string()];
    let bridge = host(dir.path(), Some(session_model()), None);
    bridge.preflight_selectors(&selectors).unwrap();
    bridge
        .preflight_selectors(&["testprov/other-model".to_string()])
        .unwrap();
    let without_session = host(dir.path(), None, None);
    let error = without_session
        .preflight_selectors(&selectors)
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("Requested factory model \"testprov/session-model\" is unavailable"),
        "{error}"
    );

    // Require a bearer key whose fixture-only command fails. Custom providers
    // can otherwise be keyless, and their model definitions require an apiKey
    // field. No network or live authentication is used.
    std::fs::write(
        dir.path().join("models.json"),
        MODELS_JSON.replace(
            "\"apiKey\": \"bridge-key\",",
            "\"apiKey\": \"!exit 1\", \"authHeader\": true,",
        ),
    )
    .unwrap();
    let bridge = host(dir.path(), Some(session_model()), None);
    assert!(bridge.preflight_selectors(&selectors).is_err());
    assert!(bridge
        .preflight_selectors(&["testprov/declared-model".to_string()])
        .is_err());
}

#[test]
fn corrupt_legacy_stores_remain_passive_beside_explicit_selector_preflight() {
    let dir = tempfile::tempdir().unwrap();
    write_catalog(dir.path());
    let bridge = host(dir.path(), None, Some(vec!["testprov/*".to_string()]));
    let selectors = vec!["testprov/declared-model".to_string()];
    bridge.preflight_selectors(&selectors).unwrap();
    let old_global = dir.path().join("harness/harness_state.json");
    let old_local = dir.path().join("local-harness/harness_state.json");
    let poison = b"not-json: legacy factory selectors must never become active policy";
    for path in [&old_global, &old_local] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, poison).unwrap();
    }
    bridge.preflight_selectors(&selectors).unwrap();
    bridge.preflight_selectors(&[]).unwrap();
    for path in [&old_global, &old_local] {
        assert_eq!(std::fs::read(path).unwrap(), poison);
    }
}

/// The request validation: known actions, required targets per action, and
/// the bounded watch timeout.
#[test]
fn factory_activity_requests_validate_like_the_kernel_frame() {
    let parse =
        |action: &str, run_id: Option<&str>, spec_id: Option<&str>, timeout_ms: Option<u64>| {
            FactoryActivityRequest::parse(action, run_id, spec_id, timeout_ms)
        };
    // the happy shapes
    assert_eq!(
        parse("graph", None, None, None).unwrap(),
        FactoryActivityRequest {
            action: "graph",
            run_id: None,
            spec_id: None,
            timeout_ms: None,
        }
    );
    assert_eq!(
        parse("graph", Some("  "), None, None).unwrap().run_id,
        None,
        "whitespace-only targets read as absent"
    );
    assert_eq!(
        parse("watch", Some("run-1"), None, Some(2_000))
            .unwrap()
            .timeout_ms,
        Some(2_000)
    );
    assert_eq!(
        parse("run", None, Some("spec-1"), None)
            .unwrap()
            .spec_id
            .as_deref(),
        Some("spec-1")
    );
    // unknown action
    assert_eq!(
        parse("bogus", None, None, None).unwrap_err().to_string(),
        "unknown factory activity action"
    );
    // required targets
    assert_eq!(
        parse("status", None, None, None).unwrap_err().to_string(),
        "factory activity status requires runId"
    );
    assert_eq!(
        parse("watch", None, None, None).unwrap_err().to_string(),
        "factory activity watch requires runId"
    );
    assert_eq!(
        parse("stop", None, None, None).unwrap_err().to_string(),
        "factory activity stop requires runId"
    );
    assert_eq!(
        parse("resume", None, None, None).unwrap_err().to_string(),
        "factory activity resume requires runId"
    );
    assert_eq!(
        parse("run", None, None, None).unwrap_err().to_string(),
        "factory activity run requires specId"
    );
    // the watch bound
    assert!(parse("watch", Some("r"), None, Some(60_000)).is_ok());
    let error = parse("watch", Some("r"), None, Some(60_001))
        .unwrap_err()
        .to_string();
    assert_eq!(error, "factory activity timeoutMs must be at most 60000");
}
