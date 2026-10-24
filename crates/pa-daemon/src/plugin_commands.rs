//! Native plugin commands shared by worker and in-process transports. Commands
//! cross the existing core handler seam; they are never submitted as model input.

use anyhow::{bail, Context};
use pa_core::plugins::{Contribution, InvocationMode};
use pa_core::session_engine::engine::SessionEngine as CoreSessionEngine;
use serde_json::{json, Value};

use crate::engine::PluginCommandOutcome;
use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::Worker;

/// The complete immutable target identity advertised to optional clients.
fn target_value(target: &Contribution) -> Value {
    json!({
        "plugin": target.plugin,
        "generation": target.generation.to_string(),
        "kind": "command",
        "name": target.name,
        "description": target.description,
        "mode": match target.mode {
            InvocationMode::Unary => "unary",
            InvocationMode::Streaming => "streaming",
        },
    })
}

pub(crate) fn command_inventory(engine: &CoreSessionEngine) -> Vec<Value> {
    engine
        .session
        .session_command_inventory()
        .into_iter()
        .filter_map(|entry| {
            let target = entry.plugin?;
            if target.mode != InvocationMode::Unary {
                return None;
            }
            Some(json!({
                "name": entry.name,
                "description": entry.description,
                "source": "plugin",
                "plugin": target.plugin,
                "target": target_value(&target),
            }))
        })
        .collect()
}

fn normalized_management_command(text: &str) -> &str {
    if pa_core::session_engine::slash_commands::parse_slash_command(text)
        .is_some_and(|(name, args)| name == "plugins" && args.trim() == "reload")
    {
        "/reload"
    } else {
        text
    }
}

/// Invoke the same core command handler used by headless session commands.
/// Validation does not admit a prompt or rewrite the generation-bound target.
pub(crate) async fn execute(
    engine: &CoreSessionEngine,
    text: &str,
    target: Option<&Value>,
) -> anyhow::Result<PluginCommandOutcome> {
    let text = normalized_management_command(text);
    let (name, args) = pa_core::session_engine::slash_commands::parse_slash_command(text)
        .context("plugin_command requires slash-command text")?;
    if name == "plugins" {
        if target.is_some() {
            bail!("Plugin management commands do not accept a target");
        }
        if !matches!(args.trim(), "" | "list") {
            bail!(
                "Usage: /plugins [list|reload]. Edit plugins in settings.json, then run /reload."
            );
        }
        let commands = command_inventory(engine);
        let mut lines = vec!["Active plugin commands:".to_string()];
        if commands.is_empty() {
            lines.push("  None.".to_string());
        }
        for entry in commands {
            lines.push(format!(
                "  /{} ({}) — {}",
                entry["name"].as_str().unwrap_or_default(),
                entry["plugin"].as_str().unwrap_or_default(),
                entry["description"].as_str().unwrap_or_default()
            ));
        }
        if let Some(diagnostic) = engine.plugin_diagnostic() {
            lines.push(format!("Plugin diagnostic: {diagnostic}"));
        }
        lines.push("Enable or disable plugins in global or project settings.json under plugins.<name>.enabled. Set path to the Lua source. Run /reload to apply changes; saved data remains intact.".to_string());
        return Ok(PluginCommandOutcome {
            text: lines.join("\n"),
            messages: Vec::new(),
            error: None,
        });
    }
    let command = engine
        .session
        .classify_session_command(text)
        .with_context(|| format!("Unknown plugin command: /{name}"))?;
    if command.name != name || command.args != args {
        bail!("Plugin commands cannot invoke prompt templates");
    }
    if command.name == "reload" {
        if target.is_some() {
            bail!("Plugin management commands do not accept a target");
        }
    } else {
        let current = command.plugin.as_ref().context("Not a plugin command")?;
        if target.is_some_and(|target| *target != target_value(current)) {
            bail!("Plugin command target is stale; refresh commands and try again");
        }
        if current.mode != InvocationMode::Unary {
            bail!("Streaming plugin commands are not supported by this client");
        }
    }
    let execution =
        pa_core::session_engine::session_commands::execute_plugin_session_command(engine, &command)
            .await;
    let text = execution
        .messages
        .last()
        .map(|message| message.content.text())
        .unwrap_or_default();
    Ok(PluginCommandOutcome {
        text,
        messages: if command.name == "reload" {
            Vec::new()
        } else {
            execution.messages
        },
        error: execution.error,
    })
}

