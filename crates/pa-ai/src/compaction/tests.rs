//! Tests cross the production adapter and an isolated local HTTP socket. No
//! provider credentials, catalog inference, repair parser, or session engine.
use super::*;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

const USAGE: &str = r#"{ "input_tokens":139,"input_tokens_details":{"cached_tokens":7,"cache_write_tokens":2,"future":true},"output_tokens":438,"output_tokens_details":{"reasoning_tokens":64},"total_tokens":577 }"#;
const RETAINED: &str = r#"[ {"type":"message","role":"user","content":[{"type":"input_text","text":"kept \\"}],"future":{"float":1e+02,"zero":-0.00,"unicode":"\u00e9"}}, {"id":"cmp_1","type":"compaction","encrypted_content":"opaque==","future":[1,{"braces":"[]{}\\\""}]} ]"#;
const TOOLS: &str = r#"[{"id":"reason_1","type":"reasoning","encrypted_content":"reason==","summary":[],"future":1},{"id":"msg_1","type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"calling"}]},{"type":"function_call","call_id":"c1","name":"tool","arguments":"{}"},{"type":"function_call_output","call_id":"c1","output":"done"},{"type":"unknown_future_item","v":{"x":[1,2]}},{"type":"message","id":"tool-message","role":"tool","status":"completed","content":[]},{"type":"function_call_output","output":"no-call-id"},{"type":"compaction","id":"cmp_2","encrypted_content":"second=="}]"#;

fn envelope(output: &str) -> String {
    format!(
        r#"{{"id":"resp_001","object":"response.compaction","created_at":1764967971,"output":{output},"usage":{USAGE},"future_envelope":{{"v":2}}}}"#
    )
}

#[derive(Clone, Copy)]
enum Mode {
    Complete,
    Truncated,
    HoldHeaders,
    HoldBody,
    UntilDisconnect,
}

struct Captured {
    head: String,
    body: String,
}
struct Fake {
    route: CompactionRoute,
    requests: mpsc::UnboundedReceiver<Captured>,
    ready: oneshot::Receiver<()>,
    disconnected: oneshot::Receiver<()>,
    release: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fake(status: u16, body: String, mode: Mode) -> Fake {
    fake_responses(vec![(status, body, mode)]).await
}

// One local protocol fixture keeps header/body/disconnect phase ownership together.
#[allow(clippy::too_many_lines)]
async fn fake_responses(responses: Vec<(u16, String, Mode)>) -> Fake {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (requests_tx, requests) = mpsc::unbounded_channel();
    let (ready_tx, ready) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let (disconnected_tx, disconnected) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        let mut released = Some(released);
        let mut disconnected_tx = Some(disconnected_tx);
        for (status, body, mode) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            let (head, offset, length) = loop {
                let n = socket.read(&mut buffer).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    let head = String::from_utf8(bytes[..offset].to_vec()).unwrap();
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    break (head, offset + 4, length);
                }
            };
            while bytes.len() < offset + length {
                let n = socket.read(&mut buffer).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            requests_tx
                .send(Captured {
                    head,
                    body: String::from_utf8(bytes[offset..offset + length].to_vec()).unwrap(),
                })
                .unwrap();
            let length = body.len()
                + if matches!(mode, Mode::Truncated) {
                    100
                } else {
                    0
                };
            let content_type = if body.starts_with("data:") {
                "text/event-stream"
            } else {
                "application/json"
            };
            let head = format!("HTTP/1.1 {status} Status\r\nContent-Length: {length}\r\nContent-Type: {content_type}\r\nX-Request-ID: local-test\r\nConnection: close\r\n\r\n");
            match mode {
                Mode::HoldHeaders => {
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(());
                    }
                    if let Some(released) = released.take() {
                        let _ = released.await;
                    }
                    if socket.write_all(head.as_bytes()).await.is_ok() {
                        let _ = socket.write_all(body.as_bytes()).await;
                    }
                }
                Mode::HoldBody => {
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(&body.as_bytes()[..1]).await.unwrap();
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(());
                    }
                    if let Some(released) = released.take() {
                        let _ = released.await;
                    }
                    let _ = socket.write_all(&body.as_bytes()[1..]).await;
                }
                Mode::UntilDisconnect => {
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(&body.as_bytes()[..1]).await.unwrap();
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(());
                    }
                    let closed = socket.read(&mut buffer).await;
                    assert!(
                        matches!(closed, Ok(0) | Err(_)),
                        "client must close pending response"
                    );
                    if let Some(disconnected_tx) = disconnected_tx.take() {
                        let _ = disconnected_tx.send(());
                    }
                }
                Mode::Complete | Mode::Truncated => {
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(body.as_bytes()).await.unwrap();
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(());
                    }
                }
            }
        }
    });
    Fake {
        route: CompactionRoute {
            provider: "explicit-test-openai".into(),
            endpoint: format!("http://{address}/v1/responses/compact"),
            model: "gpt-5.2".into(),
        },
        requests,
        ready,
        disconnected,
        release: Some(release),
        task,
    }
}

