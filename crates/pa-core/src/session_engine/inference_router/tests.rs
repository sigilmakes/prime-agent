use super::*;
use crate::session_engine::{
    engine::{create_session, SessionEngineConfig},
    PromptOptions,
};
use pa_agent::scripted::ScriptedProvider;
use serde_json::Value;
use std::fmt::Write as _;

const PROVIDER: &str = r"
return {providers={{name='custom',mode='streaming',run=function(input,ctx)
  local text=config.tag..':'..input.model.id
  local message={role='assistant',content={{type='text',text=''}},api=input.model.api,provider=input.model.provider,
    model=input.model.id,usage={input=0,output=0,cacheRead=0,cacheWrite=0,totalTokens=0,
    cost={input=0,output=0,cacheRead=0,cacheWrite=0,total=0}},stopReason='stop',timestamp=0}
  ctx.emit({type='start',partial=message})
  message.content={{type='text',text=text}}
  ctx.emit({type='text_delta',contentIndex=0,delta=text,partial=message})
  ctx.emit({type='done',reason='stop',message=message})
  return message
end}}}
";

struct Fixture {
    dir: tempfile::TempDir,
    cwd: std::path::PathBuf,
    engine: super::super::engine::SessionEngine,
    native: Arc<ScriptedProvider>,
}

impl Fixture {
    async fn new(source: &str, tag: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(cwd.join("provider.lua"), source).unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            json!({"harnessRefinements":false,
              "enableBuiltinSkills":false,"bundledSkills":{"websearch":false},
              "plugins":{"route":{"enabled":true,"path":"provider.lua","config":{"tag":tag}}}
            })
            .to_string(),
        )
        .unwrap();
        let model = model("main");
        let native = Arc::new(ScriptedProvider::new(model.clone()));
        let engine = create_session(SessionEngineConfig {
            cwd: cwd.clone(),
            agent_dir,
            model: Some(model),
            stream_fn: Some(native.stream_fn()),
            daemonless: true,
            prewarm_ipython_kernel: Some(false),
            ..Default::default()
        })
        .await
        .unwrap();
        Self {
            dir,
            cwd,
            engine,
            native,
        }
    }
    async fn stream(&self, id: &str) -> Box<dyn ModelStream> {
        self.engine.session.side_question_stream_fn().unwrap()(
            model(id),
            LlmContext::default(),
            StreamRequestOptions::default(),
        )
        .await
        .unwrap()
    }
    async fn disable(&self) {
        std::fs::write(
            self.cwd.join(".prime/agent/settings.json"),
            json!({"plugins":{"route":{"enabled":false}}}).to_string(),
        )
        .unwrap();
        self.engine.reload_plugins().await.unwrap();
    }
}

fn model(id: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: "openai-completions".into(),
        provider: "custom".into(),
        base_url: "http://127.0.0.1:1".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 10000,
        max_tokens: 100,
        max_tokens_explicit: false,
    }
}

fn text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            pa_agent::types::AssistantContent::Text(text) => Some(text.text.as_str()),
            pa_agent::types::AssistantContent::Thinking(_)
            | pa_agent::types::AssistantContent::ToolCall(_) => None,
        })
        .collect()
}

#[tokio::test]
async fn engine_main_and_auxiliary_routes_are_session_scoped_and_stream_real_deltas() {
    let a = Fixture::new(PROVIDER, "alpha").await;
    let b = Fixture::new(PROVIDER, "beta").await;
    assert_eq!(a.engine.plugin_diagnostic(), None);
    assert_eq!(b.engine.plugin_diagnostic(), None);
    a.engine
        .prompt("first main", PromptOptions::default())
        .await
        .unwrap();
    b.engine
        .prompt("second main", PromptOptions::default())
        .await
        .unwrap();
    for (fixture, expected) in [(&a, "alpha"), (&b, "beta")] {
        let mut stream = fixture.stream("auxiliary").await;
        assert!(matches!(
            stream.next_event().await,
            Some(AssistantMessageEvent::Start { .. })
        ));
        assert!(
            matches!(stream.next_event().await,Some(AssistantMessageEvent::TextDelta{delta,..}) if delta == format!("{expected}:auxiliary"))
        );
        assert!(matches!(
            stream.next_event().await,
            Some(AssistantMessageEvent::Done { .. })
        ));
        assert_eq!(
            text(&stream.result().await.unwrap()),
            format!("{expected}:auxiliary")
        );
        let requested = provider_adapter::native_model(&model("dedicated-auxiliary")).unwrap();
        let result = fixture
            .engine
            .session
            .auxiliary_inference()
            .unwrap()
            .complete(
                &requested,
                pa_types::ai::Context {
                    system_prompt: Some("auxiliary only".into()),
                    messages: vec![],
                    tools: None,
                },
                StreamRequestOptions::default(),
                None,
            )
            .await
            .unwrap();
        let result: AssistantMessage = provider_adapter::json_round_trip(&result).unwrap();
        assert_eq!(text(&result), format!("{expected}:dedicated-auxiliary"));
        let state = fixture.engine.session.agent().state().await;
        let last = state
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                    message,
                )) => Some(message),
                _ => None,
            })
            .unwrap();
        assert_eq!(text(last), format!("{expected}:main"));
        assert!(fixture.native.calls().is_empty());
    }
}

