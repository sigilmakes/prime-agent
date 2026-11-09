use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use pa_agent::abort::AbortSignal;
use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{
    AfterToolCallContext, AgentContext, AgentMessage, ToolCall, ToolResultContent,
};
use serde_json::{json, Value};

use super::plugin_stages::{observe_compaction, ToolStageHooks};
use crate::plugins::{
    CapabilityFuture, CapabilityRequest, ContributionKind, HostCapabilities, InvocationEvent,
    InvocationOutcome, PluginHost, PluginSource,
};
use crate::session::manager::SessionManager;
use crate::session_engine::{
    compact_session::{CompactOutcome, CompactRun},
    tool_bridge::bridge_tool,
    AgentSession, PromptOptions,
};
use crate::tools::tool_definition::{ToolContentBlock, ToolDefinition, ToolExecutionResult};
use pa_types::session::{CompactionEntry, FileEntry};
use tokio_util::sync::CancellationToken;

struct NoCapabilities;

impl HostCapabilities for NoCapabilities {
    fn call(&self, _: CapabilityRequest, _: CancellationToken) -> CapabilityFuture {
        Box::pin(async { Err("no capabilities in this test host".into()) })
    }
}

fn host(plugins: &[(&str, &str)]) -> Arc<PluginHost> {
    let host = PluginHost::new("hook-test".into(), Arc::new(NoCapabilities), []);
    host.reconcile(
        plugins
            .iter()
            .map(|(name, source)| PluginSource {
                name: (*name).into(),
                source: (*source).into(),
                config: json!({}),
            })
            .collect(),
    )
    .unwrap();
    Arc::new(host)
}

fn model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "hook-model", "name": "Hook model", "api": "test", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 10_000, "maxTokens": 100
    }))
    .unwrap()
}

fn probe_tool(
    executed: Arc<AtomicBool>,
    result_text: &'static str,
) -> Arc<dyn pa_agent::types::AgentTool> {
    bridge_tool(ToolDefinition {
        name: "probe".into(),
        label: "Probe".into(),
        description: "Return one fixed result".into(),
        prompt_snippet: String::new(),
        parameters: json!({"type": "object", "properties": {"input": {"type": "string"}}}),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_id, _args, _signal, _on_update| {
            let executed = Arc::clone(&executed);
            Box::pin(async move {
                executed.store(true, Ordering::SeqCst);
                Ok(ToolExecutionResult {
                    content: vec![ToolContentBlock::text(result_text)],
                    details: Some(json!({"original": true})),
                    is_error: false,
                })
            })
        }),
    })
}

struct RunResult {
    messages: Vec<AgentMessage>,
    entries: Vec<FileEntry>,
    executed: bool,
}

async fn run_probe(hooks: Option<ToolStageHooks>, result_text: &'static str) -> RunResult {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let manager = SessionManager::persisted(&cwd, &tmp.path().join("sessions"));
    let model = model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(
        Some("checking"),
        vec![("call-probe", "probe", json!({"input": "x"}))],
    );
    provider.push_text_turn("done");
    let executed = Arc::new(AtomicBool::new(false));
    let before_tool_call = hooks.as_ref().map(ToolStageHooks::before_tool_call);
    let after_tool_call = hooks.as_ref().map(ToolStageHooks::after_tool_call);
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            model: Some(model),
            tools: Some(vec![probe_tool(Arc::clone(&executed), result_text)]),
            ..Default::default()
        },
        stream_fn: Some(provider.stream_fn()),
        before_tool_call,
        after_tool_call,
        ..Default::default()
    }));
    let session = AgentSession::new(agent, manager, vec![]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        session
            .prompt("run the probe", PromptOptions::default())
            .await
            .unwrap();
        session.agent().wait_for_idle().await;
    })
    .await
    .expect("the scripted probe turn must finish");
    let messages = session.agent().state().await.messages;
    let entries = session.entries().await;
    RunResult {
        messages,
        entries,
        executed: executed.load(Ordering::SeqCst),
    }
}

fn message_rows(entries: &[FileEntry]) -> Vec<Value> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Message { message, .. } => Some(serde_json::to_value(message).unwrap()),
            _ => None,
        })
        .collect()
}

fn tool_result_row(rows: &[Value]) -> Value {
    rows.iter()
        .find(|row| row["role"] == "toolResult")
        .cloned()
        .expect("one persisted tool result row")
}

