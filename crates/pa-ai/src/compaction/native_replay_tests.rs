//! End-to-end archive replay through the public inference facade and real HTTP/SSE.
use super::*;
use crate::types::{Context, Model, SimpleStreamOptions, StopReason, StreamOptions};
use futures::FutureExt;
use std::fmt::Write as _;
use std::sync::Arc;

// Unknown retained variants, non-compaction items, escapes, spacing and numeric
// spellings all survive. A ciphertext-only or Value-based transport must fail.
const NATIVE: &str = r#"[ {"role":"user","content":"retained \u00e9","future":1e+02}, {"type":"future_native_variant","raw":{"n":-0.00,"escaped":"\/\u0061"}}, {"type":"function_call","call_id":"c1","name":"tool","arguments":"{}"}, {"type":"compaction","id":"cmp_wire","encrypted_content":"opaque=="} ]"#;
const FRESH: &str = r#"[{"type":"function_call_output","call_id":"c1","output":"fresh tool \u0061"},{"role":"user","content":"fresh user","n":2e-01}]"#;

fn inference_model(route: &CompactionRoute) -> Model {
    serde_json::from_value(json!({
        "id": route.model, "name":"local", "api":"openai-responses", "provider":route.provider,
        "baseUrl":route.endpoint.strip_suffix("/responses/compact").unwrap(),
        "reasoning":false,"input":["text"],
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
        "contextWindow":100_000,"maxTokens":1000
    }))
    .unwrap()
}

fn inference_options(window: &ResponsesWindow, adapter: &ResponsesCompactor) -> StreamOptions {
    StreamOptions {
        api_key: Some("local-inference-key".into()),
        native_responses_input: Some(
            window
                .inference_input(adapter, &CancellationToken::new())
                .unwrap(),
        ),
        session_id: Some("native-session".into()),
        headers: Some(std::collections::HashMap::from([(
            "X-Local-Test".into(),
            "set".into(),
        )])),
        ..StreamOptions::default()
    }
}