#[tokio::test]
async fn invalid_reload_preserves_route_and_disable_fences_queued_terminal_without_fallback() {
    let f = Fixture::new(PROVIDER, "kept").await;
    let mut stream = f.stream("before").await;
    assert!(matches!(
        stream.next_event().await,
        Some(AssistantMessageEvent::Start { .. })
    ));
    std::fs::write(f.cwd.join("provider.lua"), "not valid Lua !").unwrap();
    assert!(f.engine.reload_plugins().await.is_err());
    assert_eq!(
        text(&f.stream("preserved").await.result().await.unwrap()),
        "kept:preserved"
    );
    f.disable().await;
    assert!(
        matches!(stream.next_event().await,Some(AssistantMessageEvent::Error{error,..}) if error.error_message.as_deref().unwrap().contains("retired"))
    );
    assert!(stream.result().await.is_err());
    assert!(f.native.calls().is_empty());
    f.native.push_text_turn("native now");
    let mut native = f.stream("main").await;
    while native.next_event().await.is_some() {}
    assert_eq!(text(&native.result().await.unwrap()), "native now");
}

#[tokio::test]
async fn malformed_stream_and_cancelled_caller_are_terminal_diagnostics() {
    let f=Fixture::new("return {providers={{name='custom',mode='streaming',run=function(i,c) c.emit({bogus=true}); return {} end}}}","bad").await;
    let mut stream = f.stream("malformed").await;
    assert!(
        matches!(stream.next_event().await,Some(AssistantMessageEvent::Error{error,..}) if error.error_message.as_deref().unwrap().contains("invalid plugin inference event"))
    );
    assert!(stream.result().await.is_err());
    let good = Fixture::new(PROVIDER, "cancel").await;
    let controller = pa_agent::abort::AbortController::new();
    let mut stream = good.engine.session.side_question_stream_fn().unwrap()(
        model("main"),
        LlmContext::default(),
        StreamRequestOptions {
            signal: controller.signal(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        stream.next_event().await,
        Some(AssistantMessageEvent::Start { .. })
    ));
    controller.abort();
    assert!(matches!(
        stream.next_event().await,
        Some(AssistantMessageEvent::Error { .. })
    ));
    assert!(stream.result().await.is_err());
    assert!(good.native.calls().is_empty());
}

#[tokio::test]
async fn lua_can_wrap_native_request_and_transform_response_without_recursion() {
    const WRAPPER: &str = r"
return {providers={{name='custom',mode='streaming',run=function(input,ctx)
  input.context.system_prompt='wrapped instructions'
  local handle=ctx.call('native.inference.open',{request=input.request,context=input.context}).handle
  while true do
    local next=ctx.call('native.inference.next',{handle=handle})
    if next.event==null then break end
    local event=next.event
    local function change(m)
      for _,part in ipairs(m.content) do if part.type=='text' then part.text='after:'..part.text end end
    end
    if event.partial then change(event.partial) end
    if event.message then change(event.message) end
    if event.delta then event.delta='after:'..event.delta end
    ctx.emit(event)
  end
  local result=ctx.call('native.inference.result',{handle=handle})
  for _,part in ipairs(result.content) do if part.type=='text' then part.text='after:'..part.text end end
  return result
end}}}
";
    let f = Fixture::new(WRAPPER, "wrapper").await;
    f.native.push_text_turn("native content");
    let result = f.stream("auxiliary").await.result().await.unwrap();
    assert_eq!(text(&result), "after:native content");
    let calls = f.native.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].system_prompt.as_deref(),
        Some("wrapped instructions")
    );
}