fn supported_adapter(route: &CompactionRoute) -> ResponsesCompactor {
    ResponsesCompactor::new(
        route.clone(),
        Some(route.clone()),
        CompactionLimits::default(),
    )
    .unwrap()
}
fn input(route: &CompactionRoute) -> ResponsesWindow {
    ResponsesWindow::from_stateless_input(
        route,
        r#"[{"role":"user","content":"hello"}]"#,
        CompactionLimits::default(),
    )
    .unwrap()
}
fn options() -> CompactOptions<'static> {
    CompactOptions {
        bearer_token: "local-fake-key",
        instructions: Some("Keep state"),
        budget: InputBudget {
            input_tokens_upper_bound: 999,
            context_window_tokens: 1000,
        },
    }
}
async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(3), future)
        .await
        .expect("local test deadline")
}

#[tokio::test]
async fn full_ordered_native_output_survives_archive_and_next_input_byte_exactly() {
    for output in [RETAINED, TOOLS] {
        let mut server = fake_responses(vec![
            (200, envelope(output), Mode::Complete),
            (200, envelope(RETAINED), Mode::Complete),
        ])
        .await;
        let adapter = supported_adapter(&server.route);
        let cancel = CancellationToken::new();
        let candidate = bounded(adapter.compact(&input(&server.route), options(), &cancel))
            .await
            .unwrap();
        assert_eq!(
            candidate.usage(),
            CompactionUsage {
                input_tokens: 139,
                output_tokens: 438,
                total_tokens: 577,
                cached_input_tokens: Some(7),
                cache_write_tokens: Some(2)
            }
        );
        assert_eq!(candidate.raw_usage_json(), USAGE);
        let request = server.requests.recv().await.unwrap();
        assert!(request
            .head
            .starts_with("POST /v1/responses/compact HTTP/1.1\r\n"));
        assert!(request
            .head
            .to_ascii_lowercase()
            .contains("authorization: bearer local-fake-key"));
        assert_eq!(
            serde_json::from_str::<Value>(&request.body).unwrap(),
            json!({"model":"gpt-5.2","input":[{"role":"user","content":"hello"}],"instructions":"Keep state"})
        );
        let serialized = candidate.serialize().unwrap();
        assert!(!serialized.contains("local-fake-key"));
        let restored = CompactionCandidate::restore(&serialized, &adapter, &cancel).unwrap();
        let replay = restored.replay(&adapter, &cancel).unwrap();
        assert_eq!(replay.input_json(), output);

        // The listener remains bound to the exact same route for repeated compaction.
        let second_candidate = bounded(adapter.compact(&replay, options(), &cancel))
            .await
            .unwrap();
        let second_request = server.requests.recv().await.unwrap();
        assert_eq!(
            second_request.body,
            format!(r#"{{"model":"gpt-5.2","input":{output},"instructions":"Keep state"}}"#)
        );
        assert_eq!(
            second_candidate
                .replay(&adapter, &cancel)
                .unwrap()
                .input_json(),
            RETAINED
        );
    }
}

#[tokio::test]
async fn unsupported_route_and_oversized_input_decline_without_network() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let route = CompactionRoute {
        provider: "gateway".into(),
        endpoint: format!(
            "http://{}/v1/responses/compact",
            listener.local_addr().unwrap()
        ),
        model: "openai/gpt-5.2".into(),
    };
    let cancel = CancellationToken::new();
    let unsupported =
        ResponsesCompactor::new(route.clone(), None, CompactionLimits::default()).unwrap();
    assert!(matches!(
        unsupported
            .compact(&input(&route), options(), &cancel)
            .await,
        Err(CompactionError::Unsupported)
    ));
    let mut wrong_support = route.clone();
    wrong_support.provider = "openai".into();
    let mismatch = ResponsesCompactor::new(
        route.clone(),
        Some(wrong_support),
        CompactionLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        mismatch.compact(&input(&route), options(), &cancel).await,
        Err(CompactionError::Unsupported)
    ));
    let supported = supported_adapter(&route);
    let mut oversized = options();
    oversized.budget.input_tokens_upper_bound = 1001;
    assert!(matches!(
        supported.compact(&input(&route), oversized, &cancel).await,
        Err(CompactionError::InputTooLarge)
    ));
    let mut other = route.clone();
    other.model = "gpt-4".into();
    assert!(matches!(
        supported.compact(&input(&other), options(), &cancel).await,
        Err(CompactionError::RouteMismatch)
    ));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn http_diagnostics_distinguish_auth_quota_transient_route_and_capability() {
    let cases = [
        (401, "invalid_api_key", HttpFailureKind::Authentication),
        (403, "forbidden", HttpFailureKind::Permission),
        (404, "model_not_found", HttpFailureKind::Route),
        (
            400,
            "context_length_exceeded",
            HttpFailureKind::MalformedRequest,
        ),
        (404, "unsupported_endpoint", HttpFailureKind::Capability),
        (429, "insufficient_quota", HttpFailureKind::Quota),
        (429, "rate_limit_exceeded", HttpFailureKind::RateLimit),
        (500, "server_error", HttpFailureKind::Transient),
        (503, "unavailable", HttpFailureKind::Transient),
    ];
    for (status, code, kind) in cases {
        let server = fake(
            status,
            json!({"error":{"code":code}}).to_string(),
            Mode::Complete,
        )
        .await;
        let adapter = supported_adapter(&server.route);
        let result =
            bounded(adapter.compact(&input(&server.route), options(), &CancellationToken::new()))
                .await;
        let Err(CompactionError::Http {
            status: actual_status,
            kind: actual_kind,
            code: actual_code,
            request_id,
            headers,
        }) = result
        else {
            panic!("expected HTTP diagnostics");
        };
        assert_eq!(
            (actual_status, actual_kind, actual_code, request_id),
            (status, kind, Some(code.into()), Some("local-test".into()))
        );
        assert_eq!(
            headers.get("x-request-id").map(String::as_str),
            Some("local-test")
        );
    }
}

