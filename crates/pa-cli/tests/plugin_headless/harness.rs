//! First-party policy through the real CLI, without a Python kernel or external inference.

use std::net::TcpListener;
use std::path::PathBuf;

use pa_core::session::manager::SessionManager;
use serde_json::{json, Value};

use super::{stdout, success, Fixture};

fn persisted(fixture: &Fixture) -> (PathBuf, PathBuf, String) {
    let mut session = SessionManager::persisted(&fixture.cwd, &fixture.runtime.join("sessions"));
    session.append_model_change("faux", "faux-1").unwrap();
    session.flush_now().unwrap();
    (
        session.get_session_file().unwrap().to_path_buf(),
        session.get_session_artifact_dir().unwrap(),
        session.get_session_id().to_string(),
    )
}

#[test]
fn persisted_harness_context_is_model_only_and_disappears_when_disabled() {
    let fixture = Fixture::new();
    fixture.settings(&json!({"plugins":{"fixture":{"enabled":false},"harness":{
        "enabled":true,"config":{"autoRefine":{"enabled":false}}
    }}}));
    let (path, artifacts, id) = persisted(&fixture);
    let legacy = json!({"schema":1,"entries":{"memory":{"fixture":{
        "id":"fixture","kind":"memory","content":"learned context","unknown":null
    }}},"refinements":[]})
    .to_string();
    let imported_path = artifacts.join("harness/harness_state.json");
    std::fs::create_dir_all(imported_path.parent().unwrap()).unwrap();
    std::fs::write(&imported_path, &legacy).unwrap();
    let output = fixture.run(
        &[
            "--resume",
            path.to_str().unwrap(),
            "--mode",
            "json",
            "-p",
            "inspect",
        ],
        &json!({"responses":[{"systemPrompt":true}]}),
    );
    success(&output);
    let events: Vec<Value> = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let response = events
        .iter()
        .find(|event| event["type"] == "message_end" && event["message"]["role"] == "assistant")
        .unwrap()["message"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(response.contains("Optional continual harness:"));
    assert!(response.contains(&format!("[session:{id}:memory:fixture] learned context")));
    assert!(!events
        .iter()
        .any(|event| event["message"]["customType"] == "harness_digest"));
    let entries = fixture.entries();
    assert!(!entries
        .iter()
        .any(|entry| entry["customType"] == "harness_digest"
            || entry["message"]["customType"] == "harness_digest"));
    assert_eq!(std::fs::read_to_string(&imported_path).unwrap(), legacy);
    fixture.settings(&json!({"plugins":{"fixture":{"enabled":false},"harness":{"enabled":false}}}));
    let disabled = fixture.run(
        &["--resume", path.to_str().unwrap(), "-p", "inspect disabled"],
        &json!({"responses":[{"systemPrompt":true}]}),
    );
    success(&disabled);
    let response = stdout(&disabled);
    assert!(!response.contains("Optional continual harness:"));
    assert!(!response.contains("learned context"));
    assert!(!response.contains("plugin.harness."));
    assert_eq!(std::fs::read_to_string(imported_path).unwrap(), legacy);
}

const AUXILIARY: &str = r#"
local phase=0
return {providers={{name='aux',mode='streaming',run=function(input,ctx)
 assert(input.model.id=='selected', 'wrong auxiliary model')
 local review=string.find(input.context.system_prompt,'Review evidence',1,true)~=nil
 local text
 if review then
   assert(phase==0, 'review must be the first auxiliary call')
   phase=1
   text='{"shouldRefine":true,"rationale":"fixture command supplies reusable evidence"}'
 else
   assert(phase==1, 'plan must follow one approving review')
   assert(string.find(input.context.system_prompt,'Propose only',1,true)~=nil, 'unexpected auxiliary prompt')
   phase=2
   text='{"summary":"CLI learned","edits":[{"action":"create","kind":"memory","id":"learned","value":{"content":"post-exit learned memory"}}]}'
 end
 local message={role='assistant',content={{type='text',text=text}},api=input.model.api,provider=input.model.provider,
   model=input.model.id,usage={input=0,output=0,cacheRead=0,cacheWrite=0,totalTokens=0,
   cost={input=0,output=0,cacheRead=0,cacheWrite=0,total=0}},stopReason='stop',timestamp=0}
 ctx.emit({type='start',partial=message})
 ctx.emit({type='done',reason='stop',message=message})
 return message
end}}}
"#;

fn auxiliary(fixture: &Fixture, source: &str) -> TcpListener {
    let network = TcpListener::bind("127.0.0.1:0").unwrap();
    network.set_nonblocking(true).unwrap();
    std::fs::write(fixture.cwd.join("aux.lua"), source).unwrap();
    std::fs::write(
        fixture.agent.join("models.json"),
        json!({"providers":{"aux":{
            "api":"openai-completions","apiKey":"none",
            "baseUrl":format!("http://{}/v1",network.local_addr().unwrap()),
            "models":[{"id":"selected","name":"Local auxiliary"}]
        }}})
        .to_string(),
    )
    .unwrap();
    fixture.settings(&json!({"plugins":{"fixture":{"enabled":false},
    "route":{"enabled":true,"path":"aux.lua"},"harness":{"enabled":true,"config":{
        "auxiliaryModel":"aux/selected","autoRefine":{"enabled":false}
    }}}}));
    network
}

#[test]
fn first_party_refine_drains_its_admitted_job_before_cli_exit() {
    let fixture = Fixture::new();
    let network = auxiliary(&fixture, AUXILIARY);
    let (path, _artifacts, id) = persisted(&fixture);
    // An empty main-model response queue rejects any accidental user/model turn for /refine.
    let refined = fixture.run(
        &[
            "--resume",
            path.to_str().unwrap(),
            "--mode",
            "json",
            "-p",
            "/refine capture fixture evidence",
        ],
        &json!({"responses":[]}),
    );
    success(&refined);
    let outcomes: Vec<Value> = stdout(&refined)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let job = &outcomes
        .iter()
        .find(|event| event["type"] == "plugin_job_update")
        .expect("the command publishes its terminal generic job outcome")["job"];
    assert_eq!(job["state"]["status"], "completed", "{outcomes:#?}");
    assert_eq!(
        job["state"]["result"]["summary"], "CLI learned",
        "{outcomes:#?}"
    );
    assert_eq!(
        job["state"]["result"]["applied"].as_array().map(Vec::len),
        Some(1),
        "{outcomes:#?}"
    );
    let store = fixture.agent.join("plugins/harness/store.json");
    let saved = std::fs::read_to_string(store).unwrap();
    assert!(
        saved.contains("post-exit learned memory"),
        "store={saved}; command stdout={}; stderr={}",
        stdout(&refined),
        String::from_utf8_lossy(&refined.stderr)
    );
    let probe = fixture.run(
        &[
            "--resume",
            path.to_str().unwrap(),
            "-p",
            "inspect saved record",
        ],
        &json!({"responses":[{"systemPrompt":true}]}),
    );
    success(&probe);
    assert!(stdout(&probe).contains(&format!(
        "[session:{id}:memory:learned] post-exit learned memory"
    )));
    assert_eq!(
        network.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "the configured Lua auxiliary route, not HTTP inference, must serve refinement"
    );
    let entries = fixture.entries();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry["type"] == "message" && entry["message"]["role"] == "user")
            .count(),
        1,
        "only the later inspection is a user turn"
    );
}