fn sse() -> String {
    let events = [
        json!({"type":"response.created","response":{"id":"resp_inference"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_wire","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"local result"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_wire","type":"message","role":"assistant","content":[{"type":"output_text","text":"local result"}]}}),
        json!({"type":"response.completed","response":{"id":"resp_inference","status":"completed","model":"gpt-5.2","usage":{"input_tokens":30,"output_tokens":2,"total_tokens":32}}}),
    ];
    let mut body = String::new();
    for event in events {
        writeln!(body, "data: {event}\n").unwrap();
    }
    body
}

fn assert_raw_input(body: &str, expected: &str) {
    // Independently locate the array bytes in a validated outbound request.
    // No Value reserialization, substring containment, or production scanner.
    let _: Value = serde_json::from_str(body).unwrap();
    let start = body.find("\"input\":").unwrap() + "\"input\":".len();
    let mut quoted = false;
    let mut escaped = false;
    let mut depth = 0;
    for (offset, byte) in body.as_bytes()[start..].iter().copied().enumerate() {
        if escaped {
            escaped = false;
        } else if quoted && byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            quoted = !quoted;
        } else if !quoted {
            if byte == b'[' {
                depth += 1;
            } else if byte == b']' {
                depth -= 1;
                if depth == 0 {
                    let actual = &body[start..=start + offset];
                    assert_eq!(
                        serde_json::from_str::<Value>(actual).unwrap(),
                        serde_json::from_str::<Value>(expected).unwrap(),
                        "native replay must retain every item in order"
                    );
                    assert_eq!(actual, expected, "native item bytes must remain exact");
                    return;
                }
            }
        }
    }
    panic!("missing native input array end");
}

#[tokio::test]
async fn native_archive_reaches_real_inference_twice_without_normalization_or_item_loss() {
    let mut server = fake_responses(vec![
        (200, envelope(NATIVE), Mode::Complete),
        (200, sse(), Mode::Complete),
        (200, envelope(RETAINED), Mode::Complete),
        (200, sse(), Mode::Complete),
    ])
    .await;
    let adapter = supported_adapter(&server.route);
    let cancel = CancellationToken::new();
    let candidate = bounded(adapter.compact(&input(&server.route), options(), &cancel))
        .await
        .unwrap();
    let serialized = candidate.serialize().unwrap();
    assert!(!serialized.contains("local-fake-key"));
    assert!(!serialized.contains("local-inference-key"));
    let restored = CompactionCandidate::restore(&serialized, &adapter, &cancel).unwrap();
    let window = restored
        .replay(&adapter, &cancel)
        .unwrap()
        .append_input(FRESH, CompactionLimits::default())
        .unwrap();
    let model = inference_model(&server.route);
    let mut opts = inference_options(&window, &adapter);
    opts.on_payload = Some(Arc::new(|mut payload, _| {
        payload["temperature"] = json!(0.125);
        Some(payload)
    }));
    let context = Context {
        system_prompt: Some("current mandatory instructions".into()),
        ..Context::default()
    };
    let response = bounded(crate::complete_simple(
        &model,
        &context,
        Some(SimpleStreamOptions::from_base(opts)),
    ))
    .await
    .unwrap();
    assert_eq!(response.stop_reason, StopReason::Stop);
    assert_eq!(response.response_id.as_deref(), Some("resp_inference"));
    assert_eq!(response.content.len(), 1);
    assert!(
        matches!(&response.content[0], crate::types::AssistantContent::Text(text) if text.text == "local result")
    );
    assert_eq!(response.usage.input, 30);
    let first = bounded(server.requests.recv()).await.unwrap();
    assert!(first
        .head
        .starts_with("POST /v1/responses/compact HTTP/1.1"));
    let wire = bounded(server.requests.recv()).await.unwrap();
    assert!(wire.head.starts_with("POST /v1/responses HTTP/1.1"));
    let head = wire.head.to_ascii_lowercase();
    for expected in [
        "authorization: bearer local-inference-key",
        "session_id: native-session",
        "x-client-request-id: native-session",
        "x-local-test: set",
        "content-type: application/json",
    ] {
        assert!(head.contains(expected), "missing {expected}");
    }
    assert_raw_input(&wire.body, window.input_json());
    assert!(wire
        .body
        .contains(r#""instructions":"current mandatory instructions""#));
    assert!(wire.body.contains(r#""temperature":0.125"#));

    // A second compact covers this exact window (not just its encrypted item).
    let second = bounded(adapter.compact(&window, options(), &cancel))
        .await
        .unwrap();
    let next = CompactionCandidate::restore(&second.serialize().unwrap(), &adapter, &cancel)
        .unwrap()
        .replay(&adapter, &cancel)
        .unwrap()
        .append_input(
            r#"[{"role":"user","content":"second continuation"}]"#,
            CompactionLimits::default(),
        )
        .unwrap();
    let response = bounded(crate::complete(
        &model,
        &Context::default(),
        Some(inference_options(&next, &adapter)),
    ))
    .await
    .unwrap();
    assert_eq!(response.stop_reason, StopReason::Stop);
    let compact_wire = bounded(server.requests.recv()).await.unwrap();
    assert_raw_input(&compact_wire.body, window.input_json());
    let second_wire = bounded(server.requests.recv()).await.unwrap();
    assert_raw_input(&second_wire.body, next.input_json());
    assert!(next.input_json().contains("second continuation"));
    bounded(&mut server.task).await.unwrap();
}

#[tokio::test]
async fn native_route_capability_and_credentials_fail_before_network() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let route = CompactionRoute {
        provider: "explicit-local".into(),
        endpoint: format!(
            "http://{}/v1/responses/compact",
            listener.local_addr().unwrap()
        ),
        model: "gpt-5.2".into(),
    };
    let adapter = supported_adapter(&route);
    let window = input(&route);
    let unsupported =
        ResponsesCompactor::new(route.clone(), None, CompactionLimits::default()).unwrap();
    assert!(matches!(
        window.inference_input(&unsupported, &CancellationToken::new()),
        Err(CompactionError::Unsupported)
    ));
    let mut wrong_route = route.clone();
    wrong_route.model = "other".into();
    assert!(matches!(
        window.inference_input(&supported_adapter(&wrong_route), &CancellationToken::new()),
        Err(CompactionError::RouteMismatch)
    ));
    let model = inference_model(&route);
    for field in [
        "provider",
        "endpoint",
        "model",
        "codex",
        "azure",
        "completions",
        "unknown",
    ] {
        let mut changed = model.clone();
        match field {
            "provider" => changed.provider = "other".into(),
            "endpoint" => changed.base_url.push_str("/other"),
            "model" => changed.id = "other".into(),
            "codex" => changed.api = "openai-codex-responses".into(),
            "azure" => changed.api = "azure-openai-responses".into(),
            "completions" => changed.api = "openai-completions".into(),
            "unknown" => changed.api = "unknown-local-transport".into(),
            _ => unreachable!(),
        }
        assert!(
            crate::stream(
                &changed,
                &Context::default(),
                Some(inference_options(&window, &adapter))
            )
            .is_err(),
            "accepted {field}"
        );
        assert!(
            crate::stream_simple(
                &changed,
                &Context::default(),
                Some(SimpleStreamOptions::from_base(inference_options(
                    &window, &adapter
                )))
            )
            .is_err(),
            "accepted simple {field}"
        );
    }
    let mut opts = inference_options(&window, &adapter);
    opts.api_key = None;
    assert!(crate::stream(&model, &Context::default(), Some(opts)).is_err());
    let context: Context = serde_json::from_value(json!({"messages":[{"role":"user","content":"must not be silently ignored","timestamp":0}]})).unwrap();
    assert!(crate::stream(&model, &context, Some(inference_options(&window, &adapter))).is_err());
    // A cancelled bridge never grants an option, and a cancelled invocation emits Aborted.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        window.inference_input(&adapter, &cancelled),
        Err(CompactionError::Cancelled)
    ));
    let mut opts = inference_options(&window, &adapter);
    opts.signal = Some(cancelled);
    assert_eq!(
        bounded(crate::complete(&model, &Context::default(), Some(opts)))
            .await
            .unwrap()
            .stop_reason,
        StopReason::Aborted
    );
    assert!(listener.accept().now_or_never().is_none());
}

#[tokio::test]
async fn conflicting_payload_hooks_are_errors_not_native_input_rewrites() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let route = CompactionRoute {
        provider: "explicit-local".into(),
        endpoint: format!(
            "http://{}/v1/responses/compact",
            listener.local_addr().unwrap()
        ),
        model: "gpt-5.2".into(),
    };
    let adapter = supported_adapter(&route);
    let window = input(&route);
    for conflict in [
        "input",
        "model",
        "previous_response_id",
        "conversation",
        "erase",
    ] {
        let mut opts = inference_options(&window, &adapter);
        opts.on_payload = Some(Arc::new(move |mut payload, _| {
            if conflict == "erase" {
                return Some(json!({"model":"gpt-5.2"}));
            }
            payload[conflict] = json!("conflict");
            Some(payload)
        }));
        let response = bounded(crate::complete(
            &inference_model(&route),
            &Context::default(),
            Some(opts),
        ))
        .await
        .unwrap();
        assert_eq!(response.stop_reason, StopReason::Error);
        assert!(response
            .error_message
            .unwrap()
            .contains("Payload hook conflicts"));
    }
    assert!(listener.accept().now_or_never().is_none());
}

#[tokio::test]
async fn native_inference_abort_and_drop_close_pending_http_without_late_results() {
    for abort in [true, false] {
        let mut server = fake(200, sse(), Mode::UntilDisconnect).await;
        let adapter = supported_adapter(&server.route);
        let window = input(&server.route);
        let signal = CancellationToken::new();
        let mut opts = inference_options(&window, &adapter);
        opts.signal = Some(signal.clone());
        let stream = crate::stream(
            &inference_model(&server.route),
            &Context::default(),
            Some(opts),
        )
        .unwrap();
        bounded(&mut server.ready).await.unwrap();
        if abort {
            signal.cancel();
            assert_eq!(
                bounded(stream.result()).await.stop_reason,
                StopReason::Aborted
            );
        } else {
            drop(stream);
            assert!(
                !signal.is_cancelled(),
                "retiring a call must not cancel its parent's token"
            );
        }
        bounded(&mut server.disconnected).await.unwrap();
    }
}

#[tokio::test]
async fn ordinary_responses_still_convert_messages_and_allow_payload_hooks() {
    let mut server = fake(200, sse(), Mode::Complete).await;
    let model = inference_model(&server.route);
    let context:Context = serde_json::from_value(json!({"systemPrompt":"system","messages":[{"role":"user","content":"ordinary input","timestamp":0}]})).unwrap();
    let opts = StreamOptions {
        api_key: Some("ordinary-key".into()),
        on_payload: Some(Arc::new(|mut payload, _| {
            assert!(payload["input"].as_array().unwrap().len() >= 2);
            payload["model"] = json!("hook-selected-model");
            payload["input"] = json!([{"role":"user","content":"hook input"}]);
            Some(payload)
        })),
        ..StreamOptions::default()
    };
    let response = bounded(crate::complete(&model, &context, Some(opts)))
        .await
        .unwrap();
    assert_eq!(response.stop_reason, StopReason::Stop);
    let request = bounded(server.requests.recv()).await.unwrap();
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["model"], json!("hook-selected-model"));
    assert_eq!(
        body["input"],
        json!([{"role":"user","content":"hook input"}])
    );
    assert!(request
        .head
        .to_ascii_lowercase()
        .contains("authorization: bearer ordinary-key"));
}

#[tokio::test]
async fn fresh_portable_conversion_excludes_instructions_and_preserves_tool_results_on_wire() {
    let mut server = fake_responses(vec![
        (200, envelope(NATIVE), Mode::Complete),
        (200, sse(), Mode::Complete),
    ])
    .await;
    let model = inference_model(&server.route);
    let adapter = supported_adapter(&server.route);
    let cancel = CancellationToken::new();
    let initial: Context =
        serde_json::from_value(json!({"systemPrompt":"not an archived prefix", "messages":[
            {"role":"user","content":"initial portable","timestamp":0}
        ]}))
        .unwrap();
    let initial_json = responses_input_json(&model, &initial.messages).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&initial_json).unwrap(),
        json!([
            {"role":"user","content":[{"type":"input_text","text":"initial portable"}]}
        ])
    );
    let window = ResponsesWindow::from_stateless_input(
        &server.route,
        &initial_json,
        CompactionLimits::default(),
    )
    .unwrap();
    let candidate = bounded(adapter.compact(&window, options(), &cancel))
        .await
        .unwrap();
    let fresh: Context = serde_json::from_value(json!({"messages":[
        {"role":"toolResult","toolCallId":"c1","toolName":"tool","content":[{"type":"text","text":"portable tool result"}],"isError":false,"timestamp":0},
        {"role":"user","content":"portable continuation","timestamp":0}
    ]})).unwrap();
    let tool_only_json = responses_input_json(&model, &fresh.messages[..1]).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&tool_only_json).unwrap(),
        json!([{"type":"function_call_output","call_id":"c1","output":"portable tool result"}])
    );
    let mut assistant =
        crate::event_stream::initial_assistant_message(&model.api, &model.provider, &model.id);
    assistant.stop_reason = StopReason::ToolUse;
    assistant
        .content
        .push(crate::types::AssistantContent::ToolCall(
            crate::types::ToolCall {
                id: "fresh_call".into(),
                name: "tool".into(),
                arguments: serde_json::Map::new(),
                thought_signature: None,
                rest: serde_json::Map::new(),
            },
        ));
    let call_only_json =
        responses_input_json(&model, &[crate::types::Message::Assistant(assistant)]).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&call_only_json).unwrap(),
        json!([{"type":"function_call","call_id":"fresh_call","name":"tool","arguments":"{}"}]),
        "fresh conversion must not fabricate a synthetic tool result"
    );
    let fresh_json = responses_input_json(&model, &fresh.messages).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&fresh_json).unwrap(),
        json!([
            {"type":"function_call_output","call_id":"c1","output":"portable tool result"},
            {"role":"user","content":[{"type":"input_text","text":"portable continuation"}]}
        ])
    );
    let native = candidate
        .replay(&adapter, &cancel)
        .unwrap()
        .append_input(&tool_only_json, CompactionLimits::default())
        .unwrap()
        .append_input(
            &responses_input_json(&model, &fresh.messages[1..]).unwrap(),
            CompactionLimits::default(),
        )
        .unwrap();
    let context:Context = serde_json::from_value(json!({"systemPrompt":"current instructions","messages":[],"tools":[{"name":"tool","description":"local","parameters":{"type":"object"}}]})).unwrap();
    let result = bounded(crate::complete(
        &model,
        &context,
        Some(inference_options(&native, &adapter)),
    ))
    .await
    .unwrap();
    assert_eq!(result.stop_reason, StopReason::Stop);
    let compact = bounded(server.requests.recv()).await.unwrap();
    assert!(!compact.body.contains("not an archived prefix"));
    let wire = bounded(server.requests.recv()).await.unwrap();
    assert_raw_input(&wire.body, native.input_json());
    let payload: Value = serde_json::from_str(&wire.body).unwrap();
    assert_eq!(payload["instructions"], json!("current instructions"));
    assert_eq!(
        payload["tools"],
        json!([{"type":"function","name":"tool","description":"local","parameters":{"type":"object"},"strict":false}])
    );
}

