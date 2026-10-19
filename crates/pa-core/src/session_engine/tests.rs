use super::*;
use pa_agent::agent::{AgentInitialState, AgentOptions};
use pa_agent::scripted::ScriptedProvider;

fn test_model() -> pa_agent::types::Model {
    serde_json::from_value(serde_json::json!({
        "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .unwrap()
}

async fn scripted_session() -> AgentSession {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn("hello from the model");
    let options = AgentOptions {
        initial_state: AgentInitialState {
            model: Some(test_model()),
            ..Default::default()
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    };
    let agent = Agent::new(options);
    let tmp = tempfile::tempdir().unwrap();
    let session = SessionManager::in_memory(tmp.path());
    AgentSession::new(Arc::new(agent), session, vec![])
        .await
        .unwrap()
}

#[tokio::test]
async fn prompt_persists_user_and_assistant() {
    let session = scripted_session().await;
    session
        .prompt("hi there", PromptOptions::default())
        .await
        .unwrap();
    session.agent().wait_for_idle().await;
    let entries = session.entries().await;
    let roles: Vec<String> = entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Message {
                message: SessionAgentMessage::User(user),
                ..
            } => Some(format!("user:{}", user.content.text())),
            FileEntry::Message {
                message: SessionAgentMessage::Assistant(assistant),
                ..
            } => Some(format!("assistant:{}", assistant.model)),
            _ => None,
        })
        .collect();
    assert_eq!(
        roles,
        vec!["user:hi there".to_string(), "assistant:m".to_string()]
    );
}

#[tokio::test]
async fn a_skill_command_prompt_expands_into_the_skill_block() {
    // TS `_expandSkillCommand`: a `/skill:<name> [args]` submission
    // persists as the `<skill>` block plus the argument text; the
    // renderer parses that block back out.
    let mut session = scripted_session().await;
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("SKILL.md");
    std::fs::write(&file_path, "---\nname: web-search\n---\nRun a web search.").unwrap();
    session.set_skills(vec![crate::skills::Skill {
        name: "web-search".to_string(),
        description: "search the web".to_string(),
        file_path: file_path.clone(),
        base_dir: dir.path().to_path_buf(),
        source_info: crate::skills::create_synthetic_source_info(
            &file_path.display().to_string(),
            "user",
            crate::skills::SourceScope::User,
            None,
        ),
        disable_model_invocation: false,
        kind: crate::skills::SkillKind::Markdown,
        python: None,
    }]);
    session
        .prompt("/skill:web-search find rust tuis", PromptOptions::default())
        .await
        .unwrap();
    session.agent().wait_for_idle().await;
    let entries = session.entries().await;
    let user_text = entries
        .iter()
        .find_map(|entry| match entry {
            FileEntry::Message {
                message: SessionAgentMessage::User(user),
                ..
            } => Some(user.content.text()),
            _ => None,
        })
        .expect("user message persisted");
    let parsed = pa_types::skill_blocks::parse_skill_block(&user_text)
        .expect("the persisted user message is a skill block");
    assert_eq!(parsed.name, "web-search");
    assert_eq!(
        parsed.user_message.as_deref(),
        Some("find rust tuis"),
        "args persist as the trailing user message"
    );
    assert!(parsed.content.contains("Run a web search."));
}

#[tokio::test]
async fn an_unknown_skill_command_passes_through() {
    let session = scripted_session().await;
    session
        .prompt("/skill:missing do a thing", PromptOptions::default())
        .await
        .unwrap();
    session.agent().wait_for_idle().await;
    let entries = session.entries().await;
    let user_text = entries
        .iter()
        .find_map(|entry| match entry {
            FileEntry::Message {
                message: SessionAgentMessage::User(user),
                ..
            } => Some(user.content.text()),
            _ => None,
        })
        .expect("user message persisted");
    assert_eq!(user_text, "/skill:missing do a thing");
}

/// A scripted session wired with the production model-boundary conversion.
async fn provider_session(provider: Arc<ScriptedProvider>) -> (AgentSession, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let agent = Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            model: Some(test_model()),
            ..Default::default()
        },
        convert_to_llm: Some(crate::session_engine::refinement_gate::convert_to_llm()),
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    });
    let session = AgentSession::from_session_arc(
        Arc::new(agent),
        Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
            tmp.path(),
        ))),
        vec![],
    )
    .await
    .unwrap();
    (session, tmp)
}

