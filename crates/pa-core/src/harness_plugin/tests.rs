use crate::kernel::shared::HostRequestPayload;
use crate::session_engine::engine::{create_session, SessionEngineConfig};
use pa_agent::scripted::ScriptedProvider;
use serde_json::{json, Value};
use std::sync::Arc;

struct Fixture {
    _dir: tempfile::TempDir,
    agent: std::path::PathBuf,
    engine: crate::session_engine::engine::SessionEngine,
    provider: Arc<ScriptedProvider>,
}
impl Fixture {
    async fn new(settings: Value, legacy: &[u8]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(agent.join("harness")).unwrap();
        std::fs::write(agent.join("harness/harness_state.json"), legacy).unwrap();
        std::fs::write(agent.join("settings.json"), settings.to_string()).unwrap();
        if let Some(source) = settings.get("testProvider").and_then(Value::as_str) {
            std::fs::write(cwd.join("provider.lua"), source).unwrap();
        }
        std::fs::write(agent.join("models.json"), json!({"providers":{"aux":{"apiKey":"fixture-only","baseUrl":"http://127.0.0.1:9","api":"openai-completions","models":[{"id":"default","name":"Default"},{"id":"selected","name":"Selected"}]}}}).to_string()).unwrap();
        let model:pa_agent::types::Model=serde_json::from_value(json!({"id":"test","name":"Test","api":"openai-completions","provider":"test","baseUrl":"http://localhost:9","reasoning":false,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":10000,"maxTokens":100})).unwrap();
        let provider = Arc::new(ScriptedProvider::new(model.clone()));
        let engine = create_session(SessionEngineConfig {
            cwd: cwd.clone(),
            agent_dir: agent.clone(),
            model: Some(model),
            stream_fn: Some(provider.stream_fn()),
            prewarm_ipython_kernel: Some(false),
            daemonless: true,
            ..SessionEngineConfig::default()
        })
        .await
        .unwrap();
        Self {
            _dir: dir,
            agent,
            engine,
            provider,
        }
    }
    async fn call(&self, name: &str, input: Value) -> anyhow::Result<Value> {
        self.call_plugin("harness", name, input).await
    }
    async fn call_plugin(&self, plugin: &str, name: &str, input: Value) -> anyhow::Result<Value> {
        let handlers = self.engine.plugin_host_handlers();
        let session = self.engine.session.session_id().await;
        let request = HostRequestPayload {
            data: json!({"type":"plugin.catalog","session":session}),
        };
        let catalog = handlers.get("plugin.catalog").unwrap()(request).await?;
        let descriptor = catalog["exports"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["plugin"] == plugin && e["name"] == name)
            .unwrap();
        handlers.get("plugin.invoke").unwrap()(HostRequestPayload{data:json!({"type":"plugin.invoke","session":session,"plugin":plugin,"generation":descriptor["generation"],"export":name,"input":input})}).await
    }
}
const LEGACY:&[u8]=br#"{"schema":1,"entries":{"memory":{"same":{"id":"same","kind":"memory","content":"legacy secret","unknown":null}},"factory":{"raw":{"opaque":null}},"future":{"nil":null}},"refinements":[{"id":"old","unknown":true}]}"#;
fn settings(enabled: bool) -> Value {
    json!({"enableBuiltinSkills":false,"bundledSkills":{"websearch":false},"harnessRefinements":enabled,"autoRefine":{"enabled":false}})
}

#[tokio::test]
async fn configured_harness_crud_scopes_null_and_disable_cross_public_engine() {
    let f = Fixture::new(settings(true), LEGACY).await;
    assert_eq!(f.engine.plugin_diagnostic(), None);
    assert!(f
        .engine
        .session
        .session_command_inventory()
        .iter()
        .any(|c| c.name == "refine" && c.plugin.is_some()));
    assert_eq!(f.call("list", json!({})).await.unwrap(), json!([]));
    f.call("create",json!({"kind":"memory","id":"same","value":{"id":"same","content":"session private","unknown":null}})).await.unwrap();
    f.call("create", json!({"kind":"future","id":"nil","value":null}))
        .await
        .unwrap();
    let row = f
        .call("get", json!({"kind":"future","id":"nil"}))
        .await
        .unwrap();
    assert_eq!(row["value"], Value::Null);
    assert_ne!(row, Value::Null);
    let global = f
        .call(
            "get",
            json!({"scope":"legacy_global","kind":"memory","id":"same"}),
        )
        .await
        .unwrap();
    assert_eq!(global["value"]["content"], "legacy secret");
    f.provider.push_text_turn("done");
    f.engine
        .prompt("hello", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    let calls = f.provider.calls();
    assert!(!calls
        .last()
        .unwrap()
        .system_prompt
        .as_ref()
        .unwrap()
        .contains("legacy secret"));
    f.call("enroll", json!({"scopes":["legacy_global"]}))
        .await
        .unwrap();
    f.provider.push_text_turn("done");
    f.engine
        .prompt(
            "hello again",
            crate::session_engine::PromptOptions::default(),
        )
        .await
        .unwrap();
    let calls = f.provider.calls();
    assert!(calls
        .last()
        .unwrap()
        .system_prompt
        .as_ref()
        .unwrap()
        .contains("legacy secret"));
    assert_eq!(
        std::fs::read(f.agent.join("harness/harness_state.json")).unwrap(),
        LEGACY
    );
    let saved = std::fs::read(f.agent.join("plugins/harness/store.json")).unwrap();
    std::fs::write(f.agent.join("settings.json"), settings(false).to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    assert!(!f
        .engine
        .session
        .session_command_inventory()
        .iter()
        .any(|c| c.name == "refine"));
    assert!(!f
        .engine
        .assembled_system_prompt()
        .contains("plugin.harness"));
    assert_eq!(
        std::fs::read(f.agent.join("plugins/harness/store.json")).unwrap(),
        saved
    );
    std::fs::write(f.agent.join("settings.json"), settings(true).to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    f.provider.push_text_turn("restored");
    f.engine
        .prompt(
            "restored membership",
            crate::session_engine::PromptOptions::default(),
        )
        .await
        .unwrap();
    assert!(f
        .provider
        .calls()
        .last()
        .unwrap()
        .system_prompt
        .as_ref()
        .unwrap()
        .contains("legacy secret"));
    assert_eq!(
        f.call("get", json!({"kind":"future","id":"nil"}))
            .await
            .unwrap()["value"],
        Value::Null
    );
}
#[tokio::test]
async fn disabled_harness_never_reads_corrupt_legacy_store() {
    let f = Fixture::new(settings(false), b"{corrupt mid-write").await;
    assert_eq!(f.engine.plugin_diagnostic(), None);
    f.provider.push_text_turn("ordinary turn");
    f.engine
        .prompt("hello", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    assert!(!f.agent.join("plugins/harness").exists());
    assert_eq!(
        std::fs::read(f.agent.join("harness/harness_state.json")).unwrap(),
        b"{corrupt mid-write"
    );
}
#[tokio::test]
async fn active_migration_refuses_corruption_without_authoritative_empty_target() {
    let f = Fixture::new(settings(true), b"{corrupt mid-write").await;
    assert!(f.call("list", json!({})).await.is_err());
    assert!(!f.agent.join("plugins/harness/store.json").exists());
}

const AUX_PROVIDER: &str = r#"
local requests={}
return {providers={{name='aux',mode='streaming',run=function(input,ctx)
  table.insert(requests,{model=input.model.id,system=input.context.system_prompt,session=input.options.sessionId})
  local review=string.find(input.context.system_prompt,'Review evidence',1,true)~=nil
  local text
  if review then text='{"shouldRefine":true,"rationale":"fixture evidence"}'
  else text='{"summary":"fixture learned","edits":[{"action":"create","kind":"memory","id":"learned","value":{"content":"auxiliary route"}}]}' end
  local message={role='assistant',content={{type='text',text=text}},api=input.model.api,provider=input.model.provider,
    model=input.model.id,usage={input=0,output=0,cacheRead=0,cacheWrite=0,totalTokens=0,
    cost={input=0,output=0,cacheRead=0,cacheWrite=0,total=0}},stopReason='stop',timestamp=0}
  ctx.emit({type='start',partial=message})
  ctx.emit({type='done',reason='stop',message=message})
  return message
end}},exports={{name='requests',run=function() return requests end}}}
"#;
async fn await_job(f: &Fixture, id: &Value) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let result = f.call("job_status", json!({"id":id})).await.unwrap();
            if result["status"] == "completed" || result["status"] == "failed" {
                break result;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("deferred job settled")
}
#[tokio::test]
async fn manual_and_interval_refinement_use_scoped_explicit_auxiliary_review_and_plan() {
    let mut settings = settings(true);
    settings["auxiliaryModel"] = json!("aux/default");
    settings["plugins"] = json!({"harness":{"config":{"auxiliaryModel":"aux/selected","autoRefine":{"enabled":true,"turnInterval":1,"cooldownMs":1_200_000}}},"route":{"enabled":true,"path":"provider.lua"}});
    settings["testProvider"] = json!(AUX_PROVIDER);
    let f = Fixture::new(settings, LEGACY).await;
    let job = f
        .call("refine", json!({"instructions":"manual evidence"}))
        .await
        .unwrap();
    let result = await_job(&f, &job["id"]).await;
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(
        f.call("get", json!({"kind":"memory","id":"learned"}))
            .await
            .unwrap()["value"]["content"],
        "auxiliary route"
    );
    f.call("delete", json!({"kind":"memory","id":"learned"}))
        .await
        .unwrap();
    // Actual settled user run schedules interval policy in Lua, not the host.
    f.provider.push_text_turn("complete");
    f.engine
        .prompt(
            "one settled user run",
            crate::session_engine::PromptOptions::default(),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if f.call("get", json!({"kind":"memory","id":"learned"}))
                .await
                .unwrap()
                != Value::Null
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("interval job applied");
    let requests = f.call_plugin("route", "requests", json!({})).await.unwrap();
    assert_eq!(
        requests.as_array().unwrap().len(),
        4,
        "manual and interval each review and plan"
    );
    assert!(requests
        .as_array()
        .unwrap()
        .iter()
        .all(|request| request["model"] == "selected"));
    f.call("delete", json!({"kind":"memory","id":"learned"}))
        .await
        .unwrap();
    f.provider.push_text_turn("complete");
    f.engine
        .prompt(
            "cooldown prevents another job",
            crate::session_engine::PromptOptions::default(),
        )
        .await
        .unwrap();
    // The observer returns only after it evaluated cooldown; no deferred job was admitted.
    assert_eq!(
        f.call("get", json!({"kind":"memory","id":"learned"}))
            .await
            .unwrap(),
        Value::Null
    );
    let after = f.call_plugin("route", "requests", json!({})).await.unwrap();
    assert_eq!(after, requests);
}

#[tokio::test]
async fn malformed_reload_preserves_harness_data_and_explicit_activation_overrides_legacy_false() {
    let mut config = settings(false);
    config["plugins"] = json!({"harness":{"enabled":true}});
    let f = Fixture::new(config, LEGACY).await;
    f.call(
        "create",
        json!({"kind":"memory","id":"safe","value":{"unknown":null}}),
    )
    .await
    .unwrap();
    let saved = std::fs::read(f.agent.join("plugins/harness/store.json")).unwrap();
    std::fs::write(f.agent.join("settings.json"), "{malformed").unwrap();
    assert!(f.engine.reload_plugins().await.is_err());
    assert_eq!(
        f.call("get", json!({"kind":"memory","id":"safe"}))
            .await
            .unwrap()["value"],
        json!({"unknown":null})
    );
    assert_eq!(
        std::fs::read(f.agent.join("plugins/harness/store.json")).unwrap(),
        saved
    );
}

const HELD_PROVIDER: &str = r#"
local calls=0
local released=false
return {providers={{name='aux',mode='streaming',run=function(input,ctx)
  calls=calls+1
  local review=string.find(input.context.system_prompt,'Review evidence',1,true)~=nil
  if not review then while not released do local tick=1 end end
  local text=review and '{"shouldRefine":true,"rationale":"fixture evidence"}' or '{"summary":"held","edits":[{"action":"create","kind":"memory","id":"late","value":{"content":"must not arrive stale"}}]}'
  local message={role='assistant',content={{type='text',text=text}},api=input.model.api,provider=input.model.provider,model=input.model.id,
    usage={input=0,output=0,cacheRead=0,cacheWrite=0,totalTokens=0,cost={input=0,output=0,cacheRead=0,cacheWrite=0,total=0}},stopReason='stop',timestamp=0}
  ctx.emit({type='start',partial=message});ctx.emit({type='done',reason='stop',message=message});return message
end}},exports={{name='calls',run=function() return calls end},{name='release',run=function() released=true return true end}}}
"#;
async fn held_fixture() -> Fixture {
    let mut config = settings(true);
    config["testProvider"] = json!(HELD_PROVIDER);
    config["plugins"] = json!({"harness":{"config":{"auxiliaryModel":"aux/selected","autoRefine":{"enabled":false}}},"route":{"enabled":true,"path":"provider.lua"}});
    Fixture::new(config, LEGACY).await
}
async fn started(f: &Fixture) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if f.call_plugin("route", "calls", json!({}))
                .await
                .unwrap()
                .as_u64()
                .unwrap()
                > 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("auxiliary plan started");
}
#[tokio::test]
async fn deferred_cancel_and_branch_conflict_never_apply_stale_edits() {
    let f = held_fixture().await;
    let job = f.call("refine", json!({})).await.unwrap();
    started(&f).await;
    f.call("cancel_job", json!({"id":job["id"]})).await.unwrap();
    let result = await_job(&f, &job["id"]).await;
    assert_eq!(result["status"], "failed");
    f.call_plugin("route", "release", json!({})).await.unwrap();
    assert_eq!(
        f.call("get", json!({"kind":"memory","id":"late"}))
            .await
            .unwrap(),
        Value::Null
    );
    let g = held_fixture().await;
    let job = g.call("refine", json!({})).await.unwrap();
    started(&g).await;
    g.provider.push_text_turn("changed branch");
    g.engine
        .prompt(
            "new durable turn",
            crate::session_engine::PromptOptions::default(),
        )
        .await
        .unwrap();
    g.call_plugin("route", "release", json!({})).await.unwrap();
    let result = await_job(&g, &job["id"]).await;
    assert_eq!(result["status"], "failed", "{result}");
    assert!(
        result["error"].as_str().unwrap().contains("branch changed"),
        "{result}"
    );
    assert_eq!(
        g.call("get", json!({"kind":"memory","id":"late"}))
            .await
            .unwrap(),
        Value::Null
    );
}
#[tokio::test]
async fn retirement_cancels_owned_job_and_does_not_publish_after_reenable() {
    let f = held_fixture().await;
    let checkpoint = f.engine.plugin_job_checkpoint();
    f.call("refine", json!({})).await.unwrap();
    started(&f).await;
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(f.agent.join("settings.json")).unwrap()).unwrap();
    config["plugins"]["harness"]["enabled"] = json!(false);
    std::fs::write(f.agent.join("settings.json"), config.to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.engine.drain_plugin_jobs_since(&checkpoint),
    )
    .await
    .expect("retirement wakes generic drain");
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0]["state"]["status"], "failed", "{terminal:?}");
    f.call_plugin("route", "release", json!({})).await.unwrap();
    config["plugins"]["harness"]["enabled"] = json!(true);
    std::fs::write(f.agent.join("settings.json"), config.to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    assert_eq!(
        f.call("get", json!({"kind":"memory","id":"late"}))
            .await
            .unwrap(),
        Value::Null
    );
}

#[tokio::test]
async fn dynamic_refine_command_schedules_after_public_command_persistence() {
    let mut config = settings(true);
    config["testProvider"] = json!(AUX_PROVIDER);
    config["plugins"] = json!({"harness":{"config":{"auxiliaryModel":"aux/selected","autoRefine":{"enabled":false}}},"route":{"enabled":true,"path":"provider.lua"}});
    let f = Fixture::new(config, LEGACY).await;
    let crate::session_engine::PromptOutcome::SessionCommand(command) = f
        .engine
        .prompt(
            "/refine command evidence",
            crate::session_engine::PromptOptions::default(),
        )
        .await
        .unwrap()
    else {
        panic!("dynamic command fell through to inference")
    };
    assert!(command.plugin.is_some());
    let checkpoint = f.engine.plugin_job_checkpoint();
    let result = crate::session_engine::session_commands::execute_plugin_session_command(
        &f.engine, &command,
    )
    .await;
    assert!(result.error.is_none(), "{:?}", result.error);
    let payload: Value =
        serde_json::from_str(&result.messages.last().unwrap().content.text()).unwrap();
    let outcomes = f.engine.drain_plugin_jobs_since(&checkpoint).await;
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["state"]["status"], "completed", "{outcomes:?}");
    let job = await_job(&f, &payload["id"]).await;
    assert_eq!(job["status"], "completed", "{job}");
    assert!(
        f.provider.calls().is_empty(),
        "command did not enter paid main inference"
    );
}

#[tokio::test]
async fn accepted_compaction_schedules_exactly_one_automatic_auxiliary_review_and_plan() {
    let mut config = settings(true);
    config["testProvider"] = json!(AUX_PROVIDER);
    config["auxiliaryModel"] = json!("aux/default");
    config["plugins"] = json!({"default_context":{"config":{"policy":"full","compactors":["standard_compaction/standard"]}},"harness":{"config":{"auxiliaryModel":"aux/selected","autoRefine":{"enabled":true,"turnInterval":10000,"compact":true,"cooldownMs":0}}},"route":{"enabled":true,"path":"provider.lua"}});
    let f = Fixture::new(config, LEGACY).await;
    {
        let mut session = f.engine.session.session_handle().lock().await;
        for n in 0..8 {
            let message:pa_types::session::AgentMessage=serde_json::from_value(json!({"role":"user","content":format!("canonical evidence {n}: {}","durable fact ".repeat(300)),"timestamp":0})).unwrap();
            session.append_message(message).unwrap();
        }
    }
    let model:pa_types::ai::Model=serde_json::from_value(json!({"id":"test","name":"Test","api":"openai-completions","provider":"test","baseUrl":"http://127.0.0.1:9","input":["text"],"reasoning":false,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":10000,"maxTokens":100})).unwrap();
    let checkpoint = f.engine.plugin_job_checkpoint();
    let outcome = f
        .engine
        .session
        .compact(None, &model, None, None)
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            crate::session_engine::compact_session::CompactOutcome::Ran(_)
        ),
        "{outcome:?}"
    );
    let jobs = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.engine.drain_plugin_jobs_since(&checkpoint),
    )
    .await
    .expect("compact observer's job drained");
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0]["state"]["status"], "completed", "{jobs:?}");
    assert_eq!(
        f.call("get", json!({"kind":"memory","id":"learned"}))
            .await
            .unwrap()["value"]["content"],
        "auxiliary route"
    );
    let requests = f.call_plugin("route", "requests", json!({})).await.unwrap();
    assert_eq!(
        requests.as_array().unwrap().len(),
        3,
        "one summarizer + one review + one plan"
    );
    assert_eq!(requests[0]["model"], "default");
    assert_eq!(requests[1]["model"], "selected");
    assert_eq!(requests[2]["model"], "selected");
    assert!(
        f.provider.calls().is_empty(),
        "auxiliary phases bypass the main stream"
    );
}

#[tokio::test]
async fn shared_vm_cancellation_does_not_poison_concurrent_auxiliary_jobs_50_trials() {
    for trial in 0..50 {
        let f = held_fixture().await;
        let first = f.call("refine", json!({})).await.unwrap();
        let second = f.call("refine", json!({})).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if f.call_plugin("route", "calls", json!({}))
                    .await
                    .unwrap()
                    .as_u64()
                    .unwrap()
                    >= 4
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both concurrent plans started");
        f.call("cancel_job", json!({"id":first["id"]}))
            .await
            .unwrap();
        f.call_plugin("route", "release", json!({})).await.unwrap();
        let terminal = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            f.engine.drain_plugin_jobs(),
        )
        .await
        .expect("concurrent cancellation wakes drain");
        assert_eq!(terminal.len(), 2, "trial{trial}: {terminal:?}");
        let first = terminal
            .iter()
            .find(|row| row["id"] == first["id"])
            .unwrap();
        let second = terminal
            .iter()
            .find(|row| row["id"] == second["id"])
            .unwrap();
        assert_eq!(first["state"]["status"], "failed", "trial{trial}: {first}");
        assert_eq!(
            second["state"]["status"], "completed",
            "trial{trial}: {second}"
        );
        assert_eq!(
            f.call("get", json!({"kind":"memory","id":"late"}))
                .await
                .unwrap()["value"]["content"],
            "must not arrive stale"
        );
        assert!(f.provider.calls().is_empty());
    }
}

#[tokio::test]
async fn aborted_admitted_main_run_reports_cancelled_and_never_starts_automatic_inference() {
    let source=AUX_PROVIDER.replace("local requests={}","local requests={} local observed={}")
        .replace("return {providers=","return {hooks={{name='run_settled',run=function(input) table.insert(observed,input.reason) return {} end}},providers=")
        .replace("exports={{name='requests',run=function() return requests end}}}","exports={{name='requests',run=function() return requests end},{name='settled_reasons',run=function() return observed end},{name='request_count',run=function() return #requests end}}}");
    let mut config = settings(true);
    config["testProvider"] = json!(source);
    config["plugins"] = json!({"harness":{"config":{"auxiliaryModel":"aux/selected","autoRefine":{"enabled":true,"turnInterval":1,"cooldownMs":0}}},"route":{"enabled":true,"path":"provider.lua"}});
    let f = Fixture::new(config, LEGACY).await;
    let checkpoint = f.engine.plugin_job_checkpoint();
    f.provider
        .push_stalled_turn("the admitted main run is streaming");
    let prompt = f.engine.prompt(
        "start then cancel this run",
        crate::session_engine::PromptOptions::default(),
    );
    let cancel = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if f.provider.calls().len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("main provider invocation admitted");
        f.engine.session.agent().abort();
    };
    let (result, ()) = tokio::join!(prompt, cancel);
    result.unwrap();
    let jobs = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.engine.drain_plugin_jobs_since(&checkpoint),
    )
    .await
    .expect("cancelled main run settles all observers");
    let requests = f.call_plugin("route", "requests", json!({})).await.unwrap();
    assert!(
        jobs.is_empty(),
        "cancelled main run scheduled automatic work: {jobs:?}, requests={requests}"
    );
    assert_eq!(
        f.call_plugin("route", "request_count", json!({}))
            .await
            .unwrap(),
        json!(0),
        "cancelled main run must not review or plan"
    );
    assert_eq!(
        f.call_plugin("route", "settled_reasons", json!({}))
            .await
            .unwrap(),
        json!(["cancelled"])
    );
    assert_eq!(
        f.provider.calls().len(),
        1,
        "only the explicitly admitted main call is allowed"
    );
    assert_eq!(
        f.call("get", json!({"kind":"memory","id":"learned"}))
            .await
            .unwrap(),
        Value::Null
    );
    assert_eq!(
        f.call("get", json!({"kind":"_policy","id":"automatic"}))
            .await
            .unwrap(),
        Value::Null,
        "cancelled run must not advance the interval or cooldown"
    );
}

