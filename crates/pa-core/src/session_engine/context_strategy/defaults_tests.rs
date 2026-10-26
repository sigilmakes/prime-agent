//! Behavioral coverage of builtin defaults through an isolated public engine.
use super::*;

async fn configure(f: &Fixture, model: &pa_types::ai::Model, plugins: Value) {
    std::fs::write(
        f.agent.join("settings.json"),
        json!({"harnessRefinements":false,"enableBuiltinSkills":false,
            "compaction":{"keepRecentTokens":0},"plugins":plugins})
        .to_string(),
    )
    .unwrap();
    f.engine.reload_plugins().await.unwrap();
    f.engine
        .session
        .set_model(model, &model.provider, &model.id)
        .await
        .unwrap();
}

#[tokio::test]
async fn unsupported_server_defaults_to_snap_for_known_vision_or_standard_for_text() {
    for vision in [false, true] {
        let (base, mut requests, server) =
            json_server(r#"{"error":{"code":"unsupported_operation"}}"#, 404).await;
        let f = Fixture::new(ORDERED, "default_context/rolling", true).await;
        let mut model = f.model.clone();
        model.api = "openai-responses".into();
        model.provider = "local-gateway".into();
        model.id = "gpt-5".into();
        model.base_url = base;
        if vision {
            model.input.push(pa_types::ai::ModelInput::Image);
        }
        configure(&f, &model, json!({})).await;
        let before = f.engine.session.entries().await;
        let CompactOutcome::Ran(run) = f
            .engine
            .session
            .compact(None, &model, Some("local-fake-key".into()), None)
            .await
            .unwrap()
        else {
            panic!("default chain did not accept a proposal")
        };
        let archive = run.entry.context_archive.unwrap();
        assert_eq!(
            archive.compactor,
            if vision {
                "snapcompact/snapcompact"
            } else {
                "standard_compaction/standard"
            }
        );
        let after = f.engine.session.entries().await;
        assert_eq!(&after[..before.len()], before.as_slice());
        assert_eq!(after.len(), before.len() + 1);
        let compact = requests.recv().await.unwrap();
        assert!(compact.starts_with("POST /v1/responses/compact "));
        if vision {
            // Snap renders locally. There must not be a summary inference call.
            assert!(requests.try_recv().is_err());
            server.abort();
        } else {
            let summary = requests.recv().await.unwrap();
            assert!(summary.starts_with("POST /v1/responses "));
            assert!(run.result.summary.contains("local result"));
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn server_rejections_and_uncertain_outcomes_never_replay_or_mutate_history() {
    for (status, body) in [
        (401, r#"{"error":{"code":"invalid_api_key"}}"#),
        (403, r#"{"error":{"code":"forbidden"}}"#),
        (404, r#"{"error":{"code":"model_not_found"}}"#),
        (429, r#"{"error":{"code":"rate_limit_exceeded"}}"#),
        (503, r#"{"error":{"code":"server_error"}}"#),
        (200, "not a compact response"),
    ] {
        let (base, mut requests, server) = json_server(body, status).await;
        let f = Fixture::new(ORDERED, "default_context/rolling", true).await;
        let mut model = f.model.clone();
        model.api = "openai-responses".into();
        model.base_url = base;
        model.id = "gpt-5".into();
        model.input.push(pa_types::ai::ModelInput::Image);
        configure(&f, &model, json!({})).await;
        let before = f.state().await;
        let error = f
            .engine
            .session
            .compact(None, &model, Some("local-fake-key".into()), None)
            .await
            .unwrap_err();
        assert!(error
            .downcast_ref::<pa_ai::compaction::CompactionError>()
            .is_some());
        assert_eq!(f.state().await, before);
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
        server.abort();
    }
}

#[tokio::test]
async fn explicit_chain_and_disabled_compactors_override_builtin_defaults() {
    for plugins in [
        json!({"default_context":{"config":{"compactors":["standard_compaction/standard"]}},
            "snapcompact":{"config":{"profile":"explicit"}}}),
        json!({"server_compaction":{"enabled":false}, "snapcompact":{"enabled":false}}),
    ] {
        let f = Fixture::new(ORDERED, "default_context/rolling", false).await;
        let faux = pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            api: Some(format!("default-overrides-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        });
        faux.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
            pa_ai::faux::faux_assistant_text_message(
                "explicit summary",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ),
        )]);
        let mut model = faux.get_model();
        model.input.push(pa_types::ai::ModelInput::Image);
        configure(&f, &model, plugins).await;
        let CompactOutcome::Ran(run) = f
            .engine
            .session
            .compact(None, &model, None, None)
            .await
            .unwrap()
        else {
            panic!("standard override was not selected")
        };
        assert_eq!(
            run.entry.context_archive.unwrap().compactor,
            "standard_compaction/standard"
        );
        assert!(run.result.summary.contains("explicit summary"));
    }
}

#[tokio::test]
async fn default_server_cancellation_and_retirement_close_pending_request_without_fallback() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for retire in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let (ready_tx, ready) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..end]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            // Advertise an unfinished body so the request remains in flight.
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n{")
                .await
                .unwrap();
            ready_tx.send(()).unwrap();
            let result = socket.read(&mut chunk).await;
            assert!(
                matches!(result, Ok(0) | Err(_)),
                "request must close on retirement/abort"
            );
        });
        let f = Fixture::new(ORDERED, "default_context/rolling", true).await;
        let mut model = f.model.clone();
        model.api = "openai-responses".into();
        model.base_url = base;
        model.id = "gpt-5".into();
        model.input.push(pa_types::ai::ModelInput::Image);
        configure(&f, &model, json!({})).await;
        let before = f.state().await;
        let abort = pa_agent::abort::AbortController::new();
        let signal = abort.signal();
        let engine = Arc::clone(&f.engine);
        let task = tokio::spawn(async move {
            engine
                .session
                .compact(None, &model, Some("local-fake-key".into()), Some(&signal))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), ready)
            .await
            .unwrap()
            .unwrap();
        if retire {
            std::fs::write(
                f.agent.join("settings.json"),
                json!({
                    "harnessRefinements":false, "enableBuiltinSkills":false,
                    "plugins":{"server_compaction":{"enabled":false}}
                })
                .to_string(),
            )
            .unwrap();
            f.engine.reload_plugins().await.unwrap();
        } else {
            abort.abort();
        }
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(if retire {
            error.to_string().contains("retired")
        } else {
            pa_agent::abort::is_abort_error(&error)
        });
        assert_eq!(f.state().await, before);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn explicit_server_route_mismatch_is_terminal_and_null_disables_probe() {
    for disable in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let f = Fixture::new(ORDERED, "default_context/rolling", false).await;
        let mut model = f.model.clone();
        model.api = "openai-responses".into();
        model.base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        model.id = "gpt-5".into();
        model.input.push(pa_types::ai::ModelInput::Image);
        let route = if disable {
            Value::Null
        } else {
            json!({"provider":model.provider,"model":"wrong-model",
                "endpoint":format!("{}/responses/compact",model.base_url)})
        };
        configure(
            &f,
            &model,
            json!({"server_compaction":{"config":{"route":route}}}),
        )
        .await;
        let before = f.state().await;
        let result = f
            .engine
            .session
            .compact(None, &model, Some("local-fake-key".into()), None)
            .await;
        if disable {
            let CompactOutcome::Ran(run) = result.unwrap() else {
                panic!("Snap did not run")
            };
            assert_eq!(
                run.entry.context_archive.unwrap().compactor,
                "snapcompact/snapcompact"
            );
        } else {
            assert!(matches!(
                result
                    .unwrap_err()
                    .downcast_ref::<pa_ai::compaction::CompactionError>(),
                Some(pa_ai::compaction::CompactionError::RouteMismatch)
            ));
            assert_eq!(f.state().await, before);
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[tokio::test]
async fn codex_default_uses_snap_without_probing_unsupported_native_transport() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let f = Fixture::new(ORDERED, "default_context/rolling", false).await;
    let mut model = f.model.clone();
    model.api = "openai-codex-responses".into();
    model.provider = "openai-codex".into();
    model.id = "gpt-6.1-sol".into();
    model.base_url = format!(
        "http://{}/backend-api/codex",
        listener.local_addr().unwrap()
    );
    model.input.push(pa_types::ai::ModelInput::Image);
    configure(&f, &model, json!({})).await;
    let CompactOutcome::Ran(run) = f
        .engine
        .session
        .compact(None, &model, None, None)
        .await
        .unwrap()
    else {
        panic!("Codex vision route did not select Snapcompact")
    };
    assert_eq!(
        run.entry.context_archive.unwrap().compactor,
        "snapcompact/snapcompact"
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn default_server_inherits_resolved_auth_headers_without_compaction_settings() {
    let (base, mut requests, server) =
        json_server(r#"{"error":{"code":"unsupported_operation"}}"#, 404).await;
    let f = Fixture::new(ORDERED, "default_context/rolling", false).await;
    std::fs::write(f.agent.join("auth.json"), json!({"prime-inference": {
        "type":"api_key", "key":"local-stored-key", "primeTeam":{"teamId":"local-team", "name":"Local"}
    }}).to_string()).unwrap();
    let mut model = f.model.clone();
    model.api = "openai-responses".into();
    model.provider = "prime-inference".into();
    model.id = "gpt-5".into();
    model.base_url = base;
    model.headers = Some(std::collections::BTreeMap::from([(
        "X-Model-Route".into(),
        "local-route".into(),
    )]));
    model.input.push(pa_types::ai::ModelInput::Image);
    configure(&f, &model, json!({})).await;
    let CompactOutcome::Ran(run) = f
        .engine
        .session
        .compact(None, &model, Some("local-explicit-key".into()), None)
        .await
        .unwrap()
    else {
        panic!("unsupported server did not fall through to Snapcompact")
    };
    assert_eq!(
        run.entry.context_archive.unwrap().compactor,
        "snapcompact/snapcompact"
    );
    let wire = requests.recv().await.unwrap();
    let head = wire.split_once("\r\n\r\n").unwrap().0.to_ascii_lowercase();
    for expected in [
        "x-model-route: local-route",
        "x-prime-team-id: local-team",
        "authorization: bearer local-explicit-key",
    ] {
        assert!(head.contains(expected), "missing inherited {expected}");
    }
    assert!(!head.contains("local-stored-key"));
    assert!(requests.try_recv().is_err());
    server.abort();
}
