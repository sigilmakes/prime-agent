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
        || error
            .downcast_ref::<pa_ai::compaction::CompactionError>()
            .is_some()
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

// This nonce belongs only to the proposed model view. It is never put in a
// FileEntry, archive, or canonical ID. The builder emits the newest portable
// compaction summary at position zero; older retained summaries follow it.
struct ProposedSummary {
    key: String,
    token: Value,
}

impl ProposedSummary {
    fn mark(messages: &mut [AgentMessage], portable: bool) -> anyhow::Result<Option<Self>> {
        if !portable {
            return Ok(None);
        }
        let Some(AgentMessage::CompactionSummary(summary)) = messages.first_mut() else {
            bail!("portable compaction preview has no leading proposed summary");
        };
        let key = format!("__pa_proposed_summary_{}", uuid::Uuid::new_v4());
        let token = Value::String(uuid::Uuid::new_v4().to_string());
        summary.rest.insert(key.clone(), token.clone());
        Ok(Some(Self { key, token }))
    }

    fn resolve(self, messages: &mut [AgentMessage]) -> anyhow::Result<usize> {
        let mut position = None;
        for (index, message) in messages.iter().enumerate() {
            let rest = match message {
                AgentMessage::User(message) => &message.rest,
                AgentMessage::Assistant(message) => &message.rest,
                AgentMessage::ToolResult(message) => &message.rest,
                AgentMessage::BashExecution(message) => &message.rest,
                AgentMessage::Custom(message) => &message.rest,
                AgentMessage::BranchSummary(message) => &message.rest,
                AgentMessage::CompactionSummary(message) => &message.rest,
            };
            if rest.get(&self.key) == Some(&self.token) {
                if !matches!(message, AgentMessage::CompactionSummary(_)) {
                    bail!("context strategy moved proposed summary provenance to another role");
                }
                if position.replace(index).is_some() {
                    bail!("context strategy duplicated proposed summary provenance");
                }
            }
        }
        let index = position.context("context strategy lost proposed summary provenance")?;
        if let AgentMessage::CompactionSummary(summary) = &mut messages[index] {
            summary.rest.remove(&self.key);
        }
        Ok(index)
    }
}

