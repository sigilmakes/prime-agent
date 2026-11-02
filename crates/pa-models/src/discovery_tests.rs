//! Discovery metadata is optional; authority always comes from the current local inputs.
use super::*;
use serde_json::json;

fn template(provider: &str, id: &str) -> Model {
    let mut model = crate::transports::compiled_models()
        .iter()
        .next()
        .unwrap()
        .clone();
    model.provider = provider.into();
    model.id = id.into();
    model.base_url = "https://trusted.test/inference".into();
    model
}

fn source(format: CatalogFormat, defaults: Option<Model>) -> ResolvedSource {
    ResolvedSource {
        id: "custom".into(),
        provider: "custom".into(),
        format,
        url: "https://trusted.test/models".into(),
        headers: vec![("Authorization".into(), "fixture-key".into())],
        unknown_models: defaults.map_or(UnknownModelPolicy::KnownOnly, |model| {
            UnknownModelPolicy::ConfiguredDefaults { model }
        }),
        is_current: None,
    }
}

fn seed_cache(owner: &ProviderCatalog, source: &ResolvedSource, payload: &serde_json::Value) {
    let normalized = normalize(source.format, payload, None).unwrap();
    let cache = owner.cache(source);
    let snapshot = json!({"url":format!("discovery:{}", source.identity()),"scope":source.scope(),"fetchedAt":1,"payload":normalized});
    let path = owner
        .dir
        .as_ref()
        .unwrap()
        .join(format!("discovery-{}.v1.json", source.identity()));
    std::fs::write(path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    assert!(cache.reload_from_disk(&source.scope()).is_some());
}

#[test]
fn optional_metadata_preserves_baseline_and_never_changes_transport() {
    let dir = tempfile::tempdir().unwrap();
    let owner = ProviderCatalog::new(Some(dir.path().into()));
    let base = template("custom", "known");
    let src = source(CatalogFormat::OpenRouter, None);
    assert_eq!(
        owner
            .resolve(std::slice::from_ref(&src), std::slice::from_ref(&base))
            .0,
        vec![base.clone()]
    );
    seed_cache(
        &owner,
        &src,
        &json!({"data":[{"id":"known","name":"Remote label","context_length":45678,"api":"bad","baseUrl":"https://untrusted.test","headers":{"authorization":"bad"}},{"id":"unknown"}]}),
    );
    let (models, _) = owner.resolve(&[src], std::slice::from_ref(&base));
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].context_window, 45678);
    assert_eq!(models[0].api, base.api);
    assert_eq!(models[0].base_url, base.base_url);
    assert_eq!(models[0].headers, base.headers);
}

#[test]
fn explicit_defaults_admit_unknown_ids_and_scope_changes_hide_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let owner = ProviderCatalog::new(Some(dir.path().into()));
    let base = template("custom", "family");
    let src = source(CatalogFormat::OpenAi, Some(base.clone()));
    seed_cache(&owner, &src, &json!({"data":[{"id":"new"}]}));
    let models = owner.resolve(std::slice::from_ref(&src), &[]).0;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "new");
    assert_eq!(models[0].api, base.api);
    let mut changed = src.clone();
    changed.headers[0].1 = "new-account".into();
    assert!(owner.resolve(&[changed], &[]).0.is_empty());
    let mut changed = src;
    let UnknownModelPolicy::ConfiguredDefaults { model } = &mut changed.unknown_models else {
        panic!("configured policy");
    };
    model.api = "new-policy".into();
    assert!(owner.resolve(&[changed], &[]).0.is_empty());
}

#[test]
fn malformed_or_omitted_fields_keep_prior_metadata_but_removed_ids_do_not() {
    let first=normalize(CatalogFormat::OpenRouter,&json!({"data":[{"id":"one","context_length":50000,"pricing":{"prompt":"0.000002"}},{"id":"gone"}]}),None).unwrap();
    let next = normalize(
        CatalogFormat::OpenRouter,
        &json!({"data":[{"id":"one","context_length":-5,"pricing":{"prompt":"invalid"}}]}),
        Some(&first),
    )
    .unwrap();
    let rows: Vec<Metadata> = serde_json::from_value(next).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].context, Some(50000));
    assert_eq!(rows[0].input_price, Some(2.0));
    assert!(normalize(CatalogFormat::OpenAi, &json!({"invalid":[]}), None).is_err());
}

