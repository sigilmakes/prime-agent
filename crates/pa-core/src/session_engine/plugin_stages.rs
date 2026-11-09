//! Typed plugin adapters over the tool and compaction seams.
//!
//! `tool_call` may block a tool before it runs, `tool_result` may replace the
//! returned result, and `compaction_settled` observes one finished compaction.
//! Each stage dispatches only its own hook name, in alphabetical plugin order.
//! There is no shared event bus and no model, stream, turn, run, session,
//! subagent or goal stage here.

use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, bail, Context};
use pa_agent::agent_loop::{AfterToolCallFn, BeforeToolCallFn};
use pa_agent::types::{
    AfterToolCallContext, AfterToolCallResult, BeforeToolCallContext, BeforeToolCallResult,
    ToolResultContent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::plugins::{
    Contribution, ContributionKind, InvocationEvent, InvocationMode, InvocationOutcome, PluginHost,
};

use super::compact_session::CompactOutcome;

pub(crate) const TOOL_CALL_HOOK: &str = "tool_call";
pub(crate) const TOOL_RESULT_HOOK: &str = "tool_result";
pub(crate) const COMPACTION_SETTLED_HOOK: &str = "compaction_settled";

/// A mutating tool stage may not hold a turn indefinitely.
const TOOL_STAGE_TIMEOUT: Duration = Duration::from_secs(2);
/// Observers cannot delay the compaction caller.
const COMPACTION_STAGE_TIMEOUT: Duration = Duration::from_secs(1);

/// Immutable session identity plus the active Lua host. The host is reloadable;
/// each hook call resolves the current generation from its contribution.
#[derive(Clone)]
pub(crate) struct ToolStageHooks {
    host: Arc<PluginHost>,
    session: String,
}

/// Every tool stage carries the same call identity.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ToolCallStageInput {
    pub session: String,
    /// Provider response identifier; `None` when the provider exposes none.
    pub assistant_message_id: Option<String>,
    pub tool_call_id: String,
    pub tool_name: String,
    /// Schema-validated arguments, exactly as the tool would receive them.
    pub args: Value,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ToolResultSnapshot {
    pub content: Vec<ToolResultContent>,
    pub details: Value,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ToolResultStageInput {
    #[serde(flatten)]
    pub call: ToolCallStageInput,
    pub result: ToolResultSnapshot,
}

/// A missing `allow` is a contract failure, never an accidental allow.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolCallStageOutput {
    pub allow: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Absent or null fields leave the executed result unchanged.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolResultStageOutput {
    #[serde(default)]
    pub content: Option<Vec<ToolResultContent>>,
    #[serde(default)]
    pub details: Option<Value>,
    #[serde(default)]
    pub is_error: Option<bool>,
    #[serde(default)]
    pub terminate: Option<bool>,
}

/// A skipped compaction has no durable entry, so `entry` carries its reason.
#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum CompactionSettledInput<'a> {
    Ran {
        session: &'a str,
        entry: &'a pa_types::session::CompactionEntry,
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<&'a str>,
    },
    Skipped {
        session: &'a str,
        entry: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<&'a str>,
    },
}

impl ToolStageHooks {
    #[must_use]
    pub(crate) fn new(host: Arc<PluginHost>, session: String) -> Self {
        Self { host, session }
    }

    pub(crate) fn before_tool_call(&self) -> BeforeToolCallFn {
        let hooks = self.clone();
        Arc::new(move |context, _signal| {
            let hooks = hooks.clone();
            Box::pin(async move { hooks.run_tool_call(context).await })
        })
    }

    pub(crate) fn after_tool_call(&self) -> AfterToolCallFn {
        let hooks = self.clone();
        Arc::new(move |context, _signal| {
            let hooks = hooks.clone();
            Box::pin(async move { Ok(hooks.run_tool_result(context).await) })
        })
    }

    async fn run_tool_call(
        &self,
        context: BeforeToolCallContext,
    ) -> anyhow::Result<Option<BeforeToolCallResult>> {
        let targets = self.targets(TOOL_CALL_HOOK);
        if targets.is_empty() {
            return Ok(None);
        }
        let input = serde_json::to_value(ToolCallStageInput {
            session: self.session.clone(),
            assistant_message_id: context.assistant_message.response_id.clone(),
            tool_call_id: context.tool_call.id.clone(),
            tool_name: context.tool_call.name.clone(),
            args: context.args,
        })?;
        for target in targets {
            ensure_unary(&target)?;
            let output = invoke_hook(&self.host, &target, input.clone(), TOOL_STAGE_TIMEOUT)
                .await
                .with_context(|| {
                    format!("plugin {} {TOOL_CALL_HOOK} hook failed", target.plugin)
                })?;
            let output: ToolCallStageOutput =
                serde_json::from_value(output).with_context(|| {
                    format!(
                        "plugin {} {TOOL_CALL_HOOK} hook returned an invalid decision",
                        target.plugin
                    )
                })?;
            if !output.allow {
                return Ok(Some(BeforeToolCallResult {
                    block: true,
                    reason: Some(output.reason.unwrap_or_else(|| {
                        format!("Tool execution was blocked by plugin {}", target.plugin)
                    })),
                }));
            }
        }
        Ok(None)
    }

    async fn run_tool_result(&self, context: AfterToolCallContext) -> Option<AfterToolCallResult> {
        let targets = self.targets(TOOL_RESULT_HOOK);
        if targets.is_empty() {
            return None;
        }
        let mut result = context.result;
        let mut is_error = context.is_error;
        let call = ToolCallStageInput {
            session: self.session.clone(),
            assistant_message_id: context.assistant_message.response_id.clone(),
            tool_call_id: context.tool_call.id.clone(),
            tool_name: context.tool_call.name.clone(),
            args: context.args,
        };
        for target in targets {
            if target.mode != InvocationMode::Unary {
                tracing::warn!(plugin = %target.plugin, "plugin tool_result hook is not unary; ignored");
                continue;
            }
            let input = ToolResultStageInput {
                call: call.clone(),
                result: ToolResultSnapshot {
                    content: result.content.clone(),
                    details: result.details.clone(),
                    is_error,
                },
            };
            let input = match serde_json::to_value(input) {
                Ok(input) => input,
                Err(error) => {
                    tracing::warn!(plugin = %target.plugin, %error, "plugin tool_result input failed; keeping original result");
                    continue;
                }
            };
            let output = match invoke_hook(&self.host, &target, input, TOOL_STAGE_TIMEOUT).await {
                Ok(output) => output,
                Err(error) => {
                    tracing::warn!(plugin = %target.plugin, %error, "plugin tool_result hook failed; keeping original result");
                    continue;
                }
            };
            let output: Option<ToolResultStageOutput> = match serde_json::from_value(output) {
                Ok(output) => output,
                Err(error) => {
                    tracing::warn!(plugin = %target.plugin, %error, "plugin tool_result hook returned an invalid replacement; keeping original result");
                    continue;
                }
            };
            let Some(output) = output else {
                continue;
            };
            if let Some(content) = output.content {
                result.content = content;
            }
            if let Some(details) = output.details {
                result.details = details;
            }
            if let Some(terminate) = output.terminate {
                result.terminate = Some(terminate);
            }
            if let Some(error) = output.is_error {
                is_error = error;
            }
        }
        Some(AfterToolCallResult {
            content: Some(result.content),
            details: Some(result.details),
            is_error: Some(is_error),
            terminate: result.terminate,
        })
    }

    fn targets(&self, name: &str) -> Vec<Contribution> {
        let mut targets: Vec<_> = self
            .host
            .snapshot()
            .contributions
            .iter()
            .filter(|target| target.kind == ContributionKind::Hook && target.name == name)
            .cloned()
            .collect();
        targets.sort_by(|left, right| left.plugin.cmp(&right.plugin));
        targets
    }
}

/// Fire the observe-only compaction stage once for one finished attempt.
/// Failures stay local: the compaction result is already determined.
pub(crate) async fn observe_compaction(
    host: &PluginHost,
    session: &str,
    outcome: &CompactOutcome,
    instructions: Option<&str>,
) {
    let input = match outcome {
        CompactOutcome::Ran(run) => CompactionSettledInput::Ran {
            session,
            entry: &run.entry,
            instructions,
        },
        CompactOutcome::Skipped(reason) => CompactionSettledInput::Skipped {
            session,
            entry: reason,
            instructions,
        },
    };
    let Ok(input) = serde_json::to_value(input) else {
        return;
    };
    let mut targets: Vec<_> = host
        .snapshot()
        .contributions
        .iter()
        .filter(|target| {
            target.kind == ContributionKind::Hook && target.name == COMPACTION_SETTLED_HOOK
        })
        .cloned()
        .collect();
    targets.sort_by(|left, right| left.plugin.cmp(&right.plugin));
    for target in targets {
        if target.mode != InvocationMode::Unary {
            tracing::warn!(plugin = %target.plugin, "plugin compaction observer is not unary; ignored");
            continue;
        }
        match invoke_hook(host, &target, input.clone(), COMPACTION_STAGE_TIMEOUT).await {
            Ok(value) => {
                tracing::debug!(plugin = %target.plugin, outcome = %value, "plugin compaction observer settled");
            }
            Err(error) => {
                tracing::warn!(plugin = %target.plugin, %error, "plugin compaction observer failed");
            }
        }
    }
}

fn ensure_unary(target: &Contribution) -> anyhow::Result<()> {
    if target.mode == InvocationMode::Unary {
        Ok(())
    } else {
        bail!(
            "plugin {} {} hook must be unary",
            target.plugin,
            target.name
        )
    }
}

async fn invoke_hook(
    host: &PluginHost,
    target: &Contribution,
    input: Value,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let work = async {
        let mut invocation = host.invoke(target, input, &CancellationToken::new())?;
        while let Some(event) = invocation.next().await {
            match event {
                InvocationEvent::Update(_) => {}
                InvocationEvent::Finished(Ok(
                    InvocationOutcome::Unary(value) | InvocationOutcome::Streamed(value),
                )) => return Ok(value),
                InvocationEvent::Finished(Err(error)) => return Err(error.into()),
            }
        }
        Err(anyhow!("plugin invocation ended without an outcome"))
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!(
            "plugin {} {} hook timed out after {timeout:?}",
            target.plugin,
            target.name
        )),
    }
}
