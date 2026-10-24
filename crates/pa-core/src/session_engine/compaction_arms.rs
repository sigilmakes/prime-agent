use super::compact_session::CompactOutcome;
use super::{
    compaction, compaction_trace, ipython_state, provider_adapter, rebuilt_loop_messages,
    session_message_to_loop, standard_message, AgentMessage, AgentSession, FileEntry,
    SessionAgentMessage, TrailingAssistantFilter,
};

impl AgentSession {
    pub async fn latest_compaction_timestamp(&self) -> Option<u64> {
        let state = self.agent.state().await;
        state
            .messages
            .iter()
            .filter_map(|message| serde_json::to_value(message).ok())
            .filter_map(|value| serde_json::from_value::<SessionAgentMessage>(value).ok())
            .filter_map(|message| match message {
                SessionAgentMessage::CompactionSummary(summary) => Some(summary.timestamp),
                _ => None,
            })
            .max()
    }

    /// Whether an automatic threshold compaction is due at a turn boundary:
    /// the live loop context over the model's context window against the
    /// effective threshold; usage before the latest compaction never re-triggers.
    pub async fn auto_compaction_due(&self, model: &pa_types::ai::Model) -> bool {
        let model = self.configure_ollama_model(model);
        let state = self.agent.state().await;
        // The live loop context is the agent's message list (the same
        // JSON round-trip `compact` uses).
        let messages: Vec<SessionAgentMessage> = state
            .messages
            .iter()
            .filter_map(|message| serde_json::to_value(message).ok())
            .filter_map(|value| serde_json::from_value(value).ok())
            .collect();
        compaction::threshold_compaction_due(
            &messages,
            model.context_window,
            // The live thinking level decides whether the request folds a
            // thinking budget on top of the base output budget.
            compaction::request_output_budget(
                &model,
                provider_adapter::model_thinking_level(state.thinking_level),
            ),
            &self.compaction_settings(),
        )
    }

    /// Remove the trailing assistant message from the loop context, so a
    /// re-issued request does not re-send the failed turn's error message;
    /// [`TrailingAssistantFilter::ErrorOnly`] drops only an error assistant.
    pub async fn drop_trailing_assistant(&self, filter: TrailingAssistantFilter) {
        let state = self.agent.state().await;
        let mut messages = state.messages;
        let matches_filter = |message: &pa_agent::types::AgentMessage| {
            let Some(pa_agent::types::Message::Assistant(assistant)) = standard_message(message)
            else {
                return false;
            };
            match filter {
                TrailingAssistantFilter::Any => true,
                TrailingAssistantFilter::ErrorOnly => {
                    assistant.stop_reason == pa_agent::types::StopReason::Error
                }
            }
        };
        if messages.last().is_some_and(&matches_filter) {
            messages.pop();
            self.agent.set_messages(messages).await;
        }
    }

    /// Drop the failed continuation pair from the live loop context: the
    /// trailing no-progress assistant row and the `goal_context` continuation
    /// row that drove it. A USER turn's corpse stays; only the live loop drops
    /// (like [`Self::drop_trailing_assistant`]).
    pub async fn drop_failed_goal_continuation(&self) {
        // The whole drop runs under ONE state lock (the atomic mutate): a
        // concurrent append cannot drop rows between the snapshot and replace.
        self.agent
            .mutate_messages(|messages| {
                // The failed assistant row: the LAST assistant, not the last row —
                // a trailing `provider_retry_outcome` disclosure must not hide
                // the pair from the cleanup.
                let Some(corpse_index) = messages
                    .iter()
                    .rposition(|message| standard_message(message).is_some())
                else {
                    return;
                };
                let Some(pa_agent::types::Message::Assistant(corpse)) =
                    messages.get(corpse_index).and_then(standard_message)
                else {
                    return;
                };
                let no_progress = corpse.stop_reason == pa_agent::types::StopReason::Error
                    || super::goal_driver::turn_produced_no_output(corpse);
                if !no_progress {
                    return;
                }
                // The driving continuation row sits under the corpse: scan backward
                // over Custom rows only — the first goal_context continuation row wins.
                let goal_context_row_at = messages[..corpse_index]
                    .iter()
                    .enumerate()
                    .rev()
                    .take_while(|(_, message)| {
                        matches!(message, pa_agent::types::AgentMessage::Custom(_))
                    })
                    .find(|(_, message)| {
                        let pa_agent::types::AgentMessage::Custom(custom) = message else {
                            return false;
                        };
                        custom
                            .payload
                            .get("customType")
                            .and_then(serde_json::Value::as_str)
                            == Some("goal_context")
                            && custom
                                .payload
                                .get("details")
                                .and_then(|details| details.get("kind"))
                                .and_then(serde_json::Value::as_str)
                                == Some("continuation")
                    })
                    .map(|(index, _)| index);
                let Some(context_index) = goal_context_row_at else {
                    return;
                };
                // Remove the later index first so the earlier one keeps its
                // position.
                messages.remove(corpse_index);
                messages.remove(context_index);
            })
            .await;
    }

