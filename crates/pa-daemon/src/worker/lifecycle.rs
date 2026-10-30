//! Session lifecycle on the worker: shutdown, replacement handoff,
//! resume, compaction triggers, and the wait-for-settled arms.
use super::{
    hold_is_protected, json, queue_lanes, response_failure, response_success, session_snapshot,
    AgentSessionEngine, DaemonResponse, QueueCheckpoint, QueuePriority, QueuedItem, SessionFile,
    TurnPolicy, Value, Worker, SIDE_QUESTION_SETTLE_TIMEOUT,
};
use crate::engine::SessionEngine;

impl Worker {
    /// `update_snapshot` (supervisor plane, update flow spec §8): a read-only
    /// capture; the queue lanes persist BEFORE replying, so the reported queue
    /// and the durable respawn state agree (`busy` is the continuation signal).
    pub(crate) fn handle_update_snapshot(&self) -> DaemonResponse {
        // Serialize capture with other recovery checkpoints. Never publish a
        // snapshot captured before a newer checkpoint acquired this lock.
        let mut recovery = self.recovery.lock().unwrap();
        let Some(journal) = recovery.as_mut() else {
            return response_failure(
                None,
                "update_snapshot",
                "Worker recovery journal is unavailable",
                None,
            );
        };
        let (core_data, lanes) = {
            let core = self.core.lock().unwrap();
            let store = core.store.as_ref();
            let data = json!({
                "activeSessionId": core.active_session_id,
                "sessionId": store.map(crate::session_store::SessionFile::session_id).unwrap_or_default(),
                "sessionFile": core
                    .store
                    .as_ref()
                    .map(|s| s.path.to_string_lossy().to_string()),
                "cwd": core.cwd,
                "generation": core.generation,
                "runtimeMetadata": {
                    "kind": core.runtime_kind,
                    "rlmChildId": core.rlm_child_id,
                    "parentSessionId": core.parent_session_id,
                    "rlmDepth": core.rlm_depth,
                },
                "queue": {
                    "actions": serde_json::to_value(session_snapshot(&core)).ok(),
                    "steering": core.steering.iter().map(|item| item.message.clone()).collect::<Vec<_>>(),
                    "followUps": core.follow_up.iter().map(|item| item.message.clone()).collect::<Vec<_>>(),
                },
                "busy": core.busy,
                "compacting": core.compacting,
            });
            (data, queue_lanes(&core))
        };
        // Release core before disk I/O, retaining recovery through the append.
        if let Err(error) = journal.record_queue_snapshot(
            core_data["activeSessionId"].as_str().unwrap_or_default(),
            &lanes.steering,
            &lanes.follow_up,
        ) {
            return response_failure(None, "update_snapshot", &error.to_string(), None);
        }
        response_success(None, "update_snapshot", Some(core_data))
    }

    pub(crate) fn release_session_lease(&self) {
        let lease = self
            .core
            .lock()
            .unwrap()
            .store
            .as_mut()
            .and_then(|store| store.lease.take());
        drop(lease);
    }

