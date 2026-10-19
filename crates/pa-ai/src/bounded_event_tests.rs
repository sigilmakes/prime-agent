//! Opt-in event-count backpressure through the public facade and local HTTP.
use super::*;
use crate::types::{Context, Model, SimpleStreamOptions, StreamOptions};
use futures::FutureExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

const DELTAS: usize = 64;
const NATIVE: &str = r#"[ {"role":"user","content":"retained \u00e9","n":1e+02}, {"type":"compaction","id":"cmp","encrypted_content":"opaque=="} ]"#;

#[derive(Clone, Copy)]
enum Ending {
    Complete,
    Eof,
    Hold,
}

struct Server {
    base_url: String,
    request: oneshot::Receiver<String>,
    disconnected: oneshot::Receiver<()>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(5), future)
        .await
        .expect("local backpressure test deadline")
}

async fn serve(body: String, ending: Ending) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (request_tx, request) = oneshot::channel();
    let (closed_tx, disconnected) = oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let (offset, length) = loop {
            let n = socket.read(&mut buffer).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = std::str::from_utf8(&bytes[..offset]).unwrap();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                break (offset + 4, length);
            }
        };
        while bytes.len() < offset + length {
            let n = socket.read(&mut buffer).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&buffer[..n]);
        }
        let _ =
            request_tx.send(String::from_utf8(bytes[offset..offset + length].to_vec()).unwrap());
        let length = body.len()
            + if matches!(ending, Ending::Hold) {
                1000
            } else {
                0
            };
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
        socket.write_all(head.as_bytes()).await.unwrap();
        // A single provider write contains the entire >capacity parsed-event burst.
        socket.write_all(body.as_bytes()).await.unwrap();
        if matches!(ending, Ending::Hold) {
            let closed = socket.read(&mut buffer).await;
            assert!(
                matches!(closed, Ok(0) | Err(_)),
                "retired request must close its pending socket"
            );
            let _ = closed_tx.send(());
        }
    });
    Server {
        base_url,
        request,
        disconnected,
        task,
    }
}

fn model(base_url: &str, api: &str) -> Model {
    serde_json::from_value(json!({
        "id":"m","name":"local","api":api,"provider":"local-bounded-test","baseUrl":base_url,
        "reasoning":false,"input":["text"],"contextWindow":100_000,"maxTokens":1000,
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}
    }))
    .unwrap()
}

fn options(capacity: Option<usize>) -> StreamOptions {
    StreamOptions {
        api_key: Some("local-test-key".into()),
        event_buffer_capacity: capacity,
        ..StreamOptions::default()
    }
}

