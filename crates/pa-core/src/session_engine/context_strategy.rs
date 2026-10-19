//! Session context policy and the ordered, generation-bound compaction transaction.
//! Canonical rows never become strategy-owned storage. Lua owns selection, assembly,
//! and chain order; this module owns immutable capture and durable acceptance.

use anyhow::{bail, Context};
use pa_types::session::{AgentMessage, ContextArchive, ContextRepresentation, FileEntry};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tokio_util::sync::CancellationToken;

use super::{
    compact_session::{
        self, CompactOptions, CompactOutcome, CompactionAttempt, PreparedCompaction,
    },
    compaction_exec::{compaction_entry_for, CompactionDetails, CompactionResult},
    plugins::SessionPlugins,
    AgentSession,
};
use crate::plugins::{Contribution, PluginError};

mod input;
mod native;
mod selection;
pub(super) mod startup;
pub(crate) use selection::{context_snapshot, project};
pub use startup::ContextStartupBoundary;

/// Snapshot of the canonical active branch and its model-facing view. No mutable
/// session references cross a Lua, model, renderer, or disk-read await point.
pub(crate) struct Preparation {
    pub branch: Vec<FileEntry>,
    pub leaf: Option<String>,
    pub messages: Vec<AgentMessage>,
    pub archive: Option<ContextArchive>,
}

fn terminal(error: &anyhow::Error) -> bool {
    pa_agent::abort::is_abort_error(error)
        || matches!(
            error.downcast_ref::<PluginError>(),
            Some(PluginError::Retired | PluginError::Cancelled | PluginError::Missing)
        )
}

fn route(target: &Contribution) -> String {
    format!("{}/{}", target.plugin, target.name)
}

/// Reject a missing link or cycle rather than compacting a partial branch.
fn active_branch(entries: &[FileEntry], leaf: Option<&str>) -> anyhow::Result<Vec<FileEntry>> {
    let by_id: HashMap<_, _> = entries
        .iter()
        .filter_map(|entry| entry.id().map(|id| (id, entry)))
        .collect();
    let mut seen = HashSet::new();
    let mut branch = Vec::new();
    let mut current = leaf;
    while let Some(id) = current {
        if !seen.insert(id) {
            bail!("context branch contains a parent cycle");
        }
        let entry = by_id
            .get(id)
            .with_context(|| format!("context branch is missing entry {id}"))?;
        branch.push((*entry).clone());
        current = entry.parent_id();
    }
    branch.reverse();
    Ok(branch)
}

/// Append-only suffix through a captured leaf. Ordinary descendants are valid;
/// missing links, cycles, branch departure and intervening compaction are not.
fn descendant_entries(
    entries: &[FileEntry],
    from: Option<&str>,
    leaf: Option<&str>,
) -> Option<Vec<FileEntry>> {
    let by_id: HashMap<_, _> = entries
        .iter()
        .filter_map(|entry| entry.id().map(|id| (id, entry)))
        .collect();
    let mut current = leaf;
    let mut seen = HashSet::new();
    let mut tail = Vec::new();
    while current != from {
        let id = current?;
        if !seen.insert(id) {
            return None;
        }
        let entry = *by_id.get(id)?;
        if matches!(entry, FileEntry::Compaction { .. }) {
            return None;
        }
        tail.push(entry.clone());
        current = entry.parent_id();
    }
    tail.reverse();
    Some(tail)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    #[serde(default)]
    compactors: Vec<String>,
    policy: String,
}