#[tokio::test]
async fn opaque_native_wire_size_does_not_silently_clamp_output_budget() {
    for opaque_size in [16, 2800] {
        let retained = format!(
            r#"[{{"type":"compaction","id":"cmp_budget","encrypted_content":"{}"}}]"#,
            "a".repeat(opaque_size)
        );
        let mut server = fake_responses(vec![
            (200, envelope(&retained), Mode::Complete),
            (200, sse(), Mode::Complete),
        ])
        .await;
        let adapter = supported_adapter(&server.route);
        let mut model = inference_model(&server.route);
        model.context_window = 1000;
        let cancel = CancellationToken::new();
        let candidate = bounded(adapter.compact(&input(&server.route), options(), &cancel))
            .await
            .unwrap();
        let window = candidate.replay(&adapter, &cancel).unwrap();
        let mut opts = inference_options(&window, &adapter);
        opts.max_tokens = Some(500);
        let result = bounded(crate::complete_simple(
            &model,
            &Context::default(),
            Some(SimpleStreamOptions::from_base(opts)),
        ))
        .await
        .unwrap();
        assert_eq!(result.stop_reason, StopReason::Stop);
        let _compact = bounded(server.requests.recv()).await.unwrap();
        let wire = bounded(server.requests.recv()).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&wire.body).unwrap()["max_output_tokens"],
            json!(500)
        );
    }
}