#[test]
fn private_grants_require_current_private_snapshot_not_baseline_or_public_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let owner = ProviderCatalog::new(Some(dir.path().into()));
    let private_id = "internal/model";
    assert!(crate::prime_inference::is_private_prime_inference_model_id(
        private_id
    ));
    let mut src = source(
        CatalogFormat::PrimeInferencePrivate,
        Some(template("custom", "family")),
    );
    let payload = json!({"data":[{"id":private_id,"pricing":{"input_usd_per_mtok":1,"output_usd_per_mtok":2},"specs":{"context_window":32000,"max_output_tokens":4000}}]});
    seed_cache(&owner, &src, &payload);
    assert_eq!(owner.resolve(std::slice::from_ref(&src), &[]).1.len(), 1);
    src.is_current = Some(Arc::new(|| false));
    assert!(owner
        .resolve(&[src], &[template("custom", private_id)])
        .1
        .is_empty());
    let public = normalize(CatalogFormat::PrimeInference, &payload, None).unwrap();
    assert!(public.as_array().unwrap().is_empty());
}

#[test]
fn ollama_missing_optional_metadata_preserves_prior_but_tool_denial_is_explicit() {
    let first=normalize(CatalogFormat::Ollama,&json!({"listedIds":["one"],"deniedIds":[],"models":{"one":{"capabilities":["tools","thinking"],"model_info":{"family.context_length":12345}}}}),None).unwrap();
    let next = normalize(
        CatalogFormat::Ollama,
        &json!({"listedIds":["one"],"deniedIds":[],"models":{}}),
        Some(&first),
    )
    .unwrap();
    assert_eq!(next, first);
    let denied = normalize(
        CatalogFormat::Ollama,
        &json!({"listedIds":["one"],"deniedIds":["one"],"models":{}}),
        Some(&first),
    )
    .unwrap();
    assert_eq!(denied[0]["denied"], true);
}

