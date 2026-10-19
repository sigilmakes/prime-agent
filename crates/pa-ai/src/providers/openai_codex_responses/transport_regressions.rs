// Real loopback regressions for request-local transport recovery and socket-bound privacy.
use sha2::Digest as _;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::providers::openai_codex_responses::session::{
    close_websocket_sessions, get_debug_stats, session_state,
};

#[derive(Clone, Debug, PartialEq)]
struct CapturedRequest {
    socket: u64,
    websocket: bool,
    body: Value,
    body_bytes: usize,
    body_sha256: String,
}

enum Reply {
    Events(Vec<Value>),
    Close(Vec<Value>),
    Wait,
    Hold(Vec<Value>),
}

struct Loopback {
    port: u16,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    received: tokio::sync::mpsc::UnboundedReceiver<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Loopback {
    async fn start(replies: Vec<Reply>) -> Self {
        use base64::Engine as _;
        use sha1::Digest as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let scripts = Arc::new(Mutex::new(VecDeque::from(replies)));
        let (received_tx, received) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut handlers = tokio::task::JoinSet::new();
            let mut socket_id = 0;
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                socket_id += 1;
                let requests = captured.clone();
                let scripts = scripts.clone();
                let received_tx = received_tx.clone();
                handlers.spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0];
                    while !head.ends_with(b"\r\n\r\n") {
                        socket.read_exact(&mut byte).await.unwrap();
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8(head).unwrap();
                    let header = |key: &str| {
                        head.lines().find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case(key).then(|| value.trim().to_string())
                        })
                    };
                    let websocket = header("sec-websocket-key").is_some();
                    if websocket {
                        let key = header("sec-websocket-key").unwrap();
                        let accept = base64::engine::general_purpose::STANDARD.encode(
                            sha1::Sha1::digest(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()),
                        );
                        socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
                    }
                    loop {
                        let (body, body_bytes, body_sha256) = if websocket {
                            let Some((opcode, payload)) = read_client_frame(&mut socket).await.unwrap() else { return; };
                            if opcode == 8 { return; }
                            (serde_json::from_slice(&payload).unwrap(), payload.len(), hex::encode(sha2::Sha256::digest(&payload)))
                        } else {
                            let length: usize = header("content-length").unwrap().parse().unwrap();
                            let mut payload = vec![0; length];
                            socket.read_exact(&mut payload).await.unwrap();
                            (serde_json::from_slice(&payload).unwrap(), payload.len(), hex::encode(sha2::Sha256::digest(&payload)))
                        };
                        requests.lock().unwrap().push(CapturedRequest { socket: socket_id, websocket, body, body_bytes, body_sha256 });
                        let reply = scripts.lock().unwrap().pop_front().expect("unexpected request");
                        received_tx.send(()).unwrap();
                        let (events, close) = match reply {
                            Reply::Events(events) => (events, false),
                            Reply::Close(events) => (events, true),
                            Reply::Hold(events) => {
                                for event in events {
                                    if write_server_frame(&mut socket, &event).await.is_err() { return; }
                                }
                                std::future::pending::<()>().await;
                                return;
                            }
                            Reply::Wait => { std::future::pending::<()>().await; return; }
                        };
                        if websocket {
                            for event in events {
                                if write_server_frame(&mut socket, &event).await.is_err() { return; }
                            }
                            if close {
                                let _ = socket.write_all(&[0x88, 0x02, 0x03, 0xE8]).await;
                                return;
                            }
                        } else {
                            let mut body = String::new();
                            for event in events {
                                use std::fmt::Write as _;
                                write!(body, "data: {event}\n\n").unwrap();
                            }
                            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                            return;
                        }
                    }
                });
            }
        });
        Self {
            port,
            requests,
            received,
            task,
        }
    }

    fn model(&self) -> Model {
        codex_test_model(self.port)
    }

    async fn next_request(&mut self) {
        tokio::time::timeout(Duration::from_secs(5), self.received.recv())
            .await
            .unwrap()
            .unwrap();
    }

    fn captured(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn cached_connection_id(session: &str) -> Option<u64> {
    session_state()
        .lock()
        .unwrap()
        .connections
        .get(session)
        .map(|entry| entry.connection_id)
}

fn options(session: &str, transport: Transport) -> OpenAICodexResponsesOptions {
    OpenAICodexResponsesOptions {
        base: StreamOptions {
            api_key: Some(mock_codex_token()),
            transport: Some(transport),
            session_id: Some(session.into()),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn success(id: &str) -> Reply {
    Reply::Events(codex_response_events(id, "msg_test", "ok"))
}

async fn complete(
    server: &Loopback,
    context: &Context,
    options: &OpenAICodexResponsesOptions,
) -> AssistantMessage {
    tokio::time::timeout(
        Duration::from_secs(5),
        stream_openai_codex_responses(&server.model(), context, Some(options)).result(),
    )
    .await
    .expect("request completes")
}

#[tokio::test]
async fn close_fallback_is_one_request_only_next_request_probes_fresh_websocket() {
    for transport in [
        Transport::Auto,
        Transport::Websocket,
        Transport::WebsocketCached,
    ] {
        let server = Loopback::start(vec![
            Reply::Close(vec![]),
            success("resp_http"),
            success("resp_ws"),
        ])
        .await;
        let session = format!("request-local-{transport:?}");
        let options = options(&session, transport);
        let context = codex_test_context("Say hello");
        assert_eq!(
            complete(&server, &context, &options).await.stop_reason,
            StopReason::Stop
        );
        assert_eq!(
            complete(&server, &context, &options).await.stop_reason,
            StopReason::Stop
        );
        let requests = server.captured();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.websocket)
                .collect::<Vec<_>>(),
            vec![true, false, true]
        );
        assert_ne!(requests[0].socket, requests[2].socket);
        let mut expected = build_request_body(&server.model(), &context, &options);
        assert_eq!(
            requests[1].body, expected,
            "SSE fallback must use full store:false body unchanged"
        );
        expected["type"] = json!("response.create");
        assert_eq!(requests[2].body, expected);
        let stats = get_debug_stats(&session).unwrap();
        assert_eq!(
            (
                stats.websocket_failures,
                stats.sse_fallbacks,
                stats.websocket_fallback_active
            ),
            (1, 1, Some(false))
        );
        close_websocket_sessions(Some(&session));
    }
}

#[tokio::test]
async fn capped_api_error_evicts_anchor_preserves_error_and_next_socket_is_fresh() {
    let server = Loopback::start(vec![success("resp_first"), Reply::Events(vec![json!({"type":"error", "status_code":400, "error":{"code":"websocket_connection_limit_reached", "message":"Maximum connection duration reached"}})]), success("resp_fresh")]).await;
    let session = "capped-connection";
    let options = options(session, Transport::WebsocketCached);
    let first = complete(&server, &codex_test_context("Say hello"), &options).await;
    let old_id = session_state().lock().unwrap().connections[session].connection_id;
    let context = codex_followup_context(&first, "continue");
    let (writer, _reader) = AssistantMessageEventStream::new();
    let mut output = first.clone();
    output.content.clear();
    let error = run_stream(
        &server.model(),
        &context,
        Some(&options),
        &mut output,
        &writer,
    )
    .await
    .unwrap_err();
    let ProviderError::Http(error) = error else {
        panic!("cap must remain a structured API error");
    };
    assert_eq!(
        (
            error.status,
            error.sdk_name.as_deref(),
            error.provider_error_type.as_deref(),
            error.message.as_str()
        ),
        (
            Some(400),
            Some("CodexApiError"),
            Some("websocket_connection_limit_reached"),
            "Codex error: Maximum connection duration reached"
        )
    );
    assert_eq!(cached_connection_id(session), None);
    assert_eq!(
        complete(&server, &context, &options).await.stop_reason,
        StopReason::Stop
    );
    let new_id = session_state().lock().unwrap().connections[session].connection_id;
    assert_ne!(old_id, new_id);
    let requests = server.captured();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| request.websocket));
    assert_eq!(requests[0].socket, requests[1].socket);
    assert_ne!(requests[1].socket, requests[2].socket);
    assert_eq!(requests[1].body["previous_response_id"], "resp_first");
    assert!(requests[2].body.get("previous_response_id").is_none());
    assert_eq!(
        requests[2].body["input"],
        build_request_body(&server.model(), &context, &options)["input"]
    );
    close_websocket_sessions(Some(session));
}