fn user_text(message: &pa_agent::types::Message) -> String {
    let pa_agent::types::Message::User(user) = message else {
        panic!("expected user message");
    };
    match &user.content {
        pa_agent::types::UserContent::Text(text) => text.clone(),
        pa_agent::types::UserContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                pa_agent::types::UserPart::Text(text) => Some(text.text.clone()),
                pa_agent::types::UserPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// The transcript and the loop context hold ONE representation of the
/// turn (the custom row, appended once), never a duplicate user
/// message.
#[tokio::test]
async fn prompt_injected_message_persists_one_custom_row() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn("notice acknowledged");
    let (session, _tmp) = provider_session(Arc::clone(&provider)).await;
    let notice_text = "[child-exited: no-reply child:lane]";
    let notice = pa_types::session::CustomMessage {
        custom_type: "rlm_child_terminal_notice".to_string(),
        content: pa_types::ai::UserContent::Text(notice_text.to_string()),
        display: true,
        details: Some(serde_json::json!({
            "kind": "completed_without_reply",
            "childId": "sub-1",
            "sessionName": "lane",
        })),
        timestamp: 0,
        rest: serde_json::Map::default(),
    };
    session.prompt_injected_message(&notice).await.unwrap();
    session.agent().wait_for_idle().await;

    // The provider request carries the notice text as its user-role
    // view — once, with no duplicate user message.
    let calls = provider.calls();
    assert_eq!(calls.len(), 1);
    let user_texts: Vec<String> = calls[0]
        .messages
        .iter()
        .filter(|message| matches!(message, pa_agent::types::Message::User(_)))
        .map(user_text)
        .collect();
    assert_eq!(user_texts, vec![notice_text.to_string()]);
    assert_eq!(
        user_texts
            .iter()
            .filter(|text| *text == notice_text)
            .count(),
        1,
        "no duplicate user message: {calls:?}"
    );

    // One representation in the transcript: the custom row, exactly
    // once, and no user row with the same text.
    let entries = session.entries().await;
    let notice_rows = entries
        .iter()
        .filter(|entry| match entry {
            FileEntry::CustomMessage { payload, .. } => {
                payload.custom_type == "rlm_child_terminal_notice"
            }
            _ => false,
        })
        .count();
    assert_eq!(notice_rows, 1);
    let user_rows = entries
        .iter()
        .filter(|entry| match entry {
            FileEntry::Message {
                message: SessionAgentMessage::User(user),
                ..
            } => user.content.text().contains(notice_text),
            _ => false,
        })
        .count();
    assert_eq!(
        user_rows, 0,
        "the injected turn must not persist a user row"
    );
}

/// The comparison covers the whole request - system prompt, tools, and
/// every message row - with only the per-run timestamps normalized.
#[tokio::test]
async fn an_agent_message_custom_row_matches_the_plain_prompt_context() {
    let prompt = "[agent-message from child:research-lane]\n\nthe research is done";

    let plain_provider = Arc::new(ScriptedProvider::new(test_model()));
    plain_provider.push_text_turn("ack");
    let (plain_session, _plain_tmp) = provider_session(Arc::clone(&plain_provider)).await;
    plain_session
        .prompt(prompt, PromptOptions::default())
        .await
        .unwrap();
    plain_session.agent().wait_for_idle().await;

    // The delivered shape: the `agent_message` custom row whose content
    // is the same prompt (TS `createAgentSessionMessage`).
    let row_provider = Arc::new(ScriptedProvider::new(test_model()));
    row_provider.push_text_turn("ack");
    let (row_session, _row_tmp) = provider_session(Arc::clone(&row_provider)).await;
    let row = pa_types::session::CustomMessage {
        custom_type: crate::session_engine::agent_messaging::AGENT_MESSAGE_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(prompt.to_string()),
        display: true,
        details: Some(serde_json::json!({
            "id": "agentmsg_golden",
            "message": "the research is done",
            "from": {
                "activeSessionId": "child-1",
                "sessionName": "research-lane",
            },
            "fromRelationship": "child",
            "target": { "activeSessionId": "parent-1" },
        })),
        timestamp: 0,
        rest: serde_json::Map::default(),
    };
    row_session.prompt_injected_message(&row).await.unwrap();
    row_session.agent().wait_for_idle().await;

    let plain_calls = plain_provider.calls();
    let row_calls = row_provider.calls();
    assert_eq!(plain_calls.len(), 1);
    assert_eq!(row_calls.len(), 1);
    assert_eq!(
        normalized_context(&plain_calls[0]),
        normalized_context(&row_calls[0]),
        "the agent_message row must not change the provider request"
    );
}

/// The provider request with the per-run timestamps zeroed (every
/// other byte is compared).
fn normalized_context(context: &pa_agent::stream::LlmContext) -> serde_json::Value {
    let mut value = serde_json::to_value(context).unwrap();
    for message in value["messages"].as_array_mut().unwrap() {
        if let Some(timestamp) = message.get_mut("timestamp") {
            *timestamp = serde_json::json!(0);
        }
    }
    value
}

#[tokio::test]
async fn prompt_persists_tool_results() {
    struct EchoTool;
    impl pa_agent::types::AgentTool for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "echo the call"
        }
        fn parameters(&self) -> &serde_json::Value {
            static PARAMETERS: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            PARAMETERS.get_or_init(|| serde_json::json!({ "type": "object" }))
        }
        fn execute(
            self: Arc<Self>,
            _tool_call_id: String,
            _params: serde_json::Value,
            _signal: pa_agent::abort::AbortSignal,
            _on_update: pa_agent::types::AgentToolUpdateCallback,
        ) -> pa_agent::BoxFut<'static, anyhow::Result<pa_agent::types::AgentToolResult>> {
            Box::pin(async { Ok(pa_agent::types::AgentToolResult::text("tool output")) })
        }
    }
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_tool_call_turn(
        Some("calling"),
        vec![("call-1", "echo", serde_json::json!({}))],
    );
    provider.push_text_turn("done");
    let options = AgentOptions {
        initial_state: AgentInitialState {
            model: Some(test_model()),
            ..Default::default()
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    };
    let agent = Agent::new(options);
    agent.set_tools(vec![Arc::new(EchoTool)]).await;
    let tmp = tempfile::tempdir().unwrap();
    let session = SessionManager::persisted(std::path::Path::new("/w"), tmp.path());
    let engine = AgentSession::new(Arc::new(agent), session, vec![])
        .await
        .unwrap();
    engine.prompt("hi", PromptOptions::default()).await.unwrap();
    engine.agent().wait_for_idle().await;
    let entries = engine.entries().await;
    let roles: Vec<&str> = entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Message { message, .. } => Some(match message {
                SessionAgentMessage::User(_) => "user",
                SessionAgentMessage::Assistant(_) => "assistant",
                SessionAgentMessage::ToolResult(_) => "toolResult",
                _ => "other",
            }),
            _ => None,
        })
        .collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "toolResult", "assistant"],
        "entries: {entries:?}"
    );
    let tool_result = entries
        .iter()
        .find_map(|entry| match entry {
            FileEntry::Message {
                message: SessionAgentMessage::ToolResult(result),
                ..
            } => Some(result.clone()),
            _ => None,
        })
        .expect("toolResult entry persisted");
    // Whole-object compare through the TS wire shape (timestamp is
    // turn-dependent and asserted only by type).
    let value = serde_json::to_value(SessionAgentMessage::ToolResult(tool_result)).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "role": "toolResult",
            "toolCallId": "call-1",
            "toolName": "echo",
            "content": [{ "type": "text", "text": "tool output" }],
            "isError": false,
            "timestamp": value["timestamp"],
        })
    );
    // The persisted file line carries the live-TS entry envelope: the
    // message under `message`, chained to its assistant parent.
    let file = tmp
        .path()
        .join(format!("{}.jsonl", engine.session_id().await))
        .to_string_lossy()
        .to_string();
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let entry = lines
        .iter()
        .find(|entry| {
            entry.get("message").and_then(|m| m.get("role"))
                == Some(&serde_json::json!("toolResult"))
        })
        .expect("toolResult entry on disk");
    assert_eq!(entry["type"], "message");
    assert_eq!(entry["message"]["toolCallId"], "call-1");
    assert_eq!(entry["message"]["content"][0]["text"], "tool output");
    assert_eq!(entry["message"]["isError"], false);
    assert!(entry["id"].as_str().is_some_and(|id| id.len() == 8));
    assert!(entry["parentId"].as_str().is_some());
    assert!(entry["timestamp"].as_str().is_some());
}