    /// The last assistant message in the live loop context, in the session
    /// wire shape: trailing non-assistant rows are skipped.
    pub async fn last_assistant_message(&self) -> Option<SessionAgentMessage> {
        let state = self.agent.state().await;
        state.messages.iter().rev().find_map(|message| {
            let value = serde_json::to_value(message).ok()?;
            let message: SessionAgentMessage = serde_json::from_value(value).ok()?;
            matches!(message, SessionAgentMessage::Assistant(_)).then_some(message)
        })
    }

    /// Execute `/compact`: summarize the pre-cut prefix, persist the entry,
    /// and rebuild the loop context summary-first; a skip leaves the session
    /// untouched, an aborted run never commits.
    ///
    /// # Errors
    ///
    /// Returns the abort error, or the summarizer/persist failure; a skip is a normal `Ok` outcome.
    ///
    /// # Panics
    ///
    /// Panics when the compaction summary sink slot's mutex is poisoned.
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
        abort: Option<&pa_agent::abort::AbortSignal>,
    ) -> anyhow::Result<CompactOutcome> {
        // The engine's own compaction scope: TS never reviews while
        // `isCompacting` (`agent-session.ts:9318-9319`).
        let _compacting = self.turn_activity.begin_compaction();
        compaction_trace::trace(
            "compact.enter",
            &serde_json::json!({
                "customInstructions": custom_instructions.is_some(),
            }),
        );
        let started_at = std::time::Instant::now();
        let summary_delta = self
            .compaction_summary_sink
            .lock()
            .expect("compaction summary sink lock")
            .clone();
        let model = self.configure_ollama_model(model);
        let options = crate::session_engine::compact_session::CompactOptions {
            inference: self.auxiliary_inference(),
            model,
            api_key,
            custom_instructions,
            settings: self.compaction_settings(),
            abort,
            // Captured per attempt inside `compaction_attempts`: a
            // conflict retry summarizes a new branch, and inputs captured
            // here would rank the abandoned branch's terms.
            auxiliary: self.auxiliary_model.as_ref(),
            summary_delta,
            semantic_edges: self.semantic_edges(),
        };
        // The flight spans the attempts, the rebuilt-context replace, and
        // the kernel-state notice: every path here REPLACES the live loop
        // context, and `record_compaction_outcome`'s failure row
        // take the same flight. A row pushed onto the context must not
        // interleave with the replace: it would duplicate in the rebuilt
        // view or vanish under it while staying durable either way.
        let _flight = if self.canonical_live.is_none() {
            Some(self.compaction_flight.lock().await)
        } else {
            None
        };
        let mut outcome = self.compaction_attempts(&options, started_at).await?;
        if matches!(outcome, CompactOutcome::Skipped(_)) {
            compaction_trace::trace("compact.skipped", &serde_json::Value::Null);
            return Ok(outcome);
        }
        let accepted = self
            .accepted_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let rebuilt = match accepted {
            Some(messages) => messages,
            None => self.context_messages().await?,
        };
        let loop_messages: Vec<AgentMessage> = rebuilt_loop_messages(rebuilt);
        let rebuilt_message_count = loop_messages.len();
        self.agent.set_messages(loop_messages).await;
        compaction_trace::trace(
            "compact.rebuilt_context",
            &serde_json::json!({ "messages": rebuilt_message_count }),
        );
        // A kernel that survived the compaction gets its persistence notice —
        // a durable `ipython_state` row that also keeps a back-to-back second
        // `/compact` preparing (update mode) instead of skipping.
        let kernel_state = match self.kernel_state.as_ref() {
            Some(probe) => {
                ipython_state::sync_after_compaction(probe.as_ref(), &self.session, &self.agent)
                    .await?
            }
            None => None,
        };
        let notice_landed = kernel_state.is_some();
        if let CompactOutcome::Ran(run) = &mut outcome {
            run.ipython_state = kernel_state;
        }
        compaction_trace::trace(
            "compact.returned",
            &serde_json::json!({
                "notice": notice_landed,
            }),
        );
        self.notify_compaction_settled(&outcome).await;
        Ok(outcome)
    }

    /// The PREPARE -> SUMMARIZE -> COMMIT loop: prepare under the session
    /// lock, summarize with no lock held (mid-window appends chain onto
    /// the leaf and ride the retained tail), then commit under the lock —
    /// a structural conflict retries from PREPARE, an aborted run stops.
    async fn compaction_attempts(
        &self,
        options: &crate::session_engine::compact_session::CompactOptions<'_>,
        started_at: std::time::Instant,
    ) -> anyhow::Result<CompactOutcome> {
        // Only an unconsumed prepared ticket is eligible for abandonment. Once
        // commit takes the ticket, all I/O/receipt failures keep their quarantine.
        macro_rules! before_commit {
            ($ticket:ident, fail: $error:expr) => {{
                let original: anyhow::Error = $error.into();
                if let Some(ticket) = $ticket.take() {
                    if let Err(abandon) = ticket.abandon().await {
                        return Err(anyhow::Error::from(abandon).context(format!(
                            "precommit compaction failed ({original}); abandoning its prepared receipt also failed"
                        )));
                    }
                }
                return Err(original);
            }};
            ($ticket:ident, $result:expr) => {{
                match $result {
                    Ok(value) => value,
                    Err(error) => before_commit!($ticket, fail: error),
                }
            }};
        }
        /// The conflict-retry bound: a branch that keeps changing under
        /// the compaction fails instead of re-summarizing forever.
        const MAX_COMPACTION_ATTEMPTS: usize = 3;
        if self.plugins.is_some() {
            return self.strategy_compaction_attempts(options, started_at).await;
        }
        for _ in 0..MAX_COMPACTION_ATTEMPTS {
            let mut live_ticket = match &self.canonical_live {
                Some(live) => Some(
                    live.prepare_mutation(
                        crate::session::canonical::CanonicalMutationKind::ContextPolicy,
                    )
                    .await?,
                ),
                None => None,
            };
            let attempt = {
                let session = self.session.lock().await;
                crate::session_engine::compact_session::prepare_attempt(&session, options)
            };
            let mut attempt = match attempt {
                Ok(attempt) => attempt,
                Err(skip) => {
                    if let Some(ticket) = live_ticket {
                        ticket.abandon().await?;
                    }
                    return Ok(CompactOutcome::Skipped(skip.user_message()));
                }
            };
            // The digest ranks the branch THIS attempt summarizes: a
            // conflict retry prepares a new tree, and inputs captured
            // once before the loop would rank the abandoned branch's
            // terms. The capture takes the session lock itself, so it
            // runs off the prepare hold.
            attempt.history = super::refinement_gate::without_refinements(attempt.history);
            attempt.turn_prefix_messages =
                super::refinement_gate::without_refinements(attempt.turn_prefix_messages);
            compaction_trace::trace("compact.digest_captured", &serde_json::Value::Null);
            let attempt_options = crate::session_engine::compact_session::CompactOptions {
                inference: options.inference.clone(),
                model: options.model.clone(),
                api_key: options.api_key.clone(),
                custom_instructions: options.custom_instructions,
                settings: options.settings,
                abort: options.abort,
                auxiliary: options.auxiliary,
                summary_delta: options.summary_delta.clone(),
                semantic_edges: options.semantic_edges.clone(),
            };
            let summarize = crate::session_engine::compact_session::summarize_attempt(
                &attempt,
                &attempt_options,
            );
            // Cancel only provider work. The prepared transaction stays here so
            // the abort result can await its exact known-unwritten abandonment.
            // Dropping the summary future cancels its detached HTTP provider.
            let summarized = match options.abort {
                Some(signal) if live_ticket.is_some() => {
                    match pa_agent::abort::race_with_abort(summarize, signal).await {
                        Ok(result) => result,
                        Err(error) => Err(error),
                    }
                }
                _ => summarize.await,
            };
            let mut prepared = before_commit!(live_ticket, summarized);
            // A toggle during the summarizer request must not re-inject the captured digest.
            prepared.entry.harness_digest = None;
            prepared.entry.harness_state_fingerprint = None;
            let committed = if live_ticket.is_some() {
                before_commit!(
                    live_ticket,
                    pa_agent::abort::throw_if_aborted_signal(options.abort)
                );
                let draft = before_commit!(
                    live_ticket,
                    crate::session::canonical::CanonicalEntryDraft::new(
                        crate::session::canonical::CanonicalMutationKind::ContextPolicy,
                        FileEntry::Compaction {
                            base: super::canonical_live::draft_base(),
                            payload: prepared.entry.clone(),
                        },
                    )
                );
                let ticket = live_ticket
                    .take()
                    .expect("checked prepared canonical ticket");
                ticket.commit(draft).await?;
                if let Some(compaction) = attempt.semantic_compaction.as_mut() {
                    compaction.commit();
                }
                true
            } else {
                let mut session = self.session.lock().await;
                crate::session_engine::compact_session::commit_attempt(
                    &mut session,
                    &mut attempt,
                    &prepared,
                    options.abort,
                )?
            };
            if committed {
                return Ok(CompactOutcome::Ran(Box::new(
                    crate::session_engine::compact_session::CompactRun {
                        result: prepared.result,
                        entry: prepared.entry,
                        duration_ms: started_at.elapsed().as_millis() as u64,
                        ipython_state: None,
                    },
                )));
            }
            pa_agent::abort::throw_if_aborted_signal(options.abort)?;
        }
        Err(anyhow::anyhow!(
            "compaction retried {MAX_COMPACTION_ATTEMPTS} times while the branch kept changing; try again"
        ))
    }

    /// Record an unsuccessful compaction outcome: append the durable
    /// `compaction_outcome` row and push it onto the live loop context. The
    /// row is a user-facing disclosure, never model context (`convert_to_llm` drops it).
    ///
    /// # Errors
    ///
    /// Returns an error when the row cannot be appended or surfaced;
    /// it is retained in memory either way.
    pub async fn record_compaction_outcome(
        &self,
        reason: crate::session_engine::messages::CompactionOutcomeReason,
        outcome: crate::session_engine::messages::CompactionOutcomeKind,
        content: &str,
    ) -> anyhow::Result<pa_types::session::CustomMessage> {
        // The same flight as the other live-context replacers: the push
        // below read-modify-writes the live context, and a replace
        // interleaving it would roll the pushed row — or the replace —
        // back.
        let _flight = if self.canonical_live.is_none() {
            Some(self.compaction_flight.lock().await)
        } else {
            None
        };
        let row = crate::session_engine::messages::create_compaction_outcome_message(
            content, reason, outcome,
        );
        if self.canonical_live.is_some() {
            self.persist_metadata_entry(FileEntry::CustomMessage {
                base: super::canonical_live::draft_base(),
                payload: pa_types::session::CustomMessageEntry {
                    custom_type: row.custom_type.clone(),
                    content: row.content.clone(),
                    display: row.display,
                    details: row.details.clone(),
                    rest: row.rest.clone(),
                },
            })
            .await?;
        } else {
            let mut session = self.session.lock().await;
            let (_, write_error) = session.append_custom_message_retained(
                &row.custom_type,
                row.content.clone(),
                row.display,
                row.details.clone(),
            );
            if let Some(error) = write_error {
                eprintln!("pa-core: compaction outcome row not persisted: {error}");
            }
        }
        if let Some(loop_message) =
            session_message_to_loop(&SessionAgentMessage::Custom(row.clone()))
        {
            let state = self.agent.state().await;
            let mut messages = state.messages;
            messages.push(loop_message);
            self.agent.set_messages(messages).await;
        }
        Ok(row)
    }

    /// Rebuild the live loop context from a durable branch: the session adopts
    /// the branch entries, and the agent's message list rebuilds from the state.
    ///
    /// # Errors
    ///
    /// Returns an error when the post-navigation history cannot be read.
    pub async fn rebuild_branch_context(
        &self,
        branch_entries: Vec<FileEntry>,
    ) -> anyhow::Result<()> {
        self.rebuild_branch_context_at(branch_entries, &super::ContextStartupBoundary::Current, &[])
            .await
    }

    /// Adopt complete canonical branch entries and publish only the selected
    /// strategy's verified pre-admission projection, without raw-view repair.
    ///
    /// # Errors
    /// Invalid ancestry or admission IDs, branch races, retired strategies and
    /// invalid archive/views fail before canonical adoption or live publication.
    pub async fn rebuild_branch_context_at(
        &self,
        branch_entries: Vec<FileEntry>,
        boundary: &super::ContextStartupBoundary,
        admitted_user_ids: &[String],
    ) -> anyhow::Result<()> {
        let _flight = self.compaction_flight.lock().await;
        let (expected_leaf, session_id) = {
            let session = self.session.lock().await;
            (
                session.get_leaf_id().map(str::to_owned),
                session.get_session_id().to_owned(),
            )
        };
        if !matches!(boundary, super::ContextStartupBoundary::Current)
            && branch_entries.iter().any(|entry| {
                matches!(entry,
                FileEntry::Header { header } if header.id != session_id)
            })
        {
            anyhow::bail!("context branch belongs to another session");
        }
        let leaf = branch_entries
            .iter()
            .rev()
            .find_map(|entry| entry.id().map(str::to_owned));
        let target = self
            .plugins
            .as_ref()
            .map(|plugins| plugins.context_selection())
            .transpose()?;
        let (context, projection_leaf) =
            if matches!(boundary, super::ContextStartupBoundary::Current) {
                if !admitted_user_ids.is_empty() {
                    anyhow::bail!("Current startup context cannot carry admitted-user proof");
                }
                let context = match &target {
                    Some(target) => super::context_strategy::project(
                        &branch_entries,
                        leaf.as_deref(),
                        &format!("{}/{}", target.plugin, target.name),
                    ),
                    None => crate::session::build_session_context(&branch_entries, leaf.as_deref()),
                };
                (context, leaf.clone())
            } else {
                let route = target.as_ref().map_or_else(
                    || crate::plugins::context::DEFAULT_STRATEGY.to_owned(),
                    |target| format!("{}/{}", target.plugin, target.name),
                );
                super::context_strategy::startup::project_at(
                    &branch_entries,
                    leaf.as_deref(),
                    &route,
                    boundary,
                    admitted_user_ids,
                )?
            };
        let model = self
            .context_model
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let rebuilt = match (&self.plugins, &target) {
            (Some(plugins), Some(target)) => {
                super::context_strategy::assemble_for_strategy(
                    plugins,
                    target,
                    context.messages,
                    context.archive.as_ref(),
                    model.as_ref(),
                    projection_leaf.as_deref(),
                )
                .await?
            }
            _ => context.messages,
        };
        {
            let mut session = self.session.lock().await;
            if session.get_leaf_id() != expected_leaf.as_deref() {
                anyhow::bail!("session branch changed while preparing a context view; try again");
            }
            let commit = || {
                session.adopt_entries(branch_entries);
                Ok(())
            };
            match (&self.plugins, &target) {
                (Some(plugins), Some(target)) => plugins.with_strategy_authority(target, commit)?,
                _ => commit()?,
            }
        }
        *self
            .context_archive
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = context.archive;
        self.agent
            .set_messages(rebuilt_loop_messages(rebuilt))
            .await;
        Ok(())
    }
}