#[tokio::test]
async fn rejects_malformed_truncated_unusable_and_oversized_responses() {
    let mut wrong_envelope: Value = serde_json::from_str(&envelope(RETAINED)).unwrap();
    wrong_envelope["object"] = json!("response");
    let mut bad_usage: Value = serde_json::from_str(&envelope(RETAINED)).unwrap();
    bad_usage["usage"]["output_tokens"] = json!(-1);
    let mut model_mismatch: Value = serde_json::from_str(&envelope(RETAINED)).unwrap();
    model_mismatch["model"] = json!("gpt-4");
    for body in [
        "not JSON".into(),
        "{\"object\":".into(),
        wrong_envelope.to_string(),
        envelope("[]"),
        envelope(r#"[{"type":"message","role":"user"}]"#),
        envelope(r#"[{"type":"compaction","encrypted_content":"  "}]"#),
        envelope(r#"["opaque"]"#),
        bad_usage.to_string(),
        model_mismatch.to_string(),
        envelope(RETAINED).replacen("\"output\":", "\"output\":[],\"output\":", 1),
    ] {
        let server = fake(200, body, Mode::Complete).await;
        let adapter = supported_adapter(&server.route);
        assert!(bounded(adapter.compact(
            &input(&server.route),
            options(),
            &CancellationToken::new()
        ))
        .await
        .is_err());
    }
    let server = fake(200, envelope(RETAINED), Mode::Truncated).await;
    assert!(bounded(supported_adapter(&server.route).compact(
        &input(&server.route),
        options(),
        &CancellationToken::new()
    ))
    .await
    .is_err());
    let server = fake(200, envelope(RETAINED), Mode::Complete).await;
    let limits = CompactionLimits {
        max_response_bytes: 64,
        ..CompactionLimits::default()
    };
    let adapter =
        ResponsesCompactor::new(server.route.clone(), Some(server.route.clone()), limits).unwrap();
    assert!(matches!(
        bounded(adapter.compact(&input(&server.route), options(), &CancellationToken::new())).await,
        Err(CompactionError::LimitExceeded)
    ));
}

#[tokio::test]
async fn cancellation_races_headers_and_body_and_prevents_late_acceptance() {
    for mode in [Mode::HoldHeaders, Mode::HoldBody] {
        let mut server = fake(200, envelope(RETAINED), mode).await;
        let adapter = supported_adapter(&server.route);
        let window = input(&server.route);
        let cancel = CancellationToken::new();
        let pending = adapter.compact(&window, options(), &cancel);
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => { let _ = result; panic!("published before readiness"); },
            ready = bounded(&mut server.ready) => { ready.unwrap(); }
        }
        cancel.cancel();
        assert!(matches!(
            bounded(&mut pending).await,
            Err(CompactionError::Cancelled)
        ));
        let _ = server.release.take().unwrap().send(());
        bounded(&mut server.task).await.unwrap();
    }
    let server = fake(200, envelope(RETAINED), Mode::Complete).await;
    let adapter = supported_adapter(&server.route);
    let cancel = CancellationToken::new();
    let candidate = bounded(adapter.compact(&input(&server.route), options(), &cancel))
        .await
        .unwrap();
    let serialized = candidate.serialize().unwrap();
    cancel.cancel();
    assert!(matches!(
        candidate.replay(&adapter, &cancel),
        Err(CompactionError::Cancelled)
    ));
    assert!(matches!(
        CompactionCandidate::restore(&serialized, &adapter, &cancel),
        Err(CompactionError::Cancelled)
    ));
    assert!(matches!(
        adapter
            .compact(&input(&server.route), options(), &cancel)
            .await,
        Err(CompactionError::Cancelled)
    ));
}

#[tokio::test]
async fn dropped_attempt_has_no_detached_publication_and_archive_checks_exact_route() {
    let mut server = fake(200, envelope(TOOLS), Mode::HoldBody).await;
    let adapter = supported_adapter(&server.route);
    let window = input(&server.route);
    let cancel = CancellationToken::new();
    {
        let pending = adapter.compact(&window, options(), &cancel);
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => { let _ = result; panic!("early publication"); },
            ready = bounded(&mut server.ready) => { ready.unwrap(); }
        }
        // Scope exit drops the entire attempt. There is no worker or publication channel.
    }
    let _ = server.release.take().unwrap().send(());
    bounded(&mut server.task).await.unwrap();

    let server = fake(200, envelope(TOOLS), Mode::Complete).await;
    let adapter = supported_adapter(&server.route);
    let candidate = bounded(adapter.compact(&input(&server.route), options(), &cancel))
        .await
        .unwrap();
    let serialized = candidate.serialize().unwrap();
    for route in [
        CompactionRoute {
            provider: "other".into(),
            ..server.route.clone()
        },
        CompactionRoute {
            endpoint: "http://127.0.0.1:1/v1/responses/compact".into(),
            ..server.route.clone()
        },
        CompactionRoute {
            model: "gpt-5.2-new".into(),
            ..server.route.clone()
        },
    ] {
        let other = supported_adapter(&route);
        assert!(matches!(
            candidate.replay(&other, &cancel),
            Err(CompactionError::RouteMismatch)
        ));
        assert!(matches!(
            CompactionCandidate::restore(&serialized, &other, &cancel),
            Err(CompactionError::RouteMismatch)
        ));
    }
    let mut forged_usage: Value = serde_json::from_str(&serialized).unwrap();
    forged_usage["usage"]["total_tokens"] = json!(1);
    assert!(CompactionCandidate::restore(&forged_usage.to_string(), &adapter, &cancel).is_err());
    for (key, value) in [
        ("version", json!(2)),
        ("protocol", json!("other")),
        ("response_json", json!("{}")),
    ] {
        let mut corrupted: Value = serde_json::from_str(&serialized).unwrap();
        corrupted[key] = value;
        assert!(CompactionCandidate::restore(&corrupted.to_string(), &adapter, &cancel).is_err());
    }
}

#[tokio::test]
async fn whole_attempt_timeout_covers_hanging_body_without_publishing() {
    let mut server = fake(200, envelope(RETAINED), Mode::HoldBody).await;
    let adapter = supported_adapter(&server.route);
    let window = input(&server.route);
    let cancel = CancellationToken::new();
    let pending = adapter.compact(&window, options(), &cancel);
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => { let _ = result; panic!("early publication"); },
        ready = bounded(&mut server.ready) => { ready.unwrap(); }
    }
    // Advance a controlled clock only after the request reaches the held body.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    assert!(matches!(
        bounded(&mut pending).await,
        Err(CompactionError::Timeout)
    ));
    let _ = server.release.take().unwrap().send(());
    bounded(&mut server.task).await.unwrap();
}