#[tokio::test]
async fn actively_pooled_socket_rotates_at_fifty_minutes_not_idle_ttl() {
    let server = Loopback::start((0..14).map(|_| success("resp_age")).collect()).await;
    let session = "active-age-rotation";
    let options = options(session, Transport::Auto);
    let context = codex_test_context("Say hello");
    complete(&server, &context, &options).await;
    let original = session_state().lock().unwrap().connections[session].connection_id;
    // Keep each idle interval below five minutes. Advance an injected monotonic Tokio clock
    // only while the lease is busy, so idle-expiry and network scheduling cannot race the test.
    for _ in 0..13 {
        let lease = acquire_websocket(
            &resolve_codex_websocket_url(&server.model().base_url),
            &[],
            Some(session),
            /*signal*/ None,
        )
        .await
        .unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_mins(4)).await;
        tokio::time::resume();
        release_connection(lease, /*keep*/ true, /*continuation*/ None).await;
        assert_eq!(
            complete(&server, &context, &options).await.stop_reason,
            StopReason::Stop
        );
    }
    let current = session_state().lock().unwrap().connections[session].connection_id;
    assert_ne!(
        original, current,
        "active pooling must not keep a socket beyond proactive max age"
    );
    let requests = server.captured();
    assert!(requests[..13]
        .iter()
        .all(|request| request.socket == requests[0].socket));
    assert_ne!(requests[13].socket, requests[0].socket);
    close_websocket_sessions(Some(session));
}

