//! Provider discovery parity and isolated HTTP verifiers; no real credentials or live state.
mod common;

use std::sync::Arc;

use pa_models::provider_discovery::{
    discovery_headers, merge_provider_discovery_payload, parse_provider_discovery,
    DiscoverableProvider as Provider, ProviderDiscoverySource,
};
use pa_models::{transports::compiled_models, Model};
use pa_types::ai::ModelInput;
use serde_json::{json, Value};

fn template(provider: Provider) -> Model {
    compiled_models()
        .iter()
        .find(|model| model.provider == provider.provider_id())
        .unwrap()
        .clone()
}

fn rich_row(id: &str) -> Value {
    json!({
        "id": id, "name": "New model", "context_length": 32_000,
        "top_provider": {"max_completion_tokens": 4096},
        "pricing": {"prompt": "0.000001", "completion": "0.000002"},
        "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]}
    })
}

#[test]
fn discovery_updates_metadata_but_preserves_exact_installed_controls() {
    let mut original = template(Provider::OpenRouter);
    original.headers = Some([("X-Installed".into(), "yes".into())].into_iter().collect());
    original.compat = Some(pa_types::ai::ModelCompat {
        raw: [("supportsStore".into(), json!(false))]
            .into_iter()
            .collect(),
    });
    let mut row = rich_row(&original.id);
    row["api"] = json!("evil-api");
    row["baseUrl"] = json!("https://attacker.invalid");
    row["headers"] = json!({"Authorization": "attacker"});
    row["compat"] = json!({"supportsStore": true});
    row["reasoning"] = json!(!original.reasoning);
    row["thinkingLevelMap"] = json!({"high": "attacker"});
    let parsed = parse_provider_discovery(
        Provider::OpenRouter,
        &json!({"data": [row]}),
        std::slice::from_ref(&original),
        &[],
    )
    .unwrap();
    let mut expected = original;
    expected.name = "New model".into();
    expected.context_window = 32_000;
    expected.max_tokens = 4096;
    expected.max_tokens_explicit = false;
    expected.cost.input = 1.0.into();
    expected.cost.output = 2.0.into();
    assert_eq!(parsed, vec![expected]);
}

#[test]
fn unknown_openrouter_requires_complete_metadata_without_template_capabilities() {
    let mut original = template(Provider::OpenRouter);
    original.reasoning = true;
    original.featured = Some(true);
    original.headers = Some(
        [("X-Installed".into(), "secret".into())]
            .into_iter()
            .collect(),
    );
    original.thinking_level_map = Some(
        [(pa_types::ai::ModelThinkingLevel::High, Some("high".into()))]
            .into_iter()
            .collect(),
    );
    let complete = rich_row("test/complete");
    let mut input_only = rich_row("test/input-only");
    input_only["architecture"] = json!({"input_modalities": ["text"]});
    let mut audio = rich_row("test/audio");
    audio["architecture"]["output_modalities"] = json!(["audio"]);
    let mut image = rich_row("test/image");
    image["architecture"]["input_modalities"] = json!(["image"]);
    let mut no_prices = rich_row("test/no-prices");
    no_prices["pricing"] = json!({"prompt": "NaN", "completion": -1});
    let parsed = parse_provider_discovery(
        Provider::OpenRouter,
        &json!({"data": [
            {"id": "test/id-only"}, input_only, audio, image, no_prices, complete
        ]}),
        std::slice::from_ref(&original),
        &[],
    )
    .unwrap();
    let mut expected = original.clone();
    expected.id = "test/complete".into();
    expected.name = "New model".into();
    expected.context_window = 32_000;
    expected.max_tokens = 4096;
    expected.max_tokens_explicit = false;
    expected.headers = None;
    expected.compat = None;
    expected.reasoning = false;
    expected.thinking_level_map = None;
    expected.featured = None;
    expected.input = vec![ModelInput::Text];
    expected.cost =
        serde_json::from_value(json!({"input": 1,"output":2,"cacheRead":0,"cacheWrite":0}))
            .unwrap();
    assert_eq!(parsed, vec![expected, original]);
}