#[tokio::test]
async fn input_and_archive_resource_limits_are_enforced_through_public_interface() {
    let route = CompactionRoute {
        provider: "test".into(),
        endpoint: "http://127.0.0.1:1/v1/responses/compact".into(),
        model: "gpt-5.2".into(),
    };
    for invalid in [
        "[]",
        "null",
        "[{}]",
        "[1]",
        "[",
        r#"[{"type":"compaction","encrypted_content":""}]"#,
    ] {
        assert!(ResponsesWindow::from_stateless_input(
            &route,
            invalid,
            CompactionLimits::default()
        )
        .is_err());
    }
    let limits = CompactionLimits {
        max_request_bytes: 4,
        ..CompactionLimits::default()
    };
    assert!(matches!(
        ResponsesWindow::from_stateless_input(
            &route,
            r#"[{"role":"user","content":"hi"}]"#,
            limits
        ),
        Err(CompactionError::LimitExceeded)
    ));
    let limits = CompactionLimits {
        max_items: 1,
        ..CompactionLimits::default()
    };
    assert!(matches!(
        ResponsesWindow::from_stateless_input(
            &route,
            r#"[{"role":"user"},{"role":"assistant"}]"#,
            limits
        ),
        Err(CompactionError::LimitExceeded)
    ));

    let server = fake(200, envelope(RETAINED), Mode::Complete).await;
    let adapter = supported_adapter(&server.route);
    let cancel = CancellationToken::new();
    let candidate = bounded(adapter.compact(&input(&server.route), options(), &cancel))
        .await
        .unwrap();
    let serialized = candidate.serialize().unwrap();
    let restricted = ResponsesCompactor::new(
        server.route.clone(),
        Some(server.route.clone()),
        CompactionLimits {
            max_response_bytes: 4,
            ..CompactionLimits::default()
        },
    )
    .unwrap();
    assert!(matches!(
        CompactionCandidate::restore(&serialized, &restricted, &cancel),
        Err(CompactionError::LimitExceeded)
    ));
    let unsupported =
        ResponsesCompactor::new(server.route.clone(), None, CompactionLimits::default()).unwrap();
    assert!(matches!(
        CompactionCandidate::restore(&serialized, &unsupported, &cancel),
        Err(CompactionError::Unsupported)
    ));
}

