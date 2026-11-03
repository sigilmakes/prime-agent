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
    json!({"enableBuiltinSkills":false,"bundledSkills":{"websearch":false},
        "plugins":{"harness":{"enabled":enabled}}})
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
async fn refinement_presentation_uses_committed_update_delete_and_empty_changes() {
    let original = r#"{"summary":"fixture learned","edits":[{"action":"create","kind":"memory","id":"learned","value":{"content":"auxiliary route"}}]}"#;
    for action in ["update", "delete", "none"] {
        let edits = if action == "none" {
            json!([])
        } else {
            json!([{"action":action,"kind":"memory","id":"learned","value":{"content":"changed"}}])
        };
        let proposal = json!({"summary":"fixture learned","edits":edits});
        let mut config = settings(true);
        config["auxiliaryModel"] = json!("aux/selected");
        config["testProvider"] = json!(AUX_PROVIDER.replace(original, &proposal.to_string()));
        config["plugins"]["route"] = json!({"enabled":true,"path":"provider.lua"});
        let f = Fixture::new(config, LEGACY).await;
        f.call(
            "create",
            json!({"kind":"memory","id":"learned","value":{"content":"before"}}),
        )
        .await
        .unwrap();
        let job = f.call("refine", json!({})).await.unwrap();
        let result = await_job(&f, &job["id"]).await;
        assert_eq!(result["status"], "completed", "{result}");
        let edits = result["result"]["message"]["details"]["edits"]
            .as_array()
            .unwrap();
        if action == "none" {
            assert!(edits.is_empty());
        } else {
            assert_eq!(edits[0]["action"], action);
            assert_eq!(edits[0]["before"], json!({"content":"before"}));
            assert_eq!(edits[0]["applied"], true);
            if action == "update" {
                assert_eq!(edits[0]["after"], json!({"content":"changed"}));
            } else {
                assert!(edits[0].get("after").is_none());
            }
        }
        assert_eq!(
            result["result"]["applied"].as_array().unwrap().len(),
            edits.len()
        );
    }
}

#[tokio::test]
async fn manual_refinement_uses_scoped_explicit_auxiliary_review_and_plan() {
    let mut settings = settings(true);
    settings["auxiliaryModel"] = json!("aux/default");
    settings["plugins"]["harness"]["config"] = json!({"auxiliaryModel":"aux/selected"});
    settings["plugins"]["route"] = json!({"enabled":true,"path":"provider.lua"});
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
    let requests = f.call_plugin("route", "requests", json!({})).await.unwrap();
    assert_eq!(
        requests.as_array().unwrap().len(),
        2,
        "manual review and plan"
    );
    assert!(requests
        .as_array()
        .unwrap()
        .iter()
        .all(|request| request["model"] == "selected"));
}

#[tokio::test]
async fn malformed_reload_preserves_harness_data() {
    let config = settings(true);
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
    config["plugins"]["harness"]["config"] = json!({"auxiliaryModel":"aux/selected"});
    config["plugins"]["route"] = json!({"enabled":true,"path":"provider.lua"});
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
async fn retired_job_outcomes_remain_drainable_without_publishing_into_new_generation() {
    let f = held_fixture().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    f.engine
        .session
        .set_plugin_job_sink(Arc::new(move |session, job| {
            tx.send((session.to_owned(), job)).unwrap();
        }));
    f.call("refine", json!({})).await.unwrap();
    started(&f).await;
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(f.agent.join("settings.json")).unwrap()).unwrap();
    config["plugins"]["harness"]["enabled"] = json!(false);
    std::fs::write(f.agent.join("settings.json"), config.to_string()).unwrap();
    f.engine.reload_plugins().await.unwrap();
    let terminal = f.engine.drain_plugin_jobs().await;
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0]["state"]["status"], "failed");
    assert!(rx.try_recv().is_err(), "retired generation cannot publish");
    assert_eq!(f.engine.drain_plugin_jobs().await, terminal);
}