/// A scripted provider failure that the retry lane owns: the terminal message
/// carries the classified `provider_stream_failure` diagnostic the policy
/// reads, unlike the plain scripted failure.
fn retryable_failure_turn(model: &pa_agent::types::Model) -> pa_agent::scripted::ScriptedTurn {
    use pa_agent::scripted::{stream_failure_steps, ScriptStep, ScriptedTurn};
    let mut steps = stream_failure_steps(model, "partial", "provider rate limited");
    if let Some(ScriptStep::Event(event)) = steps.last_mut() {
        if let pa_agent::stream::AssistantMessageEvent::Error { error, .. } = event.as_mut() {
            error.diagnostics = Some(vec![pa_agent::types::AssistantMessageDiagnostic {
                kind: "provider_stream_failure".to_string(),
                timestamp: 0,
                error: None,
                details: Some(json!({"kind":"rate_limit","status":429})),
            }]);
        }
    }
    ScriptedTurn::Events(steps)
}

/// The recorded auxiliary requests; an empty Lua table crosses the bridge as
/// an object, so count both shapes.
fn aux_request_count(value: &Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}

fn interval_settings() -> Value {
    let mut config = settings(true);
    config["auxiliaryModel"] = json!("aux/default");
    config["testProvider"] = json!(AUX_PROVIDER);
    config["plugins"] = json!({"harness":{"config":{"auxiliaryModel":"aux/selected","autoRefine":{"enabled":true,"turnInterval":1,"cooldownMs":0}}},"route":{"enabled":true,"path":"provider.lua"}});
    config
}