#[tokio::test]
async fn unrelated_api_error_never_falls_back_or_retries() {
    let server = Loopback::start(vec![Reply::Events(codex_error_events(
        "invalid_request_error",
        "bad request",
    ))])
    .await;
    let session = "unrelated-api";
    let result = complete(
        &server,
        &codex_test_context("Say hello"),
        &options(session, Transport::Auto),
    )
    .await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Codex error: bad request")
    );
    assert_eq!(server.captured().len(), 1);
    assert!(get_debug_stats(session)
        .is_some_and(|stats| stats.sse_fallbacks == 0 && stats.websocket_failures == 0));
    assert_eq!(cached_connection_id(session), None);
}

#[tokio::test]
async fn visible_text_or_tool_event_forbids_transport_fallback() {
    for (name, events) in [
        (
            "text",
            codex_response_events("resp_partial", "msg_partial", "partial")
                .into_iter()
                .take(4)
                .collect(),
        ),
        (
            "tool",
            vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_test","call_id":"call_test","name":"bash","arguments":""}}),
                json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{}"}),
            ],
        ),
    ] {
        let server = Loopback::start(vec![Reply::Close(events)]).await;
        let session = format!("post-visible-{name}");
        let options = options(&session, Transport::Auto);
        let events = tokio::time::timeout(
            Duration::from_secs(5),
            stream_openai_codex_responses(
                &server.model(),
                &codex_test_context("Say hello"),
                Some(&options),
            )
            .collect(),
        )
        .await
        .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::Start { .. }))
                .count(),
            1
        );
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Error { .. })
        ));
        assert_eq!(server.captured().len(), 1);
        assert_eq!(get_debug_stats(&session).unwrap().sse_fallbacks, 0);
        assert_eq!(cached_connection_id(&session), None);
    }
}

#[tokio::test]
async fn abort_after_request_does_not_fallback_and_evicts_connection() {
    let mut server = Loopback::start(vec![Reply::Wait]).await;
    let session = "abort-inflight";
    let mut options = options(session, Transport::Auto);
    let signal = tokio_util::sync::CancellationToken::new();
    options.base.signal = Some(signal.clone());
    let stream = stream_openai_codex_responses(
        &server.model(),
        &codex_test_context("Say hello"),
        Some(&options),
    );
    server.next_request().await;
    signal.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), stream.result())
        .await
        .unwrap();
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(server.captured().len(), 1);
    assert_eq!(cached_connection_id(session), None);
}