impl Worker {
    pub(crate) async fn handle_plugin_command(&self, payload: &Value) -> DaemonResponse {
        self.run_native_plugin_command("plugin_command", payload)
            .await
    }

    pub(crate) async fn run_native_plugin_command(
        &self,
        command: &str,
        payload: &Value,
    ) -> DaemonResponse {
        let Some(text) = payload.get("text").and_then(Value::as_str) else {
            return response_failure(None, command, "plugin_command requires text", None);
        };
        let text = normalized_management_command(text);
        // Never wait for another mutation or a whole-session replacement: callers
        // must see busy instead of accidentally invoking in a different session.
        let Ok(_replacement) = self.replacement_gate.try_lock() else {
            return response_failure(None, command, "Session is busy", None);
        };
        if let Err(response) = self.require_created(command) {
            return response;
        }
        let _pause = {
            let core = self.core.lock().unwrap();
            let pause = self
                .input_pauses
                .hold_internal(&core.active_session_id, self.work_notify.clone());
            if core.has_ongoing_work() {
                return response_failure(
                    None,
                    command,
                    "Cannot run plugin commands while the session is busy",
                    None,
                );
            }
            pause
        };
        let reload_engine = self.agent_engine.as_ref().filter(|_| {
            pa_core::session_engine::slash_commands::parse_slash_command(text)
                .is_some_and(|(name, _)| name == "reload")
        });
        if let Some(engine) = reload_engine {
            let engine = std::sync::Arc::clone(engine);
            match tokio::task::spawn_blocking(move || engine.reload_live_inputs()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return response_failure(
                        None,
                        command,
                        &format!("session reload failed: {error}"),
                        None,
                    );
                }
                Err(error) => {
                    return response_failure(
                        None,
                        command,
                        &format!("session reload failed: {error}"),
                        None,
                    );
                }
            }
        }
        let outcome = match self
            .engine
            .plugin_command(text, payload.get("target"))
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => return response_failure(None, command, &format!("{error:#}"), None),
        };
        for message in outcome.messages {
            let value = crate::session_commands::custom_message_value(&message);
            let persisted =
                {
                    let mut core = self.core.lock().unwrap();
                    core.store
                        .as_mut()
                        .context("Session is still initializing")
                        .and_then(|store| {
                            if store.is_canonical_bound() {
                                return Ok(());
                            }
                            store.persist_entry("custom_message", json!({
                        "customType": message.custom_type, "content": value["content"],
                        "display": message.display, "details": message.details,
                    })).map(|_| ())
                        })
                };
            if let Err(error) = persisted {
                return response_failure(
                    None,
                    command,
                    &format!("Cannot persist plugin command: {error:#}"),
                    None,
                );
            }
            self.broadcast_custom_row(&value);
        }
        if let Some(error) = outcome.error {
            return response_failure(None, command, &error, None);
        }
        response_success(None, command, Some(json!({ "text": outcome.text })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use pa_agent::scripted::ScriptedProvider;
    use pa_core::session_engine::engine::{create_session, SessionEngineConfig};

    const SOURCE: &str = r"
local count = 0
return {
    commands = {
        {name='count',description='Count in the live plugin generation',run=function(input)
            count = count + 1
            return {count=count,args=input.args,configured=config.value}
        end},
        {name='stream_count',mode='streaming',run=function(input,ctx)
            count = count + 1; ctx.emit(count); return count
        end},
        {name='fail_count',run=function() error('intentional plugin failure') end}
    },
    exports = {{name='read_count',run=function() return count end}}
}
";

    struct Fixture {
        _dir: tempfile::TempDir,
        cwd: std::path::PathBuf,
        agent: std::path::PathBuf,
        worker: Arc<Worker>,
        core: Arc<CoreSessionEngine>,
        provider: Arc<ScriptedProvider>,
    }

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("project");
            let agent = dir.path().join("agent");
            std::fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
            std::fs::create_dir_all(&agent).unwrap();
            std::fs::write(cwd.join("counter.lua"), SOURCE).unwrap();
            std::fs::write(agent.join("settings.json"), json!({
                "harnessRefinements":false,"enableBuiltinSkills":false,
                "bundledSkills":{"websearch":false},
                "plugins":{"counter":{"enabled":true,"path":"counter.lua","config":{"value":"initial"}}}
            }).to_string()).unwrap();
            let model: pa_agent::types::Model = serde_json::from_value(json!({
                "id":"test","name":"Test","api":"openai-completions","provider":"test",
                "baseUrl":"http://127.0.0.1:9","reasoning":false,
                "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
                "contextWindow":10000,"maxTokens":100
            }))
            .unwrap();
            let provider = Arc::new(ScriptedProvider::new(model.clone()));
            let core = Arc::new(
                create_session(SessionEngineConfig {
                    cwd: cwd.clone(),
                    agent_dir: agent.clone(),
                    model: Some(model),
                    stream_fn: Some(provider.stream_fn()),
                    prewarm_ipython_kernel: Some(false),
                    daemonless: true,
                    ..Default::default()
                })
                .await
                .unwrap(),
            );
            // Construct the production worker/engine adapter, but do not serve a
            // daemon socket, start its runner, resolve a provider, or boot a kernel.
            let config = crate::worker::WorkerConfig {
                socket_path: dir.path().join("worker.sock"),
                supervisor_socket_path: std::path::PathBuf::new(),
                token: "test".to_string(),
                worker_instance_id: String::new(),
                active_session_id: "plugin-test".to_string(),
                agent_dir: agent.clone(),
                recovery_journal_path: dir.path().join("recovery.jsonl"),
                script: None,
                telemetry_disabled: Some(true),
            };
            let worker = Arc::new(
                tokio::task::spawn_blocking(move || Worker::new(config, None))
                    .await
                    .unwrap(),
            );
            let engine = worker.agent_engine.as_ref().expect("production engine");
            *engine.session.lock().await = Some(Arc::clone(&core));
            {
                let mut state = worker.core.lock().unwrap();
                state.created = true;
                state.cwd = cwd.display().to_string();
                state.store = Some(crate::session_store::SessionFile::create(
                    &state.cwd, None, 0,
                ));
            }
            Self {
                _dir: dir,
                cwd,
                agent,
                worker,
                core,
                provider,
            }
        }

        fn project(&self, settings: &Value) {
            std::fs::write(
                self.cwd.join(".prime/agent/settings.json"),
                settings.to_string(),
            )
            .unwrap();
        }

        async fn catalog(&self) -> Vec<Value> {
            let response = self.worker.dispatch("get_commands", &json!({})).await;
            assert!(response.success, "{response:?}");
            response.data.unwrap()["commands"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["source"] == "plugin")
                .cloned()
                .collect()
        }

        async fn command(&self, text: &str, target: Option<&Value>) -> DaemonResponse {
            let mut payload = json!({"text":text});
            if let Some(target) = target {
                payload["target"] = target.clone();
            }
            self.worker.dispatch("plugin_command", &payload).await
        }

        fn rows(&self) -> Vec<Value> {
            self.worker
                .core
                .lock()
                .unwrap()
                .store
                .as_ref()
                .unwrap()
                .messages()
        }

        async fn read_count(&self) -> Value {
            use pa_core::kernel::shared::HostRequestPayload;
            let session = self.core.session.session_id().await;
            let handlers = self.core.plugin_host_handlers();
            let catalog = handlers.get("plugin.catalog").unwrap()(HostRequestPayload {
                data: json!({"type":"plugin.catalog","session":session}),
            })
            .await
            .unwrap();
            let target = &catalog["exports"][0];
            handlers.get("plugin.invoke").unwrap()(HostRequestPayload {
                data: json!({"type":"plugin.invoke","session":session,"plugin":"counter",
                    "generation":target["generation"],"export":"read_count","input":{}}),
            })
            .await
            .unwrap()
        }
    }

    #[tokio::test]
    async fn worker_discovery_and_invocation_share_the_real_lua_generation() {
        let fixture = Fixture::new().await;
        let catalog = fixture.catalog().await;
        assert!(
            !catalog.iter().any(|entry| entry["name"] == "stream_count"),
            "unsupported streams stay out of completion"
        );
        let command = catalog
            .iter()
            .find(|entry| entry["name"] == "count")
            .unwrap();
        assert_eq!(command["target"]["plugin"], "counter");
        assert_eq!(command["target"]["kind"], "command");
        assert_eq!(command["target"]["mode"], "unary");
        assert!(command["target"]["generation"].as_str().is_some());
        let first = fixture
            .command("/count one two", Some(&command["target"]))
            .await;
        assert!(first.success, "{first:?}");
        assert_eq!(
            serde_json::from_str::<Value>(first.data.unwrap()["text"].as_str().unwrap()).unwrap(),
            json!({"count":1,"args":"one two","configured":"initial"})
        );
        let second = fixture.command("/count three", None).await;
        assert!(second.success, "{second:?}");
        assert_eq!(
            fixture.read_count().await,
            json!(2),
            "kernel exports see the command's live Lua state"
        );
        assert_eq!(fixture.rows().len(), 4, "echo and result for each command");
        assert!(
            fixture.provider.calls().is_empty(),
            "native commands must not call inference"
        );
        assert!(fixture.core.assembled_system_prompt().contains("/count"));
        let exported = fixture.worker.engine.export_system_prompt().unwrap();
        assert_eq!(exported, fixture.core.assembled_system_prompt());
    }

    #[tokio::test]
    async fn reload_retires_targets_and_disable_removes_inventory_without_history() {
        let fixture = Fixture::new().await;
        let old = fixture
            .catalog()
            .await
            .into_iter()
            .find(|entry| entry["name"] == "count")
            .unwrap();
        fixture.project(&json!({"plugins":{"counter":{"config":{"value":"replacement"}}}}));
        let before = fixture.rows();
        let core_before = fixture.core.session.entries().await;
        let reload = fixture.command("/plugins reload", None).await;
        assert!(reload.success, "{reload:?}");
        assert_eq!(fixture.rows(), before);
        assert_eq!(fixture.core.session.entries().await, core_before);
        let current = fixture
            .catalog()
            .await
            .into_iter()
            .find(|entry| entry["name"] == "count")
            .unwrap();
        assert_ne!(current["target"]["generation"], old["target"]["generation"]);
        let stale = fixture.command("/count stale", Some(&old["target"])).await;
        assert!(!stale.success, "{stale:?}");
        assert!(stale.error.unwrap().contains("stale"));
        assert_eq!(fixture.read_count().await, json!(0));
        assert_eq!(fixture.rows(), before);
        let fresh = fixture
            .command("/count current", Some(&current["target"]))
            .await;
        assert!(fresh.success, "{fresh:?}");
        assert!(fresh.data.unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("replacement"));
        fixture.project(&json!({"plugins":{"counter":{"enabled":false}}}));
        let history = fixture.rows();
        let disabled = fixture.worker.dispatch("reload", &json!({})).await;
        assert!(
            disabled.success,
            "legacy reload must also reconcile: {disabled:?}"
        );
        assert!(fixture.catalog().await.is_empty());
        assert_eq!(fixture.rows(), history);
        assert!(!fixture
            .core
            .assembled_system_prompt()
            .contains("plugin.counter.read_count"));
        let unavailable = fixture.command("/count no", None).await;
        assert!(!unavailable.success);
        assert!(unavailable
            .error
            .unwrap()
            .contains("Unknown plugin command"));
        assert_eq!(fixture.rows(), history);
        assert!(fixture.provider.calls().is_empty());
    }

    #[tokio::test]
    async fn worker_rejects_busy_streaming_and_unknown_without_model_fallback() {
        let fixture = Fixture::new().await;
        let before = fixture.rows();
        for text in [
            "/stream_count",
            "/not_a_plugin",
            "/goal status",
            "ordinary text",
        ] {
            let response = fixture.command(text, None).await;
            assert!(!response.success, "{text}: {response:?}");
        }
        fixture.worker.core.lock().unwrap().busy = true;
        let busy = fixture.command("/count", None).await;
        assert!(!busy.success);
        assert!(busy.error.unwrap().contains("busy"));
        fixture.worker.core.lock().unwrap().busy = false;
        let replacement = fixture.worker.replacement_gate.lock().await;
        let concurrent = fixture.command("/count", None).await;
        assert!(!concurrent.success);
        drop(replacement);
        assert!(
            !fixture.worker.input_pauses.paused(),
            "failed commands release their pause"
        );
        assert_eq!(fixture.read_count().await, json!(0));
        assert_eq!(fixture.rows(), before);
        assert!(fixture.provider.calls().is_empty());
    }

    #[tokio::test]
    async fn legacy_refine_and_native_refine_share_the_active_lua_handler() {
        let fixture = Fixture::new().await;
        // The command name is plugin-owned. No daemon refinement engine or
        // model adapter takes part in either transport route.
        std::fs::write(
            fixture.cwd.join("counter.lua"),
            SOURCE.replace("name='count'", "name='refine'"),
        )
        .unwrap();
        let reload = fixture.command("/reload", None).await;
        assert!(reload.success, "{reload:?}");
        let command = fixture
            .catalog()
            .await
            .into_iter()
            .find(|entry| entry["name"] == "refine")
            .unwrap();
        let native = fixture
            .command("/refine native request", Some(&command["target"]))
            .await;
        assert!(native.success, "{native:?}");
        let legacy = fixture
            .worker
            .dispatch(
                "refine",
                &json!({"global":true,"instructions":"legacy request /reload"}),
            )
            .await;
        assert!(legacy.success, "{legacy:?}");
        assert_eq!(legacy.command, "refine");
        let result: Value =
            serde_json::from_str(legacy.data.unwrap()["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            result,
            json!({"count":2,"args":"--global legacy request /reload","configured":"initial"})
        );
        assert_eq!(fixture.read_count().await, json!(2));
        let rollback = fixture
            .worker
            .dispatch("refine", &json!({"rollbackId":"record-123","global":true}))
            .await;
        assert!(rollback.success, "{rollback:?}");
        let result: Value =
            serde_json::from_str(rollback.data.unwrap()["text"].as_str().unwrap()).unwrap();
        assert_eq!(result["args"], "--global rollback record-123");
        fixture.worker.core.lock().unwrap().busy = true;
        let busy = fixture
            .worker
            .dispatch("refine", &json!({"instructions":"never run"}))
            .await;
        assert!(!busy.success && busy.error.unwrap().contains("busy"));
        fixture.worker.core.lock().unwrap().busy = false;
        assert_eq!(fixture.read_count().await, json!(3));
        let saved = fixture.agent.join("harness/memories.json");
        std::fs::create_dir_all(saved.parent().unwrap()).unwrap();
        std::fs::write(&saved, "preserved user data").unwrap();
        fixture
            .project(&json!({"plugins":{"counter":{"enabled":false},"harness":{"enabled":false}}}));
        let reload = fixture.command("/reload", None).await;
        assert!(reload.success, "{reload:?}");
        assert!(!fixture
            .catalog()
            .await
            .iter()
            .any(|entry| entry["name"] == "refine"));
        let before = fixture.rows();
        for payload in [
            json!({}),
            json!({"instructions":"do not become a model prompt"}),
        ] {
            let rejected = fixture.worker.dispatch("refine", &payload).await;
            assert!(!rejected.success, "{rejected:?}");
            assert!(rejected.error.unwrap().contains("Unknown plugin command"));
        }
        let stale = fixture
            .command("/refine stale", Some(&command["target"]))
            .await;
        assert!(!stale.success);
        assert_eq!(fixture.rows(), before);
        assert_eq!(
            std::fs::read_to_string(saved).unwrap(),
            "preserved user data"
        );
        assert!(
            fixture.provider.calls().is_empty(),
            "neither wire route may fall back to inference"
        );
    }

    #[tokio::test]
    async fn failed_reload_keeps_the_last_good_inventory_and_manager_reports_diagnostic() {
        let fixture = Fixture::new().await;
        let before = fixture.catalog().await;
        std::fs::write(fixture.cwd.join("counter.lua"), "invalid Lua !").unwrap();
        let reload = fixture.command("/reload", None).await;
        assert!(!reload.success, "{reload:?}");
        assert_eq!(fixture.catalog().await, before);
        let manager = fixture.command("/plugins", None).await;
        assert!(manager.success, "{manager:?}");
        let data = manager.data.unwrap();
        let text = data["text"].as_str().unwrap();
        assert!(
            text.contains("/count")
                && text.contains("Plugin diagnostic:")
                && text.contains("settings.json")
        );
        assert!(
            fixture.rows().is_empty(),
            "management must not change history"
        );
        let command = fixture.command("/count still-active", None).await;
        assert!(command.success, "{command:?}");
        let failed = fixture.command("/fail_count", None).await;
        assert!(!failed.success, "{failed:?}");
        assert!(failed.error.unwrap().contains("intentional plugin failure"));
        assert_eq!(
            fixture.rows().len(),
            4,
            "plugin failures retain echo and result"
        );
        assert!(fixture.provider.calls().is_empty());
        assert!(fixture.agent.join("settings.json").exists());
    }
}