#[tokio::test]
async fn scoped_refresh_retains_metadata_on_transient_failure_and_revokes_on_denial() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/models", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for (status, body) in [
            (200, json!({"data":[{"id":"known","context_length":65536}]})),
            (503, json!({})),
            (403, json!({})),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0u8; 1024];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
            assert!(request.contains("authorization: fixture-key"));
            let body = body.to_string();
            socket.write_all(format!("HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let owner = ProviderCatalog::new(Some(dir.path().into()));
    let base = template("custom", "known");
    let mut src = source(CatalogFormat::OpenRouter, None);
    src.url = endpoint;
    owner
        .refresh(std::slice::from_ref(&src), RefreshPolicy::Force, None)
        .await;
    assert_eq!(
        owner
            .resolve(std::slice::from_ref(&src), std::slice::from_ref(&base))
            .0[0]
            .context_window,
        65536
    );
    owner
        .refresh(std::slice::from_ref(&src), RefreshPolicy::Force, None)
        .await;
    assert_eq!(
        owner
            .resolve(std::slice::from_ref(&src), std::slice::from_ref(&base))
            .0[0]
            .context_window,
        65536
    );
    owner
        .refresh(std::slice::from_ref(&src), RefreshPolicy::Force, None)
        .await;
    assert_eq!(
        owner.resolve(&[src], std::slice::from_ref(&base)).0,
        vec![base]
    );
    server.await.unwrap();
}

#[test]
fn prime_optional_metadata_updates_independently_without_erasing_prices() {
    let dir = tempfile::tempdir().unwrap();
    let owner = ProviderCatalog::new(Some(dir.path().into()));
    let base = template("custom", "known");
    let src = source(CatalogFormat::PrimeInference, None);
    seed_cache(
        &owner,
        &src,
        &json!({"data":[{"id":"known","specs":{"context_window":77777},"supported_parameters":["tools","reasoning_effort"],"reasoning":{"supported_efforts":["low","high"]}}]}),
    );
    let actual = owner
        .resolve(std::slice::from_ref(&src), std::slice::from_ref(&base))
        .0;
    assert_eq!(actual[0].context_window, 77777);
    assert_eq!(actual[0].cost.input, base.cost.input);
    assert_eq!(actual[0].cost.output, base.cost.output);
    assert_eq!(actual[0].base_url, base.base_url);
    assert!(actual[0].thinking_level_map.is_some());
    let first=normalize(src.format,&json!({"data":[{"id":"known","specs":{"context_window":88888},"pricing":{"input_usd_per_mtok":9}}]}),None).unwrap();
    let next = normalize(src.format, &json!({"data":[{"id":"known"}]}), Some(&first)).unwrap();
    assert_eq!(next[0]["context"], 88888);
    assert_eq!(next[0]["input_price"], 9.0);
}

#[tokio::test]
async fn independent_sources_start_together_and_reports_keep_input_order() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut sockets = Vec::new();
        // Neither response is available until both independent requests arrive.
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0u8; 1024];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            sockets.push(socket);
        }
        for mut socket in sockets.into_iter().rev() {
            let body = r#"{"data":[{"id":"new"}]}"#;
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let owner = ProviderCatalog::new(None);
    let mut first = source(CatalogFormat::OpenAi, None);
    first.id = "first".into();
    first.url = format!("{endpoint}/first");
    let mut second = first.clone();
    second.id = "second".into();
    second.url = format!("{endpoint}/second");
    let reports = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        owner.refresh(&[first, second], RefreshPolicy::Force, None),
    )
    .await
    .unwrap();
    assert_eq!(
        reports
            .iter()
            .map(|report| report.source.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
    server.await.unwrap();
}

#[test]
fn discovery_routes_preserve_gateway_path_port_and_query() {
    let url = CatalogFormat::OpenAi
        .discovery_url("https://gateway.test:8443/prefix/v1?tenant=a")
        .unwrap();
    assert_eq!(url, "https://gateway.test:8443/prefix/v1/models?tenant=a");
    let url = CatalogFormat::Google
        .discovery_url("https://gateway.test/prefix/v1beta?tenant=a")
        .unwrap();
    assert_eq!(
        url,
        "https://gateway.test/prefix/v1beta/models?tenant=a&pageSize=1000"
    );
}

#[test]
fn codex_routes_and_reported_capabilities_are_not_template_guesses() {
    let url = CatalogFormat::Codex
        .discovery_url("https://gateway.test:8443/prefix/codex/responses?tenant=a")
        .unwrap();
    assert_eq!(
        url,
        "https://gateway.test:8443/prefix/codex/models?tenant=a&client_version=0.159.0"
    );
    let rows=normalize(CatalogFormat::Codex,&json!({"models":[{"slug":"actual","visibility":"list","input_modalities":["text","image"],"supported_reasoning_levels":[{"effort":"low"},{"effort":"xhigh"}]},{"slug":"hidden","visibility":"hide"}]}),None).unwrap();
    let rows: Vec<Metadata> = serde_json::from_value(rows).unwrap();
    let actual = rows.iter().find(|row| row.id == "actual").unwrap();
    assert_eq!(
        actual.input,
        Some(vec![ModelInput::Text, ModelInput::Image])
    );
    assert_eq!(actual.reasoning, Some(true));
    let map = actual.thinking.as_ref().unwrap();
    assert_eq!(
        map[&pa_types::ai::ModelThinkingLevel::Low],
        Some("low".into())
    );
    assert_eq!(map[&pa_types::ai::ModelThinkingLevel::High], None);
    assert!(rows.iter().find(|row| row.id == "hidden").unwrap().denied);
}

#[test]
fn private_zero_prices_and_reasoning_controls_survive_durable_reload() {
    let dir = tempfile::tempdir().unwrap();
    let source = source(
        CatalogFormat::PrimeInferencePrivate,
        Some(template("custom", "family")),
    );
    let payload = json!({"data":[{"id":"internal/zero","pricing":{"input_usd_per_mtok":0,"output_usd_per_mtok":0},"supported_parameters":["tools","reasoning_effort"],"reasoning":{"supported_efforts":["low","high"]}}]});
    let owner = ProviderCatalog::new(Some(dir.path().into()));
    seed_cache(&owner, &source, &payload);
    let (_, before) = owner.resolve(std::slice::from_ref(&source), &[]);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].cost.input, JsNumber::from(0.0));
    assert_eq!(before[0].cost.output, JsNumber::from(0.0));
    assert!(before[0].thinking_level_map.is_some());
    let fresh = ProviderCatalog::new(Some(dir.path().into()));
    assert!(fresh
        .adopt(std::slice::from_ref(&source), None)
        .applied
        .contains(&source.id));
    assert_eq!(fresh.resolve(&[source], &[]).1, before);
}

#[test]
fn all_id_only_adapters_need_trusted_models_and_reject_nonchat_metadata() {
    for format in [
        CatalogFormat::OpenAi,
        CatalogFormat::Anthropic,
        CatalogFormat::Google,
    ] {
        let base = template("custom", "known");
        let payload = if format == CatalogFormat::Google {
            json!({"models":[{"name":"models/known","supportedGenerationMethods":["generateContent"],"inputTokenLimit":500_000.0},{"name":"models/unknown"},{"name":"models/embed","supportedGenerationMethods":["embedContent"]}]})
        } else {
            json!({"data":[{"id":"known","display_name":"Known"},{"id":"unknown"}]})
        };
        let dir = tempfile::tempdir().unwrap();
        let owner = ProviderCatalog::new(Some(dir.path().into()));
        let src = source(format, None);
        seed_cache(&owner, &src, &payload);
        let models = owner.resolve(&[src], std::slice::from_ref(&base)).0;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "known");
        assert_eq!(models[0].api, base.api);
        if format == CatalogFormat::Google {
            assert_eq!(models[0].context_window, 500_000);
        }
    }
}