#[tokio::test]
async fn explicit_sse_sends_full_body_without_websocket_probe() {
    let server = Loopback::start(vec![success("resp_sse")]).await;
    let options = options("explicit-sse", Transport::Sse);
    let context = codex_test_context("Say hello");
    assert_eq!(
        complete(&server, &context, &options).await.stop_reason,
        StopReason::Stop
    );
    assert_eq!(
        server.captured(),
        vec![CapturedRequest {
            socket: 1,
            websocket: false,
            body: build_request_body(&server.model(), &context, &options),
            body_bytes: build_request_body(&server.model(), &context, &options)
                .to_string()
                .len(),
            body_sha256: hex::encode(sha2::Sha256::digest(
                build_request_body(&server.model(), &context, &options)
                    .to_string()
                    .as_bytes()
            )),
        }]
    );
}

#[tokio::test]
async fn idle_gaps_over_two_and_five_minutes_keep_same_socket_under_max_age() {
    let server = Loopback::start((0..3).map(|_| success("resp_idle")).collect()).await;
    let session = "idle-under-absolute-age";
    let options = options(session, Transport::Auto);
    let context = codex_test_context("Say hello");
    complete(&server, &context, &options).await;
    let original = session_state().lock().unwrap().connections[session].connection_id;
    for gap in [3 * 60, 6 * 60] {
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(gap)).await;
        tokio::time::resume();
        assert_eq!(
            complete(&server, &context, &options).await.stop_reason,
            StopReason::Stop
        );
        assert_eq!(cached_connection_id(session), Some(original));
    }
    let requests = server.captured();
    assert!(requests
        .iter()
        .all(|request| request.socket == requests[0].socket));
    close_websocket_sessions(Some(session));
}

#[tokio::test]
async fn capped_uncached_overlap_must_not_evict_other_busy_socket() {
    let mut server = Loopback::start(vec![
        Reply::Wait,
        Reply::Events(codex_error_events(
            "websocket_connection_limit_reached",
            "cap",
        )),
    ])
    .await;
    let session = "cap-overlapping-leases";
    let mut first_options = options(session, Transport::Auto);
    let signal = tokio_util::sync::CancellationToken::new();
    first_options.base.signal = Some(signal.clone());
    let first = stream_openai_codex_responses(
        &server.model(),
        &codex_test_context("first"),
        Some(&first_options),
    );
    server.next_request().await;
    let busy_id = session_state().lock().unwrap().connections[session].connection_id;
    let failed = complete(
        &server,
        &codex_test_context("second"),
        &options(session, Transport::Auto),
    )
    .await;
    assert_eq!(failed.stop_reason, StopReason::Error);
    let matching_busy = session_state()
        .lock()
        .unwrap()
        .connections
        .get(session)
        .is_some_and(|entry| entry.busy && entry.connection_id == busy_id);
    assert!(matching_busy, "an uncached cap error must evict only its matching socket, not another request's busy socket");
    signal.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), first.result())
            .await
            .unwrap()
            .stop_reason,
        StopReason::Aborted
    );
    assert_eq!(server.captured().len(), 2);
}

#[tokio::test]
async fn anchored_delta_close_falls_back_with_full_store_false_context() {
    let server = Loopback::start(vec![
        success("resp_anchor"),
        Reply::Close(vec![]),
        success("resp_http_full"),
        success("resp_new_socket"),
    ])
    .await;
    let session = "delta-close-full-http";
    let options = options(session, Transport::WebsocketCached);
    let first = complete(&server, &codex_test_context("Say hello"), &options).await;
    let context = codex_followup_context(&first, "continue");
    assert_eq!(
        complete(&server, &context, &options).await.stop_reason,
        StopReason::Stop
    );
    assert_eq!(
        complete(&server, &context, &options).await.stop_reason,
        StopReason::Stop
    );
    let requests = server.captured();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.websocket)
            .collect::<Vec<_>>(),
        vec![true, true, false, true]
    );
    assert_eq!(requests[1].body["previous_response_id"], "resp_anchor");
    assert_eq!(requests[1].body["input"].as_array().unwrap().len(), 1);
    let full = build_request_body(&server.model(), &context, &options);
    assert_eq!(requests[2].body, full);
    assert_eq!(requests[3].body["input"], full["input"]);
    assert!(requests
        .iter()
        .all(|request| request.body["store"] == false));
    assert!(requests[3].body.get("previous_response_id").is_none());
    close_websocket_sessions(Some(session));
}

