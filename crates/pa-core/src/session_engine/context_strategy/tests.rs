use super::*;

#[path = "defaults_tests.rs"]
mod defaults;
#[path = "snap_handoff_tests.rs"]
mod snap_handoff;
use crate::session::manager::SessionManager;
use crate::session_engine::{
    engine::{create_session, SessionEngine, SessionEngineConfig},
    PromptOptions,
};
use pa_agent::scripted::ScriptedProvider;
use std::fmt::Write as _;
use std::{path::PathBuf, sync::Arc};

fn user(text: &str) -> AgentMessage {
    AgentMessage::User(pa_types::ai::UserMessage {
        content: pa_types::ai::UserContent::Text(text.into()),
        timestamp: 0,
        rest: pa_types::JsonMap::new(),
    })
}

struct Fixture {
    _dir: tempfile::TempDir,
    cwd: PathBuf,
    agent: PathBuf,
    engine: Arc<SessionEngine>,
    provider: Arc<ScriptedProvider>,
    model: pa_types::ai::Model,
}
impl Fixture {
    async fn new(source: &str, strategy: &str, persisted: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(cwd.join("policy.lua"), source).unwrap();
        std::fs::write(agent.join("settings.json"),json!({"harnessRefinements":false,"enableBuiltinSkills":false,"prewarmIpythonKernel":false,
            "contextStrategy":strategy,"plugins":{"policy":{"enabled":true,"path":"policy.lua"}},"compaction":{"keepRecentTokens":0}}).to_string()).unwrap();
        let model:pa_types::ai::Model=serde_json::from_value(json!({"id":"test","name":"Test","api":"test","provider":"test","baseUrl":"http://127.0.0.1:9/v1","reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":100_000,"maxTokens":100})).unwrap();
        let wire = super::super::provider_adapter::json_round_trip(&model).unwrap();
        let provider = Arc::new(ScriptedProvider::new(wire));
        let mut session = if persisted {
            SessionManager::persisted(&cwd, &dir.path().join("sessions"))
        } else {
            SessionManager::in_memory(&cwd)
        };
        session.append_message(user("old first")).unwrap();
        session.append_message(user("newest kept")).unwrap();
        session.flush_now().unwrap();
        let engine = create_session(SessionEngineConfig {
            cwd: cwd.clone(),
            agent_dir: agent.clone(),
            model: Some(super::super::provider_adapter::json_round_trip(&model).unwrap()),
            model_info: Some(model.clone()),
            session_manager: Some(session),
            stream_fn: Some(provider.stream_fn()),
            prewarm_ipython_kernel: Some(false),
            daemonless: true,
            ..Default::default()
        })
        .await
        .unwrap();
        Self {
            _dir: dir,
            cwd,
            agent,
            engine: Arc::new(engine),
            provider,
            model,
        }
    }
    async fn compact(
        &self,
        abort: Option<&pa_agent::abort::AbortSignal>,
    ) -> anyhow::Result<CompactOutcome> {
        self.engine
            .session
            .compact(None, &self.model, None, abort)
            .await
    }
    async fn state(&self) -> Value {
        json!({"rows":self.engine.session.entries().await,"view":self.engine.session.agent().state().await.messages})
    }
    async fn change(&self, source: &str) {
        std::fs::write(self.cwd.join("policy.lua"), source).unwrap();
        self.engine.reload_plugins().await.unwrap();
    }
}