    /// Graceful stop: the connection loop exits the process after
    /// replying.
    pub(crate) async fn handle_shutdown(&self) -> DaemonResponse {
        // The session is closing: the continuation mint sites and their
        // settle-hook retries bail, but unlike a kill the close KEEPS the
        // resume entry — the scheduled jobs survive for the later wake.
        if let Some(agent_engine) = &self.agent_engine {
            agent_engine.mark_session_closed();
        }
        // Abort the side questions before anything else closes: otherwise the
        // reattached client's pane wedges on a turn no event will ever settle.
        self.side_questions
            .abort_all_and_settle(SIDE_QUESTION_SETTLE_TIMEOUT)
            .await;
        {
            let mut core = self.core.lock().unwrap();
            // The shutdown gate closes FIRST: a racing execute_bash must see the
            // stop before the abort runs — a claim that slipped in after the
            // abort would register a controller nothing later cancels, and the
            // exit leaves its child running.
            core.shutdown_requested = true;
            // The FIRST request arms the orphan monitor's stop deadline: a stop that
            // never completes (a refused close, a caller that died) still ends the
            // process. A retry does not extend it.
            core.shutdown_requested_at
                .get_or_insert_with(std::time::Instant::now);
            core.abort_requested = true;
        }
        // The running user bash goes with the stop: the passivation stop must
        // never leave the user's process running after the worker exits. Each
        // run kills its tree in its own task, so the stop waits (bounded) for
        // the runs to settle (TS `abortBashForClose`).
        self.user_bash.abort_and_settle().await;
        // TS `shutdown` closes through `session.abort()` -> `requestAbort()`:
        // the in-flight turn's fetch cancels now, not at its next event.
        self.engine.abort_in_flight_turn();
        self.work_notify.notify_one();
        // The settle + teardown must happen before the exit this reply unlocks:
        // `std::process` exit runs no destructors, so an undisposed kernel
        // would be orphaned (the #235 leak class).
        self.compaction.abort();
        self.tree_navigation.abort();
        self.await_session_work_settled().await;
        // Final child reports use the parent's canonical metadata authority.
        // Keep it open until the close walk has persisted their final usage.
        // A refused close retains its report identities and lease for an explicit retry.
        if let Err(error) = self
            .close_rlm_children(crate::rlm_children::ChildCloseReason::Shutdown)
            .await
        {
            return response_failure(None, "shutdown", &format!("{error:#}"), None);
        }
        // The epoch drain runs at the external idle boundary, with no core lock
        // held and no remaining child accounting producer admitted by this close.
        self.drain_canonical_epoch().await;
        if let Some(agent_engine) = &self.agent_engine {
            agent_engine.dispose_kernel().await;
        }
        self.release_session_lease();
        // The worker's quit (TS `session_shutdown` reason `quit`): the
        // pane reporter releases its pane as the last write on the wire
        // — awaited here so the release lands before this reply unlocks
        // the process exit, and no late report reclaims the pane.
        let reporter = self.herdr.lock().unwrap().clone();
        reporter.release().await;
        response_success(None, "shutdown", None)
    }

    /// Close the live canonical epoch and wait, bounded, for it to drain.
    ///
    /// The three lock-shape constraints are not style:
    ///
    /// * never inside the `shutdown_requested` scoped lock: `wait_drained` awaits,
    ///   and holding the core lock across it wedges every reader;
    /// * never inside [`Self::await_session_work_settled`]: that function takes the
    ///   core lock at its flag check, so a drain nested there can deadlock;
    /// * a `Pending`, `Blocked` or elapsed wait KEEPS the epoch and mints no
    ///   successor. A refusal is not a drain, and this is not a spin loop.
    ///
    /// The sequence itself is forced by two facts:
    ///
    /// * `wait_drained` WAITS rather than reporting `Pending`, and the epoch's own
    ///   serving anchor counts in every drain view, so every holder must give up
    ///   its handle before the wait or the close hangs for the whole budget;
    /// * dropping an epoch is NOT a clean release. An abandoned anchor poisons the
    ///   epoch and quarantines projection on purpose, so the ordered boundary is
    ///   `CanonicalEpoch::finish_ordered_boundary`, which consumes the epoch.
    async fn drain_canonical_epoch(&self) {
        /// Long enough for a settled turn's final flight to classify, short enough
        /// that an uncertain write cannot hold the exit open.
        const DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(2_000);
        let handle = {
            let mut core = self.core.lock().unwrap();
            // The step slot and the engine's admission both hold epoch clones. Clear
            // them here, under the one lock, so the handle taken below is the last one.
            core.canonical_step.lock().unwrap().take();
            core.store.as_mut().and_then(|store| store.epoch.take())
        };
        if let Some(engine) = self.agent_engine.as_ref() {
            engine.set_canonical_step_admission(None);
        }
        // No epoch attached is the pre-genesis window: nothing to drain, and this is
        // not an error (the counter already states that rows were uncovered).
        let Some(crate::session_store::canonical_adapter::EpochHandle(epoch)) = handle else {
            return;
        };
        let closing = epoch.request_close();
        // Consume the ordered boundary: this releases the serving anchor WITHOUT
        // poisoning, which is the only clean way out. `Arc::try_unwrap` failing means
        // a holder we do not know about still owns a clone; that is recorded, not
        // forced, because dropping the last `Arc` instead would quarantine the epoch.
        let Ok(epoch) = std::sync::Arc::try_unwrap(epoch) else {
            eprintln!(
                "pa-daemon: canonical epoch kept live at shutdown: a handle outlived the drain"
            );
            return;
        };
        epoch.finish_ordered_boundary();
        match tokio::time::timeout(DRAIN_BUDGET, closing.wait_drained()).await {
            Ok(pa_core::session_engine::canonical_coordinator::CanonicalDrainView::Drained) => {
                if let Err(refusal) = closing.finish() {
                    // `Pending`/`Blocked`/`WouldDrainSelf`: the epoch stays live and no
                    // successor is minted. Recorded, never retried in a loop.
                    eprintln!("pa-daemon: canonical epoch kept live at shutdown: {refusal:?}");
                }
            }
            Ok(view) => {
                eprintln!("pa-daemon: canonical epoch did not drain at shutdown: {view:?}");
            }
            Err(_) => {
                eprintln!(
                    "pa-daemon: canonical drain timed out after {DRAIN_BUDGET:?}; epoch kept live"
                );
            }
        }
    }