async fn guarded<T>(
    plugins: &SessionPlugins,
    strategy: &Contribution,
    compactor: Option<&Contribution>,
    abort: Option<&pa_agent::abort::AbortSignal>,
    work: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    pa_agent::abort::throw_if_aborted_signal(abort)?;
    if !plugins.context_is_current(strategy) {
        return Err(PluginError::Retired.into());
    }
    let retired = plugins.context_retirement(strategy)?;
    let stage_retired = compactor
        .map(|target| plugins.context_retirement(target))
        .transpose()?;
    let never = pa_agent::abort::AbortSignal::never();
    let signal = abort.unwrap_or(&never);
    let stage = stage_retired.unwrap_or_default();
    let value = tokio::select! { biased;
        () = signal.aborted() => return Err(pa_agent::abort::aborted_error()),
        () = retired.cancelled() => return Err(PluginError::Retired.into()),
        () = stage.cancelled() => return Err(PluginError::Retired.into()),
        value = work => value,
    }?;
    pa_agent::abort::throw_if_aborted_signal(abort)?;
    if !plugins.context_is_current(strategy)
        || compactor.is_some_and(|target| !plugins.context_is_current(target))
    {
        return Err(PluginError::Retired.into());
    }
    Ok(value)
}

async fn guarded_candidate<T>(
    plugins: &SessionPlugins,
    strategy: &Contribution,
    work: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    if !plugins.context_contribution_is_current(strategy) {
        return Err(PluginError::Retired.into());
    }
    let retired = plugins.context_retirement(strategy)?;
    let result = tokio::select! {biased;()=retired.cancelled()=>return Err(PluginError::Retired.into()), result=work=>result}?;
    if !plugins.context_contribution_is_current(strategy) {
        return Err(PluginError::Retired.into());
    }
    Ok(result)
}

/// Run the selected strategy for startup, restore, branch navigation, and compact
/// rebuild. Already accepted archives are checked by the current schema, not a
/// process UUID. The returned candidate is not published by this function.
pub(crate) async fn assemble(
    plugins: &SessionPlugins,
    messages: Vec<AgentMessage>,
    archive: Option<&ContextArchive>,
    model: Option<&pa_types::ai::Model>,
) -> anyhow::Result<Vec<AgentMessage>> {
    assemble_at(plugins, messages, archive, model, None).await
}

pub(crate) async fn assemble_at(
    plugins: &SessionPlugins,
    messages: Vec<AgentMessage>,
    archive: Option<&ContextArchive>,
    model: Option<&pa_types::ai::Model>,
    leaf: Option<&str>,
) -> anyhow::Result<Vec<AgentMessage>> {
    let strategy = plugins.context_selection()?;
    assemble_for_strategy(plugins, &strategy, messages, archive, model, leaf).await
}

pub(super) async fn assemble_for_strategy(
    plugins: &SessionPlugins,
    strategy: &Contribution,
    messages: Vec<AgentMessage>,
    archive: Option<&ContextArchive>,
    model: Option<&pa_types::ai::Model>,
    leaf: Option<&str>,
) -> anyhow::Result<Vec<AgentMessage>> {
    if let Some(archive) = archive {
        if archive.version != 1 || archive.strategy != route(strategy) {
            bail!("accepted context archive is not compatible with the selected strategy");
        }
        let covered: HashSet<_> = archive.covered_entry_ids.iter().collect();
        if archive.prepared_leaf.is_empty()
            || covered.is_empty()
            || covered.len() != archive.covered_entry_ids.len()
            || archive.covered_entry_ids.iter().any(String::is_empty)
            || (!matches!(
                archive.representation,
                ContextRepresentation::Standard { .. }
            ) && !covered.contains(&archive.prepared_leaf))
        {
            bail!("accepted context archive has invalid coverage");
        }
        let frame = input::Frame::new(
            plugins.context_session_id(),
            strategy,
            &messages,
            "restore",
            leaf,
        );
        let payload =
            frame.checked(json!({"stage":"restore","archive":frame.archive(Some(archive))?}))?;
        let accepted = guarded_candidate(
            plugins,
            strategy,
            plugins.invoke_strategy_candidate(strategy, payload, &CancellationToken::new()),
        )
        .await?;
        frame.check(&accepted)?;
        if accepted.get("accepted") != Some(&Value::Bool(true)) {
            bail!("context strategy rejected archive schema");
        }
    }
    let mut expanded = match archive {
        Some(archive) => native::expand(plugins, archive, model).await?,
        None => Vec::new(),
    };
    expanded.extend(messages);
    let frame = input::Frame::new(
        plugins.context_session_id(),
        strategy,
        &expanded,
        "assemble",
        leaf,
    );
    let payload = frame.checked(json!({"stage":"assemble","messages":frame.messages()?,
        "archive":frame.archive(archive)?,"model":model,"leaf":leaf}))?;
    let result = guarded_candidate(
        plugins,
        strategy,
        plugins.invoke_strategy_candidate(strategy, payload, &CancellationToken::new()),
    )
    .await?;
    let selected = frame.assembled(&result)?;
    match frame.resolve(selected)? {
        Some(messages) => Ok(messages),
        None => serde_json::from_value(selected.clone())
            .context("context strategy returned invalid messages"),
    }
}

