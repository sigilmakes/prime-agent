//! Real lease/file navigation receipts with a counted fake summary provider.
//! The scripted projection host is deliberate; bound context assembly is tested in Core.
use super::*;
use crate::engine::*;
use crate::worker::WorkerConfig;
use pa_core::session::manager::SessionManager;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingEngine {
    script: ScriptedEngine,
    calls: Arc<AtomicUsize>,
    summary_sources: Arc<Mutex<Vec<Vec<FileEntry>>>>,
    advance_during_summary: Option<Arc<Mutex<SessionCore>>>,
}

impl SessionEngine for CountingEngine {
    fn run_prompt(
        &self,
        index: usize,
        request: PromptRequest,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
        self.script.run_prompt(index, request, aborted, emit);
    }
    fn run_side_question(
        &self,
        request: SideQuestionRequest,
        signal: &pa_agent::abort::AbortSignal,
        sink: &pa_core::session_engine::side_question::SideQuestionSink,
    ) -> SideQuestionOutcome {
        self.script.run_side_question(request, signal, sink)
    }
    fn run_compaction(
        &self,
        request: CompactionRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> CompactionOutcome {
        self.script.run_compaction(request, signal)
    }
    fn run_branch_summary(
        &self,
        request: BranchSummaryRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> BranchSummaryOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.summary_sources
            .lock()
            .unwrap()
            .push(request.entries.clone());
        if let Some(core) = &self.advance_during_summary {
            core.lock()
                .unwrap()
                .store
                .as_mut()
                .unwrap()
                .persist_owner_metadata(
                    "custom",
                    json!({"customType":"concurrent-owner-metadata","data":{"exact":"AA=="}}),
                )
                .unwrap();
        }
        self.script.run_branch_summary(request, signal)
    }
    fn rebuild_session_context(
        &self,
        entries: Vec<FileEntry>,
        rule: pa_core::session_engine::goal_driver::GoalBranchReload,
    ) -> anyhow::Result<()> {
        self.script.rebuild_session_context(entries, rule)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    worker: Arc<Worker>,
    navigation: TreeNavigation,
    live: Arc<CanonicalLiveSession>,
    manager: Arc<tokio::sync::Mutex<SessionManager>>,
    calls: Arc<AtomicUsize>,
    summary_sources: Arc<Mutex<Vec<Vec<FileEntry>>>>,
    first_user: String,
    first_assistant: String,
    path: std::path::PathBuf,
    initial_unscoped_rows: u64,
}

enum SummarySource {
    Stable,
    AdvanceDuringCallback,
    UsageOverlay,
}

async fn fixture(source: SummarySource) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let worker = Arc::new(Worker::new(
        WorkerConfig {
            socket_path: dir.path().join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::default(),
            token: "isolated-token".to_owned(),
            worker_instance_id: String::new(),
            active_session_id: "canonical-navigation-test".to_owned(),
            agent_dir: dir.path().join("agent"),
            recovery_journal_path: dir.path().join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({"responses":["unused"]})),
        },
        None,
    ));
    let created = worker.dispatch("create", &json!({
        "cwd":dir.path(), "sessionDir":dir.path().join("sessions"), "name":"isolated-navigation",
    })).await;
    assert!(created.success, "{created:?}");
    let (first_user, first_assistant, path, initial_unscoped_rows) = {
        let mut core = worker.core.lock().unwrap();
        let store = core.store.as_mut().unwrap();
        let first_user = store
            .persist_entry(
                "message",
                json!({"message":{
                    "role":"user","content":"first prompt","timestamp":1,
                }}),
            )
            .unwrap();
        let first_assistant = store.persist_entry("message", json!({"message":{
            "role":"assistant", "content":[{"type":"text","text":"first answer"}],
            "api":"openai-completions","provider":"fake","model":"fake", "stopReason":"stop",
            "timestamp":2,"usage":pa_types::ai::Usage::default(),
        }})).unwrap();
        store
            .persist_entry(
                "message",
                json!({"message":{
                    "role":"user","content":"abandoned prompt","timestamp":3,
                }}),
            )
            .unwrap();
        let abandoned_assistant = store.persist_entry("message", json!({"message":{
            "role":"assistant", "content":[{"type":"text","text":"abandoned answer"}],
            "api":"openai-completions","provider":"fake","model":"fake", "stopReason":"stop",
            "timestamp":4,"usage":pa_types::ai::Usage::default(),
        }})).unwrap();
        if matches!(source, SummarySource::UsageOverlay) {
            store
                .append_child_usage_once(
                    "raw-summary-attribution",
                    &abandoned_assistant,
                    pa_types::ai::Usage {
                        input: 7,
                        ..Default::default()
                    },
                    /* origin */ None,
                )
                .unwrap();
            // Exercise a real disk-loaded usage overlay before binding the
            // first canonical owner. Retain the same actual runtime lease.
            assert!(!store.is_canonical_bound());
            let mut reloaded = crate::session_store::SessionFile::open(&store.path).unwrap();
            reloaded.lease.clone_from(&store.lease);
            *store = reloaded;
        }
        (
            first_user,
            first_assistant,
            store.path.clone(),
            store.rows_written_without_epoch(),
        )
    };
    let factory = crate::worker::context_selection_store_for_tests(worker.core.clone());
    let source_capture = factory.capture(&path).unwrap();
    // Use the actual captured SDK header, including the tagged wire decode
    // needed after a real reload; never substitute an old in-memory header.
    let header = source_capture.header.clone();
    let owner = factory.live_owner(source_capture).unwrap();
    let manager = Arc::new(tokio::sync::Mutex::new(
        SessionManager::in_memory_from_header(dir.path(), &dir.path().join("artifacts"), header)
            .unwrap(),
    ));
    let live = CanonicalLiveSession::new(owner, manager.clone());
    live.observe().await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let summary_sources = Arc::new(Mutex::new(Vec::new()));
    let engine = Arc::new(CountingEngine {
        script: ScriptedEngine::from_value(&json!({"branchSummary":{"responses":[{
            "summary":"retained paid output", "provider":"actual-aux", "modelId":"summary-model",
            "details":{"readFiles":["file"],"native":{"bytes":"AA=="}},
            "usage":pa_types::ai::Usage::default(),
        }]}}))
        .unwrap(),
        calls: calls.clone(),
        summary_sources: summary_sources.clone(),
        advance_during_summary: match source {
            SummarySource::Stable | SummarySource::UsageOverlay => None,
            SummarySource::AdvanceDuringCallback => Some(worker.core.clone()),
        },
    });
    let navigation = TreeNavigation::new(engine, worker.core.clone(), Arc::new(Notify::new()));
    Fixture {
        _dir: dir,
        worker,
        navigation,
        live,
        manager,
        calls,
        summary_sources,
        first_user,
        first_assistant,
        path,
        initial_unscoped_rows,
    }
}

