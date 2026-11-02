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
        defaults,
        is_current: None,
    }
}

fn seed_cache(owner: &ProviderCatalog, source: &ResolvedSource, payload: &serde_json::Value) {
    let normalized = normalize(source.format, payload, None).unwrap();
    let cache = owner.cache(source);
    let snapshot =
        json!({"url":source.url,"scope":source.scope(),"fetchedAt":1,"payload":normalized});
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
        owner.resolve(&[src.clone()], &[base.clone()]).0,
        vec![base.clone()]
    );
    seed_cache(
        &owner,
        &src,
        &json!({"data":[{"id":"known","name":"Remote label","context_length":45678,"api":"bad","baseUrl":"https://untrusted.test","headers":{"authorization":"bad"}},{"id":"unknown"}]}),
    );
    let (models, _) = owner.resolve(&[src], &[base.clone()]);
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
    let models = owner.resolve(&[src.clone()], &[]).0;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "new");
    assert_eq!(models[0].api, base.api);
    let mut changed = src.clone();
    changed.headers[0].1 = "new-account".into();
    assert!(owner.resolve(&[changed], &[]).0.is_empty());
    let mut changed = src;
    changed.defaults.as_mut().unwrap().api = "new-policy".into();
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
    assert_eq!(owner.resolve(&[src.clone()], &[]).1.len(), 1);
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
        .refresh(&[src.clone()], RefreshPolicy::Force, None)
        .await;
    assert_eq!(
        owner.resolve(&[src.clone()], &[base.clone()]).0[0].context_window,
        65536
    );
    owner
        .refresh(&[src.clone()], RefreshPolicy::Force, None)
        .await;
    assert_eq!(
        owner.resolve(&[src.clone()], &[base.clone()]).0[0].context_window,
        65536
    );
    owner
        .refresh(&[src.clone()], RefreshPolicy::Force, None)
        .await;
    assert_eq!(owner.resolve(&[src], &[base.clone()]).0, vec![base]);
    server.await.unwrap();
}