impl AgentSession {
    pub(crate) async fn context_messages(&self) -> anyhow::Result<Vec<AgentMessage>> {
        let (context, leaf) = if let Some(plugins) = &self.plugins {
            let snapshot = {
                let session = self.session.lock().await;
                context_snapshot(plugins, &session)?
            };
            snapshot.await?
        } else {
            let session = self.session.lock().await;
            (
                session.active_context(),
                session.get_leaf_id().map(str::to_owned),
            )
        };
        let model = self
            .context_model
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let messages = match &self.plugins {
            Some(plugins) => {
                assemble_at(
                    plugins,
                    context.messages,
                    context.archive.as_ref(),
                    model.as_ref(),
                    leaf.as_deref(),
                )
                .await
            }
            None if context.archive.as_ref().is_some_and(|archive| {
                !matches!(
                    archive.representation,
                    ContextRepresentation::Standard { .. }
                )
            }) =>
            {
                bail!("context archive needs an active context strategy")
            }
            None => Ok(context.messages),
        }?;
        *self
            .context_archive
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = context.archive;
        Ok(messages)
    }

    pub(crate) async fn strategy_compaction_attempts(
        &self,
        options: &CompactOptions<'_>,
        started: std::time::Instant,
    ) -> anyhow::Result<CompactOutcome> {
        let plugins = self
            .plugins
            .as_ref()
            .context("session context strategy is unavailable")?;
        for _ in 0..3 {
            pa_agent::abort::throw_if_aborted_signal(options.abort)?;
            let strategy = plugins.context_selection()?;
            let (snapshot, leaf, context) = {
                let session = self.session.lock().await;
                (
                    session.history_snapshot(),
                    session.get_leaf_id().map(str::to_owned),
                    session.active_context(),
                )
            };
            let history = snapshot.await?;
            let branch = active_branch(&history, leaf.as_deref())?;
            let messages = assemble(
                plugins,
                context.messages,
                context.archive.as_ref(),
                Some(&options.model),
            )
            .await?;
            let messages = super::refinement_gate::without_refinements(messages);
            let preparation = Preparation {
                branch,
                leaf,
                messages,
                archive: context.archive,
            };
            let attempt_options = CompactOptions {
                inference: self.auxiliary_inference(),
                model: options.model.clone(),
                api_key: options.api_key.clone(),
                custom_instructions: options.custom_instructions,
                settings: options.settings,
                abort: options.abort,
                auxiliary: options.auxiliary,
                summary_delta: options.summary_delta.clone(),
                semantic_edges: options.semantic_edges.clone(),
            };
            let options = &attempt_options;
            let frame = input::Frame::new(
                plugins.context_session_id(),
                &strategy,
                &preparation.messages,
                "prepare",
                preparation.leaf.as_deref(),
            );
            let payload = frame.checked(json!({"stage":"prepare","messages":frame.messages()?,
                "entries":frame.entries(&preparation.branch)?,"leaf":preparation.leaf}))?;
            let policy_output = guarded(
                plugins,
                &strategy,
                None,
                options.abort,
                plugins.invoke_context(&strategy, payload, &CancellationToken::new()),
            )
            .await?;
            frame.check(&policy_output)?;
            let policy: Policy = serde_json::from_value(policy_output)?;
            if !matches!(policy.policy.as_str(), "rolling" | "full") {
                bail!("unknown context preparation policy");
            }
            let mut transaction = None;
            let mut accepted = None;
            let mut conflict = false;
            let mut failures = Vec::new();
            let mut skip = "No eligible compactor";
            let chain = policy
                .compactors
                .iter()
                .filter(|route| !plugins.context_route_disabled(route))
                .map(|route| plugins.context_compactor(route))
                .collect::<anyhow::Result<Vec<_>>>()?;
            for target in chain {
                let frame = input::Frame::new(
                    plugins.context_session_id(),
                    &target,
                    &preparation.messages,
                    "compact",
                    preparation.leaf.as_deref(),
                );
                let output = guarded(
                    plugins,
                    &strategy,
                    Some(&target),
                    options.abort,
                    async {
                        let payload = frame.checked(json!({"stage":"compact","messages":frame.messages()?,
                            "entries":frame.entries(&preparation.branch)?,"leaf":preparation.leaf}))?;
                        let output = plugins.invoke_context(&target, payload, &CancellationToken::new()).await?;
                        frame.checked(output)
                    },
                ).await;
                let output = match output {
                    Ok(value) => value,
                    Err(error) if terminal(&error) => return Err(error),
                    Err(error) => {
                        failures.push(error.to_string());
                        continue;
                    }
                };
                let status = output
                    .get("status")
                    .and_then(Value::as_str)
                    .context("compactor must return an explicit status")?;
                match status {
                    "decline" => continue,
                    "failure" => {
                        failures.push(
                            output
                                .get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or("compactor failed")
                                .to_owned(),
                        );
                        continue;
                    }
                    "fatal" => bail!(
                        "fatal compactor error: {}",
                        output
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    ),
                    "cancelled" => return Err(pa_agent::abort::aborted_error()),
                    "success" | "native" => {}
                    other => bail!("unknown compactor status {other}"),
                }
                match native::eligible(&preparation, &policy.policy, &output, options) {
                    Ok(Ok(())) => {}
                    Ok(Err(reason)) => {
                        skip = reason;
                        continue;
                    }
                    Err(error) => {
                        failures.push(error.to_string());
                        continue;
                    }
                }
                let tokens =
                    super::compaction::estimate_context_tokens(&preparation.messages).tokens;
                let attempt = transaction.get_or_insert_with(|| {
                    CompactionAttempt::full_window(
                        preparation.leaf.clone(),
                        preparation.messages.clone(),
                        tokens,
                        options,
                    )
                });
                let work = native::run(
                    plugins,
                    &preparation,
                    &policy.policy,
                    &output,
                    attempt,
                    options,
                    false,
                );
                let result = guarded(plugins, &strategy, Some(&target), options.abort, work).await;
                let mut candidate = match result {
                    Ok(Some(candidate)) => candidate,
                    Ok(None) => continue,
                    Err(error) if terminal(&error) => return Err(error),
                    Err(error) => {
                        failures.push(error.to_string());
                        continue;
                    }
                };
                candidate.entry.harness_digest = None;
                candidate.entry.harness_state_fingerprint = None;
                let coverage = match native::coverage(&preparation, &candidate) {
                    Ok(coverage) => coverage,
                    Err(error) => {
                        failures.push(error.to_string());
                        continue;
                    }
                };
                candidate.entry.context_archive = Some(ContextArchive {
                    version: 1,
                    strategy: route(&strategy),
                    compactor: route(&target),
                    creator_generation: target.generation.to_string(),
                    prepared_leaf: preparation.leaf.clone().unwrap_or_default(),
                    covered_entry_ids: coverage,
                    representation: candidate.entry.context_archive.take().map_or(
                        ContextRepresentation::Standard {
                            rest: pa_types::JsonMap::new(),
                        },
                        |archive| archive.representation,
                    ),
                });
                // Assemble and validate the entire proposed view BEFORE append. A
                // failed strategy callback must not leave a committed row behind.
                let (preview_tail, preview_leaf) = {
                    let session = self.session.lock().await;
                    (
                        descendant_entries(
                            session.retained_entries(),
                            preparation.leaf.as_deref(),
                            session.get_leaf_id(),
                        ),
                        session.get_leaf_id().map(str::to_owned),
                    )
                };
                let Some(preview_tail) = preview_tail else {
                    conflict = true;
                    break;
                };
                let mut preview = preparation.branch.clone();
                preview.extend(preview_tail);
                preview.push(FileEntry::Compaction {
                    payload: candidate.entry.clone(),
                    base: pa_types::session::EntryBase {
                        id: Some(format!("proposal-{}", uuid::Uuid::new_v4())),
                        parent_id: preview_leaf.clone(),
                        timestamp: Some(crate::session::manager::format_iso_now()),
                        rest: pa_types::JsonMap::new(),
                    },
                });
                let context = crate::session::build_session_context(&preview, None);
                let validation = guarded(
                    plugins,
                    &strategy,
                    Some(&target),
                    options.abort,
                    assemble(
                        plugins,
                        context.messages,
                        context.archive.as_ref(),
                        Some(&options.model),
                    ),
                )
                .await;
                let mut view = match validation {
                    Ok(view) => view,
                    Err(error) if terminal(&error) => return Err(error),
                    Err(error) => {
                        failures.push(error.to_string());
                        continue;
                    }
                };
                let committed = {
                    let mut session = self.session.lock().await;
                    let Some(tail) = descendant_entries(
                        session.retained_entries(),
                        preview_leaf.as_deref(),
                        session.get_leaf_id(),
                    ) else {
                        conflict = true;
                        break;
                    };
                    plugins.with_context_authority(&strategy, &target, || {
                        let committed = compact_session::commit_attempt(
                            &mut session,
                            attempt,
                            &candidate,
                            options.abort,
                        )?;
                        if committed {
                            // A descendant append during Lua assembly stays as an
                            // uncovered continuation. No callback is retried after
                            // persistence; the next model-call policy sees this tail.
                            let tail_context = crate::session::build_session_context(&tail, None);
                            view.extend(tail_context.messages);
                            let timestamp = session.retained_entries().last().map_or(0, |entry| {
                                crate::session::timestamp_to_millis(entry.timestamp())
                            });
                            for message in &mut view {
                                if let AgentMessage::CompactionSummary(summary) = message {
                                    if summary.summary == candidate.entry.summary {
                                        summary.timestamp = timestamp;
                                    }
                                }
                            }
                            *self
                                .accepted_context
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(view);
                            self.context_archive
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .clone_from(&candidate.entry.context_archive);
                        }
                        Ok(committed)
                    })?
                };
                if committed {
                    accepted = Some(candidate);
                } else {
                    conflict = true;
                }
                // A structural conflict re-prepares. It is NEVER compactor fallback.
                break;
            }
            if let Some(candidate) = accepted {
                return Ok(CompactOutcome::Ran(Box::new(compact_session::CompactRun {
                    result: candidate.result,
                    entry: candidate.entry,
                    duration_ms: started.elapsed().as_millis() as u64,
                    ipython_state: None,
                })));
            }
            if conflict {
                continue;
            }
            if !failures.is_empty() {
                bail!("compaction chain failed: {}", failures.join("; "));
            }
            if transaction.is_none() {
                return Ok(CompactOutcome::Skipped(skip));
            }
            // A declined native stage after work is a normal skip; only a candidate
            // that lost its prefix check reaches a retry.
            if !self
                .session
                .lock()
                .await
                .compaction_prefix_intact(preparation.leaf.as_deref())
            {
                continue;
            }
            return Ok(CompactOutcome::Skipped("No eligible compactor"));
        }
        bail!("compaction retried 3 times while the branch kept changing; try again")
    }
}