/// Persisted `ipython`/`python` calls dispatch through `ToolDefinitionBridge`
/// to the registered Python tool.
#[tokio::test]
async fn python_tool_aliases_dispatch_to_python_repl() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let definition = crate::tools::tool_definition::ToolDefinition {
        name: pa_types::ai::PYTHON_TOOL_NAME.to_string(),
        label: "Python".to_string(),
        description: "Echoes its input".to_string(),
        prompt_snippet: String::new(),
        parameters: serde_json::json!({ "type": "object" }),
        execution_mode: None,
        prepare_arguments: None,
        execute: {
            let calls = Arc::clone(&calls);
            Arc::new(move |_id, params, _signal, _on_update| {
                let text = params
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    Ok(crate::tools::tool_definition::ToolExecutionResult::text(
                        format!("echo:{text}"),
                    ))
                })
            })
        },
    };
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_tool_call_turn(
        Some("calling"),
        vec![
            ("call-1", "ipython", serde_json::json!({ "text": "a" })),
            ("call-2", "python", serde_json::json!({ "text": "b" })),
        ],
    );
    provider.push_text_turn("done");
    let options = AgentOptions {
        initial_state: AgentInitialState {
            model: Some(test_model()),
            ..Default::default()
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    };
    let agent = Agent::new(options);
    agent
        .set_tools(vec![crate::session_engine::tool_bridge::bridge_tool(
            definition,
        )])
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let session = SessionManager::in_memory(tmp.path());
    let engine = AgentSession::new(Arc::new(agent), session, vec![])
        .await
        .unwrap();
    engine.prompt("go", PromptOptions::default()).await.unwrap();
    engine.agent().wait_for_idle().await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let entries = engine.entries().await;
    let texts: Vec<&str> = entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Message {
                message: SessionAgentMessage::ToolResult(result),
                ..
            } => Some(result),
            _ => None,
        })
        .map(|result| {
            assert!(!result.is_error, "result: {result:?}");
            match &result.content[..] {
                [pa_types::ai::UserContentBlock::Text(text)] => text.text.as_str(),
                _ => panic!("expected a single text block: {result:?}"),
            }
        })
        .collect();
    assert_eq!(texts, ["echo:a", "echo:b"]);
}