#[tokio::test]
async fn dynamic_refine_command_schedules_after_public_command_persistence() {
    let mut config = settings(true);
    config["testProvider"] = json!(AUX_PROVIDER);
    config["plugins"]["harness"]["config"] = json!({"auxiliaryModel":"aux/selected"});
    config["plugins"]["route"] = json!({"enabled":true,"path":"provider.lua"});
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

fn manual_settings() -> Value {
    let mut config = settings(true);
    config["auxiliaryModel"] = json!("aux/default");
    config["testProvider"] = json!(AUX_PROVIDER);
    config["plugins"]["harness"]["config"] = json!({"auxiliaryModel":"aux/selected"});
    config["plugins"]["route"] = json!({"enabled":true,"path":"provider.lua"});
    config
}

#[tokio::test]
async fn deferred_review_waits_for_the_turn_to_settle() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = Fixture::new(manual_settings(), LEGACY).await;
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
    let f = Fixture::new(manual_settings(), LEGACY).await;
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
async fn bounded_harness_context_ranks_across_scopes_without_changing_raw_records() {
    let f = Fixture::new(settings(true), LEGACY).await;
    let content = format!("東京 quantum {}", "界".repeat(16_000));
    for scope in ["first", "second"] {
        f.call(
            "create",
            json!({"scope":scope,"kind":"memory","id":"same",
            "value":{"title":"old relevant entry", "content":content,"unknown":null}}),
        )
        .await
        .unwrap();
    }
    for id in ["a", "b", "c", "d", "e", "f", "g"] {
        f.call(
            "create",
            json!({"scope":"first","kind":"memory","id":id,
            "content":"unrelated newer record"}),
        )
        .await
        .unwrap();
    }
    f.call("enroll", json!({"scopes":["first","first","second"]}))
        .await
        .unwrap();
    let path = f.agent.join("plugins/harness/store.json");
    let before = std::fs::read(&path).unwrap();
    f.provider.push_text_turn("done");
    f.engine
        .prompt(
            "東京 quantum",
            crate::session_engine::PromptOptions::default(),
        )
        .await
        .unwrap();
    let calls = f.provider.calls();
    let prompt = calls.last().unwrap().system_prompt.as_ref().unwrap();
    assert!(prompt.contains("東京 quantum"));
    assert!(prompt.contains("scope=\"first\", kind=memory, id=\"same\""));
    assert!(prompt.contains("scope=\"second\", kind=memory, id=\"same\""));
    assert_eq!(
        prompt
            .matches("scope=\"first\", kind=memory, id=\"same\"")
            .count(),
        1
    );
    assert!(!prompt.contains(&"界".repeat(181)));
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert_eq!(
        f.call("get", json!({"scope":"first","kind":"memory","id":"same"}))
            .await
            .unwrap()["value"],
        json!({"title":"old relevant entry","content":content,"unknown":null})
    );
}

#[tokio::test]
async fn oversized_harness_locator_fails_before_provider_without_truncation() {
    let f = Fixture::new(settings(true), LEGACY).await;
    let id = "界".repeat(12_000);
    f.call(
        "create",
        json!({"kind":"memory","id":id,"content":"important directive"}),
    )
    .await
    .unwrap();
    let before = std::fs::read(f.agent.join("plugins/harness/store.json")).unwrap();
    let error = f
        .engine
        .prompt("hello", crate::session_engine::PromptOptions::default())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("32768"), "{error:#}");
    assert!(f.provider.calls().is_empty());
    assert_eq!(
        std::fs::read(f.agent.join("plugins/harness/store.json")).unwrap(),
        before
    );
    assert_eq!(
        f.call("get", json!({"kind":"memory","id":id}))
            .await
            .unwrap()["value"]["content"],
        "important directive"
    );
}

#[tokio::test]
async fn generic_context_byte_limits_are_exact_and_aggregate_before_provider() {
    // 8192 four-byte codepoints exactly fill a contribution; separators count
    // toward the aggregate. The base prompt is deliberately outside this budget.
    for (plugins, chars, padding, succeeds) in [
        (1, 8192, 0, true),
        (1, 8192, 1, false),
        (1, 8193, 0, false),
        (4, 8191, 2, true),
        (4, 8191, 3, false),
    ] {
        let mut config = settings(false);
        let mut entries = serde_json::Map::new();
        entries.insert("harness".into(), json!({"enabled":false}));
        for index in 0..plugins {
            entries.insert(
                format!("budget{index}"),
                json!({"enabled":true,"path":"provider.lua","config":{"chars":chars,"padding":padding}}),
            );
        }
        config["plugins"] = Value::Object(entries);
        config["testProvider"] = json!(
            r"return {hooks={{name='context_contribution',run=function()
            return {instructions=string.rep('😀',config.chars)..string.rep('x',config.padding)}
        end}}}"
        );
        let f = Fixture::new(config, LEGACY).await;
        f.provider.push_text_turn("done");
        let result = f
            .engine
            .prompt("hello", crate::session_engine::PromptOptions::default())
            .await;
        if succeeds {
            result.unwrap();
            let calls = f.provider.calls();
            let prompt = calls.last().unwrap().system_prompt.as_ref().unwrap();
            assert_eq!(prompt.matches('😀').count(), plugins * chars);
        } else {
            let error = result.unwrap_err();
            assert!(
                format!("{error:#}").contains("refusing to truncate instructions"),
                "{error:#}"
            );
            assert!(f.provider.calls().is_empty());
        }
    }
}