    /// Wait until no turn or compaction run is in flight: the kernel dispose
    /// must never race a live run. The caller requests the aborts first.
    pub(crate) async fn await_session_work_settled(&self) {
        loop {
            // Register the permit before the flag check: a run settling between
            // the two still wakes this waiter (`notify_waiters` reaches
            // registered futures).
            let notified = self.idle_notify.notified();
            {
                let core = self.core.lock().unwrap();
                if !core.busy && !core.compacting {
                    return;
                }
            }
            notified.await;
        }
    }

    /// The whole-runtime replacement flows retire the live session before
    /// swapping onto the replacement file: the queued actions cancel, the runs
    /// abort and settle, then the runtime retires and the RLM children close.
    /// Tree moves never run this (the kernel stays warm).
    pub(crate) async fn teardown_for_replacement(&self) -> anyhow::Result<()> {
        // Mark the session closed before the children close: the marker keeps
        // settle retries from minting continuations into the retiring session.
        if let Some(agent_engine) = &self.agent_engine {
            agent_engine.mark_session_closed();
        }
        {
            let mut core = self.core.lock().unwrap();
            core.steering.clear();
            core.follow_up.clear();
        }
        self.compaction.abort();
        self.tree_navigation.abort();
        self.await_replacement_settled().await;
        self.engine.teardown_for_replacement().await;
        // A close failure rethrows out of the teardown: the
        // replacement fails with the old runtime already retired.
        self.close_rlm_children(crate::rlm_children::ChildCloseReason::Replaced)
            .await
    }

    /// Close this session's supervisor-backed RLM children: every runtime
    /// teardown that ends the session runs it, cascading to grandchildren
    /// through each child worker's own close with the same reason.
    pub(crate) async fn close_rlm_children(
        &self,
        reason: crate::rlm_children::ChildCloseReason,
    ) -> anyhow::Result<()> {
        let children = self
            .agent_engine
            .as_ref()
            .and_then(|engine| engine.children.clone());
        match children {
            Some(children) => children.close_children(reason).await,
            None => Ok(()),
        }
    }

    /// Relist the bound session's ledger children (TS
    /// `listPassiveRlmSubagents`): awaited where the identity is bound -
    /// create, and the replacement rebind inside the gate - so the scan is
    /// ordered with the close walk that follows a later replacement.
    pub(crate) async fn reseed_rlm_children(&self) {
        if let Some(children) = self
            .agent_engine
            .as_ref()
            .and_then(|engine| engine.children.clone())
        {
            children.reseed_from_ledger().await;
        }
    }

