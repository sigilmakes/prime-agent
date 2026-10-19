//! Quiescent lifecycle notifications. These are observers, not context or commit
//! control hooks. Slow or failing observers never roll back a durable operation.

use super::{compact_session::CompactOutcome, plugins::SessionPlugins, AgentSession};
use crate::plugins::{ContributionKind, InvocationEvent, InvocationOutcome};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

async fn notify(plugins: &SessionPlugins, stage: &str, input: Value) {
    let host = plugins.host();
    for target in &host.snapshot().contributions {
        if target.kind != ContributionKind::Hook || target.name != stage {
            continue;
        }
        let cancel = CancellationToken::new();
        let _cancel_on_drop = cancel.clone().drop_guard();
        let result=tokio::time::timeout(std::time::Duration::from_secs(1),async {
            let mut invocation=host.invoke(target,input.clone(),&cancel)?;
            while let Some(event)=invocation.next().await {
                match event {
                    InvocationEvent::Update(value)=>tracing::debug!(plugin=%target.plugin,update=%value,"plugin lifecycle observer update"),
                    InvocationEvent::Finished(Ok(InvocationOutcome::Unary(value)|InvocationOutcome::Streamed(value)))=>return Ok(value),
                    InvocationEvent::Finished(Err(error))=>return Err(error),
                }
            }
            Err(crate::plugins::PluginError::Interrupted)
        }).await;
        match result {
            Ok(Ok(value)) => {
                tracing::debug!(plugin=%target.plugin,outcome=%value,"plugin lifecycle observer settled");
            }
            Ok(Err(error)) => {
                tracing::warn!(plugin=%target.plugin,%error,"plugin lifecycle observer failed");
            }
            Err(error) => {
                tracing::warn!(plugin=%target.plugin,%error,"plugin lifecycle observer timed out");
            }
        }
    }
}

/// Whether the retry lane will re-issue this settle.
///
/// TS draws the auto-refine seam at exactly this question: an `agent_end` that
/// initiated a retry returns before `_scheduleAutoRefineAfterAgentEnd`
/// (`agent-session.ts:4736-4740`), so no refinement review is scheduled from a
/// settle the retry lane owns. The run slot is per attempt, so the settle alone
/// cannot answer it; the message and the live retry policy can.
async fn settle_retry_pending(
    agent: &Arc<pa_agent::agent::Agent>,
    plugins: &SessionPlugins,
    messages: &[pa_agent::types::AgentMessage],
) -> bool {
    let Some(assistant) = messages.iter().rev().find_map(|message| match message {
        pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(assistant)) => {
            Some(assistant)
        }
        _ => None,
    }) else {
        return false;
    };
    let (cwd, agent_dir) = plugins.settings_dirs();
    let policy =
        crate::settings::SettingsManager::create(cwd, agent_dir).get_provider_retry_policy();
    if !policy.enabled {
        return false;
    }
    let context_window = agent.state().await.model.context_window;
    super::provider_retry::retry_will_follow(assistant, context_window, &policy, 0)
}

/// `AgentEnd` is the complete admitted run, unlike `TurnEnd`'s individual tool loop
/// iteration. Plugin-owned counters can use this stage without changing old hooks.
pub(crate) async fn observe_runs(
    agent: &Arc<pa_agent::agent::Agent>,
    plugins: Arc<SessionPlugins>,
    session: Arc<tokio::sync::Mutex<crate::session::manager::SessionManager>>,
) {
    // The subscriber lives on the agent: hold it weakly (a strong edge
    // would cycle agent -> listener -> agent).
    let agent_weak = Arc::downgrade(agent);
    agent.subscribe(move|event,signal| {
        let plugins=Arc::clone(&plugins);let session=Arc::clone(&session);let agent=agent_weak.clone();
        Box::pin(async move {
            if let pa_agent::types::AgentEvent::AgentEnd{messages}=event {
                let branch=session.lock().await.get_leaf_id().map(str::to_owned);
                let reason=if signal.is_aborted() {"cancelled"} else {"completed"};
                // Only a stage that has a subscriber pays the classification;
                // an ordinary settle never re-runs a retry.
                let subscribed=plugins.host().snapshot().contributions.iter().any(|target| target.kind==ContributionKind::Hook&&target.name=="run_settled");
                let retry=if subscribed && reason=="completed" {
                    match agent.upgrade() {
                        Some(agent) => settle_retry_pending(&agent,&plugins,&messages).await,
                        None => false,
                    }
                } else {
                    false
                };
                notify(&plugins,"run_settled",json!({"stage":"run_settled","branch":branch,"now_ms":now_ms(),"reason":reason,"retry":retry})).await;
            }
            Ok(())
        })
    }).await;
}

impl AgentSession {
    pub(super) async fn notify_compaction_settled(&self, outcome: &CompactOutcome) {
        let (Some(plugins), CompactOutcome::Ran(run)) = (&self.plugins, outcome) else {
            return;
        };
        let branch = self.session.lock().await.get_leaf_id().map(str::to_owned);
        notify(plugins,"compaction_settled",json!({"stage":"compaction_settled","branch":branch,"now_ms":now_ms(),"reason":"accepted","entry":run.entry,"duration_ms":run.duration_ms})).await;
    }
}