#[test]
fn id_only_sources_and_google_never_invent_unknown_capabilities() {
    for provider in [Provider::OpenAi, Provider::Anthropic, Provider::Google] {
        let original = template(provider);
        let payload = match provider {
            Provider::OpenAi => json!({"data":[{"id": original.id}, {"id": "unknown"}]}),
            Provider::Anthropic => {
                json!({"data":[{"id": original.id,"display_name":"Known"},{"id":"unknown","display_name":"Unknown"}]})
            }
            Provider::Google => json!({"models":[
                {"name":format!("models/{}",original.id),"displayName":"Known","inputTokenLimit":500_000,"outputTokenLimit":8192,"supportedGenerationMethods":["generateContent"]},
                {"name":"models/unknown","inputTokenLimit":500_000,"outputTokenLimit":8192,"supportedGenerationMethods":["generateContent"]},
                {"name":"models/embed","supportedGenerationMethods":["embedContent"]}
            ]}),
            Provider::OpenRouter => unreachable!("tested separately"),
        };
        let parsed =
            parse_provider_discovery(provider, &payload, std::slice::from_ref(&original), &[])
                .unwrap();
        let mut expected = original;
        match provider {
            Provider::Anthropic => expected.name = "Known".into(),
            Provider::Google => {
                expected.name = "Known".into();
                expected.context_window = 500_000;
                expected.max_tokens = 8192;
            }
            Provider::OpenAi => {}
            Provider::OpenRouter => unreachable!("tested separately"),
        }
        expected.max_tokens_explicit = false;
        assert_eq!(parsed, vec![expected]);
        let unknown = if provider == Provider::Google {
            json!({"models":[{"name":"models/unknown","supportedGenerationMethods":["generateContent"]}]})
        } else {
            json!({"data":[{"id":"unknown"}]})
        };
        assert!(parse_provider_discovery(provider, &unknown, compiled_models(), &[]).is_err());
    }
}

#[test]
fn partial_payload_merge_strips_nested_transport_and_retains_metadata_across_restart() {
    let mut first_row = rich_row("test/a");
    first_row["headers"] = json!({"Authorization":"attacker"});
    first_row["top_provider"]["baseUrl"] = json!("https://attacker.invalid");
    first_row["pricing"]["secret"] = json!("attacker");
    first_row["architecture"]["headers"] = json!({"evil":"attacker"});
    let first = merge_provider_discovery_payload(
        Provider::OpenRouter,
        &json!({"data":[first_row,rich_row("test/b")]}),
        &Value::Null,
        compiled_models(),
    )
    .unwrap();
    let second = merge_provider_discovery_payload(Provider::OpenRouter,
        &json!({"data":[{"id":"test/a","name":"Updated","pricing":{"prompt":"0.000003","completion":"Infinity"},"baseUrl":"attacker"}]}),
        &first, compiled_models()).unwrap();
    let mut expected_a = rich_row("test/a");
    expected_a["name"] = json!("Updated");
    expected_a["pricing"]["prompt"] = json!("0.000003");
    assert_eq!(second, json!({"data":[expected_a,rich_row("test/b")]}));
    assert!(!second.to_string().contains("attacker"));
    let parsed =
        parse_provider_discovery(Provider::OpenRouter, &second, compiled_models(), &[]).unwrap();
    assert_eq!(
        parsed
            .iter()
            .filter(|model| model.id.starts_with("test/"))
            .count(),
        2
    );
}

