//! Cancellation between worker pickup and the engine's first event.
use super::*;

/// Park the real engine before its first event, after the worker has picked
/// this delivery. The channels prove the race window without a scheduling guess.
struct FirstEventBoundary {
    engine: Arc<crate::agent_engine::AgentSessionEngine>,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl SessionEngine for FirstEventBoundary {
    fn set_session_file(&self, path: std::path::PathBuf) {
        self.engine.set_session_file(path);
    }

    fn set_prompt_context_boundary(&self, leaf_id: Option<String>) {
        self.engine.set_prompt_context_boundary(leaf_id);
    }

    fn abort_in_flight_turn(&self) {
        self.engine.abort_in_flight_turn();
    }

    fn canonical_navigation_session(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<
                        Option<Arc<pa_core::session_engine::canonical_live::CanonicalLiveSession>>,
                    >,
                > + Send
                + '_,
        >,
    > {
        self.engine.canonical_navigation_session()
    }

    fn set_canonical_step_admission(
        &self,
        admission: Option<crate::session_store::canonical_adapter::CanonicalStepAdmission>,
    ) {
        self.engine.set_canonical_step_admission(admission);
    }

    fn run_prompt(
        &self,
        prompt_index: usize,
        request: PromptRequest,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the test releases the first-event boundary");
        }
        self.engine.run_prompt(prompt_index, request, aborted, emit);
    }

    fn run_side_question(
        &self,
        request: SideQuestionRequest,
        signal: &pa_agent::abort::AbortSignal,
        sink: &pa_core::session_engine::side_question::SideQuestionSink,
    ) -> SideQuestionOutcome {
        self.engine.run_side_question(request, signal, sink)
    }

    fn run_compaction(
        &self,
        request: CompactionRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> CompactionOutcome {
        self.engine.run_compaction(request, signal)
    }

    fn run_branch_summary(
        &self,
        request: crate::engine::BranchSummaryRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> crate::engine::BranchSummaryOutcome {
        self.engine.run_branch_summary(request, signal)
    }

    fn rebuild_session_context(
        &self,
        entries: Vec<pa_types::session::FileEntry>,
        goal_reload: pa_core::session_engine::goal_driver::GoalBranchReload,
    ) -> anyhow::Result<()> {
        self.engine.rebuild_session_context(entries, goal_reload)
    }
}

/// Inspect the receipt already adopted by the run. Never refresh it here: doing
/// so would hide a missing adoption/ACK or a stale first-turn delivery ledger.
async fn exact_bound_history(
    engine: &crate::agent_engine::AgentSessionEngine,
    core: &Arc<Mutex<SessionCore>>,
    path: &std::path::Path,
) -> Vec<Value> {
    let owner_leaf = {
        let core = core.lock().unwrap();
        let store = core.store.as_ref().expect("the worker owns its file");
        assert!(
            store.lease.is_some(),
            "the source binding owns a real lease"
        );
        assert!(
            store.is_canonical_bound(),
            "the actual owner receipt path ran"
        );
        store.leaf_id().map(str::to_owned)
    };
    let physical = std::fs::read(path).unwrap();
    let sdk = engine
        .session
        .lock()
        .await
        .clone()
        .expect("the bound SDK was built");
    sdk.session
        .canonical_live_session()
        .expect("not a standalone SDK fixture")
        .ensure_ready()
        .expect("the last raw adoption was ACKed");
    let persistence = sdk.session.shared_persistence();
    {
        let manager = persistence.lock().await;
        let cursor = manager
            .canonical_binding()
            .expect("the SDK names the actual source");
        assert_eq!(cursor.selected_leaf, owner_leaf);
        assert_eq!(manager.canonical_visible_leaf(), owner_leaf.as_deref());
        assert!(
            manager.canonical_pending_admitted_users().is_empty(),
            "the interrupted turn must not leave a delivery for the next turn"
        );
        let mut adopted = manager
            .canonical_raw_header()
            .expect("exact physical header")
            .raw()
            .to_vec();
        for row in manager
            .canonical_raw_full_rows()
            .expect("full owner-certified raw history")
        {
            adopted.extend_from_slice(row.raw());
        }
        // Includes every envelope ID, parent ID, timestamp, full payload and
        // metadata row. No timestamp or UUID normalization is permitted.
        assert_eq!(
            adopted, physical,
            "live canonical bytes equal the actual journal"
        );
    }
    let disk_entries: Vec<Value> = std::str::from_utf8(&physical)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == "message")
        .collect();
    let disk_messages: Vec<Value> = disk_entries
        .iter()
        .map(|row| row["message"].clone())
        .collect();
    let live = sdk.session.agent().state().await;
    let live_messages: Vec<_> = live
        .messages
        .iter()
        .filter_map(crate::engine::session_wire_value)
        .collect();
    assert_eq!(
        live_messages, disk_messages,
        "next-call messages retain exact owner values"
    );
    disk_entries
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // Faux's process-global registry owns the whole flow.
async fn abort_before_first_engine_event_keeps_the_accepted_turn_and_settles_idle() {
    let _faux = crate::agent_engine::tests::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (engine, engine_dir) = crate::agent_engine::tests::faux_engine_with_settings(
        &json!({ "responses": ["normal reply after the early abort"] }),
        1,
    );
    let engine = Arc::new(engine);
    let calls = crate::agent_engine::tests::abort_fixture::count_provider_calls(&engine);
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let boundary: Arc<dyn SessionEngine> = Arc::new(FirstEventBoundary {
        engine: Arc::clone(&engine),
        entered: Mutex::new(Some(entered)),
        release: Mutex::new(released),
    });
    let runner = Arc::new(burst_runner(Arc::clone(&boundary)));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("early-abort.jsonl");
    let mut store =
        crate::session_store::SessionFile::create(engine_dir.path().to_str().unwrap(), None, 0);
    store.set_path(path.clone());
    store.rewrite().unwrap();
    store.lease = Some(Arc::new(
        crate::lease::acquire_runtime_session_lease(&path, &engine_dir.path().join("agent"))
            .expect("the fixture owns the actual canonical file"),
    ));
    let core = Arc::clone(&runner.core);
    {
        let mut core = core.lock().unwrap();
        core.store = Some(store);
        core.cwd = engine_dir.path().to_string_lossy().into_owned();
        core.busy = true; // the pickup owns this delivery, not an unstarted queue item
    }
    // The same concrete factory Worker::new installs. Its live_owner captures
    // this exact SessionFile's weak lease, physical header and canonical path.
    engine.set_initial_context_selection_store(crate::worker::context_selection_store_for_tests(
        Arc::clone(&core),
    ));
    engine.register_arc();
    boundary.set_session_file(path.clone());
    let mut subscription = runner.events.subscribe();
    let (done, completed) = oneshot::channel();
    let turn_runner = Arc::clone(&runner);
    let turn_boundary = Arc::clone(&boundary);
    let turn = tokio::spawn(async move {
        turn_runner
            .run_turn(
                turn_boundary,
                vec![QueuedItem {
                    priority: QueuePriority::Human,
                    preview: None,
                    message: "accepted before the abort".to_string(),
                    custom_message: None,
                    agent_message: None,
                    queue_key: None,
                    admission_id: None,
                    images: Vec::new(),
                    done: Some(done),
                    queue_visible: true,
                    policy: TurnPolicy::Queued,
                    forced_batch: false,
                }],
            )
            .await;
    });
    let entered = tokio::time::timeout(std::time::Duration::from_secs(10), entering).await;
    {
        let mut core = core.lock().unwrap();
        core.abort_requested = true;
        core.queued_input_suspended = true;
    }
    boundary.abort_in_flight_turn(); // no run exists yet; registration must carry the cancel
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("the interrupted turn settles")
        .unwrap();
    entered
        .expect("the worker reached the first-event boundary")
        .unwrap();
    assert!(matches!(completed.await.unwrap(), TurnSettle::Aborted));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(!core.lock().unwrap().busy);
    assert!(core.lock().unwrap().queued_input_suspended);

    let mut events = Vec::new();
    while let Ok(frame) = subscription.try_recv() {
        if frame.outbound_type == "session_event" {
            let outbound: Value = serde_json::from_slice(&frame.payload).unwrap();
            events.push(outbound["event"].clone());
        }
    }
    let rows: Vec<Value> = events
        .iter()
        .filter(|event| event["type"] == "message_end")
        .map(|event| event["message"].clone())
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "both accepted and interrupted rows reach the wire: {events:?}"
    );
    assert_eq!(rows[0]["role"], json!("user"));
    assert_eq!(
        rows[0]["content"],
        json!([{ "type": "text", "text": "accepted before the abort" }])
    );
    assert_eq!(rows[1]["stopReason"], json!("aborted"));
    assert_eq!(rows[1]["content"], json!([{ "type": "text", "text": "" }]));
    assert_eq!(rows[1]["usage"]["totalTokens"], json!(0));
    let ends: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "agent_end")
        .collect();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0]["messages"], json!([rows[1].clone()]));
    // The SDK lifecycle failure has no turn_end. The worker still sends its
    // one transport settle frame, without an assistant or tool-result payload.
    let turn_ends = positions_of(&events, "turn_end");
    assert_eq!(turn_ends.len(), 1);
    assert_eq!(
        events[turn_ends[0]],
        json!({ "type": "turn_end", "error": ABORTED_TURN_SETTLE_ERROR })
    );
    assert!(positions_of(&events, "agent_end")[0] < turn_ends[0]);
    let first_entries = exact_bound_history(&engine, &core, &path).await;
    let disk_rows: Vec<_> = first_entries
        .iter()
        .map(|row| row["message"].clone())
        .collect();
    assert_eq!(disk_rows, rows);

    // A second pickup resumes the same owner and SDK. It must consume only its
    // own delivery, not a stale admitted-user row from the interrupted turn.
    {
        let mut core = core.lock().unwrap();
        core.abort_requested = false;
        core.retry_abort_requested = false;
        core.queued_input_suspended = false;
        core.busy = true;
    }
    let (done, completed) = oneshot::channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        runner.run_turn(
            Arc::clone(&boundary),
            vec![QueuedItem {
                priority: QueuePriority::Human,
                preview: None,
                message: "a new prompt after the interrupted turn".to_string(),
                custom_message: None,
                agent_message: None,
                queue_key: None,
                admission_id: None,
                images: Vec::new(),
                done: Some(done),
                queue_visible: true,
                policy: TurnPolicy::Queued,
                forced_batch: false,
            }],
        ),
    )
    .await
    .expect("the next canonical turn settles without a stale delivery");
    assert!(matches!(completed.await.unwrap(), TurnSettle::Completed));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!core.lock().unwrap().busy);
    let final_entries = exact_bound_history(&engine, &core, &path).await;
    assert_eq!(
        final_entries.len(),
        4,
        "exactly one user and assistant per delivery"
    );
    assert_eq!(
        &final_entries[..2],
        first_entries.as_slice(),
        "the aborted prefix is unchanged"
    );
    assert_eq!(
        final_entries[2]["message"]["content"],
        json!([
            { "type": "text", "text": "a new prompt after the interrupted turn" },
        ])
    );
    assert_eq!(final_entries[3]["message"]["stopReason"], json!("stop"));
    assert_eq!(
        final_entries[3]["message"]["content"],
        json!([
            { "type": "text", "text": "normal reply after the early abort" },
        ])
    );
    let ids: std::collections::HashSet<_> = final_entries
        .iter()
        .map(|row| row["id"].as_str().expect("every canonical row has an ID"))
        .collect();
    assert_eq!(
        ids.len(),
        4,
        "no duplicate canonical identities across the aborted boundary"
    );
}

#[tokio::test]
async fn plain_abort_keeps_only_the_initial_delivery_not_a_queued_continuation() {
    let user = json!({ "role": "user", "content": [{ "type": "text", "text": "accepted" }] });
    let queued = json!({ "role": "user", "content": [{ "type": "text", "text": "not accepted" }] });
    let events = gate_sighting_events(
        vec![
            EngineEvent::UserMessage(user.clone()),
            EngineEvent::UserMessage(queued),
        ],
        true,
        false,
    )
    .await;
    let rows: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "message_end")
        .map(|event| event["message"].clone())
        .collect();
    assert_eq!(rows, vec![user]);
}

#[tokio::test]
async fn compact_suppression_still_drops_the_initial_delivery() {
    let events = gate_sighting_events(
        vec![EngineEvent::UserMessage(json!({
            "role": "user", "content": [{ "type": "text", "text": "suppressed" }],
        }))],
        true,
        true,
    )
    .await;
    assert!(positions_of(&events, "message_end").is_empty());
}
