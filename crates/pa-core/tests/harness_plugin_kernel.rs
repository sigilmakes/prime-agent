//! Actual engine -> Python keyword export -> Lua -> single native storage verifier.
#![cfg(unix)]
use pa_agent::scripted::ScriptedProvider;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use serde_json::{json, Value};
use std::sync::Arc;
#[tokio::test]
async fn harness_kwargs_round_trip_through_real_engine_kernel_without_python_disk_writer() {
    let Some(python) = std::env::var_os("PA_CORE_KERNEL_PYTHON") else {
        eprintln!("PA_CORE_KERNEL_PYTHON unset; skipping explicit-kernel verifier");
        return;
    };
    let previous = std::env::var_os("PRIME_AGENT_KERNEL_PYTHON");
    std::env::set_var("PRIME_AGENT_KERNEL_PYTHON", &python);
    let _restore = RestorePython(previous);
    assert!(std::path::Path::new(&python).exists());
    let private = tempfile::tempdir().unwrap();
    let cwd = private.path().join("project");
    let agent = private.path().join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(agent.join("harness")).unwrap();
    let legacy =
        br#"{"schema":1,"entries":{"memory":{"old":{"content":"legacy"}}},"refinements":[]}"#;
    std::fs::write(agent.join("harness/harness_state.json"), legacy).unwrap();
    std::fs::write(agent.join("settings.json"),json!({"enableBuiltinSkills":false,"bundledSkills":{"websearch":false},"autoRefine":{"enabled":false},"plugins":{"harness":{"enabled":true}}}).to_string()).unwrap();
    let receipt = private.path().join("receipt.json");
    let code = format!(
        r"import json, rlm
assert not hasattr(rlm, 'harness') and not hasattr(rlm.rlm, 'harness')
created = await plugin.harness.create(kind='memory', id='kernel', value={{'content':'native kwargs','unknown':None}})
row = await plugin.harness.get(kind='memory', id='kernel')
assert row['value'] == {{'content':'native kwargs','unknown':None}}
assert await plugin.harness.list(scope='session:fresh') == []
open({:?},'w').write(json.dumps(row))
print('NATIVE_HARNESS_KERNEL_OK')",
        receipt.display().to_string()
    );
    let model:pa_agent::types::Model=serde_json::from_value(json!({"id":"fixture","name":"Fixture","api":"test","provider":"faux","baseUrl":"http://127.0.0.1:9","reasoning":false,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":10000,"maxTokens":100})).unwrap();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(None, vec![("cell", "python_repl", json!({"code":code}))]);
    provider.push_text_turn("done");
    let engine = Box::pin(create_session(SessionEngineConfig {
        cwd,
        agent_dir: agent.clone(),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        daemonless: true,
        prewarm_ipython_kernel: Some(false),
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        engine.prompt(
            "exercise kwargs",
            pa_core::session_engine::PromptOptions::default(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let row: Value = serde_json::from_slice(&std::fs::read(&receipt).unwrap_or_else(|error| {
        panic!(
            "kernel receipt missing: {error}; history={:?}",
            provider.calls()
        )
    }))
    .unwrap();
    assert_eq!(
        row["value"],
        json!({"content":"native kwargs","unknown":null})
    );
    assert_eq!(
        std::fs::read(agent.join("harness/harness_state.json")).unwrap(),
        legacy
    );
    assert!(agent.join("plugins/harness/store.json").exists());
}

struct RestorePython(Option<std::ffi::OsString>);
impl Drop for RestorePython {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            std::env::set_var("PRIME_AGENT_KERNEL_PYTHON", previous);
        } else {
            std::env::remove_var("PRIME_AGENT_KERNEL_PYTHON");
        }
    }
}