#[test]
fn explicit_non_chat_revokes_live_only_models_in_memory_and_on_disk() {
    for architecture in [
        json!({"input_modalities":["image"]}),
        json!({"output_modalities":["audio"]}),
    ] {
        let first = json!({"data":[rich_row("test/transition")]});
        let previous =
            parse_provider_discovery(Provider::OpenRouter, &first, compiled_models(), &[]).unwrap();
        let omitted = parse_provider_discovery(
            Provider::OpenRouter,
            &json!({"data":[{"id":"test/transition"}]}),
            compiled_models(),
            &previous,
        )
        .unwrap();
        assert!(omitted.iter().any(|model| model.id == "test/transition"));
        let non_chat = json!({"data":[{"id":"test/transition","architecture":architecture}]});
        let next = parse_provider_discovery(
            Provider::OpenRouter,
            &non_chat,
            compiled_models(),
            &previous,
        )
        .unwrap();
        assert!(!next.iter().any(|model| model.id == "test/transition"));
        let persisted = merge_provider_discovery_payload(
            Provider::OpenRouter,
            &non_chat,
            &first,
            compiled_models(),
        )
        .unwrap();
        let restarted =
            parse_provider_discovery(Provider::OpenRouter, &persisted, compiled_models(), &[])
                .unwrap();
        assert!(!restarted.iter().any(|model| model.id == "test/transition"));
    }
}

#[test]
fn malformed_lists_fail_and_invalid_rows_cannot_replace_good_snapshot() {
    let first = json!({"data":[rich_row("test/a"),rich_row("test/b")]});
    let previous =
        parse_provider_discovery(Provider::OpenRouter, &first, compiled_models(), &[]).unwrap();
    for payload in [
        json!(null),
        json!([]),
        json!({"data": {}}),
        json!({"data":[]}),
        json!({"data":[null,[],{"id":""},{"id":"bad\n"},{"id":"x".repeat(201)}]}),
    ] {
        assert!(parse_provider_discovery(
            Provider::OpenRouter,
            &payload,
            compiled_models(),
            &previous
        )
        .is_err());
    }
    let parsed = parse_provider_discovery(
        Provider::OpenRouter,
        &json!({"data":[{"id":"test/a","name":"Changed"},{"id":"bad\n"}]}),
        compiled_models(),
        &previous,
    )
    .unwrap();
    let mut expected = previous;
    let a = expected
        .iter_mut()
        .find(|model| model.id == "test/a")
        .unwrap();
    a.name = "Changed".into();
    assert_eq!(parsed, expected);
}

#[test]
fn retained_previous_models_are_filtered_and_repin_even_when_omitted() {
    let original = template(Provider::OpenRouter);
    let mut tampered = original.clone();
    tampered.api = "attacker".into();
    tampered.base_url = "https://attacker.invalid".into();
    tampered.headers = Some([("X-Evil".into(), "attacker".into())].into_iter().collect());
    tampered.reasoning = !original.reasoning;
    let parsed = parse_provider_discovery(
        Provider::OpenRouter,
        &json!({"data":[rich_row("test/new")]}),
        std::slice::from_ref(&original),
        &[tampered, template(Provider::Google)],
    )
    .unwrap();
    assert_eq!(parsed[0], original);
    assert!(parsed.iter().all(|model| model.provider == "openrouter"));
}

#[test]
fn header_sources_require_credentials_except_anonymous_openrouter() {
    for provider in [Provider::OpenAi, Provider::Anthropic, Provider::Google] {
        assert!(ProviderDiscoverySource::new(provider, None).is_none());
        assert!(ProviderDiscoverySource::new(provider, Some("")).is_none());
    }
    assert_eq!(discovery_headers(Provider::OpenRouter, None), Some(vec![]));
    assert_eq!(
        discovery_headers(Provider::Anthropic, Some("key")),
        Some(vec![
            ("x-api-key".into(), "key".into()),
            ("anthropic-version".into(), "2023-06-01".into())
        ])
    );
    assert_eq!(
        discovery_headers(Provider::Google, Some("key")),
        Some(vec![("x-goog-api-key".into(), "key".into())])
    );
}