fn normalized(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if key == "timestamp" {
                        json!(0)
                    } else {
                        normalized(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(normalized).collect()),
        value => value.clone(),
    }
}

fn assistant_message() -> pa_agent::types::AssistantMessage {
    serde_json::from_value(json!({
        "content": [], "api": "test", "provider": "test", "model": "hook-model",
        "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                  "totalTokens": 0, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}},
        "stopReason": "stop", "timestamp": 0
    }))
    .unwrap()
}

fn after_context(result_text: &str) -> AfterToolCallContext {
    AfterToolCallContext {
        assistant_message: assistant_message(),
        tool_call: ToolCall {
            id: "call-probe".into(),
            name: "probe".into(),
            arguments: json!({"input": "x"}),
            thought_signature: None,
        },
        args: json!({"input": "x"}),
        result: pa_agent::types::AgentToolResult {
            content: vec![ToolResultContent::text(result_text)],
            details: json!({"original": true}),
            is_error: false,
            terminate: None,
        },
        is_error: false,
        context: AgentContext::default(),
    }
}

const BLOCK: &str = r"
return {hooks={{name='tool_call', run=function(input)
    assert(input.session=='hook-test')
    assert(input.tool_call_id=='call-probe' and input.tool_name=='probe')
    assert(input.args.input=='x')
    return {allow=false, reason='blocked by alpha'}
end}}}
";

const TIMEOUT: &str = r"
return {hooks={{name='tool_call', run=function()
    while true do end
end}}}
";

const REWRITE: &str = r"
return {hooks={{name='tool_result', run=function(input)
    assert(input.result.content[1].type=='text')
    assert(input.result.details.original==true)
    return {content={{type='text', text='rewritten by alpha'}}, details={rewritten=true}, is_error=false, terminate=false}
end}}}
";

const FAILURE: &str = r"
return {hooks={{name='tool_result', run=function()
    error('alpha failed')
end}}}
";

const ALPHA_APPEND: &str = r"
return {hooks={{name='tool_result', run=function(input)
    return {content={{type='text', text=input.result.content[1].text .. '|alpha'}}}
end}}}
";

const ZETA_APPEND: &str = r"
return {hooks={{name='tool_result', run=function(input)
    return {content={{type='text', text=input.result.content[1].text .. '|zeta'}}}
end}}}
";