#[tokio::test]
async fn plain_navigation_adopts_raw_same_ordered_successor_without_summary_call() {
    let fixture = fixture(SummarySource::Stable).await;
    let bytes = std::fs::read(&fixture.path).unwrap();
    let first_user = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<FileEntry>(line).unwrap())
        .find(|entry| entry.id() == Some(fixture.first_user.as_str()))
        .unwrap();
    // The named worker fixture has real session metadata before its first User.
    // Navigating to a User selects that exact row's parent, not an assumed root.
    let expected_leaf = first_user.parent_id().map(str::to_owned);
    assert!(
        expected_leaf.is_some(),
        "fixture must retain its metadata prefix"
    );
    let before = fixture
        .manager
        .lock()
        .await
        .canonical_binding()
        .unwrap()
        .clone();
    let result = fixture
        .navigation
        .navigate_canonical(
            &json!({"targetId":fixture.first_user}),
            fixture.live.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({"cancelled":false,"editorText":"first prompt"})
    );
    let after = fixture
        .manager
        .lock()
        .await
        .canonical_binding()
        .unwrap()
        .clone();
    assert_eq!(after.owner, before.owner);
    assert_eq!(after.frontier, before.frontier);
    assert_eq!(after.raw_source_generation, before.raw_source_generation);
    assert_eq!(after.branch_revision, before.branch_revision + 1);
    assert_eq!(after.canonical_revision, before.canonical_revision + 1);
    assert_eq!(after.selected_leaf, expected_leaf);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), bytes);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.manager.lock().await.get_leaf_id(),
        expected_leaf.as_deref()
    );
}

#[tokio::test]
async fn summarized_navigation_commits_retained_output_once_on_exact_new_branch() {
    let fixture = fixture(SummarySource::Stable).await;
    let prefix = std::fs::read(&fixture.path).unwrap();
    let before = fixture
        .manager
        .lock()
        .await
        .canonical_binding()
        .unwrap()
        .clone();
    let result = fixture
        .navigation
        .navigate_canonical(
            &json!({
                "targetId":fixture.first_assistant, "summarize":true,
            }),
            fixture.live.clone(),
        )
        .await
        .unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    {
        let core = fixture.worker.core.lock().unwrap();
        let store = core.store.as_ref().unwrap();
        let entry = store.entries().last().unwrap();
        assert_eq!(entry.type_, "branch_summary");
        assert_eq!(
            entry.parent_id.as_deref(),
            Some(fixture.first_assistant.as_str())
        );
        assert_eq!(entry.fields["summary"], json!("retained paid output"));
        assert_eq!(entry.fields["provider"], json!("actual-aux"));
        assert_eq!(entry.fields["modelId"], json!("summary-model"));
        assert_eq!(
            entry.fields["details"],
            json!({"readFiles":["file"],"native":{"bytes":"AA=="}})
        );
        assert_eq!(result["summaryEntry"], session_tree::entry_json(entry));
        assert_eq!(
            store.rows_written_without_epoch(),
            fixture.initial_unscoped_rows,
            "navigation and its summary add no unscoped writes"
        );
    }
    let after = fixture
        .manager
        .lock()
        .await
        .canonical_binding()
        .unwrap()
        .clone();
    assert_eq!(after.branch_revision, before.branch_revision + 1);
    assert_eq!(after.canonical_revision, before.canonical_revision + 2);
    assert!(std::fs::read(&fixture.path).unwrap().starts_with(&prefix));
}

