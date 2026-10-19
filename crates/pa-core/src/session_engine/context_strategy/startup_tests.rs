//! Public engine startup boundaries must not leak admitted rows into restoration.

use crate::session::manager::SessionManager;
use crate::session_engine::{
    engine::{create_session, SessionEngine, SessionEngineConfig},
    ContextStartupBoundary, PromptOptions,
};
use pa_agent::scripted::ScriptedProvider;
use pa_types::session::{AgentMessage, FileEntry};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

const CURRENT_USER: &str = "currently admitted user";
const CALLBACK_SENTINEL: &str = "STARTUP_CALLBACK_MUST_NOT_RUN";

fn user(text: &str) -> AgentMessage {
    AgentMessage::User(pa_types::ai::UserMessage {
        content: pa_types::ai::UserContent::Text(text.into()),
        timestamp: 0,
        rest: pa_types::JsonMap::new(),
    })
}

// Legacy JSON is deliberate: these small inputs let Lua inspect message bodies,
// while the provider still records the actual typed model request.
const OBSERVING: &str = r"
local events={}
local function message_text(content)
 if type(content)=='string' then return content end
 local text={}
 for _,block in ipairs(content) do
  if block.type=='text' then text[#text+1]=block.text end
 end
 return table.concat(text,'')
end
local function observe(input)
 if input.stage=='restore' then return {accepted=true} end
 if input.stage~='assemble' then return {policy='full',compactors={}} end
 local users={}; local current=0
 for _,message in ipairs(input.messages) do
  if message.role=='user' then
   local text=message_text(message.content)
   users[#users+1]=text
   if text=='currently admitted user' then current=current+1 end
  end
 end
 if input.phase~='model' then
  assert(current==0,'admitted user leaked into startup callback')
 end
 local leaf=input.leaf; if leaf==nil or leaf==null then leaf='none' end
 events[#events+1]={phase=input.phase or 'absent',leaf=leaf,
  count=#users,users=table.concat(users,'|'),current=current}
 return {messages=input.messages}
end
return {exports={{name='events',run=function()return events end}},
 context_strategies={{name='a',run=observe},
 {name='b',run=function()error('GLOBAL_B_MUST_NOT_RUN')end}}}
";

const FORBIDDEN: &str = r"
local function forbidden() error('STARTUP_CALLBACK_MUST_NOT_RUN') end
return {context_strategies={{name='a',run=forbidden},{name='b',run=forbidden}}}
";

struct StartupFixture {
    dir: tempfile::TempDir,
    cwd: PathBuf,
    agent: PathBuf,
    model: pa_types::ai::Model,
    provider: Arc<ScriptedProvider>,
}

impl StartupFixture {
    fn new(source: &str, global: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(cwd.join("policy.lua"), source).unwrap();
        std::fs::write(
            agent.join("settings.json"),
            json!({
                "harnessRefinements":false,
                "enableBuiltinSkills":false,
                "prewarmIpythonKernel":false,
                "contextStrategy":global,
                "plugins":{"policy":{"enabled":true,"path":"policy.lua"}}
            })
            .to_string(),
        )
        .unwrap();
        let model: pa_types::ai::Model = serde_json::from_value(json!({
            "id":"test","name":"Test","api":"test","provider":"test",
            "baseUrl":"http://127.0.0.1:9/v1","reasoning":false,"input":["text"],
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
            "contextWindow":100_000,"maxTokens":100
        }))
        .unwrap();
        let provider = Arc::new(ScriptedProvider::new(
            super::super::provider_adapter::json_round_trip(&model).unwrap(),
        ));
        Self {
            dir,
            cwd,
            agent,
            model,
            provider,
        }
    }

    fn manager(&self) -> SessionManager {
        SessionManager::persisted(&self.cwd, &self.dir.path().join("sessions"))
    }

    async fn create(
        &self,
        manager: SessionManager,
        boundary: ContextStartupBoundary,
        admitted: Vec<String>,
    ) -> anyhow::Result<SessionEngine> {
        self.create_with_commit(manager, boundary, admitted, None)
            .await
    }

    async fn create_with_commit(
        &self,
        manager: SessionManager,
        boundary: ContextStartupBoundary,
        admitted: Vec<String>,
        commit: Option<crate::session_engine::engine::InitialContextSelectionCommit>,
    ) -> anyhow::Result<SessionEngine> {
        create_session(SessionEngineConfig {
            cwd: self.cwd.clone(),
            agent_dir: self.agent.clone(),
            model: Some(super::super::provider_adapter::json_round_trip(&self.model).unwrap()),
            model_info: Some(self.model.clone()),
            session_manager: Some(manager),
            context_startup_boundary: boundary,
            context_startup_admitted_user_ids: admitted,
            on_initial_context_selection: commit,
            stream_fn: Some(self.provider.stream_fn()),
            prewarm_ipython_kernel: Some(false),
            daemonless: true,
            ..Default::default()
        })
        .await
    }

    async fn reject(
        &self,
        mut manager: SessionManager,
        boundary: ContextStartupBoundary,
        admitted: Vec<String>,
    ) {
        manager.flush_now().unwrap();
        let path = manager.get_session_file().unwrap().to_owned();
        let before = std::fs::read(&path).unwrap();
        let error = match self.create(manager, boundary, admitted).await {
            Ok(_) => panic!("invalid startup boundary published an engine"),
            Err(error) => format!("{error:#}"),
        };
        assert!(!error.contains(CALLBACK_SENTINEL), "{error}");
        assert!(self.provider.calls().is_empty());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}

fn store_a(manager: &mut SessionManager) -> String {
    manager
        .append_custom_entry(
            pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE,
            Some(json!({"version":1,"strategy":"policy/a"})),
        )
        .unwrap()
}

async fn events(engine: &SessionEngine) -> Value {
    let host = engine.session.plugins.as_ref().unwrap().host();
    let export = host
        .snapshot()
        .contributions
        .iter()
        .find(|target| target.plugin == "policy" && target.name == "events")
        .unwrap()
        .clone();
    let mut invocation = host
        .invoke(&export, json!({}), &CancellationToken::new())
        .unwrap();
    match invocation.next().await.unwrap() {
        crate::plugins::InvocationEvent::Finished(Ok(
            crate::plugins::InvocationOutcome::Unary(value),
        )) => value,
        event => panic!("unexpected export event {event:?}"),
    }
}

async fn replay_and_prompt(f: &StartupFixture, engine: &SessionEngine, admitted: String) {
    engine
        .session
        .shared_persistence()
        .lock()
        .await
        .replay_admitted_user_rows(vec![admitted.clone()])
        .unwrap();
    f.provider.push_text_turn("done");
    engine
        .prompt(CURRENT_USER, PromptOptions::default())
        .await
        .unwrap();
    let calls = f.provider.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]
            .messages
            .iter()
            .filter(|message| {
                let pa_agent::types::Message::User(message) = message else {
                    return false;
                };
                let text = match &message.content {
                    pa_agent::types::UserContent::Text(text) => text.clone(),
                    pa_agent::types::UserContent::Parts(parts) => parts
                        .iter()
                        .filter_map(|part| match part {
                            pa_agent::types::UserPart::Text(text) => Some(text.text.as_str()),
                            pa_agent::types::UserPart::Image(_) => None,
                        })
                        .collect::<String>(),
                };
                text == CURRENT_USER
            })
            .count(),
        1,
        "actual typed provider input must contain the admitted user exactly once"
    );
    let entries = engine.session.entries().await;
    let current_rows: Vec<_> = entries.iter().filter(|entry| {
        matches!(entry, FileEntry::Message { message: AgentMessage::User(message), .. }
            if matches!(&message.content, pa_types::ai::UserContent::Text(text) if text == CURRENT_USER))
    }).collect();
    assert_eq!(current_rows.len(), 1);
    assert_eq!(current_rows[0].id(), Some(admitted.as_str()));
    let log = events(engine).await;
    assert!(
        log.as_array()
            .unwrap()
            .iter()
            .any(|event| { event["phase"] == "model" && event["current"] == 1 }),
        "actual model-phase Lua assembly did not see the admitted user: {log}"
    );
}

#[tokio::test]
async fn startup_leaf_uses_stored_strategy_and_prefix_then_replays_exact_admitted_user_once() {
    let f = StartupFixture::new(OBSERVING, "policy/b");
    let mut manager = f.manager();
    manager.append_message(user("older history")).unwrap();
    let leaf = store_a(&mut manager);
    let admitted = manager.append_message(user(CURRENT_USER)).unwrap();
    manager.flush_now().unwrap();
    let before = manager.retained_entries().to_vec();
    let engine = f
        .create(
            manager,
            ContextStartupBoundary::Leaf(leaf.clone()),
            vec![admitted.clone()],
        )
        .await
        .unwrap();
    {
        let persistence = engine.session.shared_persistence();
        let manager = persistence.lock().await;
        assert_eq!(&manager.retained_entries()[..before.len()], &before);
    }
    let log = events(&engine).await;
    assert_eq!(log.as_array().unwrap().len(), 1);
    assert_eq!(
        log[0],
        json!({
            "phase":"absent","leaf":leaf,"count":1,"users":"older history","current":0
        })
    );
    assert_eq!(
        super::route(
            &engine
                .session
                .plugins
                .as_ref()
                .unwrap()
                .context_selection()
                .unwrap()
        ),
        "policy/a"
    );
    replay_and_prompt(&f, &engine, admitted).await;
    let calls = f.provider.calls();
    assert!(serde_json::to_string(&calls[0].messages)
        .unwrap()
        .contains("older history"));
}

#[tokio::test]
async fn startup_empty_has_no_user_or_leaf_then_replays_first_admitted_user_once() {
    let f = StartupFixture::new(OBSERVING, "policy/a");
    let mut manager = f.manager();
    let admitted = manager.append_message(user(CURRENT_USER)).unwrap();
    manager.flush_now().unwrap();
    let before = manager.retained_entries().to_vec();
    let engine = f
        .create(
            manager,
            ContextStartupBoundary::Empty,
            vec![admitted.clone()],
        )
        .await
        .unwrap();
    {
        let persistence = engine.session.shared_persistence();
        let manager = persistence.lock().await;
        assert_eq!(&manager.retained_entries()[..before.len()], &before);
    }
    let log = events(&engine).await;
    assert_eq!(log.as_array().unwrap().len(), 1);
    assert_eq!(
        log[0],
        json!({
            "phase":"absent","leaf":"none","count":0,"users":"","current":0
        })
    );
    replay_and_prompt(&f, &engine, admitted).await;
}

#[tokio::test]
async fn startup_current_default_keeps_the_full_existing_branch() {
    let f = StartupFixture::new(OBSERVING, "policy/a");
    let mut manager = f.manager();
    manager.append_message(user("existing history")).unwrap();
    let engine = f
        .create(manager, ContextStartupBoundary::default(), vec![])
        .await
        .unwrap();
    let log = events(&engine).await;
    let leaf = log[0]["leaf"].as_str().expect("real startup leaf");
    assert!(!leaf.is_empty());
    assert_ne!(leaf, "none");
    assert!(retained_rows(&engine)
        .await
        .iter()
        .any(|entry| entry.id() == Some(leaf)));
    assert_eq!(
        log[0],
        json!({
            "phase":"absent","leaf":leaf,"count":1,"users":"existing history","current":0
        })
    );
    assert!(f.provider.calls().is_empty());
}

#[tokio::test]
async fn startup_rejects_empty_missing_and_nonancestor_leaf_before_callback() {
    for kind in ["empty", "missing", "nonancestor"] {
        let f = StartupFixture::new(FORBIDDEN, "policy/a");
        let mut manager = f.manager();
        let root = manager.append_message(user("root history")).unwrap();
        let sibling = manager.append_message(user("other branch")).unwrap();
        manager.branch(&root);
        let admitted = manager.append_message(user(CURRENT_USER)).unwrap();
        let leaf = match kind {
            "empty" => String::new(),
            "missing" => "not-a-real-entry".to_owned(),
            _ => sibling,
        };
        f.reject(manager, ContextStartupBoundary::Leaf(leaf), vec![admitted])
            .await;
    }
}

#[tokio::test]
async fn startup_leaf_requires_explicit_exact_suffix_admission_ids_before_callback() {
    for wrong_id in [false, true] {
        let f = StartupFixture::new(FORBIDDEN, "policy/a");
        let mut manager = f.manager();
        let leaf = manager.append_message(user("old history")).unwrap();
        manager.append_message(user(CURRENT_USER)).unwrap();
        let admitted = if wrong_id { vec![leaf.clone()] } else { vec![] };
        f.reject(manager, ContextStartupBoundary::Leaf(leaf), admitted)
            .await;
    }
}

#[tokio::test]
async fn startup_empty_rejects_unadmitted_old_history_before_callback() {
    let f = StartupFixture::new(FORBIDDEN, "policy/a");
    let mut manager = f.manager();
    manager.append_message(user("old history")).unwrap();
    let admitted = manager.append_message(user(CURRENT_USER)).unwrap();
    f.reject(manager, ContextStartupBoundary::Empty, vec![admitted])
        .await;
}

#[tokio::test]
async fn startup_disabled_stored_strategy_fails_closed_instead_of_using_global_default() {
    let f = StartupFixture::new(FORBIDDEN, "default_context/rolling");
    let mut manager = f.manager();
    manager.append_message(user("old history")).unwrap();
    let leaf = store_a(&mut manager);
    let admitted = manager.append_message(user(CURRENT_USER)).unwrap();
    let path = f.agent.join("settings.json");
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    settings["plugins"]["policy"]["enabled"] = json!(false);
    std::fs::write(path, settings.to_string()).unwrap();
    f.reject(manager, ContextStartupBoundary::Leaf(leaf), vec![admitted])
        .await;
}

#[tokio::test]
async fn startup_current_rejects_admission_ids_before_callback() {
    let f = StartupFixture::new(FORBIDDEN, "policy/a");
    let mut manager = f.manager();
    let admitted = manager.append_message(user(CURRENT_USER)).unwrap();
    f.reject(manager, ContextStartupBoundary::Current, vec![admitted])
        .await;
}

#[tokio::test]
async fn startup_empty_accepts_bootstrap_metadata_and_uses_stored_strategy() {
    let f = StartupFixture::new(OBSERVING, "policy/b");
    let mut manager = f.manager();
    manager.append_model_change("test", "test").unwrap();
    manager.append_thinking_level_change("off").unwrap();
    manager.append_service_tier_change(None).unwrap();
    store_a(&mut manager);
    let admitted = manager.append_message(user(CURRENT_USER)).unwrap();
    manager.flush_now().unwrap();
    let before = manager.retained_entries().to_vec();
    let engine = f
        .create(
            manager,
            ContextStartupBoundary::Empty,
            vec![admitted.clone()],
        )
        .await
        .unwrap();
    {
        let persistence = engine.session.shared_persistence();
        let manager = persistence.lock().await;
        assert_eq!(&manager.retained_entries()[..before.len()], &before);
    }
    let log = events(&engine).await;
    assert_eq!(log.as_array().unwrap().len(), 1);
    assert_eq!(
        log[0],
        json!({
            "phase":"absent","leaf":"none","count":0,"users":"","current":0
        })
    );
    assert_eq!(
        super::route(
            &engine
                .session
                .plugins
                .as_ref()
                .unwrap()
                .context_selection()
                .unwrap()
        ),
        "policy/a"
    );
    replay_and_prompt(&f, &engine, admitted).await;
}

async fn retained_rows(engine: &SessionEngine) -> Vec<FileEntry> {
    engine
        .session
        .shared_persistence()
        .lock()
        .await
        .retained_entries()
        .to_vec()
}

async fn assert_rejected_view_unchanged(
    engine: &SessionEngine,
    rows: &[FileEntry],
    messages: &[pa_agent::types::AgentMessage],
    log: &Value,
) {
    assert_eq!(retained_rows(engine).await, rows);
    assert_eq!(engine.session.agent().state().await.messages, messages);
    assert_eq!(
        events(engine).await,
        *log,
        "invalid boundary invoked a strategy callback"
    );
}

#[tokio::test]
async fn startup_rebuild_at_keeps_full_rows_and_prefix_and_rejects_invalid_proof_atomically() {
    let f = StartupFixture::new(OBSERVING, "policy/b");
    let mut manager = f.manager();
    manager.append_message(user("older history")).unwrap();
    let leaf = store_a(&mut manager);
    let admitted = manager.append_message(user(CURRENT_USER)).unwrap();
    let boundary = ContextStartupBoundary::Leaf(leaf);
    let ids = vec![admitted.clone()];
    let engine = f
        .create(manager, boundary.clone(), ids.clone())
        .await
        .unwrap();
    let rows = retained_rows(&engine).await;
    let branch_rows: Vec<_> = rows
        .iter()
        .filter(|entry| !matches!(entry, FileEntry::Header { .. }))
        .cloned()
        .collect();
    let messages = engine.session.agent().state().await.messages;
    let log = events(&engine).await;
    assert_eq!(log.as_array().unwrap().len(), 1);
    for (invalid_boundary, invalid_ids) in [
        (
            ContextStartupBoundary::Leaf("missing-entry".into()),
            ids.clone(),
        ),
        (boundary.clone(), vec![]),
    ] {
        assert!(engine
            .session
            .rebuild_branch_context_at(branch_rows.clone(), &invalid_boundary, &invalid_ids,)
            .await
            .is_err());
        assert_rejected_view_unchanged(&engine, &rows, &messages, &log).await;
    }
    engine
        .session
        .rebuild_branch_context_at(branch_rows.clone(), &boundary, &ids)
        .await
        .unwrap();
    assert_eq!(retained_rows(&engine).await, rows);
    assert_eq!(engine.session.agent().state().await.messages, messages);
    let rebuilt_log = events(&engine).await;
    assert_eq!(rebuilt_log.as_array().unwrap().len(), 2);
    assert_eq!(rebuilt_log[1], log[0]);
    assert_eq!(rebuilt_log[1]["users"], "older history");
    assert_eq!(rebuilt_log[1]["current"], 0);
    replay_and_prompt(&f, &engine, admitted).await;
}

#[tokio::test]
async fn startup_window_restore_at_keeps_full_rows_and_prefix_and_rejects_foreign_or_invalid_views()
{
    use crate::session::window::WindowedSessionStore;

    let f = StartupFixture::new(OBSERVING, "policy/b");
    let mut manager = f.manager();
    manager.append_message(user("older history")).unwrap();
    let leaf = store_a(&mut manager);
    let admitted = manager.append_message(user(CURRENT_USER)).unwrap();
    let boundary = ContextStartupBoundary::Leaf(leaf);
    let ids = vec![admitted.clone()];
    let engine = f
        .create(manager, boundary.clone(), ids.clone())
        .await
        .unwrap();
    let path = {
        let persistence = engine.session.shared_persistence();
        let mut manager = persistence.lock().await;
        manager.flush_now().unwrap();
        manager.get_session_file().unwrap().to_owned()
    };
    let journal_before = std::fs::read(&path).unwrap();
    let rows = retained_rows(&engine).await;
    let messages = engine.session.agent().state().await.messages;
    let log = events(&engine).await;
    assert_eq!(log.as_array().unwrap().len(), 1);
    for (invalid_boundary, invalid_ids) in [
        (
            ContextStartupBoundary::Leaf("missing-entry".into()),
            ids.clone(),
        ),
        (boundary.clone(), vec![]),
    ] {
        let window = WindowedSessionStore::open(&path)
            .unwrap()
            .expect("session window");
        assert!(engine
            .session
            .restore_windowed_context_at(window, &invalid_boundary, &invalid_ids,)
            .await
            .is_err());
        assert_rejected_view_unchanged(&engine, &rows, &messages, &log).await;
    }
    let foreign = StartupFixture::new(OBSERVING, "policy/a");
    let mut other = foreign.manager();
    other.append_message(user("foreign history")).unwrap();
    other.flush_now().unwrap();
    let foreign_path = other.get_session_file().unwrap().to_owned();
    let foreign_window = WindowedSessionStore::open(&foreign_path)
        .unwrap()
        .expect("foreign window");
    let error = engine
        .session
        .restore_windowed_context_at(foreign_window, &ContextStartupBoundary::Current, &[])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("another session"));
    assert_rejected_view_unchanged(&engine, &rows, &messages, &log).await;

    let mut window = WindowedSessionStore::open(&path)
        .unwrap()
        .expect("session window");
    window.ensure_full_history().await.unwrap();
    let parsed_rows = window.entries().to_vec();
    engine
        .session
        .restore_windowed_context_at(window, &boundary, &ids)
        .await
        .unwrap();
    let adopted = retained_rows(&engine).await;
    assert_eq!(adopted, parsed_rows);
    assert_eq!(std::fs::read(&path).unwrap(), journal_before);
    assert_eq!(
        adopted.iter().filter_map(FileEntry::id).collect::<Vec<_>>(),
        rows.iter().filter_map(FileEntry::id).collect::<Vec<_>>()
    );
    let original_messages: Vec<_> = rows
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Message { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    let adopted_messages: Vec<_> = adopted
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Message { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(adopted_messages, original_messages);
    assert_eq!(engine.session.agent().state().await.messages, messages);
    let restored_log = events(&engine).await;
    assert_eq!(restored_log.as_array().unwrap().len(), 2);
    assert_eq!(restored_log[1], log[0]);
    assert_eq!(restored_log[1]["users"], "older history");
    assert_eq!(restored_log[1]["current"], 0);
    replay_and_prompt(&f, &engine, admitted).await;
}

type SelectionReceipts = Arc<std::sync::Mutex<Vec<pa_types::session::ContextSelection>>>;

fn receipt_hook(
    receipts: SelectionReceipts,
    reject: bool,
) -> crate::session_engine::engine::InitialContextSelectionCommit {
    Arc::new(move |choice| {
        let receipts = Arc::clone(&receipts);
        Box::pin(async move {
            receipts.lock().unwrap().push(choice);
            if reject {
                anyhow::bail!("INITIAL_SELECTION_ACK_REJECTED");
            }
            Ok(())
        })
    })
}

fn selection_rows(rows: &[FileEntry]) -> Vec<FileEntry> {
    rows.iter()
        .filter(|entry| {
            matches!(entry, FileEntry::Custom { payload, .. }
            if payload.custom_type == pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE)
        })
        .cloned()
        .collect()
}

fn assert_receipt_a(receipts: &SelectionReceipts) {
    assert_eq!(
        serde_json::to_value(&*receipts.lock().unwrap()).unwrap(),
        json!([{"version":1,"strategy":"policy/a"}])
    );
}

#[tokio::test]
async fn startup_selection_ack_failure_precedes_strategy_callbacks_for_fresh_and_saved_choices() {
    for saved in [false, true] {
        let f = StartupFixture::new(FORBIDDEN, if saved { "policy/b" } else { "policy/a" });
        let mut manager = f.manager();
        manager.append_message(user("older history")).unwrap();
        if saved {
            store_a(&mut manager);
        }
        let receipts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let result = f
            .create_with_commit(
                manager,
                ContextStartupBoundary::Current,
                vec![],
                Some(receipt_hook(Arc::clone(&receipts), true)),
            )
            .await;
        let error = match result {
            Ok(_) => panic!("rejected initial selection acknowledgement published an engine"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains("INITIAL_SELECTION_ACK_REJECTED"), "{error}");
        assert!(!error.contains(CALLBACK_SENTINEL), "{error}");
        assert_receipt_a(&receipts);
        assert!(f.provider.calls().is_empty());
    }
}

#[tokio::test]
async fn startup_selection_ack_success_is_recorded_before_first_strategy_callback() {
    for saved in [false, true] {
        let f = StartupFixture::new(FORBIDDEN, if saved { "policy/b" } else { "policy/a" });
        let mut manager = f.manager();
        manager.append_message(user("older history")).unwrap();
        if saved {
            store_a(&mut manager);
        }
        let receipts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let result = f
            .create_with_commit(
                manager,
                ContextStartupBoundary::Current,
                vec![],
                Some(receipt_hook(Arc::clone(&receipts), false)),
            )
            .await;
        let error = match result {
            Ok(_) => panic!("forbidden strategy callback unexpectedly succeeded"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains(CALLBACK_SENTINEL), "{error}");
        // A callback failure must not prevent the earlier canonical ack receipt.
        assert_receipt_a(&receipts);
        assert!(f.provider.calls().is_empty());
    }
}

#[tokio::test]
async fn startup_selection_ack_uses_pinned_a_without_inventing_mirror_selection_rows() {
    for saved in [false, true] {
        let f = StartupFixture::new(OBSERVING, if saved { "policy/b" } else { "policy/a" });
        let mut manager = f.manager();
        manager.append_message(user("older history")).unwrap();
        if saved {
            store_a(&mut manager);
        }
        let choices_before = selection_rows(manager.retained_entries());
        assert_eq!(choices_before.len(), usize::from(saved));
        let receipts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = f
            .create_with_commit(
                manager,
                ContextStartupBoundary::Current,
                vec![],
                Some(receipt_hook(Arc::clone(&receipts), false)),
            )
            .await
            .unwrap();
        assert_receipt_a(&receipts);
        assert_eq!(
            selection_rows(&retained_rows(&engine).await),
            choices_before
        );
        assert_eq!(
            super::route(
                &engine
                    .session
                    .plugins
                    .as_ref()
                    .unwrap()
                    .context_selection()
                    .unwrap()
            ),
            "policy/a"
        );
        let log = events(&engine).await;
        assert_eq!(log.as_array().unwrap().len(), 1);
        assert_eq!(log[0]["users"], "older history");
        assert_eq!(log[0]["current"], 0);
        assert!(f.provider.calls().is_empty());
    }
}

#[tokio::test]
async fn startup_without_selection_ack_keeps_default_sdk_choice_append() {
    let f = StartupFixture::new(OBSERVING, "policy/a");
    let mut manager = f.manager();
    manager.append_message(user("older history")).unwrap();
    assert!(selection_rows(manager.retained_entries()).is_empty());
    let engine = f
        .create(manager, ContextStartupBoundary::Current, vec![])
        .await
        .unwrap();
    let rows = retained_rows(&engine).await;
    let choices = selection_rows(&rows);
    assert_eq!(choices.len(), 1);
    let FileEntry::Custom { payload, .. } = &choices[0] else {
        unreachable!()
    };
    assert_eq!(
        payload.data,
        Some(json!({"version":1,"strategy":"policy/a"}))
    );
    assert!(f.provider.calls().is_empty());
}

const STARTUP_TREE_SUMMARIZER: &str = r"
local calls={}
local function collect_text(value,out)
 if type(value)=='string' then out[#out+1]=value
 elseif type(value)=='table' then
  for _,child in pairs(value) do collect_text(child,out) end
 end
end
return {exports={{name='calls',run=function()return calls end}},providers={{
 name='tree-summarizer',mode='streaming',run=function(input,ctx)
 local text_input={};collect_text(input,text_input)
 calls[#calls+1]={id=input.model.id,provider=input.model.provider,text=table.concat(text_input,'\n')}
 local text='Historical intervals summarized; use recall for exact originals.'
 local message={role='assistant',content={{type='text',text=text}},api=input.model.api,
 provider=input.model.provider,model=input.model.id,usage={input=0,output=0,cacheRead=0,
 cacheWrite=0,totalTokens=0,cost={input=0,output=0,cacheRead=0,cacheWrite=0,total=0}},stopReason='stop',timestamp=0}
 ctx.emit({type='start',partial=message});ctx.emit({type='done',reason='stop',message=message});return message
end}}}
";

async fn startup_native_export(
    engine: &SessionEngine,
    plugin: &str,
    name: &str,
    input: Value,
) -> Value {
    let host = engine.session.plugins.as_ref().unwrap().host();
    let target = host
        .snapshot()
        .contributions
        .iter()
        .find(|target| target.plugin == plugin && target.name == name)
        .unwrap()
        .clone();
    let mut invocation = host
        .invoke(&target, input, &CancellationToken::new())
        .unwrap();
    match invocation.next().await.unwrap() {
        crate::plugins::InvocationEvent::Finished(Ok(
            crate::plugins::InvocationOutcome::Unary(value),
        )) => value,
        event => panic!("unexpected native export event {event:?}"),
    }
}

#[tokio::test]
async fn startup_native_optchat_sources_only_prefix_then_includes_admitted_user_once_at_model() {
    let f = StartupFixture::new("return {}", "optchat/tree");
    std::fs::create_dir_all(f.cwd.join(".prime/agent")).unwrap();
    std::fs::write(f.cwd.join("summarizer.lua"), STARTUP_TREE_SUMMARIZER).unwrap();
    std::fs::write(
        f.agent.join("settings.json"),
        json!({
            "harnessRefinements":false,"enableBuiltinSkills":false,"prewarmIpythonKernel":false,
            "contextStrategy":"optchat/tree","auxiliaryModel":"tree-summarizer/default",
            "plugins":{
                "optchat":{"enabled":true,"config":{
                    "budget_bytes":4096,"summary_bytes":128,"model":"tree-summarizer/chosen"
                }},
                "summary":{"enabled":true,"path":"summarizer.lua"}
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(f.agent.join("models.json"), json!({
        "providers":{"tree-summarizer":{
            "baseUrl":"http://127.0.0.1:1","apiKey":"isolated-fake-only","api":"openai-completions",
            "models":[{"id":"chosen","contextWindow":131_072},{"id":"default","contextWindow":131_072}]
        }}
    }).to_string()).unwrap();
    let mut manager = f.manager();
    manager
        .append_model_change(&f.model.provider, &f.model.id)
        .unwrap();
    manager.append_thinking_level_change("off").unwrap();
    manager.append_service_tier_change(None).unwrap();
    for index in 0..32 {
        manager
            .append_message(user(&format!(
                "historical-{index}: {}",
                "detail ".repeat(100)
            )))
            .unwrap();
    }
    let leaf = manager
        .append_custom_entry(
            pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE,
            Some(json!({"version":1,"strategy":"optchat/tree"})),
        )
        .unwrap();
    let prefix_ids: Vec<_> = manager
        .retained_entries()
        .iter()
        .filter(|entry| !matches!(entry, FileEntry::Header { .. }))
        .filter_map(|entry| entry.id().map(str::to_owned))
        .collect();
    assert!(CURRENT_USER.len() < 1024);
    let admitted = manager.append_message(user(CURRENT_USER)).unwrap();
    manager.flush_now().unwrap();
    let journal = manager.get_session_file().unwrap().to_owned();
    let journal_before = std::fs::read(&journal).unwrap();
    let original = manager.retained_entries().to_vec();
    let engine = f
        .create(
            manager,
            ContextStartupBoundary::Leaf(leaf),
            vec![admitted.clone()],
        )
        .await
        .unwrap();
    assert_eq!(retained_rows(&engine).await, original);
    assert_eq!(std::fs::read(&journal).unwrap(), journal_before);
    let initial_view =
        serde_json::to_string(&engine.session.agent().state().await.messages).unwrap();
    assert!(initial_view.contains("summary-tree view"));
    assert!(!initial_view.contains(CURRENT_USER));
    assert!(
        f.provider.calls().is_empty(),
        "historical summaries used the main provider"
    );
    let summary_calls = startup_native_export(&engine, "summary", "calls", json!({})).await;
    let calls = summary_calls.as_array().unwrap();
    assert!(
        !calls.is_empty(),
        "historical source did not force any legitimate summaries"
    );
    assert!(calls
        .iter()
        .all(|call| call["id"] == "chosen" && call["provider"] == "tree-summarizer"));
    assert!(calls
        .iter()
        .any(|call| call["text"].as_str().unwrap().contains("historical-")));
    assert!(
        calls
            .iter()
            .all(|call| !call["text"].as_str().unwrap().contains(CURRENT_USER)),
        "startup auxiliary summary inputs included the admitted user"
    );
    let status = startup_native_export(&engine, "optchat", "status", json!({})).await;
    let view = &status["last_prepared"];
    assert_eq!(view["entries"].as_u64().unwrap(), prefix_ids.len() as u64);
    assert!(view["bytes"].as_u64().unwrap() <= 4096);
    let mut covered = 0;
    let mut recalled_ids = Vec::new();
    for span in view["spans"].as_array().unwrap() {
        assert_eq!(span["start"], covered);
        covered = span["end"].as_u64().unwrap();
        let recall = startup_native_export(
            &engine,
            "optchat",
            "zoom",
            json!({"id":span["id"],"limit":256}),
        )
        .await;
        assert!(!recall.to_string().contains(CURRENT_USER));
        recalled_ids.extend(
            recall["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["id"].as_str().unwrap().to_owned()),
        );
    }
    assert_eq!(covered, prefix_ids.len() as u64);
    assert_eq!(recalled_ids, prefix_ids);

    engine
        .session
        .shared_persistence()
        .lock()
        .await
        .replay_admitted_user_rows(vec![admitted.clone()])
        .unwrap();
    f.provider.push_text_turn("native main answer");
    engine
        .prompt(CURRENT_USER, PromptOptions::default())
        .await
        .unwrap();
    let main = f.provider.calls();
    assert_eq!(main.len(), 1);
    let wire = serde_json::to_string(&main[0].messages).unwrap();
    assert!(wire.contains("summary-tree view"));
    assert_eq!(
        wire.matches(CURRENT_USER).count(),
        1,
        "native OptChat model view must contain the admitted user exactly once"
    );
    let rows = retained_rows(&engine).await;
    let current_rows: Vec<_> = rows.iter().filter(|entry| {
        matches!(entry, FileEntry::Message { message: AgentMessage::User(message), .. }
            if matches!(&message.content, pa_types::ai::UserContent::Text(text) if text == CURRENT_USER))
    }).collect();
    assert_eq!(current_rows.len(), 1);
    assert_eq!(current_rows[0].id(), Some(admitted.as_str()));
    assert_eq!(&rows[..original.len()], &original);
}

#[tokio::test]
async fn startup_invalid_boundary_does_not_invoke_external_selection_ack() {
    let f = StartupFixture::new(FORBIDDEN, "policy/a");
    let mut manager = f.manager();
    manager
        .append_message(user("historical body must remain visible"))
        .unwrap();
    let admitted = manager.append_message(user(CURRENT_USER)).unwrap();
    manager.flush_now().unwrap();
    let path = manager.get_session_file().unwrap().to_owned();
    let before = std::fs::read(&path).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink = Arc::clone(&calls);
    let commit: crate::session_engine::engine::InitialContextSelectionCommit =
        Arc::new(move |_| {
            sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        });
    let result = f
        .create_with_commit(
            manager,
            ContextStartupBoundary::Empty,
            vec![admitted],
            Some(commit),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(f.provider.calls().is_empty());
    assert_eq!(std::fs::read(path).unwrap(), before);
}
