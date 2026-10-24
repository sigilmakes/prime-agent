//! Session slash-command execution: the daemon-side behavior behind
//! `/compact`, `/goal`, and `/autonomous`. The host runtime owns
//! persistence of what this returns; errors carry the exact TS message and
//! the host renders the `Command failed: ...` result row.

use std::sync::Arc;

use pa_types::session::CustomMessage;

use crate::autonomous::{
    autonomous_status, set_autonomous_enabled, set_autonomous_limits, AutonomousRuntimeState,
};
use crate::goals::{create_goal_context_message, GoalContextKind, GoalStatus};
use crate::slash_command_args::{
    format_autonomous_status, parse_autonomous_command, parse_goal_command, AutonomousCommand,
    GoalCommand,
};

use super::compact_session::CompactOutcome;
use super::engine::SessionEngine;
use super::goal_driver::GoalDriver;
use super::messages::{
    SESSION_SLASH_COMMAND_CUSTOM_TYPE, SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE,
};
use super::slash_commands::SessionSlashCommand;

pub use crate::autonomous::AUTONOMOUS_STATUS_CUSTOM_TYPE;

/// Inputs the host runtime supplies to one execution.
pub struct SessionCommandParams<'a> {
    pub model: &'a pa_types::ai::Model,
    /// Resolved API key (None falls back to provider resolution).
    pub api_key: Option<String>,
    pub autonomous: &'a mut AutonomousRuntimeState,
}

/// A completed compaction to persist: the session record plus the
/// client-facing result.
#[derive(Debug, Clone)]
pub struct CompactionExecution {
    pub entry: pa_types::session::CompactionEntry,
    pub result: super::compaction_exec::CompactionResult,
    /// The post-compaction `ipython_state` notice row when a kernel was
    /// running (already durable; hosts broadcast its `message_start` /
    /// `message_end` pair).
    pub ipython_state: Option<CustomMessage>,
}

/// What one execution produced.
#[derive(Debug, Default)]
pub struct SessionCommandExecution {
    /// The command echo, then any result or status rows.
    pub messages: Vec<CustomMessage>,
    /// A compaction that ran (no result row: the TS `/compact` outcome is
    /// the compaction record itself).
    pub compaction: Option<CompactionExecution>,
    /// A compaction skipped: the message the wire `compaction_end`
    /// event carries (the durable transcript records nothing).
    pub compaction_skipped: Option<&'static str>,
    /// An injected custom row the follow-up turn runs on (goal start/resume):
    /// the transcript holds ONE representation of the turn, NOT part of
    /// `messages` — the loop's `message_end` persists it once admitted.
    pub continuation_message: Option<CustomMessage>,
    /// The command failed: the TS error message (the failure result row
    /// is already appended to `messages`).
    pub error: Option<String>,
}