    /// Wait until the replacement teardown can retire the runtime: no
    /// turn and no compaction in flight. Like the navigation settle, the
    /// park rides a timeout backstop - the turn runner notifies the idle
    /// notify when a run settles, but a compaction settle does not, so a
    /// missed wake must not hang the replacement.
    async fn await_replacement_settled(&self) {
        loop {
            let busy = {
                let mut core = self.core.lock().unwrap();
                let busy = core.busy || core.compacting;
                if busy {
                    core.abort_requested = true;
                }
                busy
            };
            if !busy {
                return;
            }
            // The engine abort cancels the in-flight fetch, so the
            // settle does not wait out a pending provider response.
            self.engine.abort_in_flight_turn();
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                self.idle_notify.notified(),
            )
            .await;
        }
    }

    /// The fresh session builds in the background, so the replacement session's
    /// kernel prewarm fires at the replacement, not at the first turn; the
    /// build gate deduplicates it against any racing demand seam.
    pub(crate) fn prewarm_replacement_session(&self) {
        if let Some(agent_engine) = &self.agent_engine {
            let engine = std::sync::Arc::clone(agent_engine);
            tokio::spawn(async move {
                let Ok(model) = engine.resolve_model() else {
                    return;
                };
                let _ = engine.ensure_core_session_async(&model).await;
            });
        }
    }

    /// Rebind the worker onto the replacement session's cwd: the core's cwd
    /// and the engine's cwd slot move onto the target's recorded directory
    /// (nothing old observes the move).
    pub(crate) fn rebind_worker_cwd(&self, cwd: &str) {
        {
            let mut core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            core.cwd = cwd.to_string();
        }
        self.engine.set_cwd(std::path::PathBuf::from(cwd));
    }

    /// Refresh the replacement session's derived state (TS
    /// `refreshReplacedSessionState` on the `sessionReplaced` event): the
    /// moved-to session's depth re-seeds the worker core and the engine's
    /// RLM identity (a resumed subagent keeps its persisted depth). The schedule
    /// catalog rebind runs separately (`bind_scheduled_jobs`), like the
    /// TS dispatch handlers that call `rebindCronJobsToState` after the
    /// runtime call.
    pub(crate) async fn refresh_replaced_session_state(&self) {
        let (rlm_depth, summary, child_script) = {
            let (mut core, inputs) = self.summary_inputs();
            // The moved-to file's persisted depth wins (the replacement
            // carries no create-config depth).
            let rlm_depth = core
                .store
                .as_ref()
                .and_then(SessionFile::rlm_depth)
                .unwrap_or(0);
            core.rlm_depth = rlm_depth;
            let child_script = core.child_script.clone();
            (rlm_depth, self.summary_locked(&core, inputs), child_script)
        };
        // No thinking flag rides the rebind (the create command's level is
        // already resolved on the engine), and the TS replacement runtime
        // carries no inherited max-depth: the moved-to session's persisted
        // chat override, the global setting, the env, or the default
        // resolve it (`_resolveRlmMaxDepth` precedence). The harness's
        // child engine file rides along: TS children inherit the
        // replacement runtime's `sessionConfig`, which the runtime keeps
        // across its swaps.
        match self
            .engine
            .configure_rlm_identity(crate::engine::RlmSessionIdentity {
                rlm_depth,
                rlm_max_depth: None,
                cwd: Some(summary.cwd.clone()),
                session_id: Some(summary.session_id.clone()),
                session_file: summary.session_file.clone(),
                thinking: None,
                child_script,
                // A TS replacement runtime has no semantic spawn.
                semantic_spawn: None,
            }) {
            Ok(()) => self.reseed_rlm_children().await,
            Err(error) => eprintln!("pa-daemon: replacement identity rebind failed: {error:#}"),
        }
        if let Ok(summary_value) = serde_json::to_value(&summary) {
            self.engine.set_session_summary(summary_value);
        }
    }

    /// Bind the live session's schedule catalog: register the artifact
    /// partition, rebind the stored jobs onto the live ids, and start (or
    /// wake) the scheduler. Runs at create and after every replacement swap.
    pub(crate) async fn bind_scheduled_jobs(&self) -> anyhow::Result<()> {
        let binding = {
            let core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::scheduled_jobs::live_binding(&core)
        };
        if let Some((binding, artifact_dir)) = binding {
            self.scheduled.bind_session(binding, artifact_dir).await?;
        }
        Ok(())
    }

    /// Release a suspension that has nothing left to hold. The gate exists to
    /// keep queued user work from firing across an abort/compaction boundary, so
    /// a hold with no such row (an agent-message delivery never counts) and no
    /// compacting window protects nothing; leaving it set wedges an idle session
    /// against every later prompt and agent message (issue #33). The
    /// caller-visible resume sites stay authoritative for a hold that DOES have
    /// parked work.
    pub(crate) fn release_unprotected_suspension(&self) -> bool {
        let unprotected = {
            let mut core = self.core.lock().unwrap();
            let unprotected = core.queued_input_suspended
                && !core.busy
                && !core.compacting
                && !hold_is_protected(&core);
            if unprotected {
                core.queued_input_suspended = false;
            }
            unprotected
        };
        if unprotected {
            self.resume_queued_input();
        }
        unprotected
    }

    /// Clear the queued-input suspension and wake the turn runner so parked
    /// lanes drain; an owed continuation re-evaluates here too.
    pub(crate) fn resume_queued_input(&self) {
        {
            let mut core = self.core.lock().unwrap();
            if core.queued_input_suspended {
                core.queued_input_suspended = false;
            }
        }
        self.work_notify.notify_one();
        if let Some(engine) = self.agent_engine.as_ref() {
            engine.retry_owed_goal_continuation();
        }
    }

    /// Run one compaction and answer with the TS `CompactionResult` wire shape;
    /// skips, aborts, and failures answer with the session's error message.
    pub(crate) async fn handle_compaction(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("compact") {
            return response;
        }
        let custom_instructions = payload
            .get("customInstructions")
            .and_then(Value::as_str)
            .map(str::to_string);
        {
            // `compact()` aborts first, which suspends queued-input admission. A
            // compaction that ends with nothing parked has nothing left to hold,
            // so the suspension is released at every outcome (issue #33); the
            // active-goal branch below resumes again to admit its continuation.
            let mut core = self.core.lock().unwrap();
            core.queued_input_suspended = true;
        }
        let outcome = self
            .compaction
            .run(custom_instructions, &self.idle_notify)
            .await;
        // The pump re-schedules on every outcome: a resume site that cleared the
        // suspension MID-window left its item parked behind the compacting gate;
        // without this wake the parked steer would strand forever.
        self.work_notify.notify_one();
        // Skip/failure/abort leave no turn to hit the runner's own release, so
        // the empty hold is dropped here; a hold over parked work stays and its
        // resume site remains the way out.
        self.release_unprotected_suspension();
        // Mint the owed goal continuation only when nothing is queued;
        // the resume site below clears the suspension gate and wakes
        // the runner (TS `compact()` goal branch).
        if let crate::engine::CompactionOutcome::Compacted { .. } = &outcome {
            let goal_active = self
                .engine
                .goal_state_value()
                .get("status")
                .and_then(Value::as_str)
                == Some("active");
            if goal_active {
                let has_queued = {
                    let core = self.core.lock().unwrap();
                    !core.steering.is_empty() || !core.follow_up.is_empty()
                };
                if !has_queued {
                    // The engine call blocks, so it runs on a blocking thread; a join
                    // failure leaves the continuation un-minted (logged, never silent).
                    let engine = std::sync::Arc::clone(&self.engine);
                    // The mint task's OWN handle, captured at the spawn: a join failure
                    // releases exactly this handle — never the mutable mirror, which a
                    // core rebuild may have re-swapped.
                    let mint_pending_handle = engine.goal_pending_handle();
                    let continuation = tokio::task::spawn_blocking(move || {
                        engine.mint_post_compaction_goal_continuation()
                    })
                    .await
                    .unwrap_or_else(|error| {
                        eprintln!(
                            "pa-daemon: post-compaction goal continuation mint failed: {error}"
                        );
                        // A join failure releases the mint's own captured
                        // handle, so a later boundary may mint.
                        AgentSessionEngine::release_goal_continuation_handle(
                            mint_pending_handle.as_ref(),
                        );
                        None
                    });
                    if let Some(continuation) = continuation {
                        // The state change is durable before the announcement: the mint runs
                        // outside a turn, so the store write rides here, not the emit closure.
                        if let Some(goal) = continuation.goal_update {
                            {
                                let mut core = self.core.lock().unwrap();
                                if let Some(store) = core
                                    .store
                                    .as_mut()
                                    .filter(|store| !store.is_canonical_bound())
                                {
                                    let _ = store.persist_entry(
                                        "custom",
                                        json!({
                                            "customType": pa_core::goals::GOAL_STATE_CUSTOM_TYPE,
                                            "data": goal,
                                        }),
                                    );
                                }
                            }
                            self.emit_worker_event(json!({
                                "type": "goal_update",
                                "goal": goal,
                            }));
                        }
                        {
                            let mut core = self.core.lock().unwrap();
                            core.follow_up.push_back(QueuedItem {
                                priority: QueuePriority::Background,
                                preview: None,
                                message: continuation.request.message,
                                custom_message: continuation.request.custom_message,
                                agent_message: None,
                                queue_key: None,
                                admission_id: None,
                                images: continuation.request.images,
                                done: None,
                                queue_visible: false,
                                policy: TurnPolicy::Injected,
                                forced_batch: false,
                            });
                        }
                        // The admission checkpoint: the continuation is admitted while the
                        // session is idle, so without this record a kill before the turn's
                        // settle would park it on a plain boot.
                        self.checkpoint_queue(QueueCheckpoint::Admitted {
                            operation: "follow_up_queued",
                        });
                        // The item's OWN handle releases at the
                        // admission, never the mutable mirror.
                        AgentSessionEngine::release_goal_continuation_handle(
                            continuation.pending_handle.as_ref(),
                        );
                    }
                }
                // The resume site: clears the suspension and wakes the
                // runner.
                self.resume_queued_input();
            }
        }
        match outcome {
            crate::engine::CompactionOutcome::Compacted { run } => {
                let run = *run;
                response_success(None, "compact", Some(run.result))
            }
            crate::engine::CompactionOutcome::Skipped { message } => {
                response_failure(None, "compact", &message, None)
            }
            crate::engine::CompactionOutcome::Aborted => {
                response_failure(None, "compact", "Compaction cancelled", None)
            }
            crate::engine::CompactionOutcome::Failed { error } => {
                response_failure(None, "compact", &error, None)
            }
        }
    }

    /// The idle park shared by `wait_for_idle` and the headless barrier:
    /// register the permit before the flag check, or a turn that settles
    /// between the check and the await loses its wake
    /// (`notify_waiters` only reaches registered futures).
    pub(crate) async fn wait_until_idle(&self) {
        loop {
            let idle = self.idle_notify.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            {
                let core = self.core.lock().unwrap();
                if !core.busy
                    && (core.steering.is_empty() && core.follow_up.is_empty()
                        || self.input_pauses.paused())
                {
                    return;
                }
            }
            idle.await;
        }
    }

    /// The idle wait, plus `waitForRlmQuiescence` (TS
    /// `waitForHeadlessCompletion`'s strong arm over `agent-session.ts`
    /// `waitForRlmQuiescence`): the barrier also owns descendant work - it
    /// holds past the session's idle until every tracked child run's
    /// settle funnel fires (after the terminal notice is delivered), and
    /// the settle loop re-runs the idle wait, so a settled child's
    /// terminal notice (a queued turn) drains inside the barrier exactly
    /// like TS's "work may start at the child-settlement boundary"
    /// re-check.
    async fn wait_until_quiescent(&self, payload: &Value) {
        let children = if payload.get("waitForRlmQuiescence").and_then(Value::as_bool) == Some(true)
        {
            self.agent_engine
                .as_ref()
                .and_then(|engine| engine.children.clone())
        } else {
            None
        };
        loop {
            self.wait_until_idle().await;
            let Some(children) = children.as_ref() else {
                return;
            };
            // Register the permit before the unsettled-work read: a run
            // that settles between the read and the await still wakes
            // this waiter.
            let settled = children.settle_notified();
            if !children.any_running().await {
                // A settle funnel queues its terminal-notice follow-up BEFORE it
                // marks the run settled, so one more idle wait drains the owed
                // notices before the barrier answers.
                self.wait_until_idle().await;
                // A notice can start new child work during that drain (a settle
                // hook spawning a descendant): re-read before answering, so the
                // barrier holds for the new work too.
                if !children.any_running().await {
                    return;
                }
                continue;
            }
            settled.await;
        }
    }

    /// `wait_for_idle`: park until the session is idle;
    /// `waitForRlmQuiescence` also holds until its RLM children settled
    /// (the parent-side settle watcher and `collect` wait out a child's
    /// whole subtree this way).
    pub(crate) async fn handle_wait_for_idle(&self, payload: &Value) -> DaemonResponse {
        self.wait_until_quiescent(payload).await;
        response_success(None, "wait_for_idle", None)
    }

    /// `wait_for_headless_completion` (TS daemon command): settle the
    /// headless run first (same idle wait as `wait_for_idle`), then answer
    /// the autonomous-run accounting snapshot (`DaemonAutonomousStatus`).
    pub(crate) async fn handle_wait_for_headless_completion(
        &self,
        payload: &Value,
    ) -> DaemonResponse {
        if let Err(response) = self.require_created("wait_for_headless_completion") {
            return response;
        }
        self.wait_until_quiescent(payload).await;
        // The idle wait finished, so no turn holds the accounting state;
        // the snapshot read cannot interleave with a running turn.
        let status = self
            .engine
            .autonomous_status()
            .await
            .unwrap_or_else(pa_core::autonomous::disabled_autonomous_status);
        response_success(
            None,
            "wait_for_headless_completion",
            Some(serde_json::to_value(&status).unwrap_or(Value::Null)),
        )
    }
}
/// Agents-view visibility is message-based: a resident subagent is visible
/// before its first message; a message-less top-level session is a draft the
/// view hides. A busy turn is live, so a mid-turn roster delta must never
/// classify the running session as a draft.
pub(super) fn active_lifecycle(runtime_kind: &str, messageless: bool, busy: bool) -> &'static str {
    if runtime_kind == "subagent" || !messageless || busy {
        "live"
    } else {
        "draft"
    }
}