fn generated_model_messages(value: Value) -> anyhow::Result<Vec<pa_agent::types::Message>> {
    let selected: Vec<AgentMessage> = serde_json::from_value(value)?;
    super::messages::convert_to_llm(&selected)
        .iter()
        .map(|message| {
            super::provider_adapter::json_round_trip(message)
                .context("invalid assembled model message")
        })
        .collect()
}

/// Apply current strategy policy at every actual model call. Native archive items
/// remain typed and route-bound; only uncovered ordinary continuation is converted.
pub(crate) fn wrap_stream_fn(
    plugins: std::sync::Arc<SessionPlugins>,
    archive: std::sync::Arc<std::sync::RwLock<Option<ContextArchive>>>,
    stream: pa_agent::stream::StreamFn,
) -> pa_agent::stream::StreamFn {
    std::sync::Arc::new(move |model, mut context, options| {
        let plugins = std::sync::Arc::clone(&plugins);
        let archive = archive
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let stream = std::sync::Arc::clone(&stream);
        Box::pin(async move {
            let target = plugins.context_selection()?;
            // Refs mode never round-trips existing model bodies through JSON or
            // a portable DTO. The same typed messages stay owned by this call.
            let selected = if target.context_data == crate::plugins::ContextDataMode::RefsV1 {
                let frame = input::Frame::new(
                    plugins.context_session_id(),
                    &target,
                    &context.messages,
                    "model",
                    None,
                );
                let payload = frame.checked(json!({"stage":"assemble","phase":"model",
                    "messages":frame.messages()?,"archive":frame.archive(archive.as_ref())?,"model":model,
                    "instructions":context.system_prompt,"tools":context.tools,
                    "output_tokens":options.max_tokens.unwrap_or(model.max_tokens)}))?;
                let output = guarded(
                    &plugins,
                    &target,
                    None,
                    Some(&options.signal),
                    plugins.invoke_context(&target, payload, &CancellationToken::new()),
                )
                .await?;
                let messages = frame.assembled(&output)?;
                match frame.resolve(messages)? {
                    Some(messages) => messages,
                    None => generated_model_messages(messages.clone())?,
                }
            } else {
                // Existing callbacks keep their full portable JSON contract.
                let messages: Vec<AgentMessage> = context
                    .messages
                    .iter()
                    .map(|message| {
                        super::provider_adapter::json_round_trip(message)
                            .context("invalid portable context message")
                    })
                    .collect::<anyhow::Result<_>>()?;
                let frame = input::Frame::new(
                    plugins.context_session_id(),
                    &target,
                    &messages,
                    "model",
                    None,
                );
                let payload = frame.checked(json!({"stage":"assemble","phase":"model",
                    "messages":frame.messages()?,"archive":frame.archive(archive.as_ref())?,"model":model,
                    "instructions":context.system_prompt,"tools":context.tools,
                    "output_tokens":options.max_tokens.unwrap_or(model.max_tokens)}))?;
                let output = guarded(
                    &plugins,
                    &target,
                    None,
                    Some(&options.signal),
                    plugins.invoke_context(&target, payload, &CancellationToken::new()),
                )
                .await?;
                frame.check(&output)?;
                generated_model_messages(
                    output
                        .get("messages")
                        .context("context strategy must return model messages")?
                        .clone(),
                )?
            };
            context.messages = selected;
            if let Some(archive) = archive.as_ref().filter(|archive| {
                matches!(
                    archive.representation,
                    ContextRepresentation::Responses { .. }
                )
            }) {
                let model_info = super::provider_adapter::native_model(&model)?;
                let fresh: Vec<pa_types::ai::Message> = context
                    .messages
                    .iter()
                    .map(|message| {
                        super::provider_adapter::json_round_trip(message)
                            .context("invalid continuation message")
                    })
                    .collect::<anyhow::Result<_>>()?;
                let cancel = CancellationToken::new();
                let _on_drop = cancel.clone().drop_guard();
                let input = guarded(
                    &plugins,
                    &target,
                    None,
                    Some(&options.signal),
                    native::native_input(&plugins, archive, &model_info, &fresh, &cancel),
                )
                .await?;
                context.messages.clear();
                super::provider_adapter::with_native_input(input, stream(model, context, options))
                    .await
            } else {
                stream(model, context, options).await
            }
        })
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod startup_tests;