#[tokio::test]
async fn rejects_malformed_known_retained_items_and_present_usage_details() {
    for retained in [
        r#"{"type":"message"}"#,
        r#"{"type":"message","role":"user","content":[{"type":"input_text"}]}"#,
        r#"{"type":"function_call","call_id":"x","name":"f"}"#,
        r#"{"type":"function_call_output","call_id":"x"}"#,
        r#"{"type":"reasoning","id":"r","summary":"bad"}"#,
        r#"{"type":"compaction","encrypted_content":"no-id"}"#,
    ] {
        let output = format!(
            r#"[{retained},{{"type":"compaction","id":"cmp","encrypted_content":"valid=="}}]"#
        );
        let server = fake(200, envelope(&output), Mode::Complete).await;
        let adapter = supported_adapter(&server.route);
        assert!(matches!(
            bounded(adapter.compact(&input(&server.route), options(), &CancellationToken::new()))
                .await,
            Err(CompactionError::InvalidData(_))
        ));
    }
    for (field, bad) in [
        ("input_tokens_details", Value::Null),
        ("input_tokens_details", json!("bad")),
        ("input_tokens_details", json!([])),
        ("input_tokens_details", json!({"cached_tokens":-1})),
        ("input_tokens_details", json!({"cached_tokens":140})),
        ("output_tokens_details", json!({"reasoning_tokens":"bad"})),
        ("output_tokens_details", json!({"reasoning_tokens":-1})),
        ("output_tokens_details", Value::Null),
        ("total_tokens", json!(0)),
    ] {
        let mut body: Value = serde_json::from_str(&envelope(RETAINED)).unwrap();
        body["usage"][field] = bad;
        let server = fake(200, body.to_string(), Mode::Complete).await;
        let adapter = supported_adapter(&server.route);
        assert!(matches!(
            bounded(adapter.compact(&input(&server.route), options(), &CancellationToken::new()))
                .await,
            Err(CompactionError::InvalidData(_))
        ));
    }
}