#[tokio::test]
async fn explicit_websocket_reuses_socket_but_sends_full_context() {
    let server = Loopback::start(vec![success("resp_plain"), success("resp_plain_next")]).await;
    let session = "explicit-websocket-no-delta";
    let options = options(session, Transport::Websocket);
    let first = complete(&server, &codex_test_context("Say hello"), &options).await;
    let context = codex_followup_context(&first, "continue");
    assert_eq!(
        complete(&server, &context, &options).await.stop_reason,
        StopReason::Stop
    );
    let requests = server.captured();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].socket, requests[1].socket);
    assert!(requests
        .iter()
        .all(|request| request.websocket && request.body.get("previous_response_id").is_none()));
    assert_eq!(
        requests[1].body["input"],
        build_request_body(&server.model(), &context, &options)["input"]
    );
    close_websocket_sessions(Some(session));
}

#[tokio::test]
async fn reused_socket_observes_current_request_cancellation_not_creator_signal() {
    let mut server = Loopback::start(vec![success("resp_cancel_anchor"), Reply::Wait]).await;
    let session = "reused-current-signal";
    let mut first_options = options(session, Transport::Auto);
    let first_signal = tokio_util::sync::CancellationToken::new();
    first_options.base.signal = Some(first_signal.clone());
    let first = complete(&server, &codex_test_context("Say hello"), &first_options).await;
    server.next_request().await;
    let mut next_options = options(session, Transport::Auto);
    let next_signal = tokio_util::sync::CancellationToken::new();
    next_options.base.signal = Some(next_signal.clone());
    let stream = stream_openai_codex_responses(
        &server.model(),
        &codex_followup_context(&first, "continue"),
        Some(&next_options),
    );
    server.next_request().await;
    next_signal.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), stream.result())
            .await
            .expect("current request cancellation must wake reused worker")
            .stop_reason,
        StopReason::Aborted
    );
    assert_eq!(cached_connection_id(session), None);
    assert_eq!(server.captured().len(), 2);
}

