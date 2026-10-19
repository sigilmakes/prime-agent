use super::*;
use crate::kernel::shared::HostRequestPayload;
use crate::session_engine::{
    engine::{create_session, SessionEngineConfig},
    session_commands::{execute_session_command, SessionCommandParams},
    PromptOptions, PromptOutcome,
};
use pa_agent::scripted::ScriptedProvider;

const ALPHA: &str = r"
local observed = 0
local started = false
local tick = 0
local function report(input, ctx)
    return {config=config, identity=ctx.call('session.info', {}), input=input, observed=observed, started=started, tick=tick}
end
return {
  exports={
    {name='report',run=report},
    {name='busy',run=function() started=true; while true do tick=tick+1 end end},
    {name='unsupported',run=function(input,ctx) return ctx.call('not.supported', {}) end},
    {name='null_result',run=function() return null end},
    {name='stream',mode='streaming',run=function(input,ctx) ctx.emit('update'); return 'done' end}
  },
  commands={{name='alpha_command',description='Configured alpha command',run=report}},
  hooks={{name='turn_settled',run=function(input)
    assert(input.stage=='turn_settled' and input.message.role=='assistant')
    assert(input.session~=nil and input.tool_results~=nil)
    observed=observed+1
    return {observed=observed}
  end}}
}
";
const BETA: &str = r"
return {exports={{name='value',run=function(input,ctx)
  return {configured=config.value,session=ctx.call('session.info', {}).session}
end}},commands={{name='beta_command',run=function(input) return input.args end}}}
";

struct Fixture {
    _dir: tempfile::TempDir,
    cwd: PathBuf,
    agent: PathBuf,
    engine: SessionEngine,
    provider: Arc<ScriptedProvider>,
}

impl Fixture {
    async fn new(alpha: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(cwd.join("alpha.lua"), alpha).unwrap();
        std::fs::write(cwd.join("beta.lua"), BETA).unwrap();
        std::fs::write(agent.join("settings.json"), json!({
            "harnessRefinements":false,"enableBuiltinSkills":false,"bundledSkills":{"websearch":false},
            "plugins":{
                "alpha":{"enabled":true,"path":"alpha.lua","config":{"value":"global","nested":{"kept":true}}},
                "beta":{"enabled":true,"path":"beta.lua","config":{"value":"second"}}
            }
        }).to_string()).unwrap();
        std::fs::write(
            cwd.join(".prime/agent/settings.json"),
            json!({
                "plugins":{"alpha":{"config":{"value":"project"}}}
            })
            .to_string(),
        )
        .unwrap();
        let model: pa_agent::types::Model = serde_json::from_value(json!({
            "id":"test","name":"Test","api":"openai-completions","provider":"test","baseUrl":"http://localhost",
            "reasoning":false,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":10000,"maxTokens":100
        })).unwrap();
        let provider = Arc::new(ScriptedProvider::new(model.clone()));
        let engine = create_session(SessionEngineConfig {
            cwd: cwd.clone(),
            agent_dir: agent.clone(),
            model: Some(model),
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
            engine,
            provider,
        }
    }

    fn project(&self, value: &Value) {
        std::fs::write(
            self.cwd.join(".prime/agent/settings.json"),
            value.to_string(),
        )
        .unwrap();
    }
}

async fn request(engine: &SessionEngine, kind: &str, data: Value) -> anyhow::Result<Value> {
    let handlers = engine.plugin_host_handlers();
    let mut data = data;
    data["type"] = json!(kind);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handlers.get(kind).unwrap()(HostRequestPayload { data }),
    )
    .await
    .unwrap()
}

async fn catalog(engine: &SessionEngine) -> Value {
    request(
        engine,
        "plugin.catalog",
        json!({"session":engine.session.session_id().await}),
    )
    .await
    .unwrap()
}

fn descriptor(catalog: &Value, plugin: &str, name: &str) -> Value {
    catalog["exports"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["plugin"] == plugin && entry["name"] == name)
        .unwrap()
        .clone()
}

fn payload(session: &str, descriptor: &Value, input: Value) -> Value {
    let mut request = json!({"session":session,"plugin":descriptor["plugin"],"generation":descriptor["generation"],"export":descriptor["name"]});
    request["input"] = input;
    request
}

async fn run_export(
    engine: &SessionEngine,
    plugin: &str,
    name: &str,
    input: Value,
) -> anyhow::Result<Value> {
    let catalog = catalog(engine).await;
    request(
        engine,
        "plugin.invoke",
        payload(
            catalog["session"].as_str().unwrap(),
            &descriptor(&catalog, plugin, name),
            input,
        ),
    )
    .await
}