#[tokio::test]
async fn normalized_tool_events_and_terminal_tool_result_survive_the_route() {
    let source=PROVIDER.replace("message.content={{type='text',text=text}}", "message.content={{type='toolCall',id='call_1',name='lookup',arguments={query='kept'}}}")
      .replace("ctx.emit({type='text_delta',contentIndex=0,delta=text,partial=message})",
      "ctx.emit({type='toolcall_start',contentIndex=0,partial=message}); ctx.emit({type='toolcall_delta',contentIndex=0,delta='query',partial=message}); ctx.emit({type='toolcall_end',contentIndex=0,toolCall=message.content[1],partial=message})")
      .replace("stopReason='stop'", "stopReason='toolUse'")
      .replace("reason='stop'", "reason='toolUse'");
    let f = Fixture::new(&source, "tools").await;
    let mut stream = f.stream("tool-model").await;
    assert!(matches!(
        stream.next_event().await,
        Some(AssistantMessageEvent::Start { .. })
    ));
    assert!(matches!(
        stream.next_event().await,
        Some(AssistantMessageEvent::ToolCallStart {
            content_index: 0,
            ..
        })
    ));
    assert!(
        matches!(stream.next_event().await,Some(AssistantMessageEvent::ToolCallDelta{delta,..}) if delta=="query")
    );
    assert!(
        matches!(stream.next_event().await,Some(AssistantMessageEvent::ToolCallEnd{tool_call,..}) if tool_call.arguments==json!({"query":"kept"}))
    );
    assert!(matches!(
        stream.next_event().await,
        Some(AssistantMessageEvent::Done {
            reason: StopReason::ToolUse,
            ..
        })
    ));
    let result = stream.result().await.unwrap();
    assert_eq!(result.tool_calls()[0].name, "lookup");
    assert_eq!(result.model, "tool-model");
}

async fn emitted(engine: &super::super::engine::SessionEngine) -> u64 {
    use crate::kernel::shared::HostRequestPayload;
    let handlers = engine.plugin_host_handlers();
    let session = engine.session.session_id().await;
    let catalog = handlers.get("plugin.catalog").unwrap()(HostRequestPayload {
        data: json!({"type":"plugin.catalog","session":session}),
    })
    .await
    .unwrap();
    let export = &catalog["exports"][0];
    let result = handlers.get("plugin.invoke").unwrap()(HostRequestPayload {
        data: json!({"type":"plugin.invoke","session":session,"plugin":export["plugin"],
          "generation":export["generation"],"export":"emitted","input":{}}),
    })
    .await
    .unwrap();
    result.as_u64().unwrap()
}

#[tokio::test]
async fn slow_consumer_applies_bounded_lua_backpressure_and_drop_stops_publication() {
    let source=PROVIDER.replace("return {providers", "local emitted=0; return {exports={{name='emitted',run=function() return emitted end}},providers")
      .replace("ctx.emit({type='text_delta',contentIndex=0,delta=text,partial=message})",
        "for i=1,100 do ctx.emit({type='text_delta',contentIndex=0,delta=text,partial=message}); emitted=i end");
    let f = Fixture::new(&source, "bounded").await;
    let mut stream = f.stream("many").await;
    assert!(matches!(
        stream.next_event().await,
        Some(AssistantMessageEvent::Start { .. })
    ));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if emitted(&f.engine).await >= 16 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(emitted(&f.engine).await, 16);
    drop(stream);
    // An exported read crosses the same Lua host queue after cancellation; no
    // detached producer can finish the remaining 84 publications.
    assert_eq!(emitted(&f.engine).await, 16);
    assert!(f.native.calls().is_empty());
}

async fn http_service(body: String) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/inference", listener.local_addr().unwrap());
    let job = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let n = socket.read(&mut buffer).await.unwrap();
            assert_ne!(n, 0);
            request.extend_from_slice(&buffer[..n]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]);
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|value| value.parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
        String::from_utf8(request).unwrap()
    });
    (url, job)
}