fn bind_proposed_summary(messages: &mut [AgentMessage], position: Option<usize>, timestamp: u64) {
    // No strategy callback or reordering occurs after resolve. Standalone tail
    // adoption only extends this vector, so the certified position stays exact.
    if let Some(position) = position {
        if let AgentMessage::CompactionSummary(summary) = &mut messages[position] {
            summary.timestamp = timestamp;
        }
    }
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
        let plugins = self
            .plugins
            .as_ref()
            .context("session context strategy is unavailable")?;
        for _ in 0..3 {
            pa_agent::abort::throw_if_aborted_signal(options.abort)?;
            let mut live_ticket = match &self.canonical_live {
                Some(live) => Some(
                    live.prepare_mutation(
                        crate::session::canonical::CanonicalMutationKind::ContextPolicy,
                    )
                    .await?,
                ),
                None => None,
            };
            let strategy = before_commit!(live_ticket, plugins.context_selection());
            let registration =
                before_commit!(live_ticket, plugins.capture_context_registration(&strategy));
            let (snapshot, leaf, context) = {
                let session = self.session.lock().await;
                (
                    session.history_snapshot(),
                    session.get_leaf_id().map(str::to_owned),
                    session.active_context(),
                )
            };
            let history = before_commit!(live_ticket, snapshot.await);
            let branch = before_commit!(live_ticket, active_branch(&history, leaf.as_deref()));
            let assembly = assemble(
                plugins,
                context.messages,
                context.archive.as_ref(),
                Some(&options.model),
            );
            let assembled = if live_ticket.is_some() {
                // Keep the prepared ticket outside callback cancellation so its
                // original caller can await exact known-unwritten abandonment.
                guarded(plugins, &strategy, None, options.abort, assembly).await
            } else {
                assembly.await
            };
            let messages = before_commit!(live_ticket, assembled);
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
            let frame_messages = before_commit!(live_ticket, frame.messages());
            let frame_entries = before_commit!(live_ticket, frame.entries(&preparation.branch));
            let payload = before_commit!(
                live_ticket,
                frame.checked(json!({"stage":"prepare","messages":frame_messages,
                "entries":frame_entries,"leaf":preparation.leaf}))
            );
            let policy_output = before_commit!(
                live_ticket,
                guarded(
                    plugins,
                    &strategy,
                    None,
                    options.abort,
                    plugins.invoke_context(&strategy, payload, &CancellationToken::new()),
                )
                .await
            );
            before_commit!(live_ticket, frame.check(&policy_output));
            let policy: Policy = before_commit!(live_ticket, serde_json::from_value(policy_output));
            if !matches!(policy.policy.as_str(), "rolling" | "full") {
                before_commit!(live_ticket, fail: anyhow::anyhow!("unknown context preparation policy"));
            }
            let mut transaction = None;
            let mut accepted = None;
            let mut conflict = false;
            let mut failures = Vec::new();
            let mut skip = "No eligible compactor";
            let chain = before_commit!(
                live_ticket,
                policy
                    .compactors
                    .iter()
                    .filter(|route| !plugins.context_route_disabled(route))
                    .map(|route| plugins.context_compactor(route))
                    .collect::<anyhow::Result<Vec<_>>>()
            );
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
                    Err(error) if terminal(&error) => before_commit!(live_ticket, fail: error),
                    Err(error) => {
                        failures.push(error.to_string());
                        continue;
                    }
                };
                let status = before_commit!(
                    live_ticket,
                    output
                        .get("status")
                        .and_then(Value::as_str)
                        .context("compactor must return an explicit status")
                );
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
                    "fatal" => before_commit!(live_ticket, fail: anyhow::anyhow!(
                        "fatal compactor error: {}",
                        output
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    )),
                    "cancelled" => {
                        before_commit!(live_ticket, fail: pa_agent::abort::aborted_error())
                    }
                    "success" | "native" => {}
                    other => {
                        before_commit!(live_ticket, fail: anyhow::anyhow!("unknown compactor status {other}"))
                    }
                }
                match native::eligible(&preparation, &policy.policy, &output, options) {
                    Ok(Ok(())) => {}
                    Ok(Err(reason)) => {
                        skip = reason;
                        continue;
                    }
                    Err(error) => before_commit!(live_ticket, fail: error),
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
                    Err(error)
                        if terminal(&error)
                            || output.get("operation").and_then(Value::as_str)
                                == Some("server") =>
                    {
                        before_commit!(live_ticket, fail: error)
                    }
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
                let mut context = crate::session::build_session_context(&preview, None);
                let portable = context.archive.as_ref().is_none_or(|archive| {
                    matches!(
                        archive.representation,
                        ContextRepresentation::Standard { .. }
                    )
                });
                let proposed_summary = before_commit!(
                    live_ticket,
                    ProposedSummary::mark(&mut context.messages, portable)
                );
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
                    Err(error) if terminal(&error) => before_commit!(live_ticket, fail: error),
                    Err(error) => {
                        failures.push(error.to_string());
                        continue;
                    }
                };
                // Resolve and remove private provenance BEFORE any write. A
                // dropped, duplicated, or changed proposal is never guessed by body.
                let proposed_position = before_commit!(
                    live_ticket,
                    proposed_summary
                        .map(|proposal| proposal.resolve(&mut view))
                        .transpose()
                );
                let committed = if live_ticket.is_some() {
                    before_commit!(
                        live_ticket,
                        pa_agent::abort::throw_if_aborted_signal(options.abort)
                    );
                    // Preflight is not writer admission. The concrete capability
                    // below rechecks the actual registration at the owner's real
                    // RequestedMutation/enter_io boundary before any disk I/O.
                    let admitted = plugins.with_registered_context_authority(
                        &registration,
                        &strategy,
                        &target,
                        || {
                            crate::session::canonical::CanonicalEntryDraft::new(
                                crate::session::canonical::CanonicalMutationKind::ContextPolicy,
                                FileEntry::Compaction {
                                    base: super::canonical_live::draft_base(),
                                    payload: candidate.entry.clone(),
                                },
                            )
                        },
                    );
                    let draft = before_commit!(live_ticket, admitted);
                    let admission = super::canonical_live::CanonicalLiveContextAdmission::new(
                        std::sync::Arc::clone(plugins),
                        registration.clone(),
                        strategy.clone(),
                        target.clone(),
                        draft.clone(),
                    );
                    let ticket = live_ticket
                        .take()
                        .expect("checked prepared canonical ticket");
                    let receipt = ticket
                        .commit_receipt_with_context(draft, Some(admission))
                        .await?;
                    let publication = {
                        let session = self.session.lock().await;
                        (|| -> anyhow::Result<()> {
                            if session.canonical_binding() != Some(&receipt.cursor) {
                                bail!("canonical source changed after compaction ACK");
                            }
                            let row = session
                                .canonical_raw_entry(&receipt.entry_id)
                                .context("committed compaction row is not retained")?;
                            let timestamp =
                                crate::session::timestamp_to_millis(row.entry().timestamp());
                            plugins.with_registered_context_authority(
                                &registration,
                                &strategy,
                                &target,
                                || {
                                    bind_proposed_summary(&mut view, proposed_position, timestamp);
                                    if let Some(compaction) = attempt.semantic_compaction.as_mut() {
                                        compaction.commit();
                                    }
                                    *self
                                        .accepted_context
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                        Some(view);
                                    self.context_archive
                                        .write()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .clone_from(&candidate.entry.context_archive);
                                    Ok(())
                                },
                            )
                        })()
                    };
                    if let Err(error) = publication {
                        if let Some(live) = &self.canonical_live {
                            live.block_projection();
                        }
                        return Err(error);
                    }
                    true
                } else {
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
                            bind_proposed_summary(&mut view, proposed_position, timestamp);
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
                if self.canonical_live.is_some() {
                    before_commit!(live_ticket, fail: anyhow::anyhow!("canonical source changed; paid compaction is not retried"));
                }
                continue;
            }
            if let Some(ticket) = live_ticket.take() {
                ticket.abandon().await?;
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
                if self.canonical_live.is_some() {
                    bail!("canonical source changed; paid compaction is not retried");
                }
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

/// Whether the upstream loop already assembled archives or just certified the
/// canonical continuation. Only the latter needs bitmap expansion at this boundary.
#[derive(Clone, Copy)]
pub(crate) enum ModelContextSource {
    Assembled,
    Canonical,
}

/// Apply current strategy policy at every actual model call. Native archive items
/// remain typed and route-bound; only uncovered ordinary continuation is converted.
pub(crate) fn wrap_stream_fn(
    source: ModelContextSource,
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
            if let Some(archive) = archive.as_ref().filter(|archive| {
                matches!(source, ModelContextSource::Canonical)
                    && matches!(
                        archive.representation,
                        ContextRepresentation::Snapcompact { .. }
                    )
            }) {
                let model_info = super::provider_adapter::native_model(&model)?;
                let expanded = guarded(
                    &plugins,
                    &target,
                    None,
                    Some(&options.signal),
                    native::expand(&plugins, archive, Some(&model_info)),
                )
                .await?;
                let mut messages = super::messages::convert_to_llm(&expanded)
                    .iter()
                    .map(|message| {
                        super::provider_adapter::json_round_trip(message)
                            .context("invalid bitmap archive message")
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                messages.append(&mut context.messages);
                context.messages = messages;
            }
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

#[cfg(test)]
mod proposed_summary_tests {
    use super::*;

    fn summary(timestamp: u64) -> AgentMessage {
        serde_json::from_value(json!({"role":"compactionSummary","summary":"identical summary","tokensBefore":42,"timestamp":timestamp,"futureSummaryMetadata":{"nested":[1,2,3]}})).unwrap()
    }

    #[test]
    fn identical_older_summary_is_unchanged_after_reordered_json_proposal_binding() {
        let old = summary(17);
        let mut view = vec![summary(18), old.clone()];
        let proposal = ProposedSummary::mark(&mut view, true).unwrap().unwrap();
        view.swap(0, 1);
        let mut view: Vec<AgentMessage> =
            serde_json::from_value(serde_json::to_value(view).unwrap()).unwrap();
        let position = proposal.resolve(&mut view).unwrap();
        assert_eq!(position, 1);
        bind_proposed_summary(&mut view, Some(position), 99);
        assert_eq!(view[0], old);
        assert_eq!(view[1], summary(99));
        assert!(!serde_json::to_string(&view)
            .unwrap()
            .contains("__pa_proposed_summary_"));
    }

    #[test]
    fn same_key_with_unrelated_token_is_preserved() {
        let mut view = vec![summary(18), summary(17)];
        let proposal = ProposedSummary::mark(&mut view, true).unwrap().unwrap();
        if let AgentMessage::CompactionSummary(old) = &mut view[1] {
            old.rest
                .insert(proposal.key.clone(), json!("unrelated-token"));
        }
        let old = view[1].clone();
        let position = proposal.resolve(&mut view).unwrap();
        bind_proposed_summary(&mut view, Some(position), 99);
        assert_eq!(view[1], old);
    }

    #[test]
    fn moving_provenance_to_another_role_is_rejected_before_write() {
        let mut view = vec![summary(18)];
        let proposal = ProposedSummary::mark(&mut view, true).unwrap().unwrap();
        let mut changed: AgentMessage =
            serde_json::from_value(json!({"role":"user","content":"generated","timestamp":18}))
                .unwrap();
        if let AgentMessage::User(user) = &mut changed {
            user.rest
                .insert(proposal.key.clone(), proposal.token.clone());
        }
        view[0] = changed;
        assert!(proposal.resolve(&mut view).is_err());
    }

    #[test]
    fn duplicate_provenance_is_rejected_before_write() {
        let mut view = vec![summary(18)];
        let proposal = ProposedSummary::mark(&mut view, true).unwrap().unwrap();
        view.push(view[0].clone());
        assert!(proposal.resolve(&mut view).is_err());
    }

    #[test]
    fn missing_provenance_never_falls_back_to_identical_body() {
        let mut view = vec![summary(18), summary(18)];
        let proposal = ProposedSummary::mark(&mut view, true).unwrap().unwrap();
        view.remove(0);
        assert!(proposal.resolve(&mut view).is_err());
    }

    #[test]
    fn native_archive_does_not_claim_a_retained_summary() {
        let old = summary(17);
        let mut view = vec![old.clone()];
        assert!(ProposedSummary::mark(&mut view, false).unwrap().is_none());
        bind_proposed_summary(&mut view, None, 99);
        assert_eq!(view, vec![old]);
    }
}
