//! First-party policy through the real CLI, without a Python kernel or external inference.

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
    fixture.settings(&json!({"plugins":{"fixture":{"enabled":false},"harness":{"enabled":true}}}));
    let (path, artifacts, id) = persisted(&fixture);
    let legacy = json!({"schema":1,"entries":{"memory":{"fixture":{
        "id":"fixture","kind":"memory","content":"learned context","unknown":null
    }}}})
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
    assert!(response.contains("Optional continual harness routing snippets (not full directives)."));
    assert!(response.contains(&format!(
        r#"[scope="session:{id}", kind=memory, id="fixture"] : learned context"#
    )));
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
    assert!(
        !response.contains("Optional continual harness routing snippets (not full directives).")
    );
    assert!(!response.contains("learned context"));
    assert!(!response.contains("plugin.harness."));
    assert_eq!(std::fs::read_to_string(imported_path).unwrap(), legacy);
}