#[tokio::test]
async fn custom_http_sse_and_native_gateway_are_real_engine_provider_routes() {
    let message = pa_ai::faux::faux_assistant_text_message(
        "custom network",
        pa_ai::faux::FauxAssistantMessageOptions::default(),
    );
    let event = |event: Value| format!("data: {event}\n\n");
    let body = event(json!({"type":"start","partial":message}))
        + &event(
            json!({"type":"text_delta","contentIndex":0,"delta":"custom network","partial":message}),
        )
        + &event(json!({"type":"done","reason":"stop","message":message}));
    let (url, job) = http_service(body).await;
    let source = format!(
        r"
return {{providers={{{{name='custom',mode='streaming',run=function(input,ctx)
  local stream=ctx.call('native.http.open',{{url='{url}',method='POST',format='sse',
    headers={{Authorization='Bearer explicit-test-only'}},body={{hook='before',messages=input.context.messages}}}})
  assert(stream.status==200)
  local result=nil
  while true do
    local frame=ctx.call('native.http.next',{{handle=stream.handle}}).event
    if frame==null then break end
    local event=ctx.call('native.json.decode',{{text=frame.data}})
    if event.message then result=event.message end
    ctx.emit(event)
  end
  ctx.call('native.http.close',{{handle=stream.handle}})
  return result
end}}}}}}
"
    );
    let f = Fixture::new(&source, "http").await;
    assert_eq!(
        text(&f.stream("main").await.result().await.unwrap()),
        "custom network"
    );
    let request = job.await.unwrap();
    assert!(request
        .to_lowercase()
        .contains("authorization: bearer explicit-test-only"));
    assert!(request.contains("\"hook\":\"before\""));
    assert!(f.native.calls().is_empty());

    let body=concat!("data: {\"id\":\"gateway-test\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"gateway network\"},\"finish_reason\":null}]}\n\n",
      "data: {\"id\":\"gateway-test\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n", "data: [DONE]\n\n").to_owned();
    let (url, job) = http_service(body).await;
    let source = format!(
        r"
return {{providers={{{{name='custom',mode='streaming',run=function(input,ctx)
  local m={{id='gateway-model',name='Gateway',api='openai-completions',provider='test-gateway',
    baseUrl='{url}',reasoning=false,input={{'text'}},cost={{input=0,output=0,cacheRead=0,cacheWrite=0}},contextWindow=10000,maxTokens=100}}
  local stream=ctx.call('native.inference.open',{{model=m,apiKey='explicit-test-only',context=input.context}})
  while true do
    local event=ctx.call('native.inference.next',{{handle=stream.handle}}).event
    if event==null then break end
    ctx.emit(event)
  end
  return ctx.call('native.inference.result',{{handle=stream.handle}})
end}}}}}}
"
    );
    let f = Fixture::new(&source, "gateway").await;
    assert_eq!(
        text(&f.stream("main").await.result().await.unwrap()),
        "gateway network"
    );
    let request = job.await.unwrap();
    assert!(request.contains("gateway-model"));
    assert!(f.native.calls().is_empty());
}

#[tokio::test]
async fn dropping_engine_provider_stream_closes_held_http_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/held", listener.local_addr().unwrap());
    let message = pa_ai::faux::faux_assistant_text_message(
        "partial",
        pa_ai::faux::FauxAssistantMessageOptions::default(),
    );
    let first = format!("data: {}\n\n", json!({"type":"start","partial":message}));
    let job = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut bytes = [0; 2048];
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let n = socket.read(&mut bytes).await.unwrap();
            assert_ne!(n, 0);
            request.extend_from_slice(&bytes[..n]);
        }
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100000000\r\n\r\n{first}").as_bytes()).await.unwrap();
        socket.read(&mut bytes).await.unwrap()
    });
    let source = format!(
        r"
return {{providers={{{{name='custom',mode='streaming',run=function(input,ctx)
  local stream=ctx.call('native.http.open',{{url='{url}',format='sse'}})
  while true do
    local frame=ctx.call('native.http.next',{{handle=stream.handle}}).event
    if frame==null then return {{}} end
    ctx.emit(ctx.call('native.json.decode',{{text=frame.data}}))
  end
end}}}}}}
"
    );
    let f = Fixture::new(&source, "held").await;
    let mut stream = f.stream("main").await;
    assert!(matches!(
        stream.next_event().await,
        Some(AssistantMessageEvent::Start { .. })
    ));
    drop(stream);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), job)
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(f.native.calls().is_empty());
}

#[tokio::test]
async fn dedicated_auxiliary_retains_full_requested_model_and_explicit_options() {
    let f = Fixture::new(PROVIDER, "unused").await;
    f.disable().await;
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            api: Some("dedicated-session-auxiliary".into()),
            ..Default::default()
        });
    let captured = Arc::new(std::sync::Mutex::new(None));
    let output = Arc::clone(&captured);
    registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Factory(Arc::new(
        move |context, options, _, model| {
            *output.lock().unwrap() = Some((
                model.clone(),
                context.system_prompt.clone(),
                options.and_then(|options| options.api_key.clone()),
            ));
            Ok(pa_ai::faux::faux_assistant_text_message(
                "aux native",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
        },
    ))]);
    let mut requested = registration.get_model();
    requested.id = "other-provider-model".into();
    requested.headers = Some(std::collections::BTreeMap::from([(
        "x-model-header".into(),
        "retained".into(),
    )]));
    let result = f
        .engine
        .session
        .auxiliary_inference()
        .unwrap()
        .complete(
            &requested,
            pa_types::ai::Context {
                system_prompt: Some("exact auxiliary context".into()),
                messages: vec![],
                tools: None,
            },
            StreamRequestOptions {
                api_key: Some("aux-only-test".into()),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.model, requested.id);
    assert_eq!(
        *captured.lock().unwrap(),
        Some((
            requested,
            Some("exact auxiliary context".into()),
            Some("aux-only-test".into())
        ))
    );
    assert!(f.native.calls().is_empty());
    registration.unregister();
}