#[tokio::test]
async fn bounded_harness_context_large_corpus_crosses_actual_lua_vm() {
    let f = Fixture::new(settings(true), LEGACY).await;
    // Exercise the real 16 MiB Lua VM, not just the standalone Lua formatter.
    // Both 16K ASCII bytes and 16K multibyte codepoints must remain readable.
    for index in 0..100 {
        let content = if index % 2 == 0 {
            format!("quantum {}", "界".repeat(16_000))
        } else {
            format!("quantum {}", "a".repeat(16_000))
        };
        f.call(
            "create",
            json!({"kind":"memory","id":format!("record-{index:03}"),
            "value":{"content":content,"unknown":null}}),
        )
        .await
        .unwrap();
    }
    let path = f.agent.join("plugins/harness/store.json");
    let before = std::fs::read(&path).unwrap();
    f.provider.push_text_turn("done");
    f.engine
        .prompt("quantum", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    let calls = f.provider.calls();
    let prompt = calls.last().unwrap().system_prompt.as_ref().unwrap();
    assert!(prompt.contains("memory: 100 records; 94 omitted"));
    assert_eq!(prompt.matches("kind=memory, id=\"record-").count(), 6);
    assert!(!prompt.contains(&"界".repeat(181)));
    assert!(!prompt.contains(&"a".repeat(181)));
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert_eq!(
        f.call("get", json!({"kind":"memory","id":"record-000"}))
            .await
            .unwrap()["value"]["content"],
        format!("quantum {}", "界".repeat(16_000))
    );
}

fn scoped_settings(scopes: Value) -> Value {
    let mut settings = settings(true);
    settings["harnessScopes"] = scopes;
    settings
}

async fn instructions(f: &Fixture, text: &str) -> String {
    f.provider.push_text_turn("done");
    f.engine
        .prompt(text, crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    f.provider
        .calls()
        .last()
        .unwrap()
        .system_prompt
        .clone()
        .unwrap()
}

#[test]
fn harness_scopes_resolve_defaults_and_sanitize_configured_values() {
    let unset: crate::settings::Settings = serde_json::from_value(json!({})).unwrap();
    assert_eq!(super::default_scopes(&unset), ["global"]);
    assert_eq!(super::source(&unset).config["scopes"], json!(["global"]));
    let configured: crate::settings::Settings =
        serde_json::from_value(json!({"harnessScopes":[" global ","other","global",""]})).unwrap();
    assert_eq!(super::default_scopes(&configured), ["global", "other"]);
    let empty: crate::settings::Settings =
        serde_json::from_value(json!({"harnessScopes":[]})).unwrap();
    assert!(super::default_scopes(&empty).is_empty());
    assert_eq!(super::source(&empty).config["scopes"], json!([]));
}

#[tokio::test]
async fn default_harness_scopes_load_global_without_enrollment() {
    let f = Fixture::new(settings(true), LEGACY).await;
    f.call(
        "create",
        json!({"scope":"global","kind":"memory","id":"shared","content":"global evidence"}),
    )
    .await
    .unwrap();
    let prompt = instructions(&f, "global evidence").await;
    assert!(prompt.contains("scope=\"global\", kind=memory, id=\"shared\""));
}

#[tokio::test]
async fn enrollment_overrides_default_harness_scopes() {
    let f = Fixture::new(settings(true), LEGACY).await;
    for scope in ["global", "other"] {
        f.call(
            "create",
            json!({"scope":scope,"kind":"memory","id":"shared","content":"shared evidence"}),
        )
        .await
        .unwrap();
    }
    let prompt = instructions(&f, "shared evidence").await;
    assert!(prompt.contains("scope=\"global\", kind=memory, id=\"shared\""));
    assert!(!prompt.contains("scope=\"other\""));
    f.call("enroll", json!({"scopes":["other"]})).await.unwrap();
    let prompt = instructions(&f, "shared evidence").await;
    assert!(!prompt.contains("scope=\"global\""));
    assert!(prompt.contains("scope=\"other\", kind=memory, id=\"shared\""));
}

#[tokio::test]
async fn empty_harness_scopes_keep_only_the_session_scope() {
    let f = Fixture::new(scoped_settings(json!([])), LEGACY).await;
    f.call(
        "create",
        json!({"scope":"global","kind":"memory","id":"shared","content":"global evidence"}),
    )
    .await
    .unwrap();
    f.call(
        "create",
        json!({"kind":"memory","id":"local","content":"session evidence"}),
    )
    .await
    .unwrap();
    let prompt = instructions(&f, "evidence").await;
    assert!(prompt.contains("session evidence"));
    assert!(!prompt.contains("scope=\"global\""));
}

#[tokio::test]
async fn non_table_scope_config_keeps_the_global_default() {
    for scopes in [Value::Null, json!("global")] {
        let mut document = settings(true);
        document["plugins"] = json!({"harness":{"config":{"scopes":scopes.clone()}}});
        let f = Fixture::new(document, LEGACY).await;
        f.call(
            "create",
            json!({"scope":"global","kind":"memory","id":"shared","content":"global evidence"}),
        )
        .await
        .unwrap();
        let prompt = instructions(&f, "global evidence").await;
        assert!(
            prompt.contains("scope=\"global\", kind=memory, id=\"shared\""),
            "scopes={scopes} must fall back to the global default"
        );
    }
}

#[tokio::test]
async fn configured_harness_scopes_replace_the_global_default() {
    let f = Fixture::new(scoped_settings(json!(["custom"])), LEGACY).await;
    for scope in ["global", "custom"] {
        f.call(
            "create",
            json!({"scope":scope,"kind":"memory","id":"shared","content":"scoped evidence"}),
        )
        .await
        .unwrap();
    }
    let prompt = instructions(&f, "scoped evidence").await;
    assert!(prompt.contains("scope=\"custom\", kind=memory, id=\"shared\""));
    assert!(!prompt.contains("scope=\"global\""));
}
