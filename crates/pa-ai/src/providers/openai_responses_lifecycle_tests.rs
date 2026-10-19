//! Real HTTP lifecycle regressions for all three shared Responses transports.
use crate::event_stream::{AssistantMessageEventExt, AssistantMessageEventStream};
use crate::types::{
    AssistantContent, Context, Model, StopReason, StreamOptions, ToolCall, Transport,
};
use base64::Engine as _;
use serde_json::{json, Map, Value};
use std::fmt::Write as _;
use std::task::Poll;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum CompletionEvents {
    Once,
    DuplicateAndUnmatched,
}

fn events(completions: CompletionEvents) -> String {
    let text_done = json!({"type":"response.output_item.done","output_index":0,"item":{
        "id":"msg_provider","type":"message","role":"assistant","phase":"final_answer",
        "content":[{"type":"output_text","text":"hello world","annotations":[]}]}});
    let tool_done = json!({"type":"response.output_item.done","output_index":1,"item":{
        "id":"fc_provider","type":"function_call","call_id":"call_provider","name":"lookup",
        "arguments":r#"{"key":"value"}"#}});
    let mut items = vec![
        json!({"type":"response.created","response":{"id":"resp_provider"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{
            "id":"msg_provider","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{
            "type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"hello "}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"world"}),
        text_done.clone(),
        json!({"type":"response.output_item.added","output_index":1,"item":{
            "id":"fc_provider","type":"function_call","call_id":"call_provider","name":"lookup","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":r#"{"key":"#}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":r#""value"}"#}),
        tool_done.clone(),
    ];
    if matches!(completions, CompletionEvents::DuplicateAndUnmatched) {
        items.extend([text_done,tool_done,json!({"type":"response.output_item.done","output_index":999,"item":{
            "type":"function_call","id":"fc_unmatched","call_id":"call_unmatched","name":"must_not_be_synthesized","arguments":"{}"}})]);
    }
    items.push(json!({"type":"response.completed","response":{
        "id":"resp_provider","status":"completed","model":"m",
        "usage":{"input_tokens":2,"output_tokens":8,"total_tokens":10}}}));
    let mut body = String::new();
    for event in items {
        writeln!(body, "data: {event}\n").unwrap();
    }
    body
}

struct Local {
    base_url: String,
    task: tokio::task::JoinHandle<String>,
}
impl Drop for Local {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn local(body: String) -> Local {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let (offset, length, head) = loop {
            let n = socket.read(&mut buffer).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8(bytes[..offset].to_vec()).unwrap();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                break (offset + 4, length, head);
            }
        };
        while bytes.len() < offset + length {
            let n = socket.read(&mut buffer).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&buffer[..n]);
        }
        let request: Value = serde_json::from_slice(&bytes[offset..offset + length]).unwrap();
        assert_eq!(request["model"], json!("m"));
        let length = body.len();
        let head_response=format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
        socket.write_all(head_response.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        head
    });
    Local { base_url, task }
}

fn model(base_url: &str, api: &str) -> Model {
    serde_json::from_value(json!({
        "id":"m","name":"local","api":api,"provider":api,"baseUrl":base_url,
        "reasoning":false,"input":["text"],"contextWindow":100_000,"maxTokens":1000,
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}
    }))
    .unwrap()
}
fn options(capacity: Option<usize>) -> StreamOptions {
    // Fake per-invocation Codex JWT; no credentials, environment or home discovery.
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"local-account"}}"#);
    StreamOptions {
        api_key: Some(format!("local.{payload}.signature")),
        transport: Some(Transport::Sse),
        event_buffer_capacity: capacity,
        ..StreamOptions::default()
    }
}
async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(5), future)
        .await
        .expect("local lifecycle deadline")
}
// One fixture crosses each real public inference route in legacy and bounded mode.
#[allow(clippy::too_many_lines)] // assertions describe one complete provider lifecycle.
async fn assert_lifecycle(completions: CompletionEvents) {
    use crate::types::AssistantMessageEvent;
    for api in [
        "openai-responses",
        "azure-openai-responses",
        "openai-codex-responses",
    ] {
        for capacity in [None, Some(1)] {
            let mut server = local(events(completions)).await;
            let request_model = model(&server.base_url, api);
            let stream =
                crate::stream(&request_model, &Context::default(), Some(options(capacity)))
                    .unwrap();
            let received = bounded(stream.collect()).await;
            let types = received
                .iter()
                .map(AssistantMessageEventExt::event_type)
                .collect::<Vec<_>>();
            assert_eq!(
                types,
                vec![
                    "start",
                    "text_start",
                    "text_delta",
                    "text_delta",
                    "text_end",
                    "toolcall_start",
                    "toolcall_delta",
                    "toolcall_delta",
                    "toolcall_end",
                    "done"
                ],
                "{api}, capacity={capacity:?}"
            );
            let text_end = received
                .iter()
                .find_map(|event| match event {
                    AssistantMessageEvent::TextEnd { content, .. } => Some(content.as_str()),
                    _ => None,
                })
                .unwrap();
            assert_eq!(text_end, "hello world");
            let expected = ToolCall {
                id: "call_provider|fc_provider".into(),
                name: "lookup".into(),
                arguments: json!({"key":"value"}).as_object().cloned().unwrap(),
                thought_signature: None,
                rest: Map::new(),
            };
            let tool_end = received
                .iter()
                .find_map(|event| match event {
                    AssistantMessageEvent::ToolcallEnd { tool_call, .. } => Some(tool_call),
                    _ => None,
                })
                .unwrap();
            assert_eq!(tool_end, &expected);
            let AssistantMessageEvent::Done { message, .. } = received.last().unwrap() else {
                panic!("successful terminal")
            };
            assert_eq!(message.stop_reason, StopReason::ToolUse);
            assert_eq!(message.response_id.as_deref(), Some("resp_provider"));
            assert_eq!(message.content.len(), 2);
            let AssistantContent::Text(text) = &message.content[0] else {
                panic!("text")
            };
            assert_eq!(text.text, "hello world");
            let signature: Value =
                serde_json::from_str(text.text_signature.as_ref().unwrap()).unwrap();
            assert_eq!(signature["id"], json!("msg_provider"));
            assert_eq!(message.content[1], AssistantContent::ToolCall(expected));
            let head = bounded(&mut server.task).await.unwrap();
            let path = match api {
                "openai-responses" => "POST /v1/responses HTTP/1.1",
                "azure-openai-responses" => {
                    "POST /v1/deployments/m/responses?api-version=v1 HTTP/1.1"
                }
                "openai-codex-responses" => "POST /v1/codex/responses HTTP/1.1",
                _ => unreachable!(),
            };
            assert!(head.starts_with(path), "real {api} route: {head}");
        }
    }
}

#[tokio::test]
async fn real_responses_http_text_and_tool_each_end_once_across_all_shared_transports() {
    assert_lifecycle(CompletionEvents::Once).await;
}

#[tokio::test]
async fn duplicate_and_unmatched_done_items_do_not_synthesize_another_end_or_tool() {
    assert_lifecycle(CompletionEvents::DuplicateAndUnmatched).await;
}

#[tokio::test]
async fn cancellation_at_blocked_tool_end_keeps_authoritative_final_arguments() {
    use crate::types::AssistantMessageEvent;
    let preview_prefix = format!(r#"{{"preview":"{}"#, "x".repeat(8 * 1024));
    let final_arguments = json!({"source":"authoritative final"});
    let items = [
        json!({"type":"response.output_item.added","output_index":0,"item":{
            "id":"fc_provider","type":"function_call","call_id":"call_provider","name":"lookup","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":preview_prefix}),
        // The two-byte tail is below the accumulator's refresh interval, leaving
        // a dirty preview. The provider's completed item is authoritative.
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"\"}"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{
            "id":"fc_provider","type":"function_call","call_id":"call_provider","name":"lookup","arguments":final_arguments.to_string()}}),
        json!({"type":"response.completed","response":{"id":"resp_provider","status":"completed","model":"m"}}),
    ];
    let mut body = String::new();
    for event in items {
        writeln!(body, "data: {event}\n").unwrap();
    }
    let mut server = local(body).await;
    let signal = CancellationToken::new();
    let mut request_options = options(Some(1));
    request_options.signal = Some(signal.clone());
    let mut stream: AssistantMessageEventStream = crate::stream(
        &model(&server.base_url, "openai-responses"),
        &Context::default(),
        Some(request_options),
    )
    .unwrap();
    let mut received = Vec::new();
    for expected in ["start", "toolcall_start", "toolcall_delta"] {
        let event = bounded(stream.next_event()).await.unwrap();
        assert_eq!(event.event_type(), expected);
        received.push(event);
    }
    // Observe actual bounded-send readiness, without a sleep or timing guess:
    // the dirty final delta occupies the only slot while ToolcallEnd is blocked.
    bounded(futures::future::poll_fn(|cx| {
        let mut state=stream.shared.state.lock().unwrap();
        let final_delta=matches!(state.queue.front(),Some(AssistantMessageEvent::ToolcallDelta{delta,..}) if delta=="\"}");
        if final_delta && !state.producer_wakers.is_empty() {Poll::Ready(())}
        else {state.blocked_observer=Some(cx.waker().clone());Poll::Pending}
    })).await;
    signal.cancel();
    while let Some(event) = bounded(stream.next_event()).await {
        received.push(event);
    }
    assert_eq!(
        received.iter().filter(|event| event.is_terminal()).count(),
        1
    );
    assert_eq!(
        received
            .iter()
            .filter(|event| matches!(event, AssistantMessageEvent::ToolcallEnd { .. }))
            .count(),
        0
    );
    let terminal = bounded(stream.result()).await;
    assert_eq!(terminal.stop_reason, StopReason::Aborted);
    let AssistantContent::ToolCall(call) = &terminal.content[0] else {
        panic!("provider tool call")
    };
    assert_eq!(call.id, "call_provider|fc_provider");
    assert_eq!(
        call.arguments,
        final_arguments.as_object().cloned().unwrap()
    );
    bounded(&mut server.task).await.unwrap();
}
