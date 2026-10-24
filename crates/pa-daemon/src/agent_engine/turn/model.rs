//! The model-turn runner: the streaming run over the built session agent,
//! the retry/failover policy application, and the quota-park mid-run arm.
use super::{
    aborted_message, drop_trailing_assistant, json_round_trip, map_thinking_level,
    retry_event_to_engine_event, AgentSessionEngine, EngineEvent, ProviderTarget, StopReason,
    TurnAdmission, TurnOnce, TurnPrompt, TurnResult,
};

impl AgentSessionEngine {
    /// Drive one admitted prompt through the retry-driver model loop and
    /// emit the turn outcome (provider-failure retries + final-row
    /// surfacing). The trailing `Done` is owned by the caller.
    pub(super) fn run_model_turn(
        &self,
        admission: TurnAdmission,
        prompt: &TurnPrompt,
        boundary_passed: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> TurnResult {
        #[derive(Clone)]
        struct FailoverPrimary {
            model: pa_types::ai::Model,
            thinking_level: pa_agent::types::ThinkingLevel,
            api_key: Option<String>,
            headers: Option<std::collections::BTreeMap<String, String>>,
            session_restored: bool,
        }
        if let Err(error) = self.apply_scheduled_model_metadata_refresh() {
            return TurnResult::Error {
                error: error.to_string(),
                assistant: None,
            };
        }
        // The built target pins the session identity even if the refreshed
        // catalog removes it or changes the startup default.
        let model = match self.session_model() {
            Ok(model) => model,
            Err(error) => {
                return TurnResult::Error {
                    error: error.to_string(),
                    assistant: None,
                };
            }
        };
        let selector = format!("{}/{}", model.provider, model.id);
        let allowlist = crate::model_allowlist::load(&self.cwd(), &self.config.agent_dir);
        if let Err(error) = crate::model_allowlist::assert_allowed(&allowlist, &selector) {
            return TurnResult::Error {
                error: error.to_string(),
                assistant: None,
            };
        }
        // The preflight validates the model SERVING the run: a model whose
        // provider has no configured credential fails the run before the
        // provider request, with the login-guidance message — a routed
        // image-model episode authenticates its own image model.
        let preflight_model = self
            .armed_image_route()
            .map_or_else(|| model.clone(), |route| route.target.model);
        if self.config.faux_script.is_none() && self.current_selection().api_key.is_none() {
            let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
            let mut registry = pa_core::models::ModelRegistry::create(
                auth,
                self.config.agent_dir.join("models.json"),
            );
            registry.load_private_authorization_from_cache();
            if !registry.has_configured_auth(&preflight_model) {
                let uses_oauth = registry
                    .auth
                    .get_all()
                    .credential(&preflight_model.provider)
                    .is_some_and(|credential| {
                        matches!(credential, pa_core::auth::AuthCredential::Oauth { .. })
                    });
                let message = if uses_oauth {
                    format!(
                        "Authentication failed for \"{}\". Credentials may have expired or network is unavailable.\n\nRun /login to update credentials.",
                        preflight_model.provider
                    )
                } else {
                    let docs = pa_core::packages::docs_path();
                    format!(
                        "No API key found for {}.\n\nUse /login to log into a provider via OAuth or API key. See:\n  {}\n  {}",
                        preflight_model.provider,
                        docs.join("providers.md").display(),
                        docs.join("models.md").display()
                    )
                };
                return TurnResult::Error {
                    error: message,
                    assistant: None,
                };
            }
        }
        let agent = match self.session_agent(&model) {
            Ok(agent) => agent,
            Err(error) => {
                return TurnResult::Error {
                    error: format!("{error:#}"),
                    assistant: None,
                };
            }
        };
        // Cancellation is latched at the registered run's first event in
        // run_turn_once. Returning here would discard an already accepted
        // turn without recording its interrupted outcome.
        // A routed image-model episode applies its override BEFORE the
        // first provider call: the serving target swaps to the image
        // model (the agent state itself never swaps — per-run, like TS).
        self.apply_armed_image_route(&agent);
        let episode_model_override = agent.model_override();
        let policy = self.retry_policy();
        let failover_policy = self.failover_policy();
        // A routed episode serves (and fails over within) the ROUTED
        // model: the candidate chain derives from the serving model.
        let candidates = match self.armed_image_route() {
            Some(route) => self.failover_candidates(&route.target.model),
            None => self.failover_candidates(&model),
        };
        // The pa-core retry driver owns the attempt loop; this engine
        // owns one turn. The single `emit` reference is handed through
        // a RefCell slot to whichever closure is currently running.
        let emit_cell = std::cell::RefCell::new(emit);
        // The overflow compact-and-retry re-issues the loop without a new
        // user message, so its turn starts as a continuation (TS
        // `agent.continue()`); ordinary turns start fresh.
        let first_attempt = std::cell::Cell::new(matches!(admission, TurnAdmission::FreshPrompt));
        // Failover switch/restore re-bind the live agent's model, append
        // the model-change row, and re-bind the turn-boundary model facts
        // (`model.info` follows the session model). Unbound sessions restore
        // before tools. Bound sessions flush the owner receipt at the existing
        // settled-loop boundary, outside Agent::event_delivery.
        let core = {
            let guard = self.session.blocking_lock();
            guard.as_ref().cloned()
        };
        let persistence = core.as_ref().map(|core| core.session.shared_persistence());
        // Keep retries on the failed attempt's semantic request identity.
        let semantic_edges = {
            let guard = self.session.blocking_lock();
            guard
                .as_deref()
                .and_then(|engine| engine.session.semantic_edges())
        };
        // The failover-captured primary target state (TS `_backupModel`).
        // Request auth stays on the backup until the active run settles.
        let primary_state = std::sync::Arc::new(std::sync::Mutex::new(None::<FailoverPrimary>));
        let pending_model_changes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<
            pa_types::session::ModelChangeEntry,
        >::new()));
        // The quota-park seam (TS #2375): the retry chain consults the
        // engine at its give-up; a quota failure whose reset exceeds
        // the wait cap parks the session. The weak self keeps the
        // callback `'static`.
        let quota_parked_flag = std::sync::Arc::clone(&self.quota_parked_this_run);
        let engine_weak = self
            .self_weak
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let park: Option<pa_core::session_engine::provider_park::ParkDecisionCallback> =
            Some(&mut move |message, abort| {
                let engine_weak = engine_weak.clone();
                let abort = abort.to_string();
                let quota_parked_flag = std::sync::Arc::clone(&quota_parked_flag);
                Box::pin(async move {
                    let engine = engine_weak.as_ref().and_then(std::sync::Weak::upgrade)?;
                    let outcome = engine.park_for_quota_reset(&message, &abort).await;
                    if outcome.is_some() {
                        quota_parked_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    outcome
                })
            });
        let overflow_window = match self.armed_image_route() {
            Some(route) => route.target.model.context_window,
            None => model.context_window,
        };
        // A bound owner receipt must never wait under Agent::event_delivery.
        // Its existing restore callback runs after run_turn_once has settled,
        // before the retry driver publishes restoration or starts another attempt.
        let canonical_bound = core
            .as_ref()
            .is_some_and(|core| core.session.has_canonical_live_owner());
        let restore_session: pa_core::session_engine::canonical_live::CanonicalAfterAssistantCommitFn = {
            let primary_state = primary_state.clone();
            let pending_model_changes = pending_model_changes.clone();
            let agent_weak = std::sync::Arc::downgrade(&agent);
            let core_weak = core.as_ref().map(std::sync::Arc::downgrade);
            let persistence = persistence.clone();
            let ollama_cloud = self.ollama_cloud.clone();
            let cwd = self.cwd();
            let agent_dir = self.config.agent_dir.clone();
            std::sync::Arc::new(move || {
                let primary_state = primary_state.clone();
                let pending_model_changes = pending_model_changes.clone();
                let agent_weak = agent_weak.clone();
                let core_weak = core_weak.clone();
                let persistence = persistence.clone();
                let ollama_cloud = ollama_cloud.clone();
                let cwd = cwd.clone();
                let agent_dir = agent_dir.clone();
                Box::pin(async move {
                    let primary = primary_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    let Some(mut primary) = primary.filter(|primary| !primary.session_restored)
                    else {
                        return Ok(());
                    };
                    let agent = agent_weak
                        .upgrade()
                        .ok_or_else(|| anyhow::anyhow!("restoration episode Agent retired"))?;
                    let core = match core_weak {
                        Some(core) => Some(core.upgrade().ok_or_else(|| {
                            anyhow::anyhow!("restoration episode Core retired")
                        })?),
                        None => None,
                    };
                    primary.model =
                        ollama_cloud.configure_model(&cwd, &agent_dir, &primary.model);
                    let model = json_round_trip(&primary.model)
                        .ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
                    if let Some(persistence) = persistence {
                        persist_model_change(
                            &persistence,
                            &primary.model.provider,
                            &primary.model.id,
                        )
                        .await?;
                    }
                    // ACK precedes session facts; the active backup run and
                    // provider target remain pinned until the retry driver restores.
                    agent
                        .set_model_and_thinking_level(model, primary.thinking_level)
                        .await;
                    if let Some(core) = core {
                        core.update_model_facts(&primary.model);
                    }
                    pending_model_changes
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(pa_types::session::ModelChangeEntry {
                            provider: primary.model.provider.clone(),
                            model_id: primary.model.id.clone(),
                        });
                    if let Some(primary) = primary_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_mut()
                    {
                        primary.session_restored = true;
                    }
                    Ok(())
                })
            })
        };
        let restore_commit_guard = match core
            .as_ref()
            .and_then(|core| core.session.canonical_live_session())
        {
            Some(live) => match live.register_after_assistant_commit(restore_session.clone()) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    return TurnResult::Error {
                        error: error.to_string(),
                        assistant: None,
                    };
                }
            },
            None => None,
        };
        let restore_subscription = (!canonical_bound).then(|| {
            self.runtime.block_on(async {
                let restore_session = restore_session.clone();
                agent
                    .subscribe(move |event, _signal| {
                        let restore_session = restore_session.clone();
                        Box::pin(async move {
                            if matches!(
                                event,
                                pa_agent::types::AgentEvent::MessageEnd {
                                    message: pa_agent::types::AgentMessage::Standard(
                                        pa_agent::types::Message::Assistant(assistant)
                                    )
                                } if !matches!(
                                    assistant.stop_reason,
                                    StopReason::Error | StopReason::Aborted
                                )
                            ) {
                                restore_session().await?;
                            }
                            Ok(())
                        })
                    })
                    .await
            })
        });
        let restore_primary = || {
            let episode_model_override = episode_model_override.clone();
            let emit_cell = &emit_cell;
            let primary_state = primary_state.clone();
            let agent = agent.clone();
            let persistence = persistence.clone();
            let core = core.clone();
            let primary = primary_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            async move {
                let Some(FailoverPrimary {
                    model: primary_model,
                    thinking_level,
                    api_key: primary_api_key,
                    headers: primary_headers,
                    session_restored,
                }) = primary
                else {
                    return Ok(None);
                };
                let primary_model = self.configure_ollama_model(&primary_model);
                let agent_model = json_round_trip(&primary_model)
                    .ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
                {
                    // The restore serializes with `/reload`'s
                    // live-input refresh: without this fence a
                    // reload landing between the restore's
                    // resolution and its slot write is clobbered
                    // by the restore's older pair — the lock
                    // makes whichever runs last leave the newest
                    // store standing. The whole turn (this
                    // closure included) runs on the blocking
                    // pool — the worker parks the turn there
                    // (`worker/turn.rs`) — so the synchronous
                    // credential resolution below, OAuth refresh
                    // and all, never touches an async executor
                    // worker; only a concurrently reloading
                    // session waits on this span.
                    let _reload_serialized = self
                        .reload_lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // The armed-route install goes through the
                    // fenced helper (no provider-slot lock held
                    // across the route read).
                    if !self.install_armed_route_target() {
                        // The primary's request auth re-resolves at
                        // restore: a credential rotated (and
                        // reloaded) during the failover serves from
                        // the store, never the pre-failover
                        // capture.
                        let target = self.restored_primary_target(
                            &primary_model,
                            primary_api_key,
                            primary_headers,
                        );
                        *self.provider_target.write().expect("provider target lock") = Some(target);
                    }
                }
                if let Some(persistence) = persistence.filter(|_| !session_restored) {
                    persist_model_change(&persistence, &primary_model.provider, &primary_model.id)
                        .await?;
                }
                agent.set_model(agent_model).await;
                agent.set_thinking_level(thinking_level).await;
                agent.set_model_override(episode_model_override);
                if let Some(core) = &core {
                    core.update_model_facts(&primary_model);
                }
                if !session_restored
                    && !(emit_cell.borrow_mut())(EngineEvent::ModelChange(
                        pa_types::session::ModelChangeEntry {
                            provider: primary_model.provider.clone(),
                            model_id: primary_model.id.clone(),
                        },
                    ))
                {
                    anyhow::bail!("Model restoration persistence was cancelled");
                }
                if let Some(primary) = primary_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
                {
                    primary.session_restored = true;
                }
                Ok(Some(format!(
                    "{}/{}",
                    primary_model.provider, primary_model.id
                )))
            }
        };
        let result = self.runtime.block_on(
            pa_core::session_engine::provider_failover::run_turn_with_provider_failover(
                &policy,
                &failover_policy,
                &candidates,
                overflow_window,
                None,
                || {
                    let mut emit = emit_cell.borrow_mut();
                    let first = first_attempt.get();
                    first_attempt.set(false);
                    let agent = agent.clone();
                    let prompt = prompt.clone();
                    let model = model.clone();
                    let pending_model_changes = pending_model_changes.clone();
                    async move {
                        // A retry re-issues the failed turn: the failed
                        // assistant message leaves the loop context first
                        // (TS `messages.slice(0, -1)`), then `continue`.
                        if !first {
                            drop_trailing_assistant(&agent).await;
                        }
                        let mut admit_prompt = first;
                        loop {
                            let assistant = match self
                                .run_turn_once(
                                    &agent,
                                    &prompt,
                                    admit_prompt,
                                    boundary_passed,
                                    &pending_model_changes,
                                    aborted,
                                    &mut **emit,
                                )
                                .await
                            {
                                Ok(TurnOnce::Message { assistant }) => *assistant,
                                Ok(TurnOnce::None) => {
                                    return Err(anyhow::anyhow!("No response produced."));
                                }
                                Ok(TurnOnce::Aborted) => return Ok(aborted_message(&model)),
                                Err(error) => return Err(error),
                            };
                            if !self
                                .threshold_yielded
                                .swap(false, std::sync::atomic::Ordering::SeqCst)
                            {
                                return Ok(assistant);
                            }
                            // Requested boundaries stay owned by run_turns; consuming one here
                            // would lose its explicit stop and goal-continuation semantics.
                            let core = self.session.lock().await.clone();
                            let pending = match core {
                                Some(core) => core.turn_boundary.compaction_scheduled().await,
                                None => false,
                            };
                            if aborted() {
                                return Ok(aborted_message(&model));
                            }
                            if pending
                                || self
                                    .config
                                    .queued_steering_probe
                                    .as_ref()
                                    .is_some_and(|probe| probe())
                            {
                                return Ok(assistant);
                            }
                            if aborted() {
                                return Ok(aborted_message(&model));
                            }
                            // Keep the retry/failover episode alive across this artificial
                            // yield. The engine owns a multi-thread runtime; the synchronous
                            // compaction boundary may re-enter it without holding async locks.
                            let outcome = tokio::task::block_in_place(|| {
                                self.run_auto_compaction(&mut **emit)
                            });
                            self.threshold_checks_enabled.store(
                                outcome.continues_threshold_checks(),
                                std::sync::atomic::Ordering::SeqCst,
                            );
                            if outcome == crate::auto_compaction::AutoCompactionRun::Cancelled
                                || aborted()
                            {
                                return Ok(aborted_message(&model));
                            }
                            if self
                                .config
                                .queued_steering_probe
                                .as_ref()
                                .is_some_and(|probe| probe())
                            {
                                return Ok(assistant);
                            }
                            // Continue the tool run without dropping its successful assistant
                            // or inserting another copy of the admitted user prompt.
                            admit_prompt = false;
                        }
                    }
                },
                |event| {
                    let mut emit = emit_cell.borrow_mut();
                    let semantic_edges = semantic_edges.clone();
                    async move {
                        // TS parks the id at `auto_retry_start` and clears
                        // it at the settle (`_resolveRetry`), so a
                        // body-identical retry reuses the failed
                        // attempt's id; the parked id lives until the
                        // next turn mints over it otherwise.
                        if let Some(recorder) = &semantic_edges {
                            match &event {
                                pa_core::session_engine::auto_retry::AutoRetryEvent::Start {
                                    ..
                                } => recorder.prepare_turn_retry(),
                                pa_core::session_engine::auto_retry::AutoRetryEvent::End {
                                    ..
                                } => {
                                    recorder.clear_turn_retry();
                                }
                            }
                        }
                        let engine_event = retry_event_to_engine_event(event);
                        if !emit(engine_event) {
                            anyhow::bail!("emit cancelled");
                        }
                        Ok(())
                    }
                },
                |delay| {
                    async move {
                        // Abort-aware wait: the cancel flag stops the retry sleep early (TS
                        // `_retryAbortController`).
                        let deadline = tokio::time::Instant::now() + delay;
                        loop {
                            if aborted() {
                                return false;
                            }
                            if tokio::time::Instant::now() >= deadline {
                                return true;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                },
                |next: &pa_types::ai::Model| {
                    let emit_cell = &emit_cell;
                    let agent = agent.clone();
                    let persistence = persistence.clone();
                    let core = core.clone();
                    {
                        let mut primary = primary_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        // Capture the primary state once (TS `_backupModel`), restored when the
                        // successful backup response arrives.
                        if primary.is_none() {
                            let (api_key, headers) = self.resolve_request_key_and_headers(&model);
                            *primary = Some(FailoverPrimary {
                                model: model.clone(),
                                thinking_level: map_thinking_level(self.effective_thinking()),
                                api_key,
                                headers,
                                session_restored: false,
                            });
                        } else if let Some(primary) = primary.as_mut() {
                            primary.session_restored = false;
                        }
                    }
                    let next = self.configure_ollama_model(next);
                    async move {
                        let agent_model = json_round_trip(&next)
                            .ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
                        // Clamp the level to the switched-to model (TS
                        // `clampThinkingLevel`); the primary restores with
                        // the primary.
                        let clamped =
                            pa_ai::models::clamp_thinking_level(&next, self.effective_thinking());
                        // The stream's provider target follows the switch (the same slot
                        // `set_model` swaps).
                        {
                            // The switch's resolve-and-install serializes
                            // with `/reload`'s live-input refresh — the
                            // restore path in the sibling callback takes
                            // the same lock: a reload landing between
                            // this resolution and its slot write must not
                            // be clobbered by the switch's older pair. The
                            // turn runs on the blocking pool, so the
                            // synchronous resolution never touches an
                            // async executor worker.
                            let _reload_serialized = self
                                .reload_lock
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            // A routed episode keeps serving the route's
                            // target across the failover switch — through
                            // the fenced install (the route lock covers
                            // the clone and the slot write together).
                            if !self.install_armed_route_target() {
                                let (api_key, headers) =
                                    self.resolve_request_key_and_headers(&next);
                                let mut target =
                                    self.provider_target.write().expect("provider target lock");
                                *target = Some(ProviderTarget {
                                    service_tier: *self
                                        .service_tier
                                        .read()
                                        .expect("service tier lock"),
                                    api_key,
                                    model: next.clone(),
                                    headers,
                                });
                            }
                        }
                        if let Some(persistence) = persistence {
                            persist_model_change(&persistence, &next.provider, &next.id).await?;
                        }
                        // The session facts may restore before backup tools, but
                        // every continuation of this serving episode must retain
                        // the actual backup model and request options.
                        let serving_target = self
                            .provider_target
                            .read()
                            .expect("provider target lock")
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("failover serving target missing"))?;
                        let serving_model = json_round_trip(&serving_target.model)
                            .ok_or_else(|| anyhow::anyhow!("serving model conversion failed"))?;
                        let serving_thinking = pa_ai::models::clamp_thinking_level(
                            &serving_target.model,
                            self.effective_thinking(),
                        );
                        agent.set_model_override(Some(pa_agent::agent::AgentModelOverride {
                            model: serving_model,
                            thinking_level: map_thinking_level(serving_thinking),
                        }));
                        agent.set_model(agent_model).await;
                        agent.set_thinking_level(map_thinking_level(clamped)).await;
                        if let Some(core) = &core {
                            core.update_model_facts(&next);
                        }
                        if !(emit_cell.borrow_mut())(EngineEvent::ModelChange(
                            pa_types::session::ModelChangeEntry {
                                provider: next.provider.clone(),
                                model_id: next.id.clone(),
                            },
                        )) {
                            anyhow::bail!("Model switch persistence was cancelled");
                        }
                        Ok(())
                    }
                },
                &restore_primary,
                park,
            ),
        );
        // Also restore the previous override when an attempt/listener failed
        // before the retry driver's normal restoration path.
        agent.set_model_override(episode_model_override.clone());
        drop(restore_commit_guard);
        if let Some(subscription) = restore_subscription {
            self.runtime.block_on(subscription.unsubscribe());
        }
        // A failing awaited listener (including a failed model-change
        // append) exits the retry driver before its normal restore callback.
        // Always restore the transport as well, while preserving both errors.
        let result = match result {
            Err(error) => match self.runtime.block_on(restore_primary()) {
                Ok(_) => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "{error:#}; primary restoration also failed: {cleanup:#}"
                )),
            },
            Ok(message) => Ok(message),
        };
        match result {
            Ok(message) => match message.stop_reason {
                // The failure already reached the transcript as the final
                // assistant message; the turn error still travels to
                // headless callers through the turn result.
                StopReason::Error => TurnResult::Error {
                    error: message
                        .error_message
                        .clone()
                        .filter(|error| !error.is_empty())
                        .unwrap_or_else(|| "Assistant response failed".to_string()),
                    assistant: Some(Box::new(message)),
                },
                StopReason::Aborted => TurnResult::Aborted,
                _ => TurnResult::Message(Box::new(message)),
            },
            Err(error) => TurnResult::Error {
                error: error.to_string(),
                assistant: None,
            },
        }
    }
}

/// Model selection is source-independent metadata, but its actual owner receipt
/// must complete before the loop publishes the switched model or the next call.
async fn persist_model_change(
    session: &std::sync::Arc<tokio::sync::Mutex<pa_core::session::manager::SessionManager>>,
    provider: &str,
    model_id: &str,
) -> anyhow::Result<()> {
    pa_core::session::canonical::persist_metadata_entry(
        session,
        pa_types::session::FileEntry::ModelChange {
            base: pa_types::session::EntryBase {
                id: None,
                parent_id: None,
                timestamp: None,
                rest: serde_json::Map::default(),
            },
            payload: pa_types::session::ModelChangeEntry {
                provider: provider.to_owned(),
                model_id: model_id.to_owned(),
            },
        },
    )
    .await?;
    Ok(())
}