#[tokio::test]
async fn typed_native_input_survives_lua_delegate_and_actual_responses_transport() {
    use pa_ai::compaction::{
        CompactionLimits, CompactionRoute, ResponsesCompactor, ResponsesWindow,
    };
    const RAW: &str = r#"[ {"type":"future_native","value":1e+02,"escaped":"\/\u0061"}, {"type":"compaction","id":"cmp_test","encrypted_content":"opaque=="}, {"role":"user","content":"fresh"} ]"#;
    let sse=[
      json!({"type":"response.created","response":{"id":"raw_route"}}),
      json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_raw","type":"message","role":"assistant","content":[]}}),
      json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
      json!({"type":"response.output_text.delta","output_index":0,"delta":"raw result"}),
      json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_raw","type":"message","role":"assistant","content":[{"type":"output_text","text":"raw result"}]}}),
      json!({"type":"response.completed","response":{"id":"raw_route","status":"completed","model":"main","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}),
    ].iter().fold(String::new(), |mut body,event| {
        write!(body,"data: {event}\n\n").unwrap();
        body
    });
    let (url, job) = http_service(sse).await;
    let base = url.strip_suffix("/inference").unwrap();
    let mut requested = provider_adapter::native_model(&model("main")).unwrap();
    requested.api = "openai-responses".into();
    requested.base_url = format!("{base}/v1");
    let route = CompactionRoute {
        provider: "custom".into(),
        model: "main".into(),
        endpoint: format!("{base}/v1/responses/compact"),
    };
    let adapter = ResponsesCompactor::new(
        route.clone(),
        Some(route.clone()),
        CompactionLimits::default(),
    )
    .unwrap();
    let input = ResponsesWindow::from_stateless_input(&route, RAW, CompactionLimits::default())
        .unwrap()
        .inference_input(&adapter, &CancellationToken::new())
        .unwrap();
    let source = r"
return {providers={{name='custom',run=function(input,ctx)
  input.context.system_prompt='current plugin instructions'
  local stream=ctx.call('native.inference.open',{request=input.request,context=input.context})
  while true do
    local event=ctx.call('native.inference.next',{handle=stream.handle}).event
    if event==null then break end
    ctx.emit(event)
  end
  return ctx.call('native.inference.result',{handle=stream.handle})
end}}}
";
    let mut f = Fixture::new(source, "raw").await;
    f.engine = create_session(SessionEngineConfig {
        cwd: f.cwd.clone(),
        agent_dir: f.dir.path().join("agent"),
        model: Some(provider_adapter::json_round_trip(&requested).unwrap()),
        model_info: Some(requested.clone()),
        stream_fn: Some(provider_adapter::real_stream_fn(
            Some("raw-test-key".into()),
            requested.clone(),
        )),
        daemonless: true,
        prewarm_ipython_kernel: Some(false),
        ..Default::default()
    })
    .await
    .unwrap();
    let stream = f.engine.session.side_question_stream_fn().unwrap();
    let mut stream = provider_adapter::with_native_input(
        input,
        stream(
            provider_adapter::json_round_trip(&requested).unwrap(),
            LlmContext {
                system_prompt: Some("before plugin".into()),
                ..Default::default()
            },
            StreamRequestOptions::default(),
        ),
    )
    .await
    .unwrap();
    while stream.next_event().await.is_some() {}
    assert_eq!(text(&stream.result().await.unwrap()), "raw result");
    let wire = job.await.unwrap();
    let (head, body) = wire.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(head
        .to_lowercase()
        .contains("authorization: bearer raw-test-key"));
    let parsed: Value = serde_json::from_str(body).unwrap();
    assert_eq!(parsed["instructions"], "current plugin instructions");
    assert!(
        body.split_once("\"input\":").unwrap().1.starts_with(RAW),
        "raw prefix spelling changed: {body}"
    );
}
