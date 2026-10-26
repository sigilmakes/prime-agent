//! Actual engine/model boundary coverage with owner-certified history and no network.
use super::*;
use crate::session_engine::engine::{create_session, SessionEngine, SessionEngineConfig};
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback, UserContent, UserPart};

struct ProbeTool(serde_json::Value);
impl AgentTool for ProbeTool {
    fn name(&self) -> &'static str {
        "archive_probe"
    }
    fn description(&self) -> &'static str {
        "Return a local observation."
    }
    fn parameters(&self) -> &serde_json::Value {
        &self.0
    }
    fn execute(
        self: Arc<Self>,
        _id: String,
        _params: serde_json::Value,
        _signal: pa_agent::abort::AbortSignal,
        _update: AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<AgentToolResult>> {
        Box::pin(async { Ok(AgentToolResult::text("local observation")) })
    }
}

async fn engine(
    owner: Arc<Owner>,
    dir: &std::path::Path,
) -> (
    SessionEngine,
    Arc<pa_agent::scripted::ScriptedProvider>,
    pa_types::ai::Model,
) {
    let manager = manager(&owner);
    CanonicalLiveSession::new(owner.clone(), manager.clone())
        .observe()
        .await
        .unwrap();
    let manager = Arc::try_unwrap(manager).ok().unwrap().into_inner();
    let model_info: pa_types::ai::Model = serde_json::from_value(serde_json::json!({
        "id":"gpt-5","name":"test","api":"openai-completions","provider":"test",
        "baseUrl":"http://127.0.0.1:9/v1","reasoning":false,"input":["text","image"],
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":100_000,"maxTokens":100
    })).unwrap();
    let wire = crate::session_engine::provider_adapter::json_round_trip(&model_info).unwrap();
    let provider = Arc::new(pa_agent::scripted::ScriptedProvider::new(wire));
    let engine = create_session(SessionEngineConfig {
        cwd: dir.join("project"),
        agent_dir: dir.join("agent"),
        model: Some(crate::session_engine::provider_adapter::json_round_trip(&model_info).unwrap()),
        model_info: Some(model_info.clone()),
        session_manager: Some(manager),
        canonical_live_owner: Some(owner),
        // The fixture's real selection row already exists in the certified source.
        on_initial_context_selection: Some(Arc::new(|_| Box::pin(async { Ok(()) }))),
        stream_fn: Some(provider.stream_fn()),
        tools: vec![Arc::new(ProbeTool(
            serde_json::json!({"type":"object","properties":{}}),
        ))],
        prewarm_ipython_kernel: Some(false),
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap();
    (engine, provider, model_info)
}

fn deliver(owner: &Owner, text: &str) {
    let id = owner.seed(serde_json::json!({"type":"message","message":{
        "role":"user","content":[{"type":"text","text":text}],"timestamp":7
    }}));
    owner.0.lock().unwrap().delivered_users.push_back(id);
}

async fn side_question(session: &SessionEngine, provider: &pa_agent::scripted::ScriptedProvider) {
    use crate::session_engine::side_question::{run_side_question, SideQuestionStatus};
    provider.push_text_turn("side answer");
    let result = run_side_question(
        session.session.agent(),
        session.session.side_question_stream_fn().unwrap(),
        "a side question",
        &[],
        &crate::session_engine::provider_retry::DEFAULT_PROVIDER_RETRY_POLICY,
        &pa_agent::abort::AbortSignal::never(),
        &(Arc::new(|_: &str| true) as crate::session_engine::side_question::SideQuestionSink),
    )
    .await;
    assert_eq!(result.status, SideQuestionStatus::Complete);
}

fn frames(context: &pa_agent::stream::LlmContext) -> Vec<pa_agent::types::ImageContent> {
    context
        .messages
        .iter()
        .flat_map(|message| match message {
            pa_agent::types::Message::User(user) => match &user.content {
                UserContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|part| match part {
                        UserPart::Image(image) => Some(image.clone()),
                        UserPart::Text(_) => None,
                    })
                    .collect(),
                UserContent::Text(_) => Vec::new(),
            },
            _ => Vec::new(),
        })
        .collect()
}

#[tokio::test]
async fn live_snap_archive_reaches_each_model_call_and_owner_rebuild_once() {
    for custom_spans in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::json!({
                "harnessRefinements":false,"enableBuiltinSkills":false,"prewarmIpythonKernel":false,
                "compaction":{"keepRecentTokens":0},"contextStrategy":"default_context/rolling"
            })
            .to_string(),
        )
        .unwrap();
        if custom_spans {
            std::fs::write(
                cwd.join("policy.lua"),
                r#"
return {context_strategies={{name="rolling",context_data="refs_v1",run=function(input)
    if input.stage=="restore" then return {accepted=true} end
    if input.stage=="assemble" then
        if input.phase=="model" then input.messages.spans={{first=1,count=1}} end
        return {messages=input.messages}
    end
    return {policy="rolling",compactors={"snapcompact/snapcompact"}}
end}}}
"#,
            )
            .unwrap();
            std::fs::write(agent_dir.join("settings.json"), serde_json::json!({
            "harnessRefinements":false,"enableBuiltinSkills":false,"prewarmIpythonKernel":false,
            "compaction":{"keepRecentTokens":0},"contextStrategy":"default_context/rolling",
            "plugins":{"default_context":{"path":"policy.lua"}}
        }).to_string()).unwrap();
        }
        let owner = Owner::new_at(&cwd);
        owner.seed(serde_json::json!({"type":"message","message":{
        "role":"user","content":[{"type":"text","text":"Remember the covered history sentinel. ".repeat(4000)}],"timestamp":1
    }}));
        owner.seed(
            serde_json::json!({"type":"custom","customType":"context_strategy_selection",
        "data":{"version":1,"strategy":"default_context/rolling"}}),
        );
        let (session, provider, model) = engine(owner.clone(), dir.path()).await;
        let outcome = session
            .session
            .compact(None, &model, None, None)
            .await
            .unwrap();
        let crate::session_engine::compact_session::CompactOutcome::Ran(run) = outcome else {
            panic!("no compaction")
        };
        assert_eq!(
            run.entry.context_archive.unwrap().compactor,
            "snapcompact/snapcompact"
        );
        let accepted = session.session.agent().state().await.messages;
        assert!(
            accepted
                .iter()
                .any(|message| serde_json::to_value(message).unwrap()["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|part| part["type"] == "image"))),
            "fixture must produce actual image blocks"
        );
        let canonical_rows: Vec<_> = owner
            .0
            .lock()
            .unwrap()
            .rows
            .iter()
            .map(|row| row.raw().to_vec())
            .collect();
        side_question(&session, &provider).await;
        let after_side: Vec<_> = owner
            .0
            .lock()
            .unwrap()
            .rows
            .iter()
            .map(|row| row.raw().to_vec())
            .collect();
        assert_eq!(after_side, canonical_rows);
        provider.push_tool_call_turn(
            None,
            vec![("probe", "archive_probe", serde_json::json!({}))],
        );
        provider.push_text_turn("ok");
        // Image routing installs this same per-run override. The archive must use
        // its full advertised descriptor, not the model selected at engine creation.
        let mut routed = model.clone();
        routed.id = "gpt-routed-vision".into();
        session
            .session
            .agent()
            .set_model_override(Some(pa_agent::agent::AgentModelOverride {
                model: crate::session_engine::provider_adapter::json_round_trip(&routed).unwrap(),
                thinking_level: pa_agent::types::ThinkingLevel::Off,
            }));
        deliver(&owner, "continue");
        session
            .session
            .prompt("continue", crate::session_engine::PromptOptions::default())
            .await
            .unwrap();
        let calls = provider.calls();
        assert_eq!(
            calls.len(),
            3,
            "the tool continuation makes a second call within the same run"
        );
        let expected_frames = frames(&calls[1]);
        assert!(
            !expected_frames.is_empty(),
            "covered archive disappeared before inference (custom_spans={custom_spans})"
        );
        assert_eq!(
            frames(&calls[2]),
            expected_frames,
            "second call must contain exactly the same archive frames, without duplication"
        );
        assert_eq!(
            frames(&calls[0]),
            expected_frames,
            "side question after compaction must reuse assembled frames without duplication"
        );
        let continuation = serde_json::to_string(&calls[2].messages).unwrap();
        assert_eq!(
            continuation.matches("continue").count(),
            usize::from(!custom_spans)
        );
        assert_eq!(
            continuation.matches("local observation").count(),
            usize::from(!custom_spans)
        );
        drop(session);
        // Reopening the journal creates a new owner scope, not an admission
        // transfer from the old SDK. Historical rows remain byte-identical.
        let reopened = Owner::new_at(&cwd);
        {
            let prior = owner.0.lock().unwrap();
            let mut fresh = reopened.0.lock().unwrap();
            let scope = fresh.cursor.owner.clone();
            fresh.header = prior.header.clone();
            fresh.rows = prior.rows.clone();
            fresh.appends = prior.appends;
            fresh.cursor = prior.cursor.clone();
            fresh.cursor.owner = scope;
        }
        let owner = reopened;
        let (restored, resumed_provider, _) = engine(owner.clone(), dir.path()).await;
        side_question(&restored, &resumed_provider).await;
        resumed_provider.push_text_turn("resumed");
        deliver(&owner, "resume");
        restored
            .session
            .prompt("resume", crate::session_engine::PromptOptions::default())
            .await
            .unwrap();
        let resumed = resumed_provider.calls();
        assert_eq!(resumed.len(), 2);
        assert_eq!(
            frames(&resumed[0]),
            expected_frames,
            "restored side question must not duplicate assembled frames"
        );
        assert_eq!(
            frames(&resumed[1]),
            expected_frames,
            "owner rebuild must restore the archive exactly once"
        );
        let resumed_context = serde_json::to_string(&resumed[1].messages).unwrap();
        assert_eq!(
            resumed_context.matches("continue").count(),
            usize::from(!custom_spans)
        );
        assert_eq!(
            resumed_context.matches("local observation").count(),
            usize::from(!custom_spans)
        );
        assert_eq!(
            resumed_context.matches("resume").count(),
            usize::from(!custom_spans)
        );
    }
}