/// A `toolResult` entry captured from a live TS session (read-only, from
/// the installed product's own session store) parses into the Rust
/// session types and re-serializes to the identical wire shape.
#[test]
fn ts_toolresult_entry_round_trips() {
    let golden: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/golden/corpus/toolresult-entry-live-ts.json"
    ))
    .unwrap();
    let entry: FileEntry = serde_json::from_value(golden.clone()).unwrap();
    let FileEntry::Message {
        message: SessionAgentMessage::ToolResult(tool_result),
        base,
    } = &entry
    else {
        panic!("golden entry is not a toolResult message: {entry:?}");
    };
    assert_eq!(tool_result.tool_name, "ipython");
    assert_eq!(
        tool_result.tool_call_id,
        "c2425715-419e-4d06-a101-a78da1969b96"
    );
    assert!(!tool_result.is_error);
    assert_eq!(base.id.clone().unwrap_or_default().len(), 8);
    assert_eq!(base.parent_id.as_deref(), Some("8902561b"));
    assert_eq!(
        tool_result.details,
        Some(serde_json::json!({
            "durationMs": 10,
            "status": "ok",
            "stdout": "/root/prime-agent-rs\n['.git', 'MISSION.md', 'README.md', 'WATCHDOG.md']\nTrue\n",
            "stderr": "",
            "kernelRestarted": false
        }))
    );
    let serialized = serde_json::to_value(&entry).unwrap();
    assert_eq!(serialized, golden);
}