const FAILING_AUXILIARY: &str = r"
return {providers={{name='aux',mode='streaming',run=function()
 error('intentional auxiliary failure')
end}}}
";

#[test]
fn a_failed_new_manual_plugin_job_rejects_the_command_wait() {
    let fixture = Fixture::new();
    let network = auxiliary(&fixture, FAILING_AUXILIARY);
    let (path, _artifacts, _id) = persisted(&fixture);
    let failed = fixture.run(
        &[
            "--resume",
            path.to_str().unwrap(),
            "-p",
            "/refine fail the local fixture",
        ],
        &json!({"responses":[]}),
    );
    assert_eq!(failed.status.code(), Some(1));
    assert!(stdout(&failed).is_empty());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("intentional auxiliary failure"));
    assert!(!fixture
        .entries()
        .iter()
        .any(|entry| entry["type"] == "message"));
    assert_eq!(
        network.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn earlier_automatic_job_failure_does_not_poison_a_later_command() {
    let fixture = Fixture::new();
    let network = auxiliary(&fixture, FAILING_AUXILIARY);
    fixture.settings(&json!({"plugins":{"fixture":{"enabled":false},
        "route":{"enabled":true,"path":"aux.lua"},"harness":{"enabled":true,"config":{
            "auxiliaryModel":"aux/selected","autoRefine":{"enabled":true,"turnInterval":1,"cooldownMs":1}
        }}}}));
    let output = fixture.run(
        &["-p", "ordinary user turn", "/goal status"],
        &json!({"responses":["ordinary answer"]}),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout(&output), "No active goal.\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Plugin job diagnostic:"));
    assert!(stderr.contains("intentional auxiliary failure"));
    assert_eq!(
        network.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