const ORDERED: &str = r"
local log={}
local function add(s) log[#log+1]=s end
return {exports={{name='log',run=function()return log end}},compactors={
{name='first',run=function()add('first');return {status='failure',reason='local failure'} end},
{name='second',run=function()add('second');return {status='failure',reason='render failure'} end},
{name='third',run=function(input)add('third');return {status='success',summary='accepted third',first_kept_entry_id=input.entries[#input.entries].id} end}},
context_strategies={{name='view',run=function(input)
 if input.stage=='restore' then return {accepted=input.archive.version==1} end
 if input.stage=='assemble' then return {messages=input.messages} end
 return {policy='rolling',compactors={'policy/first','policy/second','policy/third'}}
end}}}
";

#[tokio::test]
async fn public_engine_orders_failures_and_commits_once_then_restores() {
    let f = Fixture::new(ORDERED, "policy/view", true).await;
    let before = f.engine.session.entries().await;
    let CompactOutcome::Ran(run) = f.compact(None).await.unwrap() else {
        panic!("no proposal accepted")
    };
    assert_eq!(run.result.summary, "accepted third");
    let after = f.engine.session.entries().await;
    assert_eq!(after.len(), before.len() + 1);
    let archive = run.entry.context_archive.as_ref().unwrap();
    assert_eq!(archive.compactor, "policy/third");
    assert_eq!(archive.strategy, "policy/view");
    assert_eq!(
        after
            .iter()
            .filter(|entry| matches!(entry, FileEntry::Compaction { .. }))
            .count(),
        1
    );
    // Reopening starts another process generation. Accepted provenance must not
    // demand the previous UUID; the current strategy validates its schema.
    let path = f
        .engine
        .session
        .shared_persistence()
        .lock()
        .await
        .get_session_file()
        .unwrap()
        .to_owned();
    let manager = SessionManager::open(&f.cwd, path.parent().unwrap(), &path);
    let restored = create_session(SessionEngineConfig {
        cwd: f.cwd.clone(),
        agent_dir: f.agent.clone(),
        model: Some(super::super::provider_adapter::json_round_trip(&f.model).unwrap()),
        model_info: Some(f.model.clone()),
        session_manager: Some(manager),
        stream_fn: Some(f.provider.stream_fn()),
        prewarm_ipython_kernel: Some(false),
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(
        restored.session.agent().state().await.messages,
        f.engine.session.agent().state().await.messages
    );
}

#[tokio::test]
async fn ineligible_decline_does_no_work_and_all_failures_preserve_exact_state() {
    let decline=ORDERED.replace("{status='failure',reason='local failure'}","{status='decline'}").replace("{status='failure',reason='render failure'}","{status='decline'}").replace("{status='success',summary='accepted third',first_kept_entry_id=input.entries[#input.entries].id}","{status='decline'}");
    let f = Fixture::new(&decline, "policy/view", true).await;
    let before = f.state().await;
    assert!(matches!(
        f.compact(None).await.unwrap(),
        CompactOutcome::Skipped(_)
    ));
    assert_eq!(f.state().await, before);
    assert!(f.provider.calls().is_empty());
    f.change(&ORDERED.replace("{status='success',summary='accepted third',first_kept_entry_id=input.entries[#input.entries].id}","{status='failure',reason='last failed'}")).await;
    assert!(f
        .compact(None)
        .await
        .unwrap_err()
        .to_string()
        .contains("chain failed"));
    assert_eq!(f.state().await, before);
}

const HELD: &str = r"
local entered=false
return {exports={{name='ready',run=function()return entered end}},compactors={
{name='hold',run=function()entered=true;while true do end end},
{name='fallback',run=function(input)return {status='success',summary='MUST NOT FALL BACK',first_kept_entry_id=input.entries[#input.entries].id} end}},
context_strategies={{name='view',run=function(input)
 if input.stage=='assemble' then return {messages=input.messages} end
 if input.stage=='restore' then return {accepted=true} end
 return {policy='full',compactors={'policy/hold','policy/fallback'}} end}}}
";

async fn ready(f: &Fixture) {
    let plugins = f.engine.session.plugins.as_ref().unwrap();
    let host = plugins.host();
    let export = host
        .snapshot()
        .contributions
        .iter()
        .find(|target| target.plugin == "policy" && target.name == "ready")
        .unwrap()
        .clone();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let mut invocation = host
                .invoke(&export, json!({}), &CancellationToken::new())
                .unwrap();
            let value = match invocation.next().await.unwrap() {
                crate::plugins::InvocationEvent::Finished(Ok(
                    crate::plugins::InvocationOutcome::Unary(value),
                )) => value,
                event => panic!("unexpected invocation event {event:?}"),
            };
            if value == Value::Bool(true) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancellation_and_retirement_are_terminal_not_fallback() {
    for retire in [false, true] {
        let f = Fixture::new(HELD, "policy/view", true).await;
        let before = f.state().await;
        let abort = pa_agent::abort::AbortController::new();
        let signal = abort.signal();
        let engine = Arc::clone(&f.engine);
        let model = f.model.clone();
        let task = tokio::spawn(async move {
            engine
                .session
                .compact(None, &model, None, Some(&signal))
                .await
        });
        ready(&f).await;
        if retire {
            f.change(ORDERED).await;
        } else {
            abort.abort();
        }
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        if retire {
            assert!(error.to_string().contains("retired"));
        } else {
            assert!(pa_agent::abort::is_abort_error(&error));
        }
        assert_eq!(f.state().await, before);
    }
}

#[tokio::test]
async fn selected_strategy_changes_startup_branch_and_actual_model_view_not_history() {
    let source = r"return {context_strategies={{name='last',run=function(input)
      if input.stage=='restore' then return {accepted=true} end
      if input.stage=='prepare' then return {policy='rolling',compactors={}} end
      local messages={}; for _,message in ipairs(input.messages) do
        if message.role~='user' or message.content~='old first' then messages[#messages+1]=message end
      end; return {messages=messages} end}}}";
    let f = Fixture::new(source, "policy/last", false).await;
    let state = f.engine.session.agent().state().await;
    assert_eq!(state.messages.len(), 1);
    assert_eq!(
        f.engine
            .session
            .entries()
            .await
            .iter()
            .filter(|entry| matches!(entry, FileEntry::Message { .. }))
            .count(),
        2
    );
    let rows = f.engine.session.entries().await;
    f.engine.session.rebuild_branch_context(rows).await.unwrap();
    assert_eq!(
        f.engine.session.agent().state().await.messages,
        state.messages
    );
    f.provider.push_text_turn("done");
    f.engine
        .prompt("fresh continuation", PromptOptions::default())
        .await
        .unwrap();
    let calls = f.provider.calls();
    assert_eq!(calls.len(), 1);
    let wire = serde_json::to_string(&calls[0].messages).unwrap();
    assert!(!wire.contains("old first"));
    assert!(wire.contains("newest kept"));
    assert!(wire.contains("fresh continuation"));
}

#[tokio::test]
async fn standard_native_adapter_is_actual_third_fallback_and_default_skip_parity() {
    let source = ORDERED.replace("'policy/third'", "'standard_compaction/standard'");
    let f = Fixture::new(&source, "policy/view", false).await;
    let faux = pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
        api: Some(format!("context-standard-{}", uuid::Uuid::new_v4())),
        ..Default::default()
    });
    faux.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "standard third result",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
    )]);
    let CompactOutcome::Ran(run) = f
        .engine
        .session
        .compact(None, &faux.get_model(), None, None)
        .await
        .unwrap()
    else {
        panic!("standard did not run")
    };
    assert!(run.result.summary.contains("standard third result"));
    assert_eq!(
        run.entry.context_archive.unwrap().compactor,
        "standard_compaction/standard"
    );
    f.change(
        &source
            .replace(
                "{status='failure',reason='local failure'}",
                "{status='decline'}",
            )
            .replace(
                "{status='failure',reason='render failure'}",
                "{status='decline'}",
            ),
    )
    .await;
    assert!(matches!(
        f.compact(None).await.unwrap(),
        CompactOutcome::Skipped("Already compacted")
    ));
}

struct Gate {
    ready: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    entered: std::sync::atomic::AtomicUsize,
}
impl crate::plugins::HostCapabilities for Arc<Gate> {
    fn call(
        &self,
        request: crate::plugins::CapabilityRequest,
        cancel: CancellationToken,
    ) -> crate::plugins::CapabilityFuture {
        let gate = Arc::clone(self);
        Box::pin(async move {
            if request.operation != "test.hold" {
                return Err("wrong test operation".into());
            }
            gate.entered
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            gate.ready.notify_one();
            tokio::select! {biased;()=cancel.cancelled()=>Err("cancelled".into()),permit=gate.release.acquire()=>{
                permit.map_err(|error|error.to_string())?.forget();Ok(Value::Null)
            }}
        })
    }
}

const GATED: &str = r"return {compactors={{name='gate',run=function(input,ctx)
 ctx.call('test.hold',{});return {status='success',summary='gated summary',first_kept_entry_id=input.entries[#input.entries].id} end}},
 context_strategies={{name='view',run=function(input)
 if input.stage=='assemble' then return {messages=input.messages} end
 if input.stage=='restore' then return {accepted=true} end
 return {policy='full',compactors={'policy/gate'}} end}}}";

async fn gated_compact(
    f: &Fixture,
) -> (
    Arc<Gate>,
    tokio::task::JoinHandle<anyhow::Result<CompactOutcome>>,
) {
    let gate = Arc::new(Gate {
        ready: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        entered: std::sync::atomic::AtomicUsize::new(0),
    });
    f.engine
        .session
        .plugins
        .as_ref()
        .unwrap()
        .set_capability_delegate("test.", Arc::new(Arc::clone(&gate)));
    let engine = Arc::clone(&f.engine);
    let model = f.model.clone();
    let task = tokio::spawn(async move { engine.session.compact(None, &model, None, None).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.ready.notified())
        .await
        .unwrap();
    (gate, task)
}

#[tokio::test]
async fn descendant_append_is_retained_but_branch_departure_reprepares_without_fallback() {
    let f = Fixture::new(GATED, "policy/view", true).await;
    let (gate, task) = gated_compact(&f).await;
    f.engine
        .session
        .shared_persistence()
        .lock()
        .await
        .append_message(user("descendant during work"))
        .unwrap();
    gate.release.add_permits(1);
    let CompactOutcome::Ran(run) = task.await.unwrap().unwrap() else {
        panic!("did not commit")
    };
    let state = serde_json::to_string(&f.engine.session.agent().state().await.messages).unwrap();
    assert_eq!(state.matches("descendant during work").count(), 1);
    assert_eq!(gate.entered.load(std::sync::atomic::Ordering::SeqCst), 1);
    let entries = f.engine.session.entries().await;
    let persisted = entries.last().unwrap();
    assert_eq!(persisted.parent_id(), entries[entries.len() - 2].id());
    assert!(!run
        .entry
        .context_archive
        .unwrap()
        .covered_entry_ids
        .contains(&entries[entries.len() - 2].id().unwrap().to_owned()));

    let f = Fixture::new(GATED, "policy/view", true).await;
    let (gate, task) = gated_compact(&f).await;
    // Branch departure while work runs: use a valid earlier branch prefix.
    let handle = f.engine.session.shared_persistence();
    {
        let mut session = handle.lock().await;
        let mut rows = session.retained_entries().to_vec();
        rows.truncate(3);
        session.adopt_entries(rows);
    }
    gate.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.ready.notified())
        .await
        .unwrap();
    gate.release.add_permits(1);
    assert!(matches!(
        task.await.unwrap().unwrap(),
        CompactOutcome::Ran(_)
    ));
    assert_eq!(gate.entered.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn intervening_compaction_reprepares_instead_of_falling_through() {
    let f = Fixture::new(GATED, "policy/view", true).await;
    let (gate, task) = gated_compact(&f).await;
    {
        let handle = f.engine.session.shared_persistence();
        let mut session = handle.lock().await;
        let first_kept = session
            .retained_entries()
            .iter()
            .find_map(FileEntry::id)
            .unwrap()
            .to_owned();
        session
            .append_compaction(pa_types::session::CompactionEntry {
                summary: "external accepted summary".into(),
                first_kept_entry_id: Some(first_kept),
                ..Default::default()
            })
            .unwrap();
    }
    gate.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.ready.notified())
        .await
        .unwrap();
    gate.release.add_permits(1);
    assert!(matches!(
        task.await.unwrap().unwrap(),
        CompactOutcome::Ran(_)
    ));
    assert_eq!(gate.entered.load(std::sync::atomic::Ordering::SeqCst), 2);
    let rows = f.engine.session.entries().await;
    assert_eq!(
        rows.iter()
            .filter(|row| matches!(row, FileEntry::Compaction { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn failed_durable_append_never_publishes_candidate() {
    let f = Fixture::new(ORDERED, "policy/view", true).await;
    let before = f.state().await;
    let path = f
        .engine
        .session
        .shared_persistence()
        .lock()
        .await
        .get_session_file()
        .unwrap()
        .to_owned();
    let durable_before = std::fs::read(&path).unwrap();
    let backup = path.with_extension("saved-jsonl");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    let error = f.compact(None).await.unwrap_err();
    assert!(!error.to_string().is_empty());
    assert_eq!(f.state().await, before);
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), durable_before);
}

async fn json_server(
    response: &str,
    compact_status: u16,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = response.to_owned();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let events = [
            json!({"type":"response.created","response":{"id":"resp_inference"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_wire","type":"message","role":"assistant","content":[]}}),
            json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
            json!({"type":"response.output_text.delta","output_index":0,"delta":"local result"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_wire","type":"message","role":"assistant","content":[{"type":"output_text","text":"local result"}]}}),
            json!({"type":"response.completed","response":{"id":"resp_inference","status":"completed","model":"test","usage":{"input_tokens":30,"output_tokens":2,"total_tokens":32}}}),
        ];
        let mut sse = String::new();
        for event in events {
            writeln!(sse, "data: {event}\n").unwrap();
        }
        for (status, body, content_type) in [
            (compact_status, body, "application/json"),
            (200, sse, "text/event-stream"),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..end]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8(bytes).unwrap());
            socket.write_all(format!("HTTP/1.1 {status} Status\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    (format!("http://{address}/v1"), rx, task)
}

#[tokio::test]
async fn native_archive_restores_raw_prefix_and_appends_only_uncovered_continuation() {
    let envelope = r#"{"id":"response_1","created_at":1,"object":"response.compaction","output":[{"type":"message","id":"kept","role":"user","content":[{"type":"input_text","text":"kept native"}]},{"type":"compaction","id":"cmp","encrypted_content":"opaque-\u0061","future":1.2300}],"usage":{"input_tokens":20,"output_tokens":7,"total_tokens":27}}"#;
    let (base, mut request, server) = json_server(envelope, 200).await;
    let f = Fixture::new(ORDERED, "default_context/rolling", true).await;
    let mut model = f.model.clone();
    model.provider = "local-gateway".into();
    model.api = "openai-responses".into();
    model.base_url = base.clone();
    model.headers = Some(std::collections::BTreeMap::from([
        ("X-Model-Route".into(), "local-route".into()),
        ("authorization".into(), "Bearer stale-model-key".into()),
    ]));
    // No compaction settings: the resolved route drives the default chain.
    let settings = json!({"harnessRefinements":false,"enableBuiltinSkills":false});
    std::fs::write(f.agent.join("settings.json"), settings.to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    f.engine
        .session
        .set_model(&model, &model.provider, &model.id)
        .await
        .unwrap();
    let CompactOutcome::Ran(run) = f
        .engine
        .session
        .compact(None, &model, Some("synthetic-only".into()), None)
        .await
        .unwrap()
    else {
        panic!("native did not compact")
    };
    let sent = request.recv().await.unwrap();
    let head = sent.split_once("\r\n\r\n").unwrap().0.to_ascii_lowercase();
    assert!(head.contains("x-model-route: local-route"));
    assert!(head.contains("authorization: bearer synthetic-only"));
    assert!(!head.contains("stale-model-key"));
    assert!(!sent.contains("system_prompt"));
    assert!(sent.contains("old first"));
    let ContextRepresentation::Responses { archive, .. } =
        &run.entry.context_archive.as_ref().unwrap().representation
    else {
        panic!("wrong archive")
    };
    assert!(archive.contains("1.2300"));
    assert!(archive.contains("encrypted_content"));
    assert!(f.engine.session.agent().state().await.messages.is_empty());
    f.engine
        .session
        .shared_persistence()
        .lock()
        .await
        .append_message(user("only fresh continuation"))
        .unwrap();
    // New engine, same journal, new plugin UUIDs. Its first actual model call sees
    // a typed native scope and an empty ordinary array, never a summary surrogate.
    let path = f
        .engine
        .session
        .shared_persistence()
        .lock()
        .await
        .get_session_file()
        .unwrap()
        .to_owned();
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = Arc::clone(&observed);
    let inner = super::super::provider_adapter::real_stream_fn(
        Some("synthetic-inference-only".into()),
        model.clone(),
    );
    let stream: pa_agent::stream::StreamFn = Arc::new(move |model, context, options| {
        let capture = Arc::clone(&capture);
        let inner = Arc::clone(&inner);
        Box::pin(async move {
            capture.lock().unwrap().push((
                super::super::provider_adapter::current_native_input().is_some(),
                context.messages.len(),
            ));
            inner(model, context, options).await
        })
    });
    let restored = create_session(SessionEngineConfig {
        cwd: f.cwd.clone(),
        agent_dir: f.agent.clone(),
        model: Some(super::super::provider_adapter::json_round_trip(&model).unwrap()),
        model_info: Some(model.clone()),
        stream_fn: Some(stream),
        session_manager: Some(SessionManager::open(&f.cwd, path.parent().unwrap(), &path)),
        prewarm_ipython_kernel: Some(false),
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(restored.session.agent().state().await.messages.len(), 1);
    restored
        .prompt("next user", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(*observed.lock().unwrap(), vec![(true, 0)]);
    let replay = request.recv().await.unwrap();
    server.await.unwrap();
    let body = replay.split_once("\r\n\r\n").unwrap().1;
    let decoded: Value = serde_json::from_str(body).unwrap();
    assert_eq!(decoded["input"].as_array().unwrap().len(), 4);
    assert_eq!(body.matches("only fresh continuation").count(), 1);
    assert_eq!(body.matches("next user").count(), 1);
    assert!(!body.contains("old first"));
    let raw_start = envelope.find("\"output\":").unwrap() + "\"output\":".len();
    let raw_end = envelope.find(",\"usage\":").unwrap();
    let native_prefix = &envelope[raw_start..raw_end - 1];
    let outbound = &body[body.find("\"input\":").unwrap() + "\"input\":".len()..];
    assert!(
        outbound.starts_with(native_prefix),
        "native ordered item bytes changed: {outbound}"
    );
    let mut incompatible = model.clone();
    incompatible.id = "different-model".into();
    restored
        .session
        .set_model(&incompatible, &incompatible.provider, &incompatible.id)
        .await
        .unwrap();
    let before = observed.lock().unwrap().len();
    let mismatch = restored
        .prompt("must not network", PromptOptions::default())
        .await
        .unwrap_err();
    assert!(mismatch.to_string().contains("route"));
    assert_eq!(observed.lock().unwrap().len(), before);
}

#[tokio::test]
async fn snap_archive_expands_on_compaction_restore_and_branch_without_duplicate_tail() {
    let f = Fixture::new(ORDERED, "default_context/rolling", true).await;
    let settings = json!({"harnessRefinements":false,"enableBuiltinSkills":false,"contextStrategy":"default_context/rolling","plugins":{
        "default_context":{"config":{"policy":"full","compactors":["snapcompact/snapcompact"]}},
        "snapcompact":{"config":{"profile":"explicit","frame_size":128,"frame_tokens":1,"max_frames":4,"max_source_bytes":10000}}
    }});
    std::fs::write(f.agent.join("settings.json"), settings.to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    let mut model = f.model.clone();
    model.input.push(pa_types::ai::ModelInput::Image);
    f.engine
        .session
        .set_model(&model, &model.provider, &model.id)
        .await
        .unwrap();
    let mut source = String::new();
    for n in 0..1500 {
        write!(source, "archive-source-{n} detailed continuation ").unwrap();
    }
    f.engine
        .session
        .shared_persistence()
        .lock()
        .await
        .append_message(user(&source))
        .unwrap();
    let CompactOutcome::Ran(run) = f
        .engine
        .session
        .compact(None, &model, None, None)
        .await
        .unwrap()
    else {
        panic!("snap did not run")
    };
    assert!(matches!(
        run.entry.context_archive.unwrap().representation,
        ContextRepresentation::Snapcompact { .. }
    ));
    let compacted = f.engine.session.agent().state().await.messages;
    assert!(!compacted.is_empty());
    let encoded = serde_json::to_string(&compacted).unwrap();
    assert!(encoded.contains("source chars omitted") || encoded.contains("archive-source-"));
    f.engine
        .session
        .shared_persistence()
        .lock()
        .await
        .append_message(user("newly uncovered tail"))
        .unwrap();
    let entries = f.engine.session.entries().await;
    f.engine
        .session
        .rebuild_branch_context(entries.clone())
        .await
        .unwrap();
    let branch = f.engine.session.agent().state().await.messages;
    assert_eq!(
        serde_json::to_string(&branch)
            .unwrap()
            .matches("newly uncovered tail")
            .count(),
        1
    );
    let path = f
        .engine
        .session
        .shared_persistence()
        .lock()
        .await
        .get_session_file()
        .unwrap()
        .to_owned();
    let restored = create_session(SessionEngineConfig {
        cwd: f.cwd.clone(),
        agent_dir: f.agent.clone(),
        model: Some(super::super::provider_adapter::json_round_trip(&model).unwrap()),
        model_info: Some(model),
        stream_fn: Some(f.provider.stream_fn()),
        session_manager: Some(SessionManager::open(&f.cwd, path.parent().unwrap(), &path)),
        prewarm_ipython_kernel: Some(false),
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(restored.session.agent().state().await.messages, branch);
}

#[tokio::test]
async fn invalid_archive_restore_does_not_adopt_branch_or_publish_view() {
    let f = Fixture::new(ORDERED, "policy/view", true).await;
    f.compact(None).await.unwrap();
    let before = f.state().await;
    let mut branch = f.engine.session.entries().await;
    let FileEntry::Compaction { payload, .. } = branch.last_mut().unwrap() else {
        panic!("missing compaction")
    };
    let archive = payload.context_archive.as_mut().unwrap();
    archive
        .covered_entry_ids
        .push(archive.covered_entry_ids[0].clone());
    assert!(f
        .engine
        .session
        .rebuild_branch_context(branch)
        .await
        .unwrap_err()
        .to_string()
        .contains("coverage"));
    assert_eq!(f.state().await, before);
}

#[tokio::test]
async fn selected_strategy_is_durable_before_compaction_and_not_reselected_from_global_settings() {
    let source = ORDERED.replace(
        "if input.stage=='assemble' then return {messages=input.messages} end",
        "if input.stage=='assemble' then return {messages={input.messages[#input.messages]}} end",
    );
    let f = Fixture::new(&source, "policy/view", true).await;
    assert!(!f
        .engine
        .session
        .entries()
        .await
        .iter()
        .any(|row| matches!(row, FileEntry::Compaction { .. })));
    assert_eq!(f.engine.session.agent().state().await.messages.len(), 1);
    let path = f
        .engine
        .session
        .shared_persistence()
        .lock()
        .await
        .get_session_file()
        .unwrap()
        .to_owned();
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(f.agent.join("settings.json")).unwrap())
            .unwrap();
    settings["contextStrategy"] = json!("default_context/rolling");
    std::fs::write(f.agent.join("settings.json"), settings.to_string()).unwrap();
    let restored = create_session(SessionEngineConfig {
        cwd: f.cwd.clone(),
        agent_dir: f.agent.clone(),
        model: Some(super::super::provider_adapter::json_round_trip(&f.model).unwrap()),
        model_info: Some(f.model.clone()),
        stream_fn: Some(f.provider.stream_fn()),
        session_manager: Some(SessionManager::open(&f.cwd, path.parent().unwrap(), &path)),
        prewarm_ipython_kernel: Some(false),
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(
        restored.session.agent().state().await.messages,
        f.engine.session.agent().state().await.messages
    );
    assert_eq!(
        restored
            .session
            .plugins
            .as_ref()
            .unwrap()
            .context_selection()
            .unwrap()
            .plugin,
        "policy"
    );
}

#[tokio::test]
async fn strategy_rejection_or_assembly_failure_before_commit_preserves_exact_state() {
    for source in [
        ORDERED.replace("return {accepted=input.archive.version==1}","return {accepted=false}"),
        ORDERED.replace("if input.stage=='assemble' then return {messages=input.messages} end", "if input.stage=='assemble' then assert(input.archive==nil or input.archive==null,'reject proposed view');return {messages=input.messages} end"),
    ] {
        let f=Fixture::new(&source,"policy/view",true).await;
        let before=f.state().await;
        assert!(f.compact(None).await.unwrap_err().to_string().contains("chain failed"));
        assert_eq!(f.state().await,before);
    }
}

const SWITCHABLE: &str = r"
local function identity(input)
 if input.stage=='restore' then return {accepted=true} end
 if input.stage=='assemble' then return {messages=input.messages} end
 return {policy='full'}
end
return {exports={{name='marker',run=function()return 'retained' end}},
context_strategies={
 {name='view',run=identity},
 {name='tail',run=function(input)if input.stage=='assemble' then
  local messages=input.messages;while #messages>1 do table.remove(messages,1) end;return {messages=messages}
 end return identity(input) end},
 {name='reject',run=function()error('candidate rejected')end},
 {name='held',run=function(input,ctx)if input.stage=='assemble' then ctx.call('test.hold',{}) end;return identity(input)end}
}}
";

async fn reopened(f: &Fixture) -> SessionEngine {
    let path = f
        .engine
        .session
        .shared_persistence()
        .lock()
        .await
        .get_session_file()
        .unwrap()
        .to_owned();
    create_session(SessionEngineConfig {
        cwd: f.cwd.clone(),
        agent_dir: f.agent.clone(),
        model: Some(super::super::provider_adapter::json_round_trip(&f.model).unwrap()),
        model_info: Some(f.model.clone()),
        stream_fn: Some(f.provider.stream_fn()),
        session_manager: Some(SessionManager::open(&f.cwd, path.parent().unwrap(), &path)),
        prewarm_ipython_kernel: Some(false),
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn explicit_selection_is_durable_and_restores_the_exact_selected_view() {
    let f = Fixture::new(SWITCHABLE, "policy/view", true).await;
    let before = f.engine.session.entries().await;
    f.engine
        .session
        .select_context_strategy("policy/tail")
        .await
        .unwrap();
    assert_eq!(f.engine.session.agent().state().await.messages.len(), 1);
    let after = f.engine.session.entries().await;
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(&after[..before.len()], &before);
    assert!(
        matches!(after.last(),Some(FileEntry::Custom{payload,..})if payload.custom_type==pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE && payload.data.as_ref().unwrap()["strategy"]=="policy/tail")
    );
    let restored = reopened(&f).await;
    assert_eq!(
        restored.session.agent().state().await.messages,
        f.engine.session.agent().state().await.messages
    );
    assert_eq!(
        route(
            &restored
                .session
                .plugins
                .as_ref()
                .unwrap()
                .context_selection()
                .unwrap()
        ),
        "policy/tail"
    );
    f.engine
        .session
        .select_context_strategy("policy/view")
        .await
        .unwrap();
    assert_eq!(f.engine.session.agent().state().await.messages.len(), 2);
    assert!(f.provider.calls().is_empty());
}

#[tokio::test]
async fn selection_validation_and_durable_append_failures_leave_old_choice_and_view_intact() {
    let f = Fixture::new(SWITCHABLE, "policy/view", true).await;
    let before = f.state().await;
    for target in ["missing/strategy", "policy/reject"] {
        assert!(f
            .engine
            .session
            .select_context_strategy(target)
            .await
            .is_err());
        assert_eq!(f.state().await, before);
    }
    let path = f
        .engine
        .session
        .shared_persistence()
        .lock()
        .await
        .get_session_file()
        .unwrap()
        .to_owned();
    let backup = path.with_extension("selection-backup");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(f
        .engine
        .session
        .select_context_strategy("policy/tail")
        .await
        .is_err());
    assert_eq!(f.state().await, before);
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    assert_eq!(
        route(
            &f.engine
                .session
                .plugins
                .as_ref()
                .unwrap()
                .context_selection()
                .unwrap()
        ),
        "policy/view"
    );
}

fn policy_enabled(f: &Fixture, enabled: bool) {
    let path = f.agent.join("settings.json");
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    settings["plugins"]["policy"]["enabled"] = Value::Bool(enabled);
    std::fs::write(path, settings.to_string()).unwrap();
}

#[tokio::test]
async fn explicit_disable_removes_contributions_and_refuses_model_work_until_explicit_selection() {
    let f = Fixture::new(SWITCHABLE, "policy/view", true).await;
    let before = f.state().await;
    let snapshot = f.engine.session.plugins.as_ref().unwrap().host().snapshot();
    std::fs::write(f.cwd.join("policy.lua"), "invalid Lua !").unwrap();
    assert!(f.engine.reload_plugins().await.is_err());
    assert_eq!(f.state().await, before);
    assert_eq!(
        f.engine
            .session
            .plugins
            .as_ref()
            .unwrap()
            .host()
            .snapshot()
            .revision,
        snapshot.revision
    );
    std::fs::write(f.cwd.join("policy.lua"), SWITCHABLE).unwrap();
    policy_enabled(&f, false);
    f.engine.reload_plugins().await.unwrap();
    assert!(!f
        .engine
        .session
        .plugins
        .as_ref()
        .unwrap()
        .host()
        .snapshot()
        .contributions
        .iter()
        .any(|target| target.plugin == "policy"));
    assert_eq!(f.state().await, before);
    assert!(f
        .engine
        .session
        .prompt("must not call transport", PromptOptions::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("re-enable"));
    assert!(f.provider.calls().is_empty());
    assert!(f
        .engine
        .session
        .agent()
        .state()
        .await
        .error_message
        .as_deref()
        .unwrap()
        .contains("re-enable"));
    f.engine
        .session
        .select_context_strategy("default_context/rolling")
        .await
        .unwrap();
    f.provider.push_text_turn("explicit recovery");
    f.engine
        .session
        .prompt("recover", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(f.provider.calls().len(), 1);
    policy_enabled(&f, true);
    f.engine.reload_plugins().await.unwrap();
    assert_eq!(
        route(
            &f.engine
                .session
                .plugins
                .as_ref()
                .unwrap()
                .context_selection()
                .unwrap()
        ),
        "default_context/rolling"
    );
    assert!(f.cwd.join("policy.lua").is_file());
}

async fn gated_selection(f: &Fixture) -> (Arc<Gate>, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let gate = Arc::new(Gate {
        ready: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        entered: std::sync::atomic::AtomicUsize::new(0),
    });
    f.engine
        .session
        .plugins
        .as_ref()
        .unwrap()
        .set_capability_delegate("test.", Arc::new(Arc::clone(&gate)));
    let engine = Arc::clone(&f.engine);
    let task =
        tokio::spawn(async move { engine.session.select_context_strategy("policy/held").await });
    tokio::time::timeout(std::time::Duration::from_secs(2), gate.ready.notified())
        .await
        .unwrap();
    (gate, task)
}

#[tokio::test]
async fn selection_cancel_retire_and_branch_change_do_not_publish_a_choice() {
    for mode in ["cancel", "retire", "branch"] {
        let f = Fixture::new(SWITCHABLE, "policy/view", true).await;
        let before = f.state().await;
        let (gate, task) = gated_selection(&f).await;
        match mode {
            "cancel" => {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            }
            "retire" => {
                f.change(&format!("{SWITCHABLE}\n--new generation")).await;
                assert!(task.await.unwrap().is_err());
            }
            _ => {
                let entries = f.engine.session.entries().await;
                f.engine
                    .session
                    .shared_persistence()
                    .lock()
                    .await
                    .adopt_entries(entries[..entries.len() - 1].to_vec());
                gate.release.add_permits(1);
                assert!(task
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("branch changed"));
            }
        }
        assert_eq!(
            f.engine.session.agent().state().await.messages,
            before["view"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| serde_json::from_value(value.clone()).unwrap())
                .collect::<Vec<pa_agent::types::AgentMessage>>()
        );
        assert_eq!(
            route(
                &f.engine
                    .session
                    .plugins
                    .as_ref()
                    .unwrap()
                    .context_selection()
                    .unwrap()
            ),
            "policy/view"
        );
        if mode != "branch" {
            assert_eq!(f.state().await, before);
        }
    }
}

#[tokio::test]
async fn selection_rejects_an_active_main_run_without_changing_history() {
    let f = Fixture::new(SWITCHABLE, "policy/view", true).await;
    let ready = Arc::new(tokio::sync::Notify::new());
    let notify = Arc::clone(&ready);
    let _subscription = f
        .engine
        .session
        .agent()
        .subscribe(move |event, _| {
            let notify = Arc::clone(&notify);
            Box::pin(async move {
                if let pa_agent::types::AgentEvent::MessageEnd { message } = event {
                    if message.role() == "user" {
                        notify.notify_one();
                    }
                }
                Ok(())
            })
        })
        .await;
    f.provider.push_stalled_turn("held");
    f.engine
        .session
        .prompt(
            "busy",
            PromptOptions {
                return_after_accepted: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), ready.notified())
        .await
        .unwrap();
    let before = f.state().await;
    assert!(f
        .engine
        .session
        .select_context_strategy("policy/tail")
        .await
        .unwrap_err()
        .to_string()
        .contains("busy"));
    assert_eq!(f.state().await, before);
    f.engine.session.agent().abort();
    f.engine.session.agent().wait_for_idle().await;
}

struct BudgetCapture(tokio::sync::mpsc::UnboundedSender<Value>);
impl crate::plugins::HostCapabilities for BudgetCapture {
    fn call(
        &self,
        request: crate::plugins::CapabilityRequest,
        _: CancellationToken,
    ) -> crate::plugins::CapabilityFuture {
        self.0.send(request.input).unwrap();
        Box::pin(async { Ok(Value::Null) })
    }
}

#[tokio::test]
async fn model_budget_receives_exact_once_enriched_instructions_tools_and_output_override() {
    let source = r"return {exports={{name='marker',run=function()return true end}},
 hooks={{name='context_contribution',run=function()return {instructions='BUDGET-CURRENT-INSTRUCTION'}end}},
 context_strategies={{name='view',run=function(input,ctx)
  if input.stage=='assemble' then if input.phase=='model' then ctx.call('budget.capture',input) end;return {messages=input.messages} end
  if input.stage=='restore' then return {accepted=true} end;return {policy='full'}
 end}}}";
    let f = Fixture::new(source, "policy/view", true).await;
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    f.engine
        .session
        .plugins
        .as_ref()
        .unwrap()
        .set_capability_delegate("budget.", Arc::new(BudgetCapture(sent)));
    f.provider.push_text_turn("bounded");
    f.engine
        .session
        .prompt("real model request", PromptOptions::default())
        .await
        .unwrap();
    let observed = received.recv().await.unwrap();
    let calls = f.provider.calls();
    assert_eq!(
        observed["instructions"],
        serde_json::to_value(&calls[0].system_prompt).unwrap()
    );
    assert_eq!(
        observed["tools"],
        serde_json::to_value(&calls[0].tools).unwrap()
    );
    assert_eq!(observed["output_tokens"], f.model.max_tokens);
    assert_eq!(
        calls[0]
            .system_prompt
            .as_ref()
            .unwrap()
            .matches("BUDGET-CURRENT-INSTRUCTION")
            .count(),
        1
    );
    assert!(calls[0]
        .system_prompt
        .as_ref()
        .unwrap()
        .contains("plugin.policy.marker"));
    f.provider.push_text_turn("overridden");
    let mut stream = f.engine.session.agent().stream_fn().unwrap()(
        super::super::provider_adapter::json_round_trip(&f.model).unwrap(),
        pa_agent::stream::LlmContext {
            system_prompt: Some("current request".into()),
            messages: Vec::new(),
            tools: vec![pa_agent::stream::ToolDefinition {
                name: "current_tool".into(),
                description: "tool cost is current".into(),
                parameters: json!({"type":"object"}),
            }],
        },
        pa_agent::stream::StreamRequestOptions {
            max_tokens: Some(7),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    while stream.next_event().await.is_some() {}
    let overridden = received.recv().await.unwrap();
    let calls = f.provider.calls();
    assert_eq!(overridden["output_tokens"], 7);
    assert_eq!(
        overridden["instructions"],
        serde_json::to_value(&calls[1].system_prompt).unwrap()
    );
    assert_eq!(
        overridden["tools"],
        serde_json::to_value(&calls[1].tools).unwrap()
    );
    assert_eq!(
        calls[1]
            .system_prompt
            .as_ref()
            .unwrap()
            .matches("BUDGET-CURRENT-INSTRUCTION")
            .count(),
        1
    );
}

#[tokio::test]
async fn explicit_disable_is_not_blocked_by_an_unrelated_invalid_enabled_replacement() {
    let source = SWITCHABLE.replace(
        "return {exports=",
        "return {commands={{name='policy_action',run=function()return true end}},exports=",
    );
    let f = Fixture::new(&source, "policy/view", true).await;
    let before = f.state().await;
    std::fs::write(f.cwd.join("broken.lua"), "invalid Lua !").unwrap();
    let path = f.agent.join("settings.json");
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    settings["plugins"]["policy"]["enabled"] = json!(false);
    settings["plugins"]["broken"] = json!({"enabled":true,"path":"broken.lua"});
    std::fs::write(path, settings.to_string()).unwrap();
    let error = f.engine.reload_plugins().await.unwrap_err().to_string();
    assert!(error.contains("applied") && error.contains("rejected"));
    assert!(!f
        .engine
        .session
        .plugins
        .as_ref()
        .unwrap()
        .host()
        .snapshot()
        .contributions
        .iter()
        .any(|target| target.plugin == "policy"));
    assert_eq!(f.state().await, before);
    assert!(f
        .engine
        .session
        .prompt("must remain disabled", PromptOptions::default())
        .await
        .is_err());
    assert!(f.provider.calls().is_empty());
    assert!(!f
        .engine
        .session
        .session_command_inventory()
        .iter()
        .any(|command| command.name == "policy_action"));
    f.engine
        .session
        .select_context_strategy("default_context/rolling")
        .await
        .unwrap();
    f.provider.push_text_turn("explicitly recovered");
    f.engine
        .session
        .prompt("current inventory", PromptOptions::default())
        .await
        .unwrap();
    let calls = f.provider.calls();
    let prompt = calls[0].system_prompt.as_ref().unwrap();
    assert!(!prompt.contains("plugin.policy.marker") && !prompt.contains("/policy_action"));
}

#[tokio::test]
async fn explicit_startup_strategy_errors_do_not_select_rolling_or_commit_a_choice() {
    let f = Fixture::new(SWITCHABLE, "policy/view", true).await;
    for requested in ["policy/view", "policy/reject"] {
        let path = f.agent.join("settings.json");
        let mut settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        settings["contextStrategy"] = json!(requested);
        std::fs::write(path, settings.to_string()).unwrap();
        std::fs::write(
            f.cwd.join("policy.lua"),
            if requested == "policy/view" {
                "invalid Lua !"
            } else {
                SWITCHABLE
            },
        )
        .unwrap();
        let mut manager = SessionManager::persisted(
            &f.cwd,
            &f.agent.parent().unwrap().join(requested.replace('/', "-")),
        );
        manager
            .append_message(user("unchanged startup source"))
            .unwrap();
        manager.flush_now().unwrap();
        let file = manager.get_session_file().unwrap().to_owned();
        let before = std::fs::read(&file).unwrap();
        let error = create_session(SessionEngineConfig {
            cwd: f.cwd.clone(),
            agent_dir: f.agent.clone(),
            model: Some(super::super::provider_adapter::json_round_trip(&f.model).unwrap()),
            model_info: Some(f.model.clone()),
            stream_fn: Some(f.provider.stream_fn()),
            session_manager: Some(manager),
            prewarm_ipython_kernel: Some(false),
            daemonless: true,
            ..Default::default()
        })
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains(if requested == "policy/view" {
            "selected context strategy activation failed"
        } else {
            "candidate rejected"
        }));
        assert_eq!(std::fs::read(&file).unwrap(), before);
        assert!(f.provider.calls().is_empty());
    }
}

#[tokio::test]
async fn switching_strategy_uses_canonical_source_not_a_foreign_archive_or_hidden_fallback() {
    let source=ORDERED.replace("context_strategies={{name='view'",r"context_strategies={{name='tail',run=function(input)
 if input.stage=='assemble' then local m=input.messages;while #m>1 do table.remove(m,1) end;return {messages=m} end
 if input.stage=='restore' then return {accepted=true} end;return {policy='full'}end},{name='view'");
    let f = Fixture::new(&source, "policy/view", true).await;
    f.compact(None).await.unwrap();
    let compacted = f.engine.session.agent().state().await.messages;
    f.engine
        .session
        .select_context_strategy("policy/tail")
        .await
        .unwrap();
    let selected = f.engine.session.agent().state().await.messages;
    let json = serde_json::to_value(&selected).unwrap();
    assert_eq!(json[0]["role"], "user");
    assert_eq!(json[0]["content"], "newest kept");
    assert_eq!(
        reopened(&f).await.session.agent().state().await.messages,
        selected
    );
    f.engine
        .session
        .select_context_strategy("policy/view")
        .await
        .unwrap();
    assert_eq!(f.engine.session.agent().state().await.messages, compacted);
    assert!(f.provider.calls().is_empty());
}

#[tokio::test]
async fn invalid_settings_preserve_last_valid_sources_but_removing_a_source_retires_it() {
    let f = Fixture::new(SWITCHABLE, "policy/view", true).await;
    let path = f.agent.join("settings.json");
    let original = std::fs::read_to_string(&path).unwrap();
    let before = f.state().await;
    let revision = f
        .engine
        .session
        .plugins
        .as_ref()
        .unwrap()
        .host()
        .snapshot()
        .revision;
    let mut null_entry: Value = serde_json::from_str(&original).unwrap();
    null_entry["plugins"]["policy"] = Value::Null;
    let mut invalid_name: Value = serde_json::from_str(&original).unwrap();
    invalid_name["plugins"]["not-valid-python"] = json!({"enabled":true,"path":"policy.lua"});
    for invalid in [
        "{".to_owned(),
        null_entry.to_string(),
        invalid_name.to_string(),
    ] {
        std::fs::write(&path, invalid).unwrap();
        assert!(f.engine.reload_plugins().await.is_err());
        assert_eq!(
            f.engine
                .session
                .plugins
                .as_ref()
                .unwrap()
                .host()
                .snapshot()
                .revision,
            revision
        );
        assert_eq!(f.state().await, before);
    }
    let mut removed: Value = serde_json::from_str(&original).unwrap();
    removed["plugins"].as_object_mut().unwrap().remove("policy");
    std::fs::write(path, removed.to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    let snapshot = f.engine.session.plugins.as_ref().unwrap().host().snapshot();
    assert!(!snapshot
        .contributions
        .iter()
        .any(|target| target.plugin == "policy"));
    assert!(snapshot
        .contributions
        .iter()
        .any(|target| target.plugin == "default_context"));
    assert!(f
        .engine
        .session
        .plugins
        .as_ref()
        .unwrap()
        .context_selection()
        .is_err());
    assert_eq!(f.state().await, before);
    assert!(f.cwd.join("policy.lua").exists());
}

#[tokio::test]
async fn direct_agent_run_admitted_during_preparation_fences_selection_publication() {
    let f = Fixture::new(SWITCHABLE, "policy/view", true).await;
    let (gate, selection) = gated_selection(&f).await;
    let before = f.state().await;
    let ready = Arc::new(tokio::sync::Notify::new());
    let notify = Arc::clone(&ready);
    let _subscription = f
        .engine
        .session
        .agent()
        .subscribe(move |event, _| {
            let notify = Arc::clone(&notify);
            Box::pin(async move {
                if matches!(event, pa_agent::types::AgentEvent::TurnStart) {
                    notify.notify_one();
                }
                Ok(())
            })
        })
        .await;
    f.provider
        .push_stalled_turn("admitted during candidate prepare");
    let engine = Arc::clone(&f.engine);
    let main = tokio::spawn(async move { engine.session.agent().continue_run().await });
    tokio::time::timeout(std::time::Duration::from_secs(2), ready.notified())
        .await
        .unwrap();
    gate.release.add_permits(1);
    assert!(selection
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("busy"));
    assert_eq!(f.state().await, before);
    f.engine.session.agent().abort();
    let _ = main.await.unwrap();
}

#[tokio::test]
async fn mixed_reload_retires_disabled_a_and_keeps_the_last_valid_enabled_b_generation() {
    let f = Fixture::new(SWITCHABLE, "policy/view", true).await;
    let path = f.agent.join("settings.json");
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    std::fs::write(
        f.cwd.join("existing.lua"),
        "return {exports={{name='keep',run=function()return 'old B' end}}}",
    )
    .unwrap();
    settings["plugins"]["existing"] = json!({"enabled":true,"path":"existing.lua"});
    std::fs::write(&path, settings.to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    let host = f.engine.session.plugins.as_ref().unwrap().host();
    let old = host
        .snapshot()
        .contributions
        .iter()
        .find(|target| target.plugin == "existing")
        .unwrap()
        .clone();
    settings["plugins"]["policy"]["enabled"] = json!(false);
    std::fs::write(path, settings.to_string()).unwrap();
    std::fs::write(f.cwd.join("existing.lua"), "invalid Lua !").unwrap();
    let error = f.engine.reload_plugins().await.unwrap_err().to_string();
    assert!(error.contains("applied") && error.contains("rejected"));
    let snapshot = host.snapshot();
    assert!(!snapshot
        .contributions
        .iter()
        .any(|target| target.plugin == "policy"));
    assert_eq!(
        snapshot
            .contributions
            .iter()
            .find(|target| target.plugin == "existing")
            .unwrap()
            .generation,
        old.generation
    );
    let mut call = host
        .invoke(&old, json!({}), &CancellationToken::new())
        .unwrap();
    match call.next().await.unwrap() {
        crate::plugins::InvocationEvent::Finished(Ok(
            crate::plugins::InvocationOutcome::Unary(value),
        )) => assert_eq!(value, json!("old B")),
        other => panic!("last valid B failed after unrelated disable: {other:?}"),
    }
    assert!(f
        .engine
        .session
        .plugins
        .as_ref()
        .unwrap()
        .context_selection()
        .is_err());
}

const REFERENCE_VIEW: &str = r"return {context_strategies={{name='view',context_data='refs_v1',run=function(input)
 if input.stage=='restore' then return {accepted=true} end
 if input.stage=='prepare' then return {policy='full',compactors={'standard_compaction/standard'}} end
 assert(input.messages.version==1 and input.messages.kind=='messages')
 return {messages=input.messages}
end}}}";

#[tokio::test]
async fn reference_strategy_selects_explicit_spans_but_keeps_original_canonical_rows() {
    let source=REFERENCE_VIEW.replace("return {messages=input.messages}",
        "if input.messages.count>1 then input.messages.spans={{first=2,count=input.messages.count-1}} end; return {messages=input.messages}");
    let f = Fixture::new(&source, "policy/view", false).await;
    assert_eq!(f.engine.session.agent().state().await.messages.len(), 1);
    let rows = f.engine.session.entries().await;
    f.engine
        .session
        .rebuild_branch_context(rows.clone())
        .await
        .unwrap();
    assert_eq!(f.engine.session.entries().await, rows);
    assert_eq!(f.engine.session.agent().state().await.messages.len(), 1);
    f.provider.push_text_turn("done");
    f.engine
        .prompt("fresh continuation", PromptOptions::default())
        .await
        .unwrap();
    let calls = f.provider.calls();
    assert_eq!(calls.len(), 1);
    let wire = serde_json::to_string(&calls[0].messages).unwrap();
    assert!(!wire.contains("old first"));
    assert!(wire.contains("fresh continuation"));
}

#[tokio::test]
async fn reference_replay_is_rejected_before_actual_provider_or_selection_publication() {
    let source = REFERENCE_VIEW
        .replace(
            "return {context_strategies",
            "local old=nil; return {context_strategies",
        )
        .replace(
            "return {messages=input.messages}",
            "if old==nil then old=input.messages end; return {messages=old}",
        );
    let f = Fixture::new(&source, "policy/view", true).await;
    let before = f.state().await;
    let error = f
        .engine
        .session
        .select_context_strategy("policy/view")
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("invocation"));
    assert_eq!(f.state().await, before);
    let plugins = Arc::clone(f.engine.session.plugins.as_ref().unwrap());
    let stream = wrap_stream_fn(
        ModelContextSource::Assembled,
        plugins,
        Arc::new(std::sync::RwLock::new(None)),
        f.provider.stream_fn(),
    );
    let wire_model = super::super::provider_adapter::json_round_trip(&f.model).unwrap();
    let context = pa_agent::stream::LlmContext {
        messages: vec![pa_agent::types::Message::User(
            pa_agent::types::UserMessage {
                content: pa_agent::types::UserContent::Text("not persisted".into()),
                timestamp: 0,
            },
        )],
        ..Default::default()
    };
    let result = stream(
        wire_model,
        context,
        pa_agent::stream::StreamRequestOptions::default(),
    )
    .await;
    assert!(result
        .err()
        .is_some_and(|error| format!("{error:#}").contains("invocation")));
    assert!(f.provider.calls().is_empty());
    assert_eq!(f.state().await, before);
}

#[tokio::test]
async fn reference_compactors_cancel_or_retire_without_fallback_or_commit() {
    let source = HELD
        .replace("name='hold',run", "name='hold',context_data='refs_v1',run")
        .replace(
            "name='fallback',run",
            "name='fallback',context_data='refs_v1',run",
        )
        .replace("name='view',run", "name='view',context_data='refs_v1',run")
        .replace(
            "first_kept_entry_id=input.entries[#input.entries].id",
            "first_kept_entry_id=''",
        );
    for retire in [false, true] {
        let f = Fixture::new(&source, "policy/view", true).await;
        let before = f.state().await;
        let abort = pa_agent::abort::AbortController::new();
        let signal = abort.signal();
        let engine = Arc::clone(&f.engine);
        let model = f.model.clone();
        let task = tokio::spawn(async move {
            engine
                .session
                .compact(None, &model, None, Some(&signal))
                .await
        });
        ready(&f).await;
        if retire {
            f.change(ORDERED).await;
        } else {
            abort.abort();
        }
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        if retire {
            assert!(error.to_string().contains("retired"));
        } else {
            assert!(pa_agent::abort::is_abort_error(&error));
        }
        assert_eq!(f.state().await, before);
        assert!(f.provider.calls().is_empty());
    }
}

#[tokio::test]
async fn large_legacy_callback_fails_explicitly_and_builtin_name_override_keeps_body_contract() {
    let f = Fixture::new(REFERENCE_VIEW, "policy/view", false).await;
    let source = r"return {context_strategies={{name='rolling',run=function(input)
      assert(type(input.messages)=='table' and input.messages[1].content=='UNIQUE BODY')
      return {messages=input.messages} end}}}";
    std::fs::write(f.cwd.join("override.lua"), source).unwrap();
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(f.agent.join("settings.json")).unwrap())
            .unwrap();
    settings["plugins"]["default_context"] = json!({"enabled":true,"path":"override.lua"});
    std::fs::write(f.agent.join("settings.json"), settings.to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    let plugins = f.engine.session.plugins.as_ref().unwrap();
    let target = plugins.context_target("default_context/rolling").unwrap();
    assert_eq!(target.context_data, crate::plugins::ContextDataMode::Json);
    let small = vec![user("UNIQUE BODY")];
    assert_eq!(
        assemble_for_strategy(plugins, &target, small.clone(), None, None, None)
            .await
            .unwrap(),
        small
    );
    let before = f.state().await;
    let error = assemble_for_strategy(
        plugins,
        &target,
        vec![user(&"x".repeat(32 * 1024 * 1024))],
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("json full-body callback"));
    assert_eq!(f.state().await, before);
    assert!(f.provider.calls().is_empty());
}

#[tokio::test]
async fn reference_model_echo_matches_prior_tool_image_signature_conversion_and_exact_controls() {
    let source=REFERENCE_VIEW.replace("return {messages=input.messages}",
        "if input.phase=='model' then assert(input.instructions=='EXACT PROMPT' and input.tools[1].name=='actual_tool' and input.output_tokens==77) end; return {messages=input.messages}");
    let f = Fixture::new(&source, "policy/view", false).await;
    let mut messages:Vec<pa_agent::types::Message>=serde_json::from_value(json!([
      {"role":"user","content":[{"type":"image","data":"AA==","mimeType":"image/png"},{"type":"text","text":"image question"}],"timestamp":1},
      {"role":"assistant","content":[{"type":"thinking","thinking":"opaque","thinkingSignature":"raw signature","redacted":true},
        {"type":"toolCall","id":"call","name":"ipython","arguments":{"code":"print(1)"},"thoughtSignature":"opaque"}],
        "usage":{},"stopReason":"toolUse","timestamp":2},
      {"role":"toolResult","toolCallId":"call","toolName":"ipython","content":[{"type":"text","text":"1"}],"isError":false,"timestamp":3}
    ])).unwrap();
    messages.push(pa_agent::types::Message::User(
        pa_agent::types::UserMessage {
            content: pa_agent::types::UserContent::Text("x".repeat(32 * 1024 * 1024)),
            timestamp: 4,
        },
    ));
    let old = generated_model_messages(serde_json::to_value(&messages).unwrap()).unwrap();
    assert_eq!(old, messages);
    let plugins = Arc::clone(f.engine.session.plugins.as_ref().unwrap());
    let stream = wrap_stream_fn(
        ModelContextSource::Assembled,
        plugins,
        Arc::new(std::sync::RwLock::new(None)),
        f.provider.stream_fn(),
    );
    let mut declared_model = f.model.clone();
    declared_model.context_window = 64 * 1024 * 1024;
    let model = super::super::provider_adapter::json_round_trip(&declared_model).unwrap();
    let context = pa_agent::stream::LlmContext {
        messages: messages.clone(),
        system_prompt: Some("EXACT PROMPT".into()),
        tools: vec![pa_agent::stream::ToolDefinition {
            name: "actual_tool".into(),
            description: "exact".into(),
            parameters: json!({"type":"object","properties":{"x":{"type":"integer"}}}),
        }],
    };
    f.provider.push_text_turn("done");
    let _stream = stream(
        model,
        context,
        pa_agent::stream::StreamRequestOptions {
            max_tokens: Some(77),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(f.provider.calls()[0].messages, messages);
}

#[tokio::test]
async fn reference_default_restore_keeps_a_large_unknown_archive_body_out_of_lua() {
    let f = Fixture::new(REFERENCE_VIEW, "policy/view", false).await;
    let plugins = f.engine.session.plugins.as_ref().unwrap();
    let target = plugins.context_target("default_context/rolling").unwrap();
    let archive = ContextArchive {
        version: 1,
        strategy: "default_context/rolling".into(),
        compactor: "standard_compaction/standard".into(),
        creator_generation: "prior process provenance".into(),
        prepared_leaf: "leaf".into(),
        covered_entry_ids: vec!["leaf".into()],
        representation: ContextRepresentation::Standard {
            rest: [(
                "future_opaque".into(),
                json!("OPAQUE".repeat(6 * 1024 * 1024)),
            )]
            .into_iter()
            .collect(),
        },
    };
    let before = archive.clone();
    let messages = vec![user("retained continuation")];
    assert_eq!(
        assemble_for_strategy(
            plugins,
            &target,
            messages.clone(),
            Some(&archive),
            None,
            None
        )
        .await
        .unwrap(),
        messages
    );
    assert_eq!(archive, before);
    assert!(f.provider.calls().is_empty());
}

#[tokio::test]
async fn reference_selected_disable_fails_closed_until_explicit_recovery() {
    let f = Fixture::new(REFERENCE_VIEW, "policy/view", true).await;
    let before = f.state().await;
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(f.agent.join("settings.json")).unwrap())
            .unwrap();
    settings["plugins"]["policy"]["enabled"] = json!(false);
    std::fs::write(f.agent.join("settings.json"), settings.to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    let plugins = Arc::clone(f.engine.session.plugins.as_ref().unwrap());
    let stream = wrap_stream_fn(
        ModelContextSource::Assembled,
        plugins,
        Arc::new(std::sync::RwLock::new(None)),
        f.provider.stream_fn(),
    );
    let model = super::super::provider_adapter::json_round_trip(&f.model).unwrap();
    assert!(stream(
        model,
        pa_agent::stream::LlmContext::default(),
        pa_agent::stream::StreamRequestOptions::default()
    )
    .await
    .is_err());
    assert_eq!(f.state().await, before);
    assert!(f.provider.calls().is_empty());
    f.engine
        .session
        .select_context_strategy("default_context/rolling")
        .await
        .unwrap();
    assert_eq!(
        f.engine
            .session
            .plugins
            .as_ref()
            .unwrap()
            .context_selection()
            .unwrap()
            .context_data,
        crate::plugins::ContextDataMode::RefsV1
    );
}

#[tokio::test]
async fn oversized_legacy_compactor_remains_a_failed_stage_not_a_chain_bypass() {
    let source = r"local called=0; return {exports={{name='calls',run=function()return called end}},
      compactors={{name='body',run=function(input)called=called+1; assert(input.messages[1].content=='old first'); return {status='failure',reason='body callback ran'} end}},
      context_strategies={{name='view',context_data='refs_v1',run=function(input)
       if input.stage=='assemble' then return {messages=input.messages} end
       if input.stage=='restore' then return {accepted=true} end
       return {policy='full',compactors={'policy/body','standard_compaction/standard'}} end}}}";
    let f = Fixture::new(source, "policy/view", false).await;
    f.engine
        .session
        .session
        .lock()
        .await
        .append_message(user(&"x".repeat(32 * 1024 * 1024)))
        .unwrap();
    let faux = pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
        api: Some(format!("refs-json-fallback-{}", uuid::Uuid::new_v4())),
        ..Default::default()
    });
    let response =
        pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
            "native fallback accepted",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ));
    // The large final user turn needs both history and turn-prefix summaries.
    faux.set_responses(vec![response.clone(), response]);
    let CompactOutcome::Ran(run) = f
        .engine
        .session
        .compact(None, &faux.get_model(), None, None)
        .await
        .unwrap()
    else {
        panic!("ordered native fallback did not run")
    };
    assert_eq!(
        run.entry.context_archive.unwrap().compactor,
        "standard_compaction/standard"
    );
    let plugins = f.engine.session.plugins.as_ref().unwrap();
    let host = plugins.host();
    let target = host
        .snapshot()
        .contributions
        .iter()
        .find(|target| target.plugin == "policy" && target.name == "calls")
        .unwrap()
        .clone();
    let mut invocation = host
        .invoke(&target, json!({}), &CancellationToken::new())
        .unwrap();
    assert!(
        matches!(invocation.next().await,Some(crate::plugins::InvocationEvent::Finished(Ok(crate::plugins::InvocationOutcome::Unary(value)))) if value==json!(0)),
        "oversized JSON callback must not receive empty bodies"
    );
}