#[tokio::test]
async fn template_expansion_applies() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn("ok");
    let options = AgentOptions {
        initial_state: AgentInitialState {
            model: Some(test_model()),
            ..Default::default()
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    };
    let agent = Agent::new(options);
    let tmp = tempfile::tempdir().unwrap();
    let session = SessionManager::in_memory(tmp.path());
    let template = PromptTemplate {
        name: "fix".to_string(),
        description: "fix".to_string(),
        argument_hint: None,
        content: "Fix $1 please".to_string(),
        source_info: crate::skills::create_synthetic_source_info(
            "/p",
            "local",
            crate::skills::SourceScope::User,
            None,
        ),
        file_path: "/p/fix.md".to_string(),
    };
    let engine = AgentSession::new(Arc::new(agent), session, vec![template])
        .await
        .unwrap();
    engine
        .prompt("/fix lint", PromptOptions::default())
        .await
        .unwrap();
    engine.agent().wait_for_idle().await;
    let entries = engine.entries().await;
    let user_text = entries
        .iter()
        .find_map(|entry| match entry {
            FileEntry::Message {
                message: SessionAgentMessage::User(user),
                ..
            } => Some(user.content.text()),
            _ => None,
        })
        .unwrap();
    assert_eq!(user_text, "Fix lint please");
}

/// Git state is captured at both run boundaries (the TS run-boundary event
/// path calls `recordGitStateIfChanged` on `agent_start`/`agent_end`): a commit
/// made between session creation and the run lands as a `git_state` entry, a
/// branch switch between turns is recorded by the next run, and an unchanged
/// context at `agent_end` adds nothing.
#[tokio::test]
async fn run_boundaries_record_git_state() {
    fn git(cwd: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git is available in the test environment");
        assert!(output.status.success(), "git {args:?} failed");
    }
    fn commit(dir: &std::path::Path, message: &str) -> String {
        std::fs::write(dir.join("file.txt"), format!("{message}\n")).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", message]);
        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    let repo = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@t.co"]);
    git(repo.path(), &["config", "user.name", "t"]);
    commit(repo.path(), "init");

    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn("ok");
    let options = AgentOptions {
        initial_state: AgentInitialState {
            model: Some(test_model()),
            ..Default::default()
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    };
    let agent = Agent::new(options);
    let session = SessionManager::persisted(repo.path(), sessions.path());
    let engine = AgentSession::new(Arc::new(agent), session, vec![])
        .await
        .unwrap();

    let second_sha = commit(repo.path(), "second");
    engine.prompt("hi", PromptOptions::default()).await.unwrap();
    engine.agent().wait_for_idle().await;

    git(repo.path(), &["checkout", "-q", "-b", "feature"]);
    provider.push_text_turn("ok");
    engine
        .prompt("again", PromptOptions::default())
        .await
        .unwrap();
    engine.agent().wait_for_idle().await;

    let entries = engine.entries().await;
    let git_states: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::GitState { payload, .. } => Some(payload.git.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        git_states,
        vec![
            pa_types::session::GitContext {
                repo_url: None,
                commit: Some(second_sha.clone()),
                branch: Some("main".to_string()),
                rest: pa_types::JsonMap::new(),
            },
            pa_types::session::GitContext {
                repo_url: None,
                commit: Some(second_sha),
                branch: Some("feature".to_string()),
                rest: pa_types::JsonMap::new(),
            },
        ]
    );
}