#[tokio::test]
async fn retryable_provider_failure_settle_does_not_schedule_review() {
    let f = Fixture::new(interval_settings(), LEGACY).await;
    let model: pa_agent::types::Model=serde_json::from_value(json!({"id":"test","name":"Test","api":"openai-completions","provider":"test","baseUrl":"http://localhost:9","reasoning":false,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":10000,"maxTokens":100})).unwrap();
    // A settle the retry lane will re-issue is not a turn boundary: no review.
    f.provider.push_turn(retryable_failure_turn(&model));
    let _ = f
        .engine
        .prompt(
            "failing turn",
            crate::session_engine::PromptOptions::default(),
        )
        .await;
    assert_eq!(f.engine.plugin_job_checkpoint(), Vec::<String>::new());
    assert_eq!(
        f.call_plugin("route", "requests", json!({})).await.unwrap(),
        json!({}),
        "no auxiliary request reached the provider"
    );
    // Control: a settle with no retry behind it schedules exactly one review.
    f.provider.push_text_turn("done");
    f.engine
        .prompt(
            "settled turn",
            crate::session_engine::PromptOptions::default(),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if aux_request_count(&f.call_plugin("route", "requests", json!({})).await.unwrap()) >= 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the settled turn's review and plan ran");
    let requests = f.call_plugin("route", "requests", json!({})).await.unwrap();
    assert_eq!(aux_request_count(&requests), 2, "one review + one plan");
}

#[tokio::test]
async fn deferred_review_waits_for_the_turn_to_settle() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = Fixture::new(interval_settings(), LEGACY).await;
    let busy = Arc::new(AtomicBool::new(true));
    let probe_busy = Arc::clone(&busy);
    f.engine.session.set_turn_activity_probe(Arc::new(move || {
        crate::session_engine::TurnActivity {
            streaming: probe_busy.load(Ordering::SeqCst),
            ..crate::session_engine::TurnActivity::default()
        }
    }));
    let job = f
        .call("refine", json!({"instructions":"held evidence"}))
        .await
        .unwrap();
    // The turn (a provider retry episode included) is still live: the job is
    // admitted but must not reach the provider.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(
        f.call("job_status", json!({"id":job["id"]})).await.unwrap()["status"],
        "queued"
    );
    assert_eq!(
        f.call_plugin("route", "requests", json!({})).await.unwrap(),
        json!({}),
        "no auxiliary request reached the provider"
    );
    // The turn settles: the same job runs at that boundary.
    busy.store(false, Ordering::SeqCst);
    let result = await_job(&f, &job["id"]).await;
    assert_eq!(result["status"], "completed", "{result}");
    let requests = f.call_plugin("route", "requests", json!({})).await.unwrap();
    assert_eq!(requests.as_array().unwrap().len(), 2, "review + plan");
}

#[tokio::test]
async fn review_request_carries_the_session_id() {
    let f = Fixture::new(interval_settings(), LEGACY).await;
    let session_id = f.engine.session.session_id().await;
    let job = f
        .call("refine", json!({"instructions":"identified evidence"}))
        .await
        .unwrap();
    let result = await_job(&f, &job["id"]).await;
    assert_eq!(result["status"], "completed", "{result}");
    let requests = f.call_plugin("route", "requests", json!({})).await.unwrap();
    let requests = requests.as_array().unwrap();
    assert_eq!(requests.len(), 2, "{requests:?}");
    for request in requests {
        assert_eq!(request["session"], json!(session_id), "{request}");
    }
}

#[tokio::test]
async fn disabled_plugin_settle_enqueues_no_job() {
    let f = Fixture::new(settings(false), LEGACY).await;
    f.provider.push_text_turn("done");
    f.engine
        .prompt(
            "ordinary turn",
            crate::session_engine::PromptOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(f.engine.plugin_job_checkpoint(), Vec::<String>::new());
    assert!(!f.agent.join("plugins/harness").exists());
}