#[tokio::test]
async fn large_context_wire_full_then_delta_survives_idle_until_absolute_rotation() {
    // The optional external fixture is shared with the TypeScript parity run. Keep the default
    // equally large so the checked-in regression never silently degrades to a tiny transcript.
    let text = match std::env::var("PA_CODEX_LONG_CONTEXT_FIXTURE") {
        Ok(path) => std::fs::read_to_string(path).expect("large parity fixture"),
        Err(_) => " 123".repeat(160_000),
    };
    assert_eq!(text.len(), 640_000);
    assert_eq!(text.split_whitespace().count(), 160_000);
    let server = Loopback::start((0..4).map(|_| success("resp_large")).collect()).await;
    let session = "large-context-idle-rotation";
    let options = options(session, Transport::WebsocketCached);
    let mut context = codex_test_context(&text);
    let mut first = complete(&server, &context, &options).await;
    assert_eq!(first.stop_reason, StopReason::Stop);
    for gap in [3 * 60, 6 * 60, 41 * 60] {
        context
            .messages
            .push(crate::types::Message::Assistant(first));
        context
            .messages
            .extend(codex_test_context("continue").messages);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(gap)).await;
        tokio::time::resume();
        first = complete(&server, &context, &options).await;
        assert_eq!(first.stop_reason, StopReason::Stop);
    }
    let requests = server.captured();
    assert_eq!(requests.len(), 4);
    assert!(requests
        .iter()
        .all(|request| request.websocket && request.body["store"] == false));
    assert_eq!(requests[0].socket, requests[1].socket);
    assert_eq!(requests[0].socket, requests[2].socket);
    assert_ne!(requests[0].socket, requests[3].socket);
    assert!(requests[0].body_bytes > 640_000);
    assert!(requests[1].body_bytes < 1024 && requests[2].body_bytes < 1024);
    assert!(requests[3].body_bytes > 640_000);
    assert!(requests[0].body.get("previous_response_id").is_none());
    assert!(requests[1].body.get("previous_response_id").is_some());
    assert!(requests[2].body.get("previous_response_id").is_some());
    assert!(requests[3].body.get("previous_response_id").is_none());
    let metadata = json!({
        "fixtureBytes": text.len(), "numericPieces": text.split_whitespace().count(),
        "requestBytes": requests.iter().map(|request| request.body_bytes).collect::<Vec<_>>(),
        "requestBodySha256": requests.iter().map(|request| &request.body_sha256).collect::<Vec<_>>(),
        "canonicalBodySha256": requests.iter().map(|request| {
            let mut canonical = request.body.clone();
            canonical.sort_all_objects();
            hex::encode(sha2::Sha256::digest(canonical.to_string().as_bytes()))
        }).collect::<Vec<_>>(),
        "inputSha256": requests.iter().map(|request| hex::encode(sha2::Sha256::digest(request.body["input"].to_string().as_bytes()))).collect::<Vec<_>>(),
        "previousIdPresent": requests.iter().map(|request| request.body.get("previous_response_id").is_some()).collect::<Vec<_>>(),
        "sameSocketAsFirst": requests.iter().map(|request| request.socket == requests[0].socket).collect::<Vec<_>>(),
        "storeFalse": requests.iter().all(|request| request.body["store"] == false),
        "idleGapSeconds": [180, 360, 2460]
    });
    println!("CODEX_SAFE_WIRE_METADATA {metadata}");
    close_websocket_sessions(Some(session));
}

fn visible_hold() -> Reply {
    Reply::Hold(
        codex_response_events("resp_busy", "msg_busy", "partial")
            .into_iter()
            .take(4)
            .collect(),
    )
}

#[tokio::test]
async fn aged_active_visible_lease_is_not_evicted_by_concurrent_checkout() {
    let server = Loopback::start(vec![visible_hold(), success("resp_uncached")]).await;
    let session = "aged-active-visible";
    let mut first_options = options(session, Transport::Auto);
    let signal = tokio_util::sync::CancellationToken::new();
    first_options.base.signal = Some(signal.clone());
    let mut stream = stream_openai_codex_responses(
        &server.model(),
        &codex_test_context("first"),
        Some(&first_options),
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), stream.next_event())
            .await
            .unwrap(),
        Some(AssistantMessageEvent::Start { .. })
    ));
    let old_id = cached_connection_id(session).unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_mins(51)).await;
    tokio::time::resume();
    let next = complete(
        &server,
        &codex_test_context("second"),
        &options(session, Transport::Auto),
    )
    .await;
    assert_eq!(next.stop_reason, StopReason::Stop);
    assert_eq!(cached_connection_id(session), Some(old_id));
    signal.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), stream.result())
            .await
            .unwrap()
            .stop_reason,
        StopReason::Aborted
    );
    assert_eq!(cached_connection_id(session), None);
    assert_eq!(server.captured().len(), 2);
}