async fn history(engine: &SessionEngine) -> Value {
    json!({"entries":engine.session.entries().await,"messages":engine.session.agent().state().await.messages})
}

async fn command(
    engine: &SessionEngine,
    text: &str,
) -> super::super::session_commands::SessionCommandExecution {
    let PromptOutcome::SessionCommand(command) =
        engine.prompt(text, PromptOptions::default()).await.unwrap()
    else {
        panic!("not a core session command")
    };
    let model = serde_json::from_value(json!({
        "id":"test","name":"Test","api":"openai-completions","provider":"test","baseUrl":"http://localhost",
        "reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":10000,"maxTokens":100
    })).unwrap();
    let mut autonomous = crate::autonomous::create_autonomous_runtime_state(None, None);
    execute_session_command(
        engine,
        &mut SessionCommandParams {
            model: &model,
            api_key: None,
            autonomous: &mut autonomous,
        },
        &command,
        &[],
    )
    .await
}

#[tokio::test]
async fn configured_plugins_cross_engine_kernel_and_slash_surfaces() {
    let f = Fixture::new(ALPHA).await;
    let engine = &f.engine;
    assert_eq!(engine.plugin_diagnostic(), None);
    let session = engine.session.session_id().await;
    let inventory = catalog(engine).await;
    assert_eq!(inventory["session"], session);
    assert_eq!(inventory["exports"].as_array().unwrap().len(), 6);
    let info = run_export(engine, "alpha", "report", json!({"hello":"world"}))
        .await
        .unwrap();
    assert_eq!(
        info,
        json!({"config":{"value":"project","nested":{"kept":true}},"identity":{"session":session,"cwd":f.cwd,"role":"root"},"input":{"hello":"world"},"observed":0,"started":false,"tick":0})
    );
    assert_eq!(
        run_export(engine, "beta", "value", json!({}))
            .await
            .unwrap(),
        json!({"configured":"second","session":session})
    );
    assert_eq!(
        run_export(engine, "alpha", "null_result", json!({}))
            .await
            .unwrap(),
        Value::Null
    );
    assert!(run_export(engine, "alpha", "unsupported", json!({}))
        .await
        .unwrap_err()
        .to_string()
        .contains("unsupported plugin capability"));
    assert!(run_export(engine, "alpha", "stream", json!({}))
        .await
        .unwrap_err()
        .to_string()
        .contains("streaming"));
    // A Python tool call can arrive with the session already locked. The production
    // handler must not reacquire that lock or the enclosing tool turn deadlocks.
    let lock = engine.session.shared_persistence();
    let guard = lock.lock().await;
    let report = descriptor(&inventory, "alpha", "report");
    request(
        engine,
        "plugin.invoke",
        payload(&session, &report, json!({})),
    )
    .await
    .unwrap();
    drop(guard);
    assert!(
        request(engine, "plugin.catalog", json!({"session":"foreign"}))
            .await
            .is_err()
    );
    let mut wrong = payload(&session, &report, json!({}));
    wrong["plugin"] = json!("beta");
    assert!(request(engine, "plugin.invoke", wrong).await.is_err());
    assert!(request(
        engine,
        "plugin.invoke",
        payload(&session, &report, json!([]))
    )
    .await
    .is_err());
    let listed = engine.session.session_command_inventory();
    assert!(listed
        .iter()
        .any(|entry| entry.name == "alpha_command" && entry.plugin.is_some()));
    assert!(
        listed.iter().all(|entry| entry.name != "refine"),
        "an inactive harness contributes no command and core reserves no refine command"
    );
    assert_eq!(engine.session.classify_session_command("/refine"), None);
    let result = command(engine, "/alpha_command some arguments").await;
    assert_eq!(result.error, None);
    assert!(result
        .messages
        .last()
        .unwrap()
        .content
        .text()
        .contains("some arguments"));
    assert!(f.provider.calls().is_empty());
    f.provider.push_text_turn("settled");
    engine
        .prompt("observe this actual turn", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(
        run_export(engine, "alpha", "report", json!({}))
            .await
            .unwrap()["observed"],
        1
    );
}

#[tokio::test]
async fn reload_is_atomic_config_sensitive_and_history_neutral() {
    let f = Fixture::new(ALPHA).await;
    let engine = &f.engine;
    let initial = catalog(engine).await;
    let before = history(engine).await;
    let result = command(engine, "/reload").await;
    assert_eq!(result.error, None);
    assert_eq!(result.messages.iter().skip(1).count(), 1);
    assert!(result.messages[1]
        .content
        .text()
        .contains("Plugins reloaded"));
    assert_eq!(catalog(engine).await, initial);
    assert_eq!(history(engine).await, before);
    f.project(&json!({"plugins":{"alpha":{"config":{"value":"changed"}}}}));
    assert_eq!(command(engine, "/reload").await.error, None);
    let changed = catalog(engine).await;
    assert_ne!(
        descriptor(&changed, "alpha", "report")["generation"],
        descriptor(&initial, "alpha", "report")["generation"]
    );
    assert_eq!(
        descriptor(&changed, "beta", "value"),
        descriptor(&initial, "beta", "value")
    );
    assert_eq!(
        run_export(engine, "alpha", "report", json!({}))
            .await
            .unwrap()["config"]["value"],
        "changed"
    );
    assert!(request(
        engine,
        "plugin.invoke",
        payload(
            initial["session"].as_str().unwrap(),
            &descriptor(&initial, "alpha", "report"),
            json!({})
        )
    )
    .await
    .is_err());
    // A malformed replacement must preserve the entire previous host, including
    // the unchanged second plugin. Settings errors must not masquerade as disable.
    std::fs::write(f.cwd.join("alpha.lua"), "not Lua !").unwrap();
    assert!(command(engine, "/reload").await.error.is_some());
    assert_eq!(catalog(engine).await, changed);
    assert!(engine.plugin_diagnostic().is_some());
    for malformed in [Value::Null, json!([])] {
        f.project(&malformed);
        assert!(engine.reload_plugins().await.is_err());
        assert_eq!(catalog(engine).await, changed);
    }
    f.project(&json!({"plugins":null,"defaultProvider":42}));
    assert!(engine.reload_plugins().await.is_err());
    assert_eq!(catalog(engine).await, changed);
    f.project(&json!({"plugins":null}));
    assert!(engine.reload_plugins().await.is_err());
    assert_eq!(catalog(engine).await, changed);
    f.project(&json!({"plugins":{"alpha":{"config":null}}}));
    assert!(engine.reload_plugins().await.is_err());
    assert_eq!(catalog(engine).await, changed);
    f.project(&json!({"plugins":"malformed"}));
    assert!(engine.reload_plugins().await.is_err());
    assert_eq!(catalog(engine).await, changed);
    std::fs::write(f.cwd.join(".prime/agent/settings.json"), "{malformed").unwrap();
    assert!(engine.reload_plugins().await.is_err());
    assert_eq!(catalog(engine).await, changed);
    f.project(&json!({"plugins":{"alpha":{"enabled":false}}}));
    assert_eq!(command(engine, "/reload").await.error, None);
    let disabled = catalog(engine).await;
    assert_eq!(disabled["exports"].as_array().unwrap().len(), 1);
    assert!(!engine
        .session
        .session_command_inventory()
        .iter()
        .any(|entry| entry.name == "alpha_command"));
    assert!(engine
        .session
        .classify_session_command("/alpha_command")
        .is_none());
    assert_eq!(
        run_export(engine, "beta", "value", json!({}))
            .await
            .unwrap()["configured"],
        "second"
    );
    assert_eq!(history(engine).await, before);
    assert_eq!(engine.plugin_diagnostic(), None);
    // No manager operation deletes files or saved data.
    assert!(f.cwd.join("alpha.lua").is_file());
    assert!(f.agent.join("settings.json").is_file());
}

#[tokio::test]
async fn disable_cancels_active_kernel_work_and_fences_saved_command() {
    let f = Fixture::new(ALPHA).await;
    let engine = &f.engine;
    let before = history(engine).await;
    let old_command = engine
        .session
        .classify_session_command("/alpha_command old")
        .unwrap();
    let initial = catalog(engine).await;
    let handlers = engine.plugin_host_handlers();
    let mut data = payload(
        initial["session"].as_str().unwrap(),
        &descriptor(&initial, "alpha", "busy"),
        json!({}),
    );
    data["type"] = json!("plugin.invoke");
    let future = handlers.get("plugin.invoke").unwrap()(HostRequestPayload { data });
    let running = tokio::spawn(future);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if run_export(engine, "alpha", "report", json!({}))
                .await
                .unwrap()["started"]
                == true
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    f.project(&json!({"plugins":{"alpha":{"enabled":false}}}));
    engine.reload_plugins().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), running)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("retired")
    );
    assert!(engine
        .invoke_plugin_command(old_command.plugin.as_ref().unwrap(), &old_command.args)
        .await
        .is_err());
    assert!(request(
        engine,
        "plugin.invoke",
        payload(
            initial["session"].as_str().unwrap(),
            &descriptor(&initial, "alpha", "report"),
            json!({})
        )
    )
    .await
    .is_err());
    assert!(!engine
        .session
        .session_command_inventory()
        .iter()
        .any(|entry| entry.plugin.as_ref().is_some_and(|p| p.plugin == "alpha")));
    assert_eq!(history(engine).await, before);
}