#[test]
fn normalized_partial_metadata_drops_transport_fields_and_bad_fresh_envelopes_fail() {
    let first=normalize(CatalogFormat::OpenRouter,&json!({"data":[{"id":"one","context_length":32000.0,"pricing":{"prompt":"0.000001","completion":"0.000002","secret":"attacker"},"top_provider":{"baseUrl":"attacker"},"architecture":{"input_modalities":["text"],"headers":{"authorization":"attacker"}},"headers":{"authorization":"attacker"}}]}),None).unwrap();
    assert!(!first.to_string().contains("attacker"));
    let next = normalize(
        CatalogFormat::OpenRouter,
        &json!({"data":[{"id":"one","pricing":{"prompt":"Infinity"},"context_length":-3}]}),
        Some(&first),
    )
    .unwrap();
    assert_eq!(next[0]["context"], 32000);
    assert_eq!(next[0]["input_price"], 1.0);
    for malformed in [
        json!(null),
        json!([]),
        json!({"data":{}}),
        json!({"data":[{"id":"bad\n"}]}),
    ] {
        assert!(normalize(CatalogFormat::OpenRouter, &malformed, Some(&first)).is_err());
    }
    let denied = normalize(
        CatalogFormat::OpenRouter,
        &json!({"data":[{"id":"one","architecture":{"output_modalities":["audio"]}}]}),
        Some(&first),
    )
    .unwrap();
    assert_eq!(denied[0]["denied"], true);
}

#[test]
fn automatic_unknown_models_require_complete_actual_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let owner = ProviderCatalog::new(Some(dir.path().into()));
    let mut src = source(CatalogFormat::OpenRouter, None);
    src.unknown_models = UnknownModelPolicy::ProviderMetadata {
        template: template("custom", "family"),
    };
    let complete = Metadata {
        id: "new".into(),
        context: Some(32000),
        max_tokens: Some(4096),
        input: Some(vec![ModelInput::Text]),
        reasoning: Some(false),
        input_price: Some(1.0),
        output_price: Some(2.0),
        cache_read: Some(0.5),
        cache_write: Some(1.5),
        ..Default::default()
    };
    for missing in 0..5 {
        let mut row = complete.clone();
        match missing {
            0 => row.input_price = None,
            1 => row.output_price = None,
            2 => row.cache_read = None,
            3 => row.cache_write = None,
            _ => {}
        }
        let path = dir
            .path()
            .join(format!("discovery-{}.v1.json", src.identity()));
        std::fs::write(
            path,
            serde_json::to_vec(&json!({
                "url":format!("discovery:{}", src.identity()), "scope":src.scope(),
                "fetchedAt":missing+1, "payload":[row]
            }))
            .unwrap(),
        )
        .unwrap();
        owner.cache(&src).reload_from_disk(&src.scope());
        let models = owner.resolve(std::slice::from_ref(&src), &[]).0;
        assert_eq!(models.len(), usize::from(missing == 4));
        if let Some(model) = models.first() {
            assert_eq!(model.cost.input, JsNumber::from(1.0));
            assert_eq!(model.cost.output, JsNumber::from(2.0));
            assert_eq!(model.cost.cache_read, JsNumber::from(0.5));
            assert_eq!(model.cost.cache_write, JsNumber::from(1.5));
        }
    }
}

#[test]
fn discovery_route_rejects_embedded_userinfo_and_fragments() {
    for url in [
        "https://user:secret@host.test/v1",
        "https://host.test/v1#secret",
    ] {
        assert!(CatalogFormat::OpenAi.discovery_url(url).is_none());
    }
}

#[test]
fn openrouter_complete_published_metadata_admits_unknown_but_id_only_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let owner = ProviderCatalog::new(Some(dir.path().into()));
    let mut src = source(CatalogFormat::OpenRouter, None);
    src.unknown_models = UnknownModelPolicy::ProviderMetadata {
        template: template("custom", "family"),
    };
    seed_cache(
        &owner,
        &src,
        &json!({"data":[
            {"id":"unpriced"},
            {"id":"priced", "context_length":32000,
             "top_provider":{"max_completion_tokens":4096},
             "architecture":{"input_modalities":["text"], "output_modalities":["text"]},
             "supported_parameters":["reasoning_effort"],
             "pricing":{"prompt":"0.000001", "completion":"0.000002", "input_cache_read":"0.0000005", "input_cache_write":"0.0000015"}}
        ]}),
    );
    let models = owner.resolve(&[src], &[]).0;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "priced");
    assert!(models[0].reasoning);
    assert_eq!(models[0].cost.input, JsNumber::from(1.0));
    for (parameters, expected) in [
        (json!([]), Some(false)),
        (json!(["reasoning"]), Some(true)),
        (json!(null), None),
    ] {
        let rows = normalize_rows(
            CatalogFormat::OpenRouter,
            &json!({"data":[{"id":"known", "supported_parameters":parameters}]}),
        )
        .unwrap();
        assert_eq!(rows[0].reasoning, expected);
    }
}