#[tokio::test]
async fn stale_lease_abort_preserves_replacement_socket_and_continuation() {
    let server = Loopback::start(vec![
        visible_hold(),
        success("resp_replacement"),
        success("resp_replacement_next"),
    ])
    .await;
    let session = "stale-lease-replacement";
    let mut first_options = options(session, Transport::Auto);
    let signal = tokio_util::sync::CancellationToken::new();
    first_options.base.signal = Some(signal.clone());
    let mut stream = stream_openai_codex_responses(
        &server.model(),
        &codex_test_context("first"),
        Some(&first_options),
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), stream.next_event())
            .await
            .unwrap(),
        Some(AssistantMessageEvent::Start { .. })
    ));
    let old_id = cached_connection_id(session).unwrap();
    close_websocket_sessions(Some(session));
    let options = options(session, Transport::Auto);
    let replacement = complete(&server, &codex_test_context("Say hello"), &options).await;
    assert_eq!(replacement.stop_reason, StopReason::Stop);
    let replacement_id = cached_connection_id(session).unwrap();
    assert_ne!(old_id, replacement_id);
    signal.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), stream.result())
            .await
            .unwrap()
            .stop_reason,
        StopReason::Aborted
    );
    assert_eq!(cached_connection_id(session), Some(replacement_id));
    let followup = complete(
        &server,
        &codex_followup_context(&replacement, "continue"),
        &options,
    )
    .await;
    assert_eq!(followup.stop_reason, StopReason::Stop);
    let requests = server.captured();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].socket, requests[2].socket);
    assert!(requests[2].body.get("previous_response_id").is_some());
    assert_eq!(requests[2].body["input"].as_array().unwrap().len(), 1);
    close_websocket_sessions(Some(session));
}

#[tokio::test]
async fn cancelling_completed_creator_does_not_abort_reused_request() {
    let server = Loopback::start(vec![success("resp_creator"), success("resp_later")]).await;
    let session = "completed-creator-cancel";
    let mut first_options = options(session, Transport::Auto);
    let signal = tokio_util::sync::CancellationToken::new();
    first_options.base.signal = Some(signal.clone());
    let first = complete(&server, &codex_test_context("Say hello"), &first_options).await;
    signal.cancel();
    let next = complete(
        &server,
        &codex_followup_context(&first, "continue"),
        &options(session, Transport::Auto),
    )
    .await;
    assert_eq!(next.stop_reason, StopReason::Stop);
    let requests = server.captured();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.websocket));
    assert_eq!(requests[0].socket, requests[1].socket);
    close_websocket_sessions(Some(session));
}

#[tokio::test]
async fn reasoning_summary_default_and_overrides_send_exact_request_bodies() {
    for (summary, wire_summary) in [
        (None, "detailed"),
        (Some(ReasoningSummary::Auto), "auto"),
        (Some(ReasoningSummary::Concise), "concise"),
        (Some(ReasoningSummary::Detailed), "detailed"),
    ] {
        let server = Loopback::start(vec![success("resp_summary_body")]).await;
        let mut model = server.model();
        model.thinking_level_map = Some(std::collections::BTreeMap::from([(
            ModelThinkingLevel::Medium,
            Some("mapped-medium".into()),
        )]));
        let context = codex_test_context("summary request");
        let mut options = options("summary-body", Transport::Sse);
        let mut expected = build_request_body(&model, &context, &options);
        expected["reasoning"] = json!({"effort":"mapped-medium", "summary":wire_summary});
        options.reasoning_effort = Some(ModelThinkingLevel::Medium);
        options.reasoning_summary = summary;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            stream_openai_codex_responses(&model, &context, Some(&options)).result(),
        )
        .await
        .unwrap();
        assert_eq!(result.stop_reason, StopReason::Stop);
        let requests = server.captured();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body, expected);
        assert_eq!(requests[0].body_bytes, expected.to_string().len());
        assert!(!requests[0].websocket);
    }
}

#[tokio::test]
async fn summary_preference_does_not_enable_reasoning_without_effort() {
    let server = Loopback::start(vec![success("resp_no_reasoning")]).await;
    let context = codex_test_context("no reasoning requested");
    let mut options = options("no-effort-summary", Transport::Sse);
    let expected = build_request_body(&server.model(), &context, &options);
    options.reasoning_summary = Some(ReasoningSummary::Detailed);
    assert_eq!(
        complete(&server, &context, &options).await.stop_reason,
        StopReason::Stop
    );
    assert!(expected.get("reasoning").is_none());
    assert_eq!(server.captured()[0].body, expected);
}