impl SessionCommandExecution {
    fn push_message(&mut self, message: CustomMessage) {
        self.messages.push(message);
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// The durable command echo row. Public for the host transports that
/// emit the echo before execution.
#[must_use]
pub fn session_command_echo_row(command: &SessionSlashCommand) -> CustomMessage {
    CustomMessage {
        custom_type: SESSION_SLASH_COMMAND_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(command.text.clone()),
        display: true,
        details: Some(command_details(command)),
        timestamp: now_millis(),
        rest: serde_json::Map::default(),
    }
}

/// The command description carried by echo and result rows.
fn command_details(command: &SessionSlashCommand) -> serde_json::Value {
    serde_json::json!({
        "command": {
            "name": command.name,
            "args": command.args,
            "text": command.text,
        }
    })
}

/// Hosts append it so the transcript still records the failed
/// attempt.
#[must_use]
pub fn session_command_failure_row(command: &SessionSlashCommand, error: &str) -> CustomMessage {
    slash_command_result(
        command,
        format!("Command failed: {error}"),
        false,
        "error",
        Some(error),
        true,
    )
}

/// The durable result row (`session_slash_command_result`).
fn slash_command_result(
    command: &SessionSlashCommand,
    content: String,
    success: bool,
    severity: &'static str,
    error: Option<&str>,
    display: bool,
) -> CustomMessage {
    let mut details = command_details(command);
    details["success"] = serde_json::json!(success);
    details["severity"] = serde_json::json!(severity);
    if let Some(error) = error {
        details["error"] = serde_json::json!(error);
    }
    CustomMessage {
        custom_type: SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(content),
        display,
        details: Some(details),
        timestamp: now_millis(),
        rest: serde_json::Map::default(),
    }
}

/// The goal status line.
fn goal_status_text(state: &crate::goals::GoalState) -> String {
    match &state.objective {
        Some(objective) if state.status != GoalStatus::Idle => {
            format!("Goal {}: {objective}", state.status.slug())
        }
        _ => "No active goal.".to_string(),
    }
}

/// Execute only native plugin commands or plugin reload without provider inputs.
/// Plugin echo/result rows keep the shared durable framing. Reload is management
/// work and never enters canonical history. Other commands are rejected here.
pub async fn execute_plugin_session_command(
    engine: &SessionEngine,
    command: &SessionSlashCommand,
) -> SessionCommandExecution {
    let mut execution = SessionCommandExecution::default();
    let _plugin_job_guard = if command.plugin.is_some() {
        Some(engine.session.plugin_command_guard().await)
    } else {
        None
    };
    // Management operations never enter canonical history or rebuild live context.
    if command.name == "reload" {
        // Host transports emit the echo separately, then skip this first row.
        // Keep that framing without persisting either management row.
        execution.push_message(session_command_echo_row(command));
        let result = engine.reload_plugins().await;
        let (text, success) = match result {
            Ok(snapshot) => (
                format!("Plugins reloaded (revision {}).", snapshot.revision),
                true,
            ),
            Err(error) => {
                let message = error.to_string();
                execution.error = Some(message.clone());
                (format!("Plugin reload failed: {message}"), false)
            }
        };
        execution.push_message(slash_command_result(
            command,
            text,
            success,
            if success { "info" } else { "error" },
            execution.error.as_deref(),
            true,
        ));
        return execution;
    }
    let Some(target) = command.plugin.as_ref() else {
        let error = format!("Not a plugin command: /{}", command.name);
        execution
            .messages
            .push(session_command_failure_row(command, &error));
        execution.error = Some(error);
        return execution;
    };
    let echo = session_command_echo_row(command);
    execution.push_message(echo.clone());
    if let Err(error) = persist_rows(engine, std::iter::once(&echo)).await {
        execution.error = Some(error);
        return execution;
    }
    match engine.invoke_plugin_command(target, &command.args).await {
        Ok(value) => execution.push_message(slash_command_result(
            command,
            value.to_string(),
            true,
            "info",
            None,
            true,
        )),
        Err(error) => {
            let message = error.to_string();
            execution.push_message(session_command_failure_row(command, &message));
            execution.error = Some(message);
        }
    }
    if let Err(error) = persist_rows(engine, execution.messages.iter().skip(1)).await {
        execution.error = Some(error);
    }
    if let Err(error) = sync_live_context(engine).await {
        execution.error = Some(error);
    }
    execution
}

/// Execute one session command. The echo row is durable whether the command
/// succeeds or fails; a failure appends the failure row and reports `error`.
pub async fn execute_session_command(
    engine: &SessionEngine,
    params: &mut SessionCommandParams<'_>,
    command: &SessionSlashCommand,
    images: &[pa_agent::types::ImageContent],
) -> SessionCommandExecution {
    if command.name == "reload" || command.plugin.is_some() {
        return execute_plugin_session_command(engine, command).await;
    }
    let mut execution = SessionCommandExecution::default();
    // The echo row is recorded BEFORE the command runs, so the command's
    // own work sees it in the session branch.
    let echo = session_command_echo_row(command);
    execution.push_message(echo.clone());
    if let Err(error) = persist_rows(engine, std::iter::once(&echo)).await {
        execution.error = Some(error);
        return execution;
    }
    let result = match command.name.as_str() {
        "compact" => execute_compact(engine, params, command, &mut execution).await,
        "goal" => execute_goal(engine, command, images, &mut execution).await,
        "autonomous" => execute_autonomous(params, command, &mut execution),
        other => Err(format!("Unknown session command: {other}")),
    };
    if let Err(message) = result {
        execution.push_message(slash_command_result(
            command,
            format!("Command failed: {message}"),
            false,
            "error",
            Some(&message),
            true,
        ));
        execution.error = Some(message);
    }
    // The echo row is already durable; the rest follow in order.
    if let Err(error) = persist_rows(engine, execution.messages.iter().skip(1)).await {
        execution.error = Some(error);
    }
    if let Err(error) = sync_live_context(engine).await {
        execution.error = Some(error);
    }
    execution
}

/// The live agent context mirrors the durable rows (so the next admitted
/// turn's request carries them); the rebuild is idempotent — the compaction
/// path rebuilds mid-execution.
async fn sync_live_context(engine: &SessionEngine) -> Result<(), String> {
    if let Some(live) = &engine.session.canonical_live {
        live.ensure_ready().map_err(|error| error.to_string())?;
    }
    let _flight = engine.session.compaction_flight.lock().await;
    let rebuilt = engine
        .session
        .context_messages()
        .await
        .map_err(|error| format!("{error:#}"))?;
    // The raw session messages (not the LLM view): custom rows keep their
    // wire identity in the live context, like TS's state push.
    let loop_messages: Vec<pa_agent::types::AgentMessage> = rebuilt
        .iter()
        .filter_map(super::session_message_to_loop)
        .collect();
    engine.session.agent().set_messages(loop_messages).await;
    Ok(())
}

/// `/compact`: summarize and cut, or skip silently (TS `CompactionSkippedError`).
async fn execute_compact(
    engine: &SessionEngine,
    params: &SessionCommandParams<'_>,
    command: &SessionSlashCommand,
    execution: &mut SessionCommandExecution,
) -> Result<(), String> {
    let instructions = (!command.args.is_empty()).then_some(command.args.as_str());
    let outcome = engine
        .session
        .compact(instructions, params.model, params.api_key.clone(), None)
        .await
        .map_err(|error| format!("{error:#}"))?;
    match outcome {
        CompactOutcome::Skipped(message) => {
            execution.compaction_skipped = Some(message);
        }
        CompactOutcome::Ran(run) => {
            execution.compaction = Some(CompactionExecution {
                entry: run.entry,
                result: run.result,
                ipython_state: run.ipython_state,
            });
        }
    }
    Ok(())
}

/// `/goal`: status, clear, pause, resume, and start (which schedules
/// the first continuation turn).
async fn execute_goal(
    engine: &SessionEngine,
    command: &SessionSlashCommand,
    images: &[pa_agent::types::ImageContent],
    execution: &mut SessionCommandExecution,
) -> Result<(), String> {
    let goal = parse_goal_command(&command.args)?;
    let driver: Arc<tokio::sync::Mutex<GoalDriver>> = engine.goal_driver.clone();
    let session = engine.session.session_handle().clone();
    if matches!(
        &goal,
        GoalCommand::Clear | GoalCommand::Pause | GoalCommand::Start { .. }
    ) {
        engine.purge_queued_goal_contexts();
    }
    let (status_text, context_message) =
        super::goal_driver::persist_goal_mutation(&driver, &session, |driver, drafts| {
            let mut context_message = None;
            let mut cleared_goal = false;
            match goal {
                GoalCommand::Status => {}
                GoalCommand::Clear => {
                    cleared_goal = driver.state().objective.is_some()
                        && driver.state().status != GoalStatus::Idle;
                    driver.clear(drafts)?;
                }
                GoalCommand::Pause => {
                    driver.pause(drafts, "Paused by user")?;
                }
                GoalCommand::Resume => {
                    context_message = driver.resume(drafts)?;
                }
                GoalCommand::Start {
                    objective,
                    token_budget,
                } => {
                    let state = driver.start(drafts, &objective, token_budget)?;
                    context_message = Some(create_goal_context_message(
                        &state,
                        GoalContextKind::Continuation,
                        images,
                    )?);
                }
            }
            let status_text = if cleared_goal {
                "Goal cleared.".to_string()
            } else {
                goal_status_text(driver.state())
            };
            Ok((status_text, context_message))
        })
        .await
        .map_err(|error| format!("{error:#}"))?;
    execution.push_message(slash_command_result(
        command,
        status_text,
        true,
        "info",
        None,
        true,
    ));
    // The goal-context row becomes the turn's primary record (an
    // injected custom row), never an early durable row — the loop
    // admission appends it once.
    execution.continuation_message = context_message;
    Ok(())
}

/// `/autonomous`: status, on (with budget flags), off; emits the
/// durable `autonomous_status` row.
fn execute_autonomous(
    params: &mut SessionCommandParams<'_>,
    command: &SessionSlashCommand,
    execution: &mut SessionCommandExecution,
) -> Result<(), String> {
    let parsed = parse_autonomous_command(&command.args)?;
    match parsed {
        AutonomousCommand::Status => {}
        AutonomousCommand::On { config } => {
            set_autonomous_enabled(params.autonomous, true);
            set_autonomous_limits(params.autonomous, &config);
        }
        AutonomousCommand::Off => set_autonomous_enabled(params.autonomous, false),
    }
    let status = autonomous_status(params.autonomous);
    execution.push_message(CustomMessage {
        custom_type: AUTONOMOUS_STATUS_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(format_autonomous_status(&status)),
        display: true,
        details: serde_json::to_value(&status).ok(),
        timestamp: now_millis(),
        rest: serde_json::Map::default(),
    });
    Ok(())
}

/// The rows are durable in the session's own entry chain: the live context
/// rebuild and a later `/compact` see the same rows the host runtime persists.
async fn persist_rows<'a>(
    engine: &SessionEngine,
    messages: impl Iterator<Item = &'a CustomMessage>,
) -> Result<(), String> {
    for message in messages {
        engine
            .session
            .persist_metadata_entry(pa_types::session::FileEntry::CustomMessage {
                base: super::canonical_live::draft_base(),
                payload: pa_types::session::CustomMessageEntry {
                    custom_type: message.custom_type.clone(),
                    content: message.content.clone(),
                    display: message.display,
                    details: message.details.clone(),
                    rest: message.rest.clone(),
                },
            })
            .await
            .map_err(|error| error.to_string())?;
    }
    // Standalone managers retain their existing explicit flush boundary.
    let session = engine.session.session_handle().clone();
    let mut session = session.lock().await;
    if session.canonical_binding().is_none() {
        session.flush_now().map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goals::{empty_goal_state, GoalState, GoalStatus};
    use pa_types::ai::UserContent;

    fn command(name: &'static str, args: &str) -> SessionSlashCommand {
        let text = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        SessionSlashCommand {
            name: name.to_string(),
            plugin: None,
            args: args.to_string(),
            text,
        }
    }

    fn message_text(message: &CustomMessage) -> String {
        message.content.text()
    }

    #[test]
    fn echo_row_shape_matches_ts() {
        let echo = session_command_echo_row(&command("compact", "focus on tests"));
        assert_eq!(echo.custom_type, "session_slash_command");
        assert_eq!(message_text(&echo), "/compact focus on tests");
        assert!(echo.display);
        let details = echo.details.unwrap();
        assert_eq!(
            details["command"],
            serde_json::json!({
                "name": "compact",
                "args": "focus on tests",
                "text": "/compact focus on tests",
            })
        );
    }

    #[test]
    fn result_row_shape_matches_ts() {
        let result = slash_command_result(
            &command("goal", "ship it"),
            "Goal active: ship it".to_string(),
            true,
            "info",
            None,
            true,
        );
        assert_eq!(result.custom_type, "session_slash_command_result");
        assert_eq!(message_text(&result), "Goal active: ship it");
        let details = result.details.unwrap();
        assert_eq!(details["success"], serde_json::json!(true));
        assert_eq!(details["severity"], serde_json::json!("info"));

        let failed = slash_command_result(
            &command("compact", ""),
            "Command failed: boom".to_string(),
            false,
            "error",
            Some("boom"),
            true,
        );
        assert_eq!(failed.details.unwrap()["error"], serde_json::json!("boom"));
    }

    #[test]
    fn goal_status_text_matches_ts() {
        let mut state = empty_goal_state();
        assert_eq!(goal_status_text(&state), "No active goal.");
        state.objective = Some("ship it".to_string());
        state.status = GoalStatus::Active;
        assert_eq!(goal_status_text(&state), "Goal active: ship it");
        state.status = GoalStatus::Paused;
        assert_eq!(goal_status_text(&state), "Goal paused: ship it");
        state.status = GoalStatus::BudgetLimited;
        assert_eq!(goal_status_text(&state), "Goal budget_limited: ship it");
    }

    #[tokio::test]
    async fn autonomous_status_row_emitted() {
        // The autonomous branch touches only the runtime state.
        let mut autonomous = crate::autonomous::create_autonomous_runtime_state(None, None);
        let mut execution = SessionCommandExecution::default();
        execute_autonomous(
            &mut SessionCommandParams {
                model: &scripted_model(),
                api_key: None,
                autonomous: &mut autonomous,
            },
            &command("autonomous", "on --max-turns 5"),
            &mut execution,
        )
        .unwrap();
        assert!(autonomous.enabled);
        assert_eq!(autonomous.limits.max_turns, 5);
        assert_eq!(execution.messages.len(), 1);
        let status = &execution.messages[0];
        assert_eq!(status.custom_type, "autonomous_status");
        assert!(message_text(status).starts_with("[autonomous-status: on]"));
        assert!(matches!(status.content, UserContent::Text(_)));
    }

    fn scripted_model() -> pa_types::ai::Model {
        serde_json::from_value(serde_json::json!({
            "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
            "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn plugin_executor_needs_no_provider_inputs_and_reload_is_history_free() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            cwd.join("test.lua"),
            "return {commands={{name='local_command',run=function(input) return input.args end}}}",
        )
        .unwrap();
        std::fs::write(
            agent.join("settings.json"),
            serde_json::json!({
                "enableBuiltinSkills": false, "harnessRefinements":false,
                "plugins":{"test":{"enabled":true,"path":"test.lua"}}
            })
            .to_string(),
        )
        .unwrap();
        // Session creation still resolves a model. Command execution itself takes no
        // provider parameters and must not consume the fixture's empty provider queue.
        let model: pa_agent::types::Model =
            serde_json::from_value(serde_json::to_value(scripted_model()).unwrap()).unwrap();
        let provider = Arc::new(pa_agent::scripted::ScriptedProvider::new(model.clone()));
        let engine =
            super::super::engine::create_session(super::super::engine::SessionEngineConfig {
                cwd,
                agent_dir: agent,
                model: Some(model),
                stream_fn: Some(provider.stream_fn()),
                prewarm_ipython_kernel: Some(false),
                ..Default::default()
            })
            .await
            .unwrap();
        let plugin = engine
            .session
            .classify_session_command("/local_command exact")
            .unwrap();
        let result = execute_plugin_session_command(&engine, &plugin).await;
        assert_eq!(result.error, None);
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.messages[1].content.text(), "\"exact\"");
        let before = engine
            .session
            .session_handle()
            .lock()
            .await
            .active_context()
            .messages;
        let result = execute_plugin_session_command(&engine, &command("reload", "")).await;
        assert_eq!(result.error, None);
        assert_eq!(
            engine
                .session
                .session_handle()
                .lock()
                .await
                .active_context()
                .messages,
            before
        );
        let rejected = execute_plugin_session_command(&engine, &command("compact", "")).await;
        assert!(rejected.error.unwrap().contains("Not a plugin command"));
        assert!(provider.calls().is_empty());
        assert_eq!(
            engine
                .session
                .session_handle()
                .lock()
                .await
                .active_context()
                .messages,
            before
        );
    }

    #[test]
    fn goal_state_slugs() {
        let state = GoalState {
            status: GoalStatus::Complete,
            ..empty_goal_state()
        };
        assert_eq!(state.status.slug(), "complete");
    }
}
