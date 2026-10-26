//! Cross the real public session engine, scoped auxiliary inference and kernel
//! export handlers. No tree-only or inference-capability test bypass is used.
use crate::{
    kernel::shared::HostRequestPayload,
    session::manager::SessionManager,
    session_engine::{
        engine::{create_session, SessionEngine, SessionEngineConfig},
        PromptOptions,
    },
};
use pa_agent::scripted::ScriptedProvider;
use pa_types::session::{AgentMessage, FileEntry};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

const SUMMARIZER: &str = r"
local calls={}
return {exports={{name='calls',run=function()return {count=#calls,items=calls} end}},providers={{
 name='tree-summarizer',mode='streaming',run=function(input,ctx)
 calls[#calls+1]={id=input.model.id,provider=input.model.provider}
 if config.fail then error('deterministic summary failure') end
 local text='Summary covers both child intervals; use recall for exact originals.'
 local message={role='assistant',content={{type='text',text=text}},api=input.model.api,
 provider=input.model.provider,model=input.model.id,usage={input=0,output=0,cacheRead=0,
 cacheWrite=0,totalTokens=0,cost={input=0,output=0,cacheRead=0,cacheWrite=0,total=0}},stopReason='stop',timestamp=0}
 ctx.emit({type='start',partial=message});ctx.emit({type='done',reason='stop',message=message});return message
end}}}
";
fn user(content: &str) -> AgentMessage {
    serde_json::from_value(json!({"role":"user","content":content,"timestamp":0})).unwrap()
}
fn model() -> pa_types::ai::Model {
    serde_json::from_value(json!({"id":"main","name":"Main","api":"openai-completions","provider":"native-main",
    "baseUrl":"http://127.0.0.1:1","reasoning":false,"input":["text"],
    "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":131_072,"maxTokens":128})).unwrap()
}
struct Fixture {
    _dir: tempfile::TempDir,
    cwd: PathBuf,
    agent_dir: PathBuf,
    engine: Arc<SessionEngine>,
    native: Arc<ScriptedProvider>,
    journal: PathBuf,
    cache: PathBuf,
    original: Vec<FileEntry>,
}
impl Fixture {
    async fn new(count: usize) -> Self {
        Self::with_main(count, model(), None).await
    }
    async fn with_main(
        count: usize,
        ai: pa_types::ai::Model,
        stream: Option<pa_agent::stream::StreamFn>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(cwd.join("summarizer.lua"), SUMMARIZER).unwrap();
        let settings = json!({"harnessRefinements":false,"enableBuiltinSkills":false,"prewarmIpythonKernel":false,
          "contextStrategy":"optchat/tree","auxiliaryModel":"tree-summarizer/default",
          "plugins":{"optchat":{"enabled":true,"config":{"budget_bytes":4096,"summary_bytes":128,"model":"tree-summarizer/chosen"}},
          "summary":{"enabled":true,"path":"summarizer.lua"}}});
        std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
        std::fs::write(agent_dir.join("models.json"),json!({"providers":{"tree-summarizer":{
          "baseUrl":"http://127.0.0.1:1","apiKey":"isolated-fake-only","api":"openai-completions",
          "models":[{"id":"chosen","contextWindow":131_072},{"id":"default","contextWindow":131_072}]
        }}}).to_string()).unwrap();
        let mut session = SessionManager::persisted(&cwd, &dir.path().join("sessions"));
        for index in 0..count {
            session
                .append_message(user(&format!(
                    "canonical-{index}: {}",
                    "detail ".repeat(100)
                )))
                .unwrap();
        }
        session
            .append_message(AgentMessage::Assistant(
                pa_ai::faux::faux_assistant_text_message(
                    "canonical final reply",
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ),
            ))
            .unwrap();
        session.append_message(serde_json::from_value(json!({"role":"toolResult","toolCallId":"call","toolName":"bash","content":[{"type":"text","text":"exact tool result retained"}],"isError":false,"timestamp":0})).unwrap()).unwrap();
        session
            .append_custom_entry(
                "test-event",
                Some(json!({"verbatim":"custom original retained"})),
            )
            .unwrap();
        session.flush_now().unwrap();
        let journal = session.get_session_file().unwrap().to_owned();
        let before = std::fs::read(&journal).unwrap();
        let original = session.retained_entries().to_vec();
        let cache = session
            .get_session_artifact_dir()
            .unwrap()
            .join("plugins/optchat/tree-v1.json");
        let native = Arc::new(ScriptedProvider::new(
            super::super::provider_adapter::json_round_trip(&ai).unwrap(),
        ));
        let engine = create_session(SessionEngineConfig {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            model: Some(super::super::provider_adapter::json_round_trip(&ai).unwrap()),
            model_info: Some(ai),
            session_manager: Some(session),
            stream_fn: Some(stream.unwrap_or_else(|| native.stream_fn())),
            daemonless: true,
            prewarm_ipython_kernel: Some(false),
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(
            std::fs::read(&journal).unwrap().starts_with(&before),
            "canonical prefix rewritten"
        );
        Self {
            _dir: dir,
            cwd,
            agent_dir,
            engine: Arc::new(engine),
            native,
            journal,
            cache,
            original,
        }
    }
    async fn export(&self, plugin: &str, name: &str, input: Value) -> anyhow::Result<Value> {
        export(&self.engine, plugin, name, input).await
    }
    async fn calls(&self) -> Vec<Value> {
        let result = self.export("summary", "calls", json!({})).await.unwrap();
        if result["count"] == 0 {
            Vec::new()
        } else {
            result["items"].as_array().unwrap().clone()
        }
    }
}
async fn export(
    engine: &SessionEngine,
    plugin: &str,
    name: &str,
    input: Value,
) -> anyhow::Result<Value> {
    let handlers = engine.plugin_host_handlers();
    let session = engine.session.session_id().await;
    let catalog = handlers.get("plugin.catalog").unwrap()(HostRequestPayload {
        data: json!({"type":"plugin.catalog","session":session}),
    })
    .await?;
    let target = catalog["exports"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["plugin"] == plugin && entry["name"] == name)
        .ok_or_else(|| anyhow::anyhow!("missing export"))?;
    handlers.get("plugin.invoke").unwrap()(HostRequestPayload{data:json!({"type":"plugin.invoke","session":session,"plugin":plugin,"generation":target["generation"],"export":name,"input":input})}).await
}

#[tokio::test]
async fn public_engine_tree_covers_history_scoped_auxiliary_recall_append_and_main_call() {
    let f = Fixture::new(96).await;
    let calls = f.calls().await;
    assert!(!calls.is_empty());
    assert!(calls
        .iter()
        .all(|call| call == &json!({"id":"chosen","provider":"tree-summarizer"})));
    assert!(f.native.calls().is_empty(), "summary used main provider");
    let status = f.export("optchat", "status", json!({})).await.unwrap();
    let view = &status["last_prepared"];
    assert!(view["bytes"].as_u64().unwrap() <= 4096);
    let mut exact = Vec::new();
    let mut covered = 0;
    for span in view["spans"].as_array().unwrap() {
        assert_eq!(span["start"], covered);
        covered = span["end"].as_u64().unwrap();
        let recall = f
            .export("optchat", "zoom", json!({"id":span["id"],"limit":256}))
            .await
            .unwrap();
        exact.extend(recall["entries"].as_array().unwrap().iter().cloned());
    }
    assert_eq!(covered, view["entries"].as_u64().unwrap());
    let original: Vec<_> = f
        .original
        .iter()
        .filter(|entry| !matches!(entry, pa_types::session::FileEntry::Header { .. }))
        .map(|entry| serde_json::to_value(entry).unwrap())
        .collect();
    assert!(
        exact.len() >= original.len(),
        "recall omitted a canonical source interval"
    );
    assert_eq!(&exact[..original.len()], original);
    for query in ["exact tool result retained", "custom original retained"] {
        let hits = f
            .export("optchat", "search", json!({"query":query}))
            .await
            .unwrap();
        let recalled = f
            .export(
                "optchat",
                "zoom",
                json!({"id":hits["matches"][0]["id"],"limit":1}),
            )
            .await
            .unwrap();
        assert!(recalled.to_string().contains(query));
    }
    let journal = std::fs::read(&f.journal).unwrap();
    f.engine
        .session
        .rebuild_branch_context(f.engine.session.entries().await)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(&f.journal).unwrap(),
        journal,
        "tree rebuild wrote canonical journal"
    );
    let before = f.calls().await.len();
    assert!(
        before - calls.len() < calls.len(),
        "metadata tail rebuilt the full tree"
    );
    f.engine
        .session
        .rebuild_branch_context(f.engine.session.entries().await)
        .await
        .unwrap();
    assert_eq!(
        f.calls().await.len(),
        before,
        "unchanged source cache was not reused"
    );
    f.native.push_text_turn("main final answer");
    f.engine
        .prompt("fresh latest request", PromptOptions::default())
        .await
        .unwrap();
    let main = f.native.calls();
    assert_eq!(main.len(), 1);
    let sent = serde_json::to_string(&main[0].messages).unwrap();
    assert!(sent.contains("fresh latest request"));
    assert!(sent.contains("summary-tree view"));
    assert!(
        f.calls().await.len() - before < before,
        "append rebuilt whole history"
    );
    assert!(f
        .engine
        .session
        .entries()
        .await
        .iter()
        .all(|entry| !matches!(entry, FileEntry::Compaction { .. })));
    assert!(matches!(
        f.engine
            .session
            .compact(None, &model(), None, None)
            .await
            .unwrap(),
        crate::session_engine::compact_session::CompactOutcome::Skipped(_)
    ));
    assert!(f.cache.exists());
    assert_eq!(f.engine.session.agent().state().await.model.id, "main");
}

#[tokio::test]
async fn public_engine_failed_summary_keeps_previous_view_and_journal() {
    let f = Fixture::new(32).await;
    let previous = f.export("optchat", "status", json!({})).await.unwrap();
    let previous_model_context = f.engine.session.agent().state().await.messages;
    let cached = std::fs::read(&f.cache).unwrap();
    // Append enough original source to force new summaries; no view is published
    // while these canonical messages and their results are still being prepared.
    {
        let persistence = f.engine.session.shared_persistence();
        let mut s = persistence.lock().await;
        for n in 0..32 {
            s.append_message(user(&format!("new-{n} {}", "new detail ".repeat(100))))
                .unwrap();
        }
        s.flush_now().unwrap();
    }
    let before = std::fs::read(&f.journal).unwrap();
    std::fs::write(
        f.cwd.join("summarizer.lua"),
        SUMMARIZER.replace("if config.fail then", "if true then"),
    )
    .unwrap();
    f.engine.reload_plugins().await.unwrap();
    assert!(f
        .engine
        .session
        .rebuild_branch_context(f.engine.session.entries().await)
        .await
        .is_err());
    assert_eq!(
        f.export("optchat", "status", json!({})).await.unwrap(),
        previous
    );
    assert_eq!(
        f.engine.session.agent().state().await.messages,
        previous_model_context
    );
    assert_eq!(std::fs::read(&f.cache).unwrap(), cached);
    assert_eq!(std::fs::read(&f.journal).unwrap(), before);
}

#[tokio::test]
async fn public_engine_resume_window_uses_full_source_and_fresh_session_never_imports_tree() {
    let f = Fixture::new(64).await;
    {
        let persistence = f.engine.session.shared_persistence();
        let mut manager = persistence.lock().await;
        manager.append_model_change("native-main", "main").unwrap();
        let first_kept = f.original[f.original.len() - 3].id().unwrap().to_owned();
        manager
            .append_compaction(pa_types::session::CompactionEntry {
                summary: "legacy rolling representation only".into(),
                first_kept_entry_id: Some(first_kept),
                ..Default::default()
            })
            .unwrap();
        manager.flush_now().unwrap();
    }
    let before = std::fs::read(&f.journal).unwrap();
    let mut manager = SessionManager::open(&f.cwd, f.journal.parent().unwrap(), &f.journal);
    let window = crate::session::window::WindowedSessionStore::open(&f.journal)
        .unwrap()
        .unwrap();
    assert!(!window.full_history());
    manager.adopt_window(window);
    assert!(manager.retained_entries().len() < f.original.len());
    let resumed = create_session(SessionEngineConfig {
        cwd: f.cwd.clone(),
        agent_dir: f.agent_dir.clone(),
        model: Some(super::super::provider_adapter::json_round_trip(&model()).unwrap()),
        model_info: Some(model()),
        session_manager: Some(manager),
        stream_fn: Some(f.native.stream_fn()),
        daemonless: true,
        prewarm_ipython_kernel: Some(false),
        ..Default::default()
    })
    .await
    .unwrap();
    let hits = export(
        &resumed,
        "optchat",
        "search",
        json!({"query":"canonical-0:"}),
    )
    .await
    .unwrap();
    assert_eq!(hits["matches"].as_array().unwrap().len(), 1);
    let exact = export(
        &resumed,
        "optchat",
        "zoom",
        json!({"id":hits["matches"][0]["id"],"limit":1}),
    )
    .await
    .unwrap();
    assert_eq!(
        exact["entries"][0],
        serde_json::to_value(&f.original[1]).unwrap()
    );
    assert_eq!(
        std::fs::read(&f.journal).unwrap(),
        before,
        "resume tree overwrote journal"
    );
    let fresh = SessionManager::persisted(&f.cwd, f.journal.parent().unwrap());
    let fresh_id = fresh.get_session_id().to_owned();
    assert_ne!(fresh_id, resumed.session.session_id().await);
    let fresh = create_session(SessionEngineConfig {
        cwd: f.cwd.clone(),
        agent_dir: f.agent_dir.clone(),
        model: Some(super::super::provider_adapter::json_round_trip(&model()).unwrap()),
        model_info: Some(model()),
        session_manager: Some(fresh),
        stream_fn: Some(f.native.stream_fn()),
        daemonless: true,
        prewarm_ipython_kernel: Some(false),
        ..Default::default()
    })
    .await
    .unwrap();
    let hits = export(&fresh, "optchat", "search", json!({"query":"canonical-0:"}))
        .await
        .unwrap();
    assert!(hits["matches"].as_array().unwrap().is_empty());
    assert_eq!(
        export(&fresh, "summary", "calls", json!({})).await.unwrap()["count"],
        0
    );
    assert!(
        f.cache.exists(),
        "fresh session removed original plugin data"
    );
}

#[tokio::test]
async fn public_model_switch_rechecks_bound_without_source_loss() {
    let f = Fixture::new(64).await;
    f.native.push_text_turn("large model answer");
    f.engine
        .prompt("measure full large-model request", PromptOptions::default())
        .await
        .unwrap();
    let sent = f.native.calls().remove(0);
    let instructions = sent.system_prompt.as_ref().map_or(0, String::len);
    let tools = serde_json::to_vec(&sent.tools).unwrap().len();
    let fixed = instructions + tools + 128 + 1024;
    let mut smaller =
        super::super::provider_adapter::json_round_trip::<_, pa_agent::types::Model>(&model())
            .unwrap();
    smaller.context_window = u64::try_from(fixed + 3072).unwrap();
    smaller.id = "smaller".into();
    f.engine.session.agent().set_model(smaller).await;
    f.native.push_text_turn("small model answer");
    f.engine
        .prompt("use the smaller context", PromptOptions::default())
        .await
        .unwrap();
    let status = f.export("optchat", "status", json!({})).await.unwrap();
    assert!(status["last_prepared"]["bytes"].as_u64().unwrap() <= 3072);
    let hits = f
        .export("optchat", "search", json!({"query":"canonical-0:"}))
        .await
        .unwrap();
    assert_eq!(hits["matches"].as_array().unwrap().len(), 1);
    assert_eq!(f.native.calls().len(), 2);
    assert!(
        serde_json::to_string(&f.native.calls()[1].messages)
            .unwrap()
            .contains("use the smaller context"),
        "small-model layout summarized away the newest request"
    );
}

#[tokio::test]
async fn public_oversized_mandatory_instructions_fail_before_main_inference() {
    let f = Fixture::new(32).await;
    let previous = f.engine.session.agent().state().await.messages;
    f.engine
        .session
        .agent()
        .set_system_prompt("mandatory ".repeat(20_000))
        .await;
    let _failure = f
        .engine
        .prompt(
            "do not send an oversized model context",
            PromptOptions::default(),
        )
        .await;
    assert!(
        f.native.calls().is_empty(),
        "whole context budget ignored mandatory instructions"
    );
    let original = f
        .export("optchat", "search", json!({"query":"canonical-0:"}))
        .await
        .unwrap();
    assert_eq!(original["matches"].as_array().unwrap().len(), 1);
    assert!(serde_json::to_string(&previous)
        .unwrap()
        .contains("summary-tree view"));
}

struct Gate {
    entered: tokio::sync::Notify,
    ended: Arc<tokio::sync::Notify>,
    release: tokio::sync::Semaphore,
}
struct Dropped(Arc<tokio::sync::Notify>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
impl crate::plugins::HostCapabilities for Arc<Gate> {
    fn call(
        &self,
        _request: crate::plugins::CapabilityRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> crate::plugins::CapabilityFuture {
        let gate = Arc::clone(self);
        Box::pin(async move {
            let _dropped = Dropped(Arc::clone(&gate.ended));
            gate.entered.notify_one();
            tokio::select! {biased;()=cancel.cancelled()=>Err("cancelled".into()),permit=gate.release.acquire()=>{permit.map_err(|error|error.to_string())?.forget();Ok(Value::Null)}}
        })
    }
}
#[tokio::test]
async fn public_engine_cancelled_or_retired_auxiliary_work_cannot_publish() {
    for retire in [false, true] {
        let f = Fixture::new(32).await;
        let previous = f.export("optchat", "status", json!({})).await.unwrap();
        let previous_model_context = f.engine.session.agent().state().await.messages;
        let cached = std::fs::read(&f.cache).unwrap();
        {
            let persistence = f.engine.session.shared_persistence();
            let mut s = persistence.lock().await;
            for n in 0..32 {
                s.append_message(user(&format!("held-tail-{n} {}", "detail ".repeat(100))))
                    .unwrap();
            }
            s.flush_now().unwrap();
        }
        let journal = std::fs::read(&f.journal).unwrap();
        let gate = Arc::new(Gate {
            entered: tokio::sync::Notify::default(),
            ended: Arc::default(),
            release: tokio::sync::Semaphore::new(0),
        });
        f.engine
            .session
            .plugins
            .as_ref()
            .unwrap()
            .set_capability_delegate("test.", Arc::new(Arc::clone(&gate)));
        std::fs::write(
            f.cwd.join("summarizer.lua"),
            SUMMARIZER.replace("local text=", "ctx.call('test.hold',{}); local text="),
        )
        .unwrap();
        f.engine.reload_plugins().await.unwrap();
        let engine = Arc::clone(&f.engine);
        let task = tokio::spawn(async move {
            engine
                .session
                .rebuild_branch_context(engine.session.entries().await)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        if retire {
            let replacement = f.cwd.join("optchat-replacement.lua");
            std::fs::write(
                &replacement,
                format!(
                    "{}\n-- new generation",
                    include_str!("../../plugins/builtins/optchat.lua")
                ),
            )
            .unwrap();
            std::fs::write(f.cwd.join(".prime/agent/settings.json"),json!({"plugins":{"optchat":{"enabled":true,"path":"optchat-replacement.lua","config":{"budget_bytes":4096,"summary_bytes":128,"model":"tree-summarizer/chosen"}}}}).to_string()).unwrap();
            f.engine.reload_plugins().await.unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
        } else {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.ended.notified())
            .await
            .unwrap();
        gate.release.add_permits(1);
        assert_eq!(
            f.export("optchat", "status", json!({})).await.unwrap(),
            previous
        );
        assert_eq!(
            f.engine.session.agent().state().await.messages,
            previous_model_context
        );
        assert_eq!(std::fs::read(&f.cache).unwrap(), cached);
        assert_eq!(std::fs::read(&f.journal).unwrap(), journal);
    }
}

#[tokio::test]
async fn public_branch_navigation_builds_candidate_tree_not_previous_sibling() {
    let f = Fixture::new(32).await;
    let foreign = f
        .export("optchat", "search", json!({"query":"canonical-31:"}))
        .await
        .unwrap()["matches"][0]["id"]
        .clone();
    let before = f.engine.session.entries().await;
    let prefix: Vec<_> = before
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                FileEntry::Message {
                    message: AgentMessage::User(_),
                    ..
                }
            )
        })
        .take(16)
        .cloned()
        .collect();
    let candidate = {
        let persistence = f.engine.session.shared_persistence();
        let mut manager = persistence.lock().await;
        manager.adopt_entries(prefix);
        manager
            .append_message(user("sibling-only request"))
            .unwrap();
        manager.flush_now().unwrap();
        let candidate = manager
            .retained_entries()
            .iter()
            .filter(|entry| !matches!(entry, FileEntry::Header { .. }))
            .cloned()
            .collect();
        manager.adopt_entries(before);
        candidate
    };
    let journal = std::fs::read(&f.journal).unwrap();
    f.engine
        .session
        .rebuild_branch_context(candidate)
        .await
        .unwrap();
    let actual = serde_json::to_string(&f.engine.session.agent().state().await.messages).unwrap();
    assert!(
        actual.contains("sibling-only request"),
        "candidate branch was not used"
    );
    let hits = f
        .export("optchat", "search", json!({"query":"canonical-31:"}))
        .await
        .unwrap();
    assert!(
        hits["matches"].as_array().unwrap().is_empty(),
        "sibling history mixed into selected branch"
    );
    assert!(f
        .export("optchat", "zoom", json!({"id":foreign}))
        .await
        .is_err());
    assert_eq!(
        std::fs::read(&f.journal).unwrap(),
        journal,
        "branch tree rewrote canonical history"
    );
}

#[tokio::test]
async fn public_explicit_selection_disable_and_resume_never_fall_back() {
    let f = Fixture::new(48).await;
    f.engine
        .session
        .select_context_strategy("default_context/rolling")
        .await
        .unwrap();
    let rolling = serde_json::to_string(&f.engine.session.agent().state().await.messages).unwrap();
    assert!(rolling.contains("canonical-0:"));
    assert!(!rolling.contains("summary-tree view"));
    f.engine
        .session
        .select_context_strategy("optchat/tree")
        .await
        .unwrap();
    let selected = serde_json::to_string(&f.engine.session.agent().state().await.messages).unwrap();
    assert!(selected.contains("summary-tree view"));
    assert!(f
        .engine
        .session
        .entries()
        .await
        .iter()
        .all(|entry| !matches!(entry, FileEntry::Compaction { .. })));
    let cached = std::fs::read(&f.cache).unwrap();
    std::fs::write(
        f.cwd.join(".prime/agent/settings.json"),
        json!({"plugins":{"optchat":{"enabled":false}}}).to_string(),
    )
    .unwrap();
    f.engine.reload_plugins().await.unwrap();
    assert!(f.export("optchat", "status", json!({})).await.is_err());
    assert!(!f
        .engine
        .session
        .session_command_inventory()
        .iter()
        .any(|command| command.name == "optchat"));
    assert!(f
        .engine
        .session
        .plugins
        .as_ref()
        .unwrap()
        .host()
        .snapshot()
        .contributions
        .iter()
        .all(|target| target.plugin != "optchat"));
    assert!(!f
        .engine
        .assembled_system_prompt()
        .contains("plugin.optchat"));
    let _failure = f
        .engine
        .prompt(
            "must not infer with disabled selected strategy",
            PromptOptions::default(),
        )
        .await;
    assert!(
        f.native.calls().is_empty(),
        "disabled strategy silently fell back to main inference"
    );
    assert_eq!(
        std::fs::read(&f.cache).unwrap(),
        cached,
        "disable removed saved data"
    );
    f.engine
        .session
        .select_context_strategy("default_context/rolling")
        .await
        .unwrap();
    let manager = SessionManager::open(&f.cwd, f.journal.parent().unwrap(), &f.journal);
    let resumed = create_session(SessionEngineConfig {
        cwd: f.cwd.clone(),
        agent_dir: f.agent_dir.clone(),
        model: Some(super::super::provider_adapter::json_round_trip(&model()).unwrap()),
        model_info: Some(model()),
        session_manager: Some(manager),
        stream_fn: Some(f.native.stream_fn()),
        daemonless: true,
        prewarm_ipython_kernel: Some(false),
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(export(&resumed, "optchat", "status", json!({}))
        .await
        .is_err());
    let view = serde_json::to_string(&resumed.session.agent().state().await.messages).unwrap();
    assert!(view.contains("canonical-0:"));
    assert!(!view.contains("summary-tree view"));
    assert_eq!(std::fs::read(&f.cache).unwrap(), cached);
}

#[tokio::test]
async fn public_failed_tree_selection_preserves_rolling_authority_and_history() {
    let f = Fixture::new(32).await;
    f.engine
        .session
        .select_context_strategy("default_context/rolling")
        .await
        .unwrap();
    {
        let persistence = f.engine.session.shared_persistence();
        let mut s = persistence.lock().await;
        for n in 0..32 {
            s.append_message(user(&format!(
                "unprepared-{n} {}",
                "new detail ".repeat(100)
            )))
            .unwrap();
        }
        s.flush_now().unwrap();
    }
    let previous = f.engine.session.agent().state().await.messages;
    let journal = std::fs::read(&f.journal).unwrap();
    std::fs::write(
        f.cwd.join("summarizer.lua"),
        SUMMARIZER.replace("if config.fail then", "if true then"),
    )
    .unwrap();
    f.engine.reload_plugins().await.unwrap();
    assert!(f
        .engine
        .session
        .select_context_strategy("optchat/tree")
        .await
        .is_err());
    assert_eq!(f.engine.session.agent().state().await.messages, previous);
    assert_eq!(
        std::fs::read(&f.journal).unwrap(),
        journal,
        "failed selection changed canonical journal"
    );
    assert!(!serde_json::to_string(&previous)
        .unwrap()
        .contains("summary-tree view"));
}

struct LiveTextTool(std::sync::atomic::AtomicUsize);
impl pa_agent::types::AgentTool for LiveTextTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "Execute a deterministic text-only fixture tool"
    }
    fn parameters(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| json!({"type":"object","properties":{"token":{"type":"string"}},"required":["token"]}))
    }
    fn execute(
        self: Arc<Self>,
        _id: String,
        params: Value,
        _signal: pa_agent::abort::AbortSignal,
        _update: pa_agent::types::AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<pa_agent::types::AgentToolResult>> {
        Box::pin(async move {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(pa_agent::types::AgentToolResult::text(format!(
                "fresh-executed-tool-result:{}",
                params["token"].as_str().unwrap()
            )))
        })
    }
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Value {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    let (header_end, length) = loop {
        let mut chunk = [0; 4096];
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "HTTP fixture closed before request headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .expect("native provider request has Content-Length");
            break (end + 4, length);
        }
    };
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "HTTP fixture closed before request body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

#[tokio::test]
async fn public_text_tree_two_round_native_http_preserves_current_executed_tool_outcome() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for round in 0..2 {
            let (mut socket, _) =
                tokio::time::timeout(std::time::Duration::from_secs(10), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            requests.push(
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    read_http_request(&mut socket),
                )
                .await
                .unwrap(),
            );
            let (delta, reason) = if round == 0 {
                (
                    json!({"role":"assistant","tool_calls":[{"index":0,"id":"live-call","type":"function","function":{"name":"echo","arguments":"{\"token\":\"execution-ticket-42\"}"}}]}),
                    "tool_calls",
                )
            } else {
                (
                    json!({"role":"assistant","content":"used the freshly executed result"}),
                    "stop",
                )
            };
            let first = json!({"id":"live-completion","object":"chat.completion.chunk","created":0,"model":"main","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
            let done = json!({"id":"live-completion","object":"chat.completion.chunk","created":0,"model":"main","choices":[{"index":0,"delta":{},"finish_reason":reason}]});
            let body = format!("data: {first}\n\ndata: {done}\n\ndata: [DONE]\n\n");
            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        requests
    });
    let mut ai = model();
    ai.base_url = format!("http://{address}/v1");
    let native = super::super::provider_adapter::real_stream_fn(
        Some("isolated-loopback-fixture-only".into()),
        ai.clone(),
    );
    let f = Fixture::with_main(32, ai, Some(native)).await;
    let tool = Arc::new(LiveTextTool(std::sync::atomic::AtomicUsize::new(0)));
    f.engine.session.agent().set_tools(vec![tool.clone()]).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        f.engine.prompt(
            "Run the fixture tool, then report its output",
            PromptOptions::default(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let requests = server.await.unwrap();
    assert_eq!(tool.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(requests.len(), 2);
    let result = "fresh-executed-tool-result:execution-ticket-42";
    assert!(!requests[0].to_string().contains(result));
    assert!(
        requests[1].to_string().contains(result),
        "tree dropped the CURRENT executed tool result before round two"
    );
    // The projection is intentionally a standalone text transcript, not a
    // dangling native tool-result message without its original tool-call row.
    for request in &requests {
        assert!(request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "echo"));
        for message in request["messages"].as_array().unwrap() {
            assert!(matches!(
                message["role"].as_str(),
                Some("system" | "developer" | "user")
            ));
            assert!(message.get("tool_call_id").is_none());
            assert!(message.get("tool_calls").is_none());
            match &message["content"] {
                Value::String(_) => {}
                Value::Array(blocks) => assert!(blocks.iter().all(|block| block["type"] == "text")),
                other => panic!("unexpected provider content {other:?}"),
            }
        }
    }
    let source = f.engine.session.entries().await;
    let original=source.iter().find(|entry|matches!(entry,FileEntry::Message{message:AgentMessage::ToolResult(message),..} if message.tool_call_id=="live-call")).unwrap();
    let hits = f
        .export("optchat", "search", json!({"query":result}))
        .await
        .unwrap();
    let recalled = f
        .export(
            "optchat",
            "zoom",
            json!({"id":hits["matches"][0]["id"],"limit":1}),
        )
        .await
        .unwrap();
    assert_eq!(
        recalled["entries"][0],
        serde_json::to_value(original).unwrap()
    );
    assert!(serde_json::to_string(&source)
        .unwrap()
        .contains("used the freshly executed result"));
}

#[tokio::test]
async fn public_tiny_typed_image_fails_before_any_inference_and_requires_explicit_strategy_change()
{
    let f = Fixture::new(32).await;
    let before = f.engine.session.agent().state().await.messages;
    let prepared = f.export("optchat", "status", json!({})).await.unwrap();
    let cache = std::fs::read(&f.cache).unwrap();
    let journal = std::fs::read(&f.journal).unwrap();
    let aux = f.calls().await.len();
    let pinned = |entries: Vec<FileEntry>| {
        entries.into_iter().filter(|entry|matches!(entry,FileEntry::Custom{payload,..} if payload.custom_type==pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE)).collect::<Vec<_>>()
    };
    let selection = pinned(f.engine.session.entries().await);
    let png="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a9ZkAAAAASUVORK5CYII=";
    let mut descriptor = serde_json::to_value(model()).unwrap();
    descriptor["input"] = json!(["text", "image"]);
    f.engine
        .session
        .agent()
        .set_model(serde_json::from_value(descriptor).unwrap())
        .await;
    let image = pa_agent::types::ImageContent {
        data: png.into(),
        mime_type: "image/png".into(),
    };
    let _failure = f
        .engine
        .session
        .prompt_with_images(
            "tiny real typed image",
            vec![image],
            PromptOptions::default(),
        )
        .await;
    assert!(
        f.native.calls().is_empty(),
        "unsupported image reached main inference"
    );
    assert_eq!(
        f.calls().await.len(),
        aux,
        "unsupported image reached auxiliary inference"
    );
    let after = f.engine.session.agent().state().await.messages;
    assert!(
        after.starts_with(&before),
        "unsupported image replaced the previous accepted view"
    );
    let error = serde_json::to_string(&after).unwrap();
    assert!(
        error.contains("text/JSON") && error.contains("default_context/rolling"),
        "missing explicit recovery guidance: {error}"
    );
    assert_eq!(
        pinned(f.engine.session.entries().await),
        selection,
        "image failure silently changed the route pin"
    );
    assert_eq!(
        f.export("optchat", "status", json!({})).await.unwrap(),
        prepared
    );
    assert_eq!(std::fs::read(&f.cache).unwrap(), cache);
    {
        f.engine
            .session
            .shared_persistence()
            .lock()
            .await
            .flush_now()
            .unwrap();
    }
    assert!(std::fs::read(&f.journal).unwrap().starts_with(&journal));
    let entries = f.engine.session.entries().await;
    let original=entries.iter().find(|entry|matches!(entry,FileEntry::Message{message:AgentMessage::User(message),..} if matches!(&message.content,pa_types::ai::UserContent::Blocks(blocks) if blocks.iter().any(|block|matches!(block,pa_types::ai::UserContentBlock::Image(image) if image.data==png))))).unwrap();
    let hits = f
        .export("optchat", "search", json!({"query":png}))
        .await
        .unwrap();
    let recalled = f
        .export(
            "optchat",
            "zoom",
            json!({"id":hits["matches"][0]["id"],"limit":1}),
        )
        .await
        .unwrap();
    assert_eq!(
        recalled["entries"][0],
        serde_json::to_value(original).unwrap(),
        "image source recall changed original bytes"
    );
    // A retry remains closed, and never silently selects a different strategy.
    let _retry = f
        .engine
        .prompt(
            "retry without choosing another strategy",
            PromptOptions::default(),
        )
        .await;
    assert!(f.native.calls().is_empty());
    assert_eq!(f.calls().await.len(), aux);
    assert_eq!(std::fs::read(&f.cache).unwrap(), cache);
    f.engine
        .session
        .select_context_strategy("default_context/rolling")
        .await
        .unwrap();
    f.native.push_text_turn("explicit rolling recovery");
    f.engine
        .prompt(
            "continue after explicit strategy change",
            PromptOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(f.native.calls().len(), 1);
    let typed_image = f.native.calls()[0].messages.iter().any(|message| {
        let pa_agent::types::Message::User(user) = message else {
            return false;
        };
        let pa_agent::types::UserContent::Parts(blocks) = &user.content else {
            return false;
        };
        blocks
            .iter()
            .any(|block| matches!(block,pa_agent::types::UserPart::Image(image) if image.data==png))
    });
    assert!(typed_image, "default rolling lost the typed original image");
    let rolling = f.engine.session.agent().state().await.messages;
    let rolling_pin = pinned(f.engine.session.entries().await);
    f.engine
        .session
        .shared_persistence()
        .lock()
        .await
        .flush_now()
        .unwrap();
    let rolling_journal = std::fs::read(&f.journal).unwrap();
    let rejected = f
        .engine
        .session
        .select_context_strategy("optchat/tree")
        .await
        .unwrap_err();
    assert!(format!("{rejected:#}").contains("default_context/rolling"));
    assert_eq!(f.engine.session.agent().state().await.messages, rolling);
    assert_eq!(pinned(f.engine.session.entries().await), rolling_pin);
    assert_eq!(std::fs::read(&f.journal).unwrap(), rolling_journal);
    assert_eq!(f.native.calls().len(), 1);
    assert_eq!(f.calls().await.len(), aux);
    assert_eq!(std::fs::read(&f.cache).unwrap(), cache);
}

async fn actual_responses_archive_fixture(
) -> (pa_types::session::ContextRepresentation, &'static str) {
    use pa_ai::compaction::{
        CompactOptions, CompactionLimits, CompactionRoute, InputBudget, ResponsesCompactor,
        ResponsesWindow,
    };
    use tokio::io::AsyncWriteExt;
    let marker = "opaque-native-compaction-ciphertext-never-project==";
    let output = format!(
        r#"[ {{"type":"compaction","id":"cmp_native","encrypted_content":"{marker}"}}, {{"type":"future_native_variant","raw":{{"n":1e+02,"escaped":"\/\u0061"}}}} ]"#
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let response = format!(
        r#"{{"object":"response.compaction","id":"cmp_native","created_at":0,"model":"native-archive-model","output":{output},"usage":{{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}}"#
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) =
            tokio::time::timeout(std::time::Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap();
        let request = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_http_request(&mut socket),
        )
        .await
        .unwrap();
        assert_eq!(
            request["input"][0]["content"],
            "portable archive fixture source"
        );
        let response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len());
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let route = CompactionRoute {
        provider: "native-archive-route".into(),
        endpoint: format!("http://{address}/responses/compact"),
        model: "native-archive-model".into(),
    };
    let limits = CompactionLimits::default();
    let adapter = ResponsesCompactor::new(route.clone(), Some(route.clone()), limits).unwrap();
    let window = ResponsesWindow::from_stateless_input(
        &route,
        r#"[{"role":"user","content":"portable archive fixture source"}]"#,
        limits,
    )
    .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let candidate = adapter
        .compact(
            &window,
            CompactOptions {
                bearer_token: "isolated-loopback-native-fixture",
                headers: None,
                instructions: None,
                budget: InputBudget {
                    input_tokens_upper_bound: 1024,
                    context_window_tokens: 100_000,
                },
            },
            &cancel,
        )
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(
        candidate.replay(&adapter, &cancel).unwrap().input_json(),
        output
    );
    let archive = candidate.serialize().unwrap();
    let restored =
        pa_ai::compaction::CompactionCandidate::restore(&archive, &adapter, &cancel).unwrap();
    assert_eq!(
        restored.replay(&adapter, &cancel).unwrap().input_json(),
        output,
        "fixture native bytes did not survive real compaction/archive recovery"
    );
    let representation=serde_json::from_value(json!({"kind":"responses","provider":route.provider,"endpoint":route.endpoint,"model":route.model,"archive":archive})).unwrap();
    (representation, marker)
}

fn actual_snapcompact_archive_fixture() -> (pa_types::session::ContextRepresentation, &'static str)
{
    let marker = "native-bitmap-archive-source-never-project";
    let limits = crate::snapcompact::ReplayLimits {
        vision: true,
        profile: Some(crate::snapcompact::RenderProfile::Explicit {
            frame_size: 128,
            frame_tokens: 100,
        }),
        max_images: 80,
        max_image_bytes: 1_000_000,
        max_payload_bytes: 4_000_000,
        available_tokens: 100_000,
        text_fallback_chars: None,
    };
    let archive = crate::snapcompact::prepare(
        None,
        &[user(&format!("{marker}\n").repeat(16))],
        &[],
        &crate::snapcompact::Config::default(),
        &limits,
        &|| false,
    )
    .unwrap();
    assert!(
        archive.frame_count() > 0,
        "fixture did not construct an actual bitmap archive"
    );
    let json = archive.to_json().unwrap();
    assert_eq!(
        crate::snapcompact::Archive::from_json(&json).unwrap(),
        archive
    );
    (
        serde_json::from_value(json!({"kind":"snapcompact","archive":json})).unwrap(),
        marker,
    )
}

#[tokio::test]
async fn public_native_archives_and_typed_signatures_fail_before_projection_inference() {
    enum Source {
        Archive(pa_types::session::ContextRepresentation),
        Message(Box<AgentMessage>),
    }
    let mut cases = Vec::new();
    let (archive, marker) = actual_responses_archive_fixture().await;
    cases.push((Source::Archive(archive), marker));
    let (archive, marker) = actual_snapcompact_archive_fixture();
    cases.push((Source::Archive(archive), marker));
    cases.extend(
        super::tests::signed_source_messages()
            .into_iter()
            .map(|(message, marker)| (Source::Message(Box::new(message)), marker)),
    );
    for (source, marker) in cases {
        let f = Fixture::new(32).await;
        let view = f.engine.session.agent().state().await.messages;
        let prepared = f.export("optchat", "status", json!({})).await.unwrap();
        let cache = std::fs::read(&f.cache).unwrap();
        let aux = f.calls().await.len();
        let selections = |entries: Vec<FileEntry>| {
            entries.into_iter().filter(|entry|matches!(entry,FileEntry::Custom{payload,..} if payload.custom_type==pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE)).collect::<Vec<_>>()
        };
        let pins = selections(f.engine.session.entries().await);
        let source_id = {
            let persistence = f.engine.session.shared_persistence();
            let mut manager = persistence.lock().await;
            let id = match source {
                Source::Message(message) => manager.append_message(*message).unwrap(),
                Source::Archive(representation) => {
                    let parent = manager.get_leaf_id().unwrap().to_owned();
                    let first = f.original.iter().find_map(FileEntry::id).unwrap();
                    let entry=serde_json::from_value(json!({"summary":"native archive is not portable text","firstKeptEntryId":first,"tokensBefore":100,"contextArchive":{"version":1,"strategy":"default_context/rolling","compactor":"default_context/responses","creatorGeneration":"native-fixture","preparedLeaf":parent,"coveredEntryIds":[parent],"representation":representation}})).unwrap();
                    manager.append_compaction(entry).unwrap()
                }
            };
            manager.flush_now().unwrap();
            id
        };
        let journal = std::fs::read(&f.journal).unwrap();
        let result = f
            .engine
            .prompt(
                "Do not send unsupported native source to any model",
                PromptOptions::default(),
            )
            .await;
        let returned_error = result
            .err()
            .map_or_else(String::new, |error| format!("{error:#}"));
        let after = f.engine.session.agent().state().await.messages;
        assert!(
            f.native.calls().is_empty(),
            "native opaque source reached main inference"
        );
        assert_eq!(
            f.calls().await.len(),
            aux,
            "native opaque source reached auxiliary inference"
        );
        let reported = format!(
            "{returned_error} {}",
            serde_json::to_string(&after).unwrap()
        );
        assert!(
            reported.contains("provider-native archives")
                && reported.contains("default_context/rolling"),
            "missing typed native boundary error: {reported}"
        );
        assert!(
            !reported.contains(marker),
            "native opaque value leaked through the error/view"
        );
        assert!(
            after.starts_with(&view),
            "unsupported native source replaced accepted view"
        );
        assert_eq!(
            selections(f.engine.session.entries().await),
            pins,
            "native rejection changed route pin"
        );
        assert_eq!(
            f.export("optchat", "status", json!({})).await.unwrap(),
            prepared
        );
        assert_eq!(std::fs::read(&f.cache).unwrap(), cache);
        f.engine
            .session
            .shared_persistence()
            .lock()
            .await
            .flush_now()
            .unwrap();
        assert!(std::fs::read(&f.journal).unwrap().starts_with(&journal));
        let original = f
            .engine
            .session
            .entries()
            .await
            .into_iter()
            .find(|entry| entry.id() == Some(source_id.as_str()))
            .unwrap();
        let hits = f
            .export("optchat", "search", json!({"query":marker}))
            .await
            .unwrap();
        let recalled = f
            .export(
                "optchat",
                "zoom",
                json!({"id":hits["matches"][0]["id"],"limit":1}),
            )
            .await
            .unwrap();
        assert_eq!(
            recalled["entries"][0],
            serde_json::to_value(original).unwrap(),
            "native source/archive recall changed bytes"
        );
    }
}