#[tokio::test]
async fn simple_reasoning_effort_and_model_capability_gates_remain_unchanged() {
    for (capable, requested) in [
        (false, Some(ModelThinkingLevel::Medium)),
        (true, Some(ModelThinkingLevel::Off)),
        (true, None),
    ] {
        let server = Loopback::start(vec![success("resp_capability")]).await;
        let mut model = server.model();
        model.reasoning = capable;
        let context = codex_test_context("capability request");
        let native = options("summary-capability", Transport::Sse);
        let expected = build_request_body(&model, &context, &native);
        let options = SimpleStreamOptions {
            base: native.base,
            reasoning: requested,
            ..Default::default()
        };
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            stream_simple_openai_codex_responses(&model, &context, Some(&options)).result(),
        )
        .await
        .unwrap();
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert!(expected.get("reasoning").is_none());
        assert_eq!(server.captured()[0].body, expected);
    }
}

#[tokio::test]
async fn reasoning_summary_deltas_replay_through_existing_sse_and_websocket_mapping() {
    let reasoning = json!({"type":"reasoning", "id":"rs_summary", "summary":[{"type":"summary_text","text":"First thought"}], "encrypted_content":"test_signature"});
    let script = vec![
        json!({"type":"response.created","response":{"id":"resp_summary_replay"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_summary","summary":[]}}),
        json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"First "}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"thought"}),
        json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
        json!({"type":"response.completed","response":{"id":"resp_summary_replay","status":"completed","usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}}),
    ];
    for transport in [Transport::Sse, Transport::WebsocketCached] {
        let server = Loopback::start(vec![Reply::Events(script.clone())]).await;
        let session = format!("summary-replay-{transport:?}");
        let mut options = options(&session, transport);
        options.reasoning_effort = Some(ModelThinkingLevel::Medium);
        let events = tokio::time::timeout(
            Duration::from_secs(5),
            stream_openai_codex_responses(
                &server.model(),
                &codex_test_context("reason"),
                Some(&options),
            )
            .collect(),
        )
        .await
        .unwrap();
        let deltas = events
            .iter()
            .filter_map(|event| match event {
                AssistantMessageEvent::ThinkingDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deltas, ["First ", "thought"]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::Start { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::ThinkingStart { .. }))
                .count(),
            1
        );
        assert_eq!(events.iter().filter(|event| matches!(event, AssistantMessageEvent::ThinkingEnd { content, .. } if content == "First thought")).count(), 1);
        let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
            panic!("completed summary replay");
        };
        let [crate::types::AssistantContent::Thinking(block)] = message.content.as_slice() else {
            panic!("recorded thinking block");
        };
        assert_eq!(block.thinking, "First thought");
        assert_eq!(
            block.thinking_signature.as_deref(),
            Some(reasoning.to_string().as_str())
        );
        close_websocket_sessions(Some(&session));
    }
}

#[tokio::test]
async fn fresh_full_request_stale_api_error_is_not_retried_or_fallen_back() {
    let server = Loopback::start(vec![
        Reply::Events(codex_error_events(
            "previous_response_not_found",
            "synthetic unanchored rejection",
        )),
        success("resp_forbidden_retry"),
    ])
    .await;
    let session = "fresh-unanchored-stale-error";
    let context = codex_test_context("fresh full request");
    let options = options(session, Transport::WebsocketCached);
    let (writer, _reader) = AssistantMessageEventStream::new();
    let mut output = AssistantMessage {
        content: Vec::new(),
        api: API_OPENAI_CODEX_RESPONSES.into(),
        provider: server.model().provider,
        model: server.model().id,
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
        rest: Map::new(),
    };
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_stream(
            &server.model(),
            &context,
            Some(&options),
            &mut output,
            &writer,
        ),
    )
    .await
    .unwrap();
    assert!(
        result.is_err(),
        "an unanchored API error must not be hidden by a paid retry"
    );
    let ProviderError::Http(error) = result.unwrap_err() else {
        panic!("structured stale API error");
    };
    assert_eq!(
        (
            error.sdk_name.as_deref(),
            error.provider_error_type.as_deref()
        ),
        (Some("CodexApiError"), Some("previous_response_not_found"))
    );
    let requests = server.captured();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].websocket);
    assert!(requests[0].body.get("previous_response_id").is_none());
    assert_eq!(cached_connection_id(session), None);
    let stats = get_debug_stats(session).unwrap();
    assert_eq!((stats.sse_fallbacks, stats.websocket_failures), (0, 0));
}