fn responses(ending: Ending) -> String {
    let mut events = vec![
        json!({"type":"response.created","response":{"id":"bounded-response"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
    ];
    for i in 0..DELTAS {
        events.push(
            json!({"type":"response.output_text.delta","output_index":0,"delta":format!("{i},")}),
        );
    }
    if matches!(ending, Ending::Complete) {
        events.push(json!({"type":"response.completed","response":{"id":"bounded-response","status":"completed","model":"m","usage":{"input_tokens":2,"output_tokens":DELTAS,"total_tokens":DELTAS+2}}}));
    }
    encode(events)
}

fn encode(events: Vec<Value>) -> String {
    use std::fmt::Write as _;
    let mut body = String::new();
    for event in events {
        writeln!(body, "data: {event}\n").unwrap();
    }
    body
}

async fn wait_blocked(stream: &AssistantMessageEventStream) {
    bounded(futures::future::poll_fn(|cx| {
        let mut state = stream.shared.state.lock().unwrap();
        if state.producer_wakers.is_empty() {
            state.blocked_observer = Some(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }))
    .await;
}

fn queue_len(stream: &AssistantMessageEventStream) -> usize {
    stream.shared.state.lock().unwrap().queue.len()
}

#[tokio::test]
async fn one_write_responses_burst_pauses_per_event_then_delivers_every_event_and_native_input() {
    let mut server = serve(responses(Ending::Complete), Ending::Complete).await;
    let model = model(&server.base_url, "openai-responses");
    let route = crate::compaction::CompactionRoute {
        provider: model.provider.clone(),
        endpoint: format!("{}/responses/compact", server.base_url),
        model: model.id.clone(),
    };
    let limits = crate::compaction::CompactionLimits::default();
    let adapter =
        crate::compaction::ResponsesCompactor::new(route.clone(), Some(route.clone()), limits)
            .unwrap();
    let window =
        crate::compaction::ResponsesWindow::from_stateless_input(&route, NATIVE, limits).unwrap();
    let mut opts = options(Some(16));
    opts.native_responses_input = Some(
        window
            .inference_input(&adapter, &CancellationToken::new())
            .unwrap(),
    );
    let mut stream = crate::stream(&model, &Context::default(), Some(opts)).unwrap();
    wait_blocked(&stream).await;
    assert_eq!(queue_len(&stream), 16);
    {
        let state = stream.shared.state.lock().unwrap();
        assert!(
            !state.done,
            "producer must not consume the rest of the same HTTP chunk"
        );
        assert_eq!(state.queue.back().unwrap().delta(), Some("13,"));
    }
    let request = bounded(&mut server.request).await.unwrap();
    assert!(
        request.contains(&format!("\"input\":{NATIVE}")),
        "native bytes remain authoritative in bounded mode"
    );
    let mut types = Vec::new();
    let mut deltas = Vec::new();
    while let Some(event) = bounded(stream.next_event()).await {
        assert!(queue_len(&stream) <= 16);
        types.push(event.event_type());
        if let AssistantMessageEvent::TextDelta { delta, .. } = event {
            deltas.push(delta);
        }
    }
    let mut expected = vec!["start", "text_start"];
    expected.extend(std::iter::repeat_n("text_delta", DELTAS));
    expected.push("done");
    assert_eq!(types, expected);
    assert_eq!(
        deltas,
        (0..DELTAS).map(|i| format!("{i},")).collect::<Vec<_>>()
    );
    assert_eq!(stream.result().await.stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn result_only_and_both_complete_facades_progress_past_capacity() {
    use std::fmt::Write as _;
    let mut expected_text = String::new();
    for i in 0..DELTAS {
        write!(expected_text, "{i},").unwrap();
    }
    for mode in ["result", "complete", "simple"] {
        let server = serve(responses(Ending::Complete), Ending::Complete).await;
        let model = model(&server.base_url, "openai-responses");
        let result = match mode {
            "result" => {
                let stream =
                    crate::stream(&model, &Context::default(), Some(options(Some(1)))).unwrap();
                wait_blocked(&stream).await;
                bounded(stream.result()).await
            }
            "complete" => bounded(crate::complete(
                &model,
                &Context::default(),
                Some(options(Some(16))),
            ))
            .await
            .unwrap(),
            "simple" => bounded(crate::complete_simple(
                &model,
                &Context::default(),
                Some(SimpleStreamOptions::from_base(options(Some(16)))),
            ))
            .await
            .unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(result.stop_reason, StopReason::Stop);
        let AssistantContent::Text(text) = &result.content[0] else {
            panic!("text result");
        };
        assert_eq!(text.text, expected_text);
    }
}

#[tokio::test]
async fn cancellation_and_drop_while_full_close_the_real_http_socket() {
    for cancel in [true, false] {
        let mut server = serve(responses(Ending::Hold), Ending::Hold).await;
        let signal = CancellationToken::new();
        let mut opts = options(Some(16));
        opts.signal = Some(signal.clone());
        let stream = crate::stream(
            &model(&server.base_url, "openai-responses"),
            &Context::default(),
            Some(opts),
        )
        .unwrap();
        wait_blocked(&stream).await;
        assert_eq!(queue_len(&stream), 16);
        if cancel {
            signal.cancel();
            // Cancellation tears down HTTP even before the consumer makes room
            // for the one ordered terminal error.
            bounded(&mut server.disconnected).await.unwrap();
            assert_eq!(
                bounded(stream.result()).await.stop_reason,
                StopReason::Aborted
            );
        } else {
            drop(stream);
            bounded(&mut server.disconnected).await.unwrap();
            assert!(
                !signal.is_cancelled(),
                "drop must not cancel a parent token"
            );
        }
    }
}

#[tokio::test]
async fn eof_failure_after_a_full_burst_preserves_order_and_one_terminal() {
    let server = serve(responses(Ending::Eof), Ending::Eof).await;
    let mut strict_model = model(&server.base_url, "openai-responses");
    // XAI's existing Responses contract rejects EOF without response.completed.
    // Other providers intentionally retain their legacy permissive EOF behavior.
    strict_model.provider = "xai".into();
    let mut stream =
        crate::stream(&strict_model, &Context::default(), Some(options(Some(16)))).unwrap();
    wait_blocked(&stream).await;
    let mut events = Vec::new();
    while let Some(event) = bounded(stream.next_event()).await {
        events.push(event);
        assert!(queue_len(&stream) <= 16);
    }
    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error { .. })
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AssistantMessageEvent::TextDelta { .. }))
            .count(),
        DELTAS
    );
    assert_eq!(
        bounded(stream.result()).await.stop_reason,
        StopReason::Error
    );
    // The same EOF error must resolve a result-only call from a full queue.
    let server = serve(responses(Ending::Eof), Ending::Eof).await;
    strict_model.base_url = server.base_url.clone();
    let stream = crate::stream(&strict_model, &Context::default(), Some(options(Some(1)))).unwrap();
    wait_blocked(&stream).await;
    assert_eq!(
        bounded(stream.result()).await.stop_reason,
        StopReason::Error
    );
}

#[tokio::test]
async fn unbounded_default_still_readies_the_complete_burst_without_a_reader() {
    let server = serve(responses(Ending::Complete), Ending::Complete).await;
    let stream = crate::stream(
        &model(&server.base_url, "openai-responses"),
        &Context::default(),
        Some(options(None)),
    )
    .unwrap();
    bounded(futures::future::poll_fn(|cx| {
        let mut state = stream.shared.state.lock().unwrap();
        if state.done {
            Poll::Ready(())
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }))
    .await;
    assert_eq!(queue_len(&stream), DELTAS + 3);
    assert_eq!(stream.shared.delivery.capacity, None);
    assert!(stream.abort_on_drop.is_none());
    assert_eq!(stream.result().await.stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn zero_and_unsupported_custom_bounded_requests_never_dispatch_or_open_a_socket() {
    struct Unsupported(Arc<AtomicUsize>);
    impl crate::registry::Provider for Unsupported {
        fn api(&self) -> &'static str {
            "unsupported-bounded-test"
        }
        fn stream(
            &self,
            _: &Model,
            _: &Context,
            _: Option<&StreamOptions>,
        ) -> AssistantMessageEventStream {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("unsupported dispatch");
        }
        fn stream_simple(
            &self,
            _: &Model,
            _: &Context,
            _: Option<&SimpleStreamOptions>,
        ) -> AssistantMessageEventStream {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("unsupported simple dispatch");
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    crate::registry::ensure_builtins();
    crate::registry::register_api_provider(
        Arc::new(Unsupported(calls.clone())),
        Some("bounded-test-only"),
    );
    for api in ["unsupported-bounded-test", "openai-responses"] {
        let model = model(&endpoint, api);
        let capacity = if api == "openai-responses" { 0 } else { 16 };
        assert!(crate::stream(&model, &Context::default(), Some(options(Some(capacity)))).is_err());
        assert!(crate::stream_simple(
            &model,
            &Context::default(),
            Some(SimpleStreamOptions::from_base(options(Some(capacity))))
        )
        .is_err());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(listener.accept().now_or_never().is_none());
    crate::registry::unregister_api_providers("bounded-test-only");
}

#[tokio::test]
async fn completions_tool_lifecycle_resumes_every_event_from_one_chunk() {
    let mut chunks = vec![
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_local","type":"function","function":{"name":"tool","arguments":""}}]}}]}),
    ];
    let arguments = format!(r#"{{"text":"{}"}}"#, "x".repeat(DELTAS));
    for character in arguments.chars() {
        chunks.push(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":character.to_string()}}]}}]}));
    }
    chunks.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}));
    let server = serve(encode(chunks), Ending::Complete).await;
    let mut stream = crate::stream(
        &model(&server.base_url, "openai-completions"),
        &Context::default(),
        Some(options(Some(16))),
    )
    .unwrap();
    wait_blocked(&stream).await;
    assert_eq!(queue_len(&stream), 16);
    let mut events = Vec::new();
    while let Some(event) = bounded(stream.next_event()).await {
        assert!(queue_len(&stream) <= 16);
        events.push(event);
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AssistantMessageEvent::ToolcallStart { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AssistantMessageEvent::ToolcallEnd { .. }))
            .count(),
        1
    );
    let actual = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::ToolcallDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(actual, arguments);
    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    let result = bounded(stream.result()).await;
    assert_eq!(result.stop_reason, StopReason::ToolUse);
    let AssistantContent::ToolCall(call) = &result.content[0] else {
        panic!("tool result")
    };
    assert_eq!(
        call.arguments,
        json!({"text":"x".repeat(DELTAS)})
            .as_object()
            .cloned()
            .unwrap()
    );
}