#[tokio::test]
async fn native_prefix_survives_fresh_continuation_and_second_compaction() {
    let mut server = fake_responses(vec![
        (200, envelope(TOOLS), Mode::Complete),
        (200, envelope(RETAINED), Mode::Complete),
    ])
    .await;
    let adapter = supported_adapter(&server.route);
    let cancel = CancellationToken::new();
    let first = bounded(adapter.compact(&input(&server.route), options(), &cancel))
        .await
        .unwrap();
    let recovered =
        CompactionCandidate::restore(&first.serialize().unwrap(), &adapter, &cancel).unwrap();
    let replay = recovered.replay(&adapter, &cancel).unwrap();
    let fresh = r#"[ {"role":"user","content":"next turn"} ]"#;
    let continued = replay
        .append_input(fresh, CompactionLimits::default())
        .unwrap();
    let expected = format!(
        "{}, {} ]",
        &TOOLS[..TOOLS.len() - 1],
        r#"{"role":"user","content":"next turn"}"#
    );
    assert_eq!(continued.input_json(), expected);
    assert_eq!(replay.input_json(), TOOLS); // Immutable preparation.
    let second = bounded(adapter.compact(&continued, options(), &cancel))
        .await
        .unwrap();
    server.requests.recv().await.unwrap();
    let request = server.requests.recv().await.unwrap();
    assert_eq!(
        request.body,
        format!(r#"{{"model":"gpt-5.2","input":{expected},"instructions":"Keep state"}}"#)
    );
    assert_eq!(
        second.replay(&adapter, &cancel).unwrap().input_json(),
        RETAINED
    );
    let limits = CompactionLimits {
        max_items: 1,
        ..CompactionLimits::default()
    };
    assert!(matches!(
        replay.append_input(fresh, limits),
        Err(CompactionError::LimitExceeded)
    ));
    assert!(replay
        .append_input(r#"[{"type":"message"}]"#, CompactionLimits::default())
        .is_err());
}

#[path = "native_replay_tests.rs"]
mod native_replay;