#[tokio::test]
async fn concurrent_owner_source_advance_never_retries_paid_callback_or_commits_summary() {
    let fixture = fixture(SummarySource::AdvanceDuringCallback).await;
    let prefix = std::fs::read(&fixture.path).unwrap();
    let before = fixture
        .manager
        .lock()
        .await
        .canonical_binding()
        .unwrap()
        .clone();
    let result = fixture
        .navigation
        .navigate_canonical(
            &json!({
                "targetId":fixture.first_assistant,"summarize":true,
            }),
            fixture.live.clone(),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.manager.lock().await.canonical_binding(),
        Some(&before)
    );
    fixture.live.ensure_ready().unwrap();
    {
        let core = fixture.worker.core.lock().unwrap();
        assert!(!core
            .store
            .as_ref()
            .unwrap()
            .entries()
            .iter()
            .any(|entry| entry.type_ == "branch_summary"));
    }
    let advanced = std::fs::read(&fixture.path).unwrap();
    assert!(advanced.starts_with(&prefix));
    let metadata: FileEntry = serde_json::from_slice(&advanced[prefix.len()..]).unwrap();
    let metadata_json = serde_json::to_value(&metadata).unwrap();
    assert_eq!(metadata_json["type"], json!("custom"));
    assert_eq!(
        metadata_json["customType"],
        json!("concurrent-owner-metadata")
    );
    assert_eq!(metadata_json["data"], json!({"exact":"AA=="}));

    fixture.live.observe().await.unwrap();
    fixture.live.ensure_ready().unwrap();
    let manager = fixture.manager.lock().await;
    let after = manager.canonical_binding().unwrap();
    assert_eq!(after.owner, before.owner);
    assert_eq!(after.branch_revision, before.branch_revision);
    assert_eq!(after.canonical_revision, before.canonical_revision + 1);
    assert!(manager.get_entries().contains(&metadata));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), advanced);
}

#[tokio::test]
async fn paid_summary_uses_complete_certified_raw_rows_not_usage_folded_index() {
    let fixture = fixture(SummarySource::UsageOverlay).await;
    let raw_entries = std::fs::read_to_string(&fixture.path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<FileEntry>(line).unwrap())
        .collect::<Vec<_>>();
    let old_leaf = fixture
        .manager
        .lock()
        .await
        .canonical_binding()
        .unwrap()
        .selected_leaf
        .clone();
    let expected =
        pa_core::session_engine::branch_summarization::collect_entries_for_branch_summary(
            &raw_entries,
            old_leaf.as_deref(),
            &fixture.first_assistant,
        )
        .entries;
    let attribution = raw_entries
        .iter()
        .find(|entry| entry.id() == Some("raw-summary-attribution"))
        .unwrap();
    let FileEntry::ChildUsageAttributed { payload, .. } = attribution else {
        panic!("raw attribution fixture");
    };
    let raw_assistant = expected
        .iter()
        .find(|entry| entry.id() == Some(payload.target_id.as_str()))
        .unwrap();
    let raw_id = raw_assistant.id().unwrap();
    {
        let core = fixture.worker.core.lock().unwrap();
        let folded = core.store.as_ref().unwrap().entry(raw_id).unwrap();
        assert_eq!(
            folded.fields["message"]["usage"]["input"],
            json!(7),
            "fixture must exercise the derived usage overlay"
        );
    }
    let FileEntry::Message {
        message: pa_types::session::AgentMessage::Assistant(raw),
        ..
    } = raw_assistant
    else {
        panic!("raw assistant fixture");
    };
    assert_eq!(raw.usage.input, 0);
    fixture
        .navigation
        .navigate_canonical(
            &json!({"targetId":fixture.first_assistant,"summarize":true}),
            fixture.live.clone(),
        )
        .await
        .unwrap();
    assert_eq!(*fixture.summary_sources.lock().unwrap(), vec![expected]);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}