#[tokio::test]
async fn malformed_activation_does_not_kill_normal_startup_and_sessions_are_private() {
    let bad = Fixture::new("not valid Lua !").await;
    assert!(bad.engine.plugin_diagnostic().is_some());
    assert_eq!(catalog(&bad.engine).await["exports"], json!([]));
    bad.provider.push_text_turn("normal operation");
    bad.engine
        .prompt("still usable", PromptOptions::default())
        .await
        .unwrap();
    let first = Fixture::new(ALPHA).await;
    let second = Fixture::new(ALPHA).await;
    assert_ne!(
        catalog(&first.engine).await["session"],
        catalog(&second.engine).await["session"]
    );
    first.project(&json!({"plugins":{"alpha":{"enabled":false}}}));
    first.engine.reload_plugins().await.unwrap();
    assert_eq!(
        catalog(&second.engine).await["exports"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    second.project(&json!({"plugins":{"beta":{"path":"missing.lua"}}}));
    let prior = catalog(&second.engine).await;
    assert!(second.engine.reload_plugins().await.is_err());
    assert_eq!(catalog(&second.engine).await, prior);
}

#[tokio::test]
async fn namespace_incompatible_replacement_preserves_native_inventory() {
    let f = Fixture::new(ALPHA).await;
    let engine = &f.engine;
    let before = catalog(engine).await;
    for name in [
        "refresh",
        "inventory",
        "session",
        "revision",
        "async",
        "await",
        "class",
        "return",
    ] {
        f.project(&json!({"plugins":{(name):{"enabled":true,"path":"beta.lua","config":{}}}}));
        assert!(
            engine.reload_plugins().await.is_err(),
            "accepted plugin name {name}"
        );
        assert_eq!(catalog(engine).await, before);
    }
    f.project(&json!({}));
    for name in ["return", "class", "await", "_private"] {
        std::fs::write(
            f.cwd.join("alpha.lua"),
            format!("return {{exports={{{{name='{name}',run=function() return true end}}}}}}"),
        )
        .unwrap();
        assert!(
            engine.reload_plugins().await.is_err(),
            "accepted export name {name}"
        );
        assert_eq!(catalog(engine).await, before);
    }
    // Python root-manager names are valid on a plugin view; soft keywords are
    // valid identifiers. Native command/hook names do not inherit Python bans.
    std::fs::write(f.cwd.join("alpha.lua"),"return {exports={{name='refresh',run=function() return true end},{name='match',run=function() return true end}},commands={{name='class',run=function() return true end}},hooks={{name='return',run=function() return true end}}}").unwrap();
    engine.reload_plugins().await.unwrap();
    assert_eq!(
        run_export(engine, "alpha", "refresh", json!({}))
            .await
            .unwrap(),
        json!(true)
    );
}

#[tokio::test]
async fn active_inventory_is_a_live_model_prompt_tail_not_canonical_history() {
    let f = Fixture::new(ALPHA).await;
    let engine = &f.engine;
    let base = engine.system_prompt.clone();
    let expected = engine.assembled_system_prompt();
    assert!(expected.starts_with(&base));
    assert!(expected.contains("plugin.alpha.report(**kwargs)"));
    assert!(expected.contains("plugin.beta.value(**kwargs)"));
    assert!(!expected.contains("plugin.alpha.stream(**kwargs)"));
    f.provider.push_text_turn("first");
    engine
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(
        f.provider.calls()[0].system_prompt.as_deref(),
        Some(expected.as_str())
    );
    let before = history(engine).await;
    f.project(&json!({"plugins":{"alpha":{"enabled":false},"beta":{"enabled":false}}}));
    engine.reload_plugins().await.unwrap();
    assert_eq!(history(engine).await, before);
    assert_eq!(engine.assembled_system_prompt(), base);
    f.provider.push_text_turn("second");
    engine
        .prompt("second", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(
        f.provider.calls()[1].system_prompt.as_deref(),
        Some(base.as_str())
    );
}

#[tokio::test]
async fn actual_kernel_preimports_refreshes_and_fences_plugin_callables() {
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
    use crate::kernel::shared::{ExecuteOptions, ExecuteStatus};
    let f = Fixture::new(ALPHA).await;
    let engine = &f.engine;
    let before = history(engine).await;
    let manager = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        engine.provisioner.ensure(None, None),
    )
    .await
    .unwrap()
    .unwrap();
    let persistence = engine.session.shared_persistence();
    let session_guard = persistence.lock().await;
    let first = tokio::time::timeout(std::time::Duration::from_secs(10),manager.execute(r#"
import json
saved = plugin.alpha.report
assert len(plugin.inventory) == 6
assert plugin.session == (await plugin.alpha.report())["identity"]["session"]
print(json.dumps({"alpha":await plugin.alpha.report(hello="kernel"),"beta":await plugin.beta.value()}))
"#,ExecuteOptions::default())).await.unwrap().unwrap();
    drop(session_guard);
    assert_eq!(first.status, ExecuteStatus::Ok, "{:?}", first.error);
    let output: Value = serde_json::from_str(first.stdout.trim()).unwrap();
    assert_eq!(output["alpha"]["input"], json!({"hello":"kernel"}));
    assert_eq!(output["alpha"]["config"]["value"], "project");
    assert_eq!(output["beta"]["configured"], "second");
    assert_eq!(
        output["alpha"]["identity"]["session"],
        engine.session.session_id().await
    );
    f.project(&json!({"plugins":{"alpha":{"config":{"value":"kernel-change"}}}}));
    engine.reload_plugins().await.unwrap();
    let changed = manager
        .execute(
            r#"
try:
    await saved()
except RuntimeError as error:
    assert "generation" in str(error)
else:
    raise AssertionError("saved callable silently retargeted")
assert (await plugin.alpha.report())["config"]["value"] == "kernel-change"
print("NEW_GENERATION_OK")
"#,
            ExecuteOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(changed.status, ExecuteStatus::Ok, "{:?}", changed.error);
    assert!(changed.stdout.contains("NEW_GENERATION_OK"));
    std::fs::write(
        f.cwd.join("alpha.lua"),
        "return {exports={{name='class',run=function() return true end}}}",
    )
    .unwrap();
    assert!(engine.reload_plugins().await.is_err());
    let preserved = manager
        .execute(
            "assert (await plugin.alpha.report())['config']['value'] == 'kernel-change'",
            ExecuteOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(preserved.status, ExecuteStatus::Ok, "{:?}", preserved.error);
    f.project(&json!({"plugins":{"alpha":{"enabled":false},"beta":{"enabled":false}}}));
    engine.reload_plugins().await.unwrap();
    std::fs::write(f.cwd.join("alpha.lua"), "return {exports={{name='refresh',run=function() return true end},{name='match',run=function() return true end},{name='case',run=function() return true end},{name='type',run=function() return true end}}}").unwrap();
    f.project(&json!({}));
    engine.reload_plugins().await.unwrap();
    let accepted = manager.execute("assert await plugin.alpha.refresh() is True\nassert await plugin.alpha.match() is True\nassert await plugin.alpha.case() is True\nassert await plugin.alpha.type() is True",ExecuteOptions::default()).await.unwrap();
    assert_eq!(accepted.status, ExecuteStatus::Ok, "{:?}", accepted.error);
    f.project(&json!({"plugins":{"alpha":{"enabled":false},"beta":{"enabled":false}}}));
    engine.reload_plugins().await.unwrap();
    let disabled = manager
        .execute(
            r"
assert plugin.inventory == ()
assert not hasattr(plugin, 'alpha')
assert not hasattr(plugin, 'beta')
try:
    await saved()
except RuntimeError:
    pass
else:
    raise AssertionError('disabled saved callable still works')
print('DISABLED_OK')
",
            ExecuteOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(disabled.status, ExecuteStatus::Ok, "{:?}", disabled.error);
    assert!(disabled.stdout.contains("DISABLED_OK"));
    assert_eq!(history(engine).await, before);
    engine.provisioner.kill();
    }).await.expect("real plugin kernel scenario timed out");
}
