//! Real CLI subprocesses and a supervisor/worker over private paths and local providers only.
#![cfg(unix)]

use serde_json::{json, Value};

#[path = "plugin_headless/daemon.rs"]
mod daemon;
#[path = "plugin_headless/harness.rs"]
mod harness;
use std::path::PathBuf;
use std::process::{Command, Output};

const PLUGIN: &str = r"
return {
 exports={{name='report',run=function() return config end}},
 commands={
  {name='plugin_echo',run=function(input,ctx)
    return {args=input.args,value=config.value,identity=ctx.call('session.info',{})}
  end},
  {name='refine',run=function(input) return {args=input.args,policy='configured-lua'} end},
  {name='plugin_fail',run=function() error('fixture command failed') end},
  {name='plugin_stream',mode='streaming',run=function() return null end}
 }
}
";

struct Fixture {
    _dir: tempfile::TempDir,
    cwd: PathBuf,
    agent: PathBuf,
    runtime: PathBuf,
    outbound_proxy: std::net::TcpListener,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        let runtime = dir.path().join("runtime");
        for path in [&cwd, &agent, &runtime] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(cwd.join("fixture.lua"), PLUGIN).unwrap();
        std::fs::write(agent.join("settings.json"), json!({
            "enableBuiltinSkills":false,"harnessRefinements":false,
            "plugins":{"fixture":{"enabled":true,"path":"fixture.lua","config":{"value":"global"}}}
        }).to_string()).unwrap();
        let fixture = Self {
            _dir: dir,
            cwd,
            agent,
            runtime,
            outbound_proxy: std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
        };
        fixture.settings(&json!({"plugins":{"fixture":{"config":{"value":"project"}}}}));
        fixture
    }

    fn settings(&self, value: &Value) {
        let dir = self.cwd.join(".prime/agent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), value.to_string()).unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
        command.env_clear();
        for name in ["PATH", "LD_LIBRARY_PATH", "PI_PACKAGE_DIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        for name in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
            "TMPDIR",
            "PRIME_AGENT_KERNEL_VENV",
            "RLM_SESSION_DIR",
            "RLM_HARNESS_STATE_DIR",
            "RLM_GLOBAL_HARNESS_STATE_DIR",
            "UV_CACHE_DIR",
        ] {
            let path = self.runtime.join(name);
            std::fs::create_dir_all(&path).unwrap();
            command.env(name, path);
        }
        // Non-loopback HTTP(S), including any background catalog discovery, cannot leave
        // this test. A bound listener with no forwarding handler is the proxy sink.
        let proxy = format!("http://{}", self.outbound_proxy.local_addr().unwrap());
        command
            .env("HTTP_PROXY", &proxy)
            .env("http_proxy", &proxy)
            .env("HTTPS_PROXY", &proxy)
            .env("https_proxy", &proxy)
            .env("ALL_PROXY", &proxy)
            .env("all_proxy", &proxy)
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1");
        command
            .env("PRIME_AGENT_CODING_AGENT_DIR", &self.agent)
            .env("PRIME_AGENT_SESSION_DIR", self.runtime.join("sessions"))
            .env(
                "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
                self.runtime.join("daemon.sock"),
            )
            .env("PI_OFFLINE", "1")
            .env("UV_OFFLINE", "1")
            .env("TERM", "dumb")
            .current_dir(&self.cwd);
        command
    }

    fn run(&self, args: &[&str], script: &Value) -> Output {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let child = self
            .command()
            .args(args)
            .env("PRIME_AGENT_FAUX_SCRIPT", script.to_string())
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _ = sender.send(child.wait_with_output());
        });
        let result = receiver.recv_timeout(std::time::Duration::from_secs(30));
        if result.is_err() {
            // This group belongs only to this test child and its descendants.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        waiter.join().unwrap();
        result.expect("CLI subprocess exceeded 30 seconds").unwrap()
    }

    fn entries(&self) -> Vec<Value> {
        let mut directories = vec![self.runtime.join("sessions")];
        let mut entries = Vec::new();
        while let Some(directory) = directories.pop() {
            let Ok(files) = std::fs::read_dir(directory) else {
                continue;
            };
            for file in files {
                let file = file.unwrap();
                if file.file_type().unwrap().is_dir() {
                    directories.push(file.path());
                } else if file.path().extension().is_some_and(|ext| ext == "jsonl") {
                    let journal: Vec<Value> = std::fs::read_to_string(file.path())
                        .unwrap()
                        .lines()
                        .map(|line| serde_json::from_str(line).unwrap())
                        .collect();
                    // Only canonical session journals, not auxiliary artifact ledgers.
                    if journal.first().is_some_and(|row| row["type"] == "session") {
                        entries.extend(journal);
                    }
                }
            }
        }
        entries
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn configured_lua_command_crosses_cli_and_persists_only_command_rows() {
    let fixture = Fixture::new();
    let output = fixture.run(
        &["-p", "/plugin_echo exact arguments"],
        &json!({"responses":[]}),
    );
    success(&output);
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["args"], "exact arguments");
    assert_eq!(value["value"], "project");
    assert_eq!(value["identity"]["cwd"], fixture.cwd.display().to_string());
    assert_eq!(value["identity"]["role"], "root");
    assert!(value["identity"]["session"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    let entries = fixture.entries();
    assert_eq!(entries.first().unwrap()["type"], "session");
    let messages: Vec<_> = entries
        .iter()
        .filter(|row| row["type"] == "custom_message")
        .collect();
    assert_eq!(
        messages
            .iter()
            .map(|row| row["customType"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["session_slash_command", "session_slash_command_result"]
    );
    assert_eq!(messages[1]["details"]["success"], true);
    assert!(!entries.iter().any(|row| row["type"] == "message"));
}

#[test]
fn lua_command_json_uses_shared_executor_frames_without_a_model_turn() {
    let fixture = Fixture::new();
    let output = fixture.run(
        &["--mode", "json", "-p", "/plugin_echo json args"],
        &json!({"responses":[]}),
    );
    success(&output);
    let frames: Vec<Value> = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!frames.iter().any(|frame| frame["type"] == "agent_start"));
    let result = frames
        .iter()
        .find(|frame| {
            frame["type"] == "message_end"
                && frame["message"]["customType"] == "session_slash_command_result"
        })
        .unwrap();
    assert_eq!(
        result["message"]["details"]["command"]["name"],
        "plugin_echo"
    );
    assert_eq!(result["message"]["details"]["success"], true);
    assert!(frames
        .iter()
        .any(|frame| frame["type"] == "session_action_update"
            && frame["actions"]["active"]["phase"] == "running"));
    let value: Value =
        serde_json::from_str(result["message"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(value["args"], "json args");
}

#[test]
fn unavailable_slash_input_never_becomes_a_model_prompt() {
    for prompt in [
        "/unknown_plugin args",
        "  /unknown_plugin",
        "/model",
        "/",
        "/skill:missing",
    ] {
        for args in [
            vec!["-p", prompt, "must not run"],
            vec!["--mode", "json", "-p", prompt, "must not run"],
        ] {
            let fixture = Fixture::new();
            let output = fixture.run(&args, &json!({"responses":["MODEL MUST NEVER RUN"]}));
            assert_eq!(output.status.code(), Some(1), "prompt: {prompt}");
            assert!(String::from_utf8_lossy(&output.stderr)
                .contains("Unknown or unavailable headless command:"));
            assert!(!stdout(&output).contains("MODEL MUST NEVER RUN"));
            assert!(!fixture
                .entries()
                .iter()
                .any(|row| row["type"] == "message" || row["type"] == "custom_message"));
        }
    }
}

#[test]
fn disabled_command_is_unavailable_and_saved_data_survives() {
    let fixture = Fixture::new();
    let saved = fixture.agent.join("saved-plugin-data.json");
    std::fs::write(&saved, "retained").unwrap();
    fixture.settings(&json!({"plugins":{"fixture":{"enabled":false}}}));
    let output = fixture.run(
        &["-p", "/plugin_echo"],
        &json!({"responses":["must not run"]}),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert_eq!(std::fs::read_to_string(saved).unwrap(), "retained");
}

#[test]
fn lua_failures_and_unsupported_streams_reject_the_prompt_wait() {
    for (prompt, error) in [
        ("/plugin_fail", "fixture command failed"),
        (
            "/plugin_stream",
            "streaming plugin commands require a streaming client",
        ),
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(
            &["-p", prompt, "must not run"],
            &json!({"responses":["must not run"]}),
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(stdout(&output).is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains(error));
        assert!(!fixture
            .entries()
            .iter()
            .any(|row| row["message"]["role"] == "user"));
    }
}

#[test]
fn reload_has_visible_output_but_no_turn_or_management_history_rows() {
    for args in [
        vec!["-p", "/reload"],
        vec!["--mode", "json", "-p", "/reload"],
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(&args, &json!({"responses":[]}));
        success(&output);
        assert!(stdout(&output).contains("Plugins reloaded (revision"));
        assert!(!fixture
            .entries()
            .iter()
            .any(|row| row["type"] == "message" || row["type"] == "custom_message"));
    }
}

#[test]
fn prompt_inspection_lists_active_contributions_without_persistence_or_kernel() {
    let fixture = Fixture::new();
    let output = fixture.run(&["prompt", "--json"], &json!({"responses":[]}));
    success(&output);
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    let prompt = value["prompt"].as_str().unwrap();
    assert!(prompt.contains("plugin.fixture.report(**kwargs)"));
    assert!(prompt.contains("/plugin_echo"));
    assert!(value["segments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|segment| segment["name"] == "active-plugins" && segment["kind"] == "dynamic"));
    assert!(fixture.entries().is_empty());
    assert!(!fixture
        .runtime
        .join("PRIME_AGENT_KERNEL_VENV/pyvenv.cfg")
        .exists());
    fixture.settings(&json!({"plugins":{"fixture":{"enabled":false}}}));
    let disabled = fixture.run(&["prompt", "--json"], &json!({"responses":[]}));
    success(&disabled);
    let disabled: Value = serde_json::from_str(&stdout(&disabled)).unwrap();
    assert!(!disabled["prompt"]
        .as_str()
        .unwrap()
        .contains("plugin.fixture.report"));
    assert!(!disabled["prompt"]
        .as_str()
        .unwrap()
        .contains("/plugin_echo"));
    assert_eq!(value["cachedPrefixLen"], disabled["cachedPrefixLen"]);
    let prefix = usize::try_from(value["cachedPrefixLen"].as_u64().unwrap()).unwrap();
    assert_eq!(
        &prompt[..prefix],
        &disabled["prompt"].as_str().unwrap()[..prefix]
    );
}

#[test]
fn actual_model_prompt_includes_the_active_plugin_inventory() {
    let fixture = Fixture::new();
    let output = fixture.run(
        &["-p", "inspect"],
        &json!({"responses":[{"systemPrompt":true}]}),
    );
    success(&output);
    assert!(stdout(&output).contains("plugin.fixture.report(**kwargs)"));
    assert!(stdout(&output).contains("/plugin_echo"));
}

#[test]
fn templates_and_skills_still_expand_before_command_routing() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.agent.join("prompts")).unwrap();
    std::fs::write(
        fixture.agent.join("prompts/alias.md"),
        "/plugin_echo $ARGUMENTS",
    )
    .unwrap();
    let output = fixture.run(
        &["-p", "/alias templated arguments"],
        &json!({"responses":[]}),
    );
    success(&output);
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["args"], "templated arguments");
    let skill = fixture.agent.join("skills/local");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: local\ndescription: Local fixture skill\n---\nLocal skill body.",
    )
    .unwrap();
    let output = fixture.run(
        &["-p", "/skill:local do it"],
        &json!({"responses":["skill used"]}),
    );
    success(&output);
    assert_eq!(stdout(&output), "skill used\n");
}

#[test]
fn prompt_inspection_keeps_model_rules_and_append_layers_consistent() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.agent.join("model-prompts.toml"),
        "[[rule]]\nmatch = ['mock/mock-1']\nfiles = ['model.md']\n",
    )
    .unwrap();
    std::fs::write(
        fixture.agent.join("model.md"),
        "Model-specific fixture layer.",
    )
    .unwrap();
    std::fs::write(
        fixture.agent.join("APPEND_SYSTEM.md"),
        "Appended fixture instructions.",
    )
    .unwrap();
    let output = fixture.run(
        &["prompt", "--json", "--model", "mock/mock-1"],
        &json!({"responses":[]}),
    );
    success(&output);
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert!(value["prompt"]
        .as_str()
        .unwrap()
        .contains("Model-specific fixture layer."));
    assert!(value["prompt"]
        .as_str()
        .unwrap()
        .contains("Appended fixture instructions."));
}

#[test]
fn configured_refine_is_a_generic_lua_command_without_core_refinement_events() {
    let fixture = Fixture::new();
    let output = fixture.run(
        &["--mode", "json", "-p", "/refine capture this tactic"],
        &json!({"responses":[]}),
    );
    success(&output);
    let frames: Vec<Value> = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!frames.iter().any(|frame| matches!(
        frame["type"].as_str(),
        Some("agent_start" | "refine_complete" | "refine_failed")
    )));
    let row = frames
        .iter()
        .find(|frame| {
            frame["type"] == "message_end"
                && frame["message"]["customType"] == "session_slash_command_result"
        })
        .unwrap();
    let result: Value = serde_json::from_str(row["message"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        result,
        json!({"args":"capture this tactic","policy":"configured-lua"})
    );
    assert_eq!(row["message"]["details"]["command"]["name"], "refine");
    let entries = fixture.entries();
    let rows: Vec<_> = entries
        .iter()
        .filter(|entry| entry["type"] == "custom_message")
        .collect();
    assert_eq!(
        rows.iter()
            .map(|entry| entry["customType"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["session_slash_command", "session_slash_command_result"]
    );
}

#[test]
fn disabled_refine_policy_has_no_command_prompt_inventory_or_model_turn() {
    let fixture = Fixture::new();
    fixture.settings(&json!({"plugins":{"fixture":{"enabled":false},"harness":{"enabled":false}}}));
    let output = fixture.run(
        &["-p", "/refine must not run"],
        &json!({"responses":["MODEL MUST NEVER RUN"]}),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Unknown or unavailable headless command:"));
    assert!(!fixture
        .entries()
        .iter()
        .any(|entry| entry["type"] == "message" || entry["type"] == "custom_message"));
    let inspected = fixture.run(&["prompt", "--json"], &json!({"responses":[]}));
    success(&inspected);
    let prompt: Value = serde_json::from_str(&stdout(&inspected)).unwrap();
    assert!(!prompt["prompt"].as_str().unwrap().contains("/refine"));
    assert!(!prompt["prompt"]
        .as_str()
        .unwrap()
        .contains("plugin.harness."));
}

#[test]
fn prompt_inspection_uses_first_party_harness_activation_settings() {
    let fixture = Fixture::new();
    // Explicit plugin activation wins over the fixture's legacy harnessRefinements=false.
    fixture.settings(&json!({"plugins":{"fixture":{"enabled":false},"harness":{
        "enabled":true,"config":{"autoRefine":{"enabled":false}}
    }}}));
    let active = fixture.run(&["prompt", "--json"], &json!({"responses":[]}));
    success(&active);
    let active: Value = serde_json::from_str(&stdout(&active)).unwrap();
    assert!(active["prompt"]
        .as_str()
        .unwrap()
        .contains("plugin.harness."));
    assert!(active["prompt"].as_str().unwrap().contains("/refine"));
    fixture.settings(&json!({"plugins":{"fixture":{"enabled":false},"harness":{"enabled":false}}}));
    let disabled = fixture.run(&["prompt", "--json"], &json!({"responses":[]}));
    success(&disabled);
    let disabled: Value = serde_json::from_str(&stdout(&disabled)).unwrap();
    assert!(!disabled["prompt"]
        .as_str()
        .unwrap()
        .contains("plugin.harness."));
    assert!(!disabled["prompt"].as_str().unwrap().contains("/refine"));
    assert!(fixture.entries().is_empty());
}