const COMPACTION_OBSERVER: &str = r"
local seen = {}
return {
    hooks={{name='compaction_settled', run=function(input)
        seen[#seen+1] = input
        return {observed=#seen}
    end}},
    exports={{name='seen', run=function()
        return {count=#seen, last=seen[#seen] or null}
    end}}
}
";

async fn export(host: &PluginHost, name: &str) -> Value {
    let target = host
        .snapshot()
        .contributions
        .iter()
        .find(|target| target.kind == ContributionKind::Export && target.name == name)
        .cloned()
        .unwrap();
    let mut invocation = host
        .invoke(&target, Value::Null, &CancellationToken::new())
        .unwrap();
    match invocation.next().await {
        Some(InvocationEvent::Finished(Ok(
            InvocationOutcome::Unary(value) | InvocationOutcome::Streamed(value),
        ))) => value,
        other => panic!("unexpected export outcome: {other:?}"),
    }
}

#[tokio::test]
async fn tool_call_block_denies_with_reason_and_skips_execution() {
    let hooks = ToolStageHooks::new(host(&[("alpha", BLOCK)]), "hook-test".into());
    let run = run_probe(Some(hooks), "original result").await;
    assert!(!run.executed);
    let row = tool_result_row(&message_rows(&run.entries));
    assert_eq!(
        row["content"],
        json!([{"type": "text", "text": "blocked by alpha"}])
    );
    assert_eq!(row["isError"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_call_timeout_blocks_and_produces_an_error_result() {
    let hooks = ToolStageHooks::new(host(&[("alpha", TIMEOUT)]), "hook-test".into());
    let run = run_probe(Some(hooks), "original result").await;
    assert!(!run.executed);
    let row = tool_result_row(&message_rows(&run.entries));
    assert_eq!(row["isError"], true);
    let text = row["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("timed out"),
        "unexpected error result: {text}"
    );
}

#[tokio::test]
async fn tool_result_rewrite_persists_on_the_existing_tool_result_row() {
    let hooks = ToolStageHooks::new(host(&[("alpha", REWRITE)]), "hook-test".into());
    let run = run_probe(Some(hooks), "original result").await;
    assert!(run.executed);
    let rows = message_rows(&run.entries);
    let results: Vec<_> = rows
        .iter()
        .filter(|row| row["role"] == "toolResult")
        .collect();
    assert_eq!(
        results.len(),
        1,
        "the rewrite must not add a parallel record"
    );
    assert_eq!(
        results[0]["content"],
        json!([{"type": "text", "text": "rewritten by alpha"}])
    );
    assert_eq!(results[0]["details"], json!({"rewritten": true}));
    assert_eq!(results[0]["isError"], false);
}

#[tokio::test]
async fn tool_result_failure_keeps_the_original_result() {
    let hooks = ToolStageHooks::new(host(&[("alpha", FAILURE)]), "hook-test".into());
    let run = run_probe(Some(hooks), "original result").await;
    assert!(run.executed);
    let row = tool_result_row(&message_rows(&run.entries));
    assert_eq!(
        row["content"],
        json!([{"type": "text", "text": "original result"}])
    );
    assert_eq!(row["details"], json!({"original": true}));
    assert_eq!(row["isError"], false);
}

#[tokio::test]
async fn no_plugins_keeps_byte_identical_messages() {
    let plain = run_probe(None, "original result").await;
    let empty = run_probe(
        Some(ToolStageHooks::new(host(&[]), "hook-test".into())),
        "original result",
    )
    .await;
    assert!(plain.executed && empty.executed);
    assert_eq!(
        normalized(&serde_json::to_value(&plain.messages).unwrap()),
        normalized(&serde_json::to_value(&empty.messages).unwrap()),
    );
    assert_eq!(
        normalized(&serde_json::to_value(message_rows(&plain.entries)).unwrap()),
        normalized(&serde_json::to_value(message_rows(&empty.entries)).unwrap()),
    );
}

#[tokio::test]
async fn tool_result_hooks_run_in_alphabetical_plugin_order() {
    let hooks = ToolStageHooks::new(
        host(&[("zeta", ZETA_APPEND), ("alpha", ALPHA_APPEND)]),
        "hook-test".into(),
    );
    let result = hooks.after_tool_call()(after_context("base"), AbortSignal::default())
        .await
        .unwrap()
        .expect("a successful hook returns an override");
    assert_eq!(
        result.content,
        Some(vec![ToolResultContent::text("base|alpha|zeta")])
    );
    assert_eq!(result.details, Some(json!({"original": true})));
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.terminate, None);
}

#[tokio::test]
async fn compaction_observer_fires_once_for_ran_and_skipped() {
    let host = host(&[("alpha", COMPACTION_OBSERVER)]);
    let skipped = CompactOutcome::Skipped("session too short");
    observe_compaction(&host, "hook-test", &skipped, Some("keep the tests")).await;
    let seen = export(&host, "seen").await;
    assert_eq!(seen["count"], 1);
    assert_eq!(seen["last"]["outcome"], "skipped");
    assert_eq!(seen["last"]["entry"], "session too short");
    assert_eq!(seen["last"]["instructions"], "keep the tests");
    assert_eq!(seen["last"]["session"], "hook-test");

    let ran = CompactOutcome::Ran(Box::new(CompactRun {
        result: crate::session_engine::compaction_exec::CompactionResult {
            summary: "summary".into(),
            first_kept_entry_id: Some("kept".into()),
            tokens_before: 10,
            usage: None,
        },
        entry: CompactionEntry {
            summary: "summary".into(),
            first_kept_entry_id: Some("kept".into()),
            tokens_before: 10,
            ..Default::default()
        },
        duration_ms: 1,
        ipython_state: None,
    }));
    observe_compaction(&host, "hook-test", &ran, None).await;
    let seen = export(&host, "seen").await;
    assert_eq!(seen["count"], 2);
    assert_eq!(seen["last"]["outcome"], "ran");
    assert_eq!(seen["last"]["entry"]["summary"], "summary");
    assert_eq!(seen["last"]["entry"]["firstKeptEntryId"], "kept");
    assert!(seen["last"].get("instructions").is_none());
}