#[tokio::test]
async fn all_adapters_fetch_from_isolated_servers_with_credentials_and_cache_revalidation() {
    use common::{not_modified, ok_json, MockServer};
    use pa_models::cache::{CatalogCache, RefreshOptions};
    use pa_models::fetch::CatalogFetcher;
    for provider in [
        Provider::OpenRouter,
        Provider::OpenAi,
        Provider::Anthropic,
        Provider::Google,
    ] {
        let original = template(provider);
        let payload = if provider == Provider::Google {
            json!({"models":[{"name":format!("models/{}",original.id),"supportedGenerationMethods":["generateContent"]}]})
        } else {
            json!({"data":[{"id":original.id}]})
        };
        let server = MockServer::start(vec![
            ok_json(payload.to_string(), Some("v1")),
            not_modified(),
        ])
        .await;
        let mut source = ProviderDiscoverySource::new(provider, Some("test-key")).unwrap();
        source.url = server.url("/models");
        let dir = tempfile::tempdir().unwrap();
        let cached_path = dir.path().join("provider.json");
        let cache = CatalogCache::new(
            &source.url,
            Some(cached_path.clone()),
            Arc::new(CatalogFetcher::new()),
            Arc::new(move |payload, _| {
                parse_provider_discovery(provider, payload, compiled_models(), &[])
            }),
        );
        let refresh = || RefreshOptions {
            force: true,
            headers: source.headers.clone(),
            ..RefreshOptions::default()
        };
        let first = cache.refresh("test-scope", refresh()).await.unwrap();
        assert_eq!(cache.refresh("test-scope", refresh()).await, Some(first));
        let requests = server.recorded_requests();
        assert_eq!(requests.len(), 2);
        for (name, value) in &source.headers {
            assert!(requests[0].to_lowercase().contains(&format!(
                "{}: {}",
                name.to_lowercase(),
                value.to_lowercase()
            )));
        }
        assert!(requests[1].to_lowercase().contains("if-none-match: v1"));
        assert!(!std::fs::read_to_string(cached_path)
            .unwrap()
            .contains("test-key"));
    }
}

#[test]
fn integral_float_limits_are_supported_but_bad_limits_and_prices_do_not_overwrite_metadata() {
    let original = template(Provider::OpenRouter);
    let payload = json!({"data":[{
        "id":original.id,"context_length":64_000.0,
        "top_provider":{"max_completion_tokens":8192.0},
        "pricing":{"prompt":" 0 ","completion":"1e-6","input_cache_read":"1e309","input_cache_write":-1}
    }]});
    let parsed = parse_provider_discovery(
        Provider::OpenRouter,
        &payload,
        std::slice::from_ref(&original),
        &[],
    )
    .unwrap();
    let mut expected = original.clone();
    expected.context_window = 64_000;
    expected.max_tokens = 8192;
    expected.cost.input = 0.0.into();
    expected.cost.output = 1.0.into();
    expected.max_tokens_explicit = false;
    assert_eq!(parsed, vec![expected]);
    let bad = json!({"data":[{"id":original.id,"context_length":0,"top_provider":{"max_completion_tokens":12.5},"pricing":{"prompt":"1e308","completion":false}}]});
    assert_eq!(
        parse_provider_discovery(
            Provider::OpenRouter,
            &bad,
            std::slice::from_ref(&original),
            &[]
        )
        .unwrap(),
        vec![original]
    );
}

#[test]
fn merging_cannot_hide_a_malformed_fresh_envelope_behind_previous_data() {
    let previous = json!({"data":[rich_row("test/previous")]});
    for payload in [
        Value::Null,
        json!({}),
        json!({"data":{}}),
        json!({"data":"invalid"}),
    ] {
        assert!(merge_provider_discovery_payload(
            Provider::OpenRouter,
            &payload,
            &previous,
            compiled_models()
        )
        .is_err());
    }
}
