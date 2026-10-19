//! The interactive-mode unit battery: the session flag mapping, the continue-recent
//! view target, the onboarding gate, and the startup fork/select contracts.

use super::*;
// The tests call the trait methods directly (the impl header alone
// does not import them).
use pa_tui::interactive::OnboardingSink;
use serde_json::Map;

#[test]
fn session_flags_map_to_selections() {
    let mut session = crate::mode::SessionOptions::default();
    let dir = tempfile::TempDir::new().expect("temp dir");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("sessions dir");
    assert_eq!(
        session_selection(&session, Some(session_dir.as_path())),
        SessionSelection::New
    );
    // `--continue` never maps to a resume: the candidate resolves through the
    // agents view or falls through to a fresh session.
    session.continue_recent = true;
    assert_eq!(
        session_selection(&session, Some(session_dir.as_path())),
        SessionSelection::New
    );
    session.continue_recent = false;
    session.resume = Some("a1b2c3".to_string());
    assert_eq!(
        session_selection(&session, Some(session_dir.as_path())),
        SessionSelection::Attach("a1b2c3".to_string())
    );
    let saved = session_dir.join("deadbeefcafe.jsonl");
    std::fs::write(&saved, "{}\n").expect("write file");
    session.resume = Some("deadbeefcafe".to_string());
    assert_eq!(
        session_selection(&session, Some(session_dir.as_path())),
        SessionSelection::Resume(saved)
    );

    let file = dir.path().join("saved.jsonl");
    std::fs::write(&file, "{}\n").expect("write file");
    session.resume = Some(file.to_string_lossy().to_string());
    assert_eq!(
        session_selection(&session, Some(session_dir.as_path())),
        SessionSelection::Resume(file)
    );
}

fn run_options_for_continue(dir: &std::path::Path) -> RunOptions {
    use crate::mode::{AppMode, RuntimeConfig};
    RunOptions {
        app_mode: AppMode::Interactive,
        config: RuntimeConfig {
            cwd: dir.to_path_buf(),
            agent_dir: dir.join("agent"),
            ..Default::default()
        },
        session: crate::mode::SessionOptions::default(),
        messages: Vec::new(),
        file_args: Vec::new(),
        daemon_socket: None,
        list_models: None,
        initial_message: None,
        initial_images: Vec::new(),
        verbose: false,
        offline: false,
        agents_view_requested: false,
        attach_agent: None,
    }
}

/// A saved session file the cwd-scoped scans resolve, in the same
/// shape `SessionFile::create` writes (header with id + cwd).
fn seed_saved_session(
    session_dir: &std::path::Path,
    id: &str,
    cwd: &std::path::Path,
) -> std::path::PathBuf {
    use std::io::Write;
    std::fs::create_dir_all(session_dir).expect("sessions dir");
    let path = session_dir.join(format!("{id}.jsonl"));
    let mut file = std::fs::File::create(&path).expect("create session file");
    let header = serde_json::json!({
        "type": "session", "id": id,
        "cwd": cwd.display().to_string(),
        "timestamp": "2024-01-01T00:00:00.000Z", "version": 3,
    });
    writeln!(file, "{header}").expect("write header");
    path
}

#[test]
fn continue_recent_targets_the_newest_saved_session_for_the_cwd() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let session_dir = agent_dir.join("sessions");
    seed_saved_session(&session_dir, "old0000000000000000000000000001", dir.path());
    let candidate =
        seed_saved_session(&session_dir, "newest00000000000000000000000001", dir.path());
    // A session from another cwd is never the candidate.
    let other_cwd = tempfile::TempDir::new().expect("other cwd");
    seed_saved_session(
        &session_dir,
        "foreign0000000000000000000000001",
        other_cwd.path(),
    );

    let future = std::time::SystemTime::now() + std::time::Duration::from_mins(1);
    let handle = std::fs::File::options()
        .append(true)
        .open(&candidate)
        .expect("open candidate");
    handle.set_modified(future).expect("nudge mtime");

    let mut options = run_options_for_continue(dir.path());
    options.session.continue_recent = true;
    options.session.session_dir = Some(session_dir);
    let view = continue_recent_view(&options, false).expect("candidate resolves");
    assert_eq!(
        view.session_id, "newest00000000000000000000000001",
        "the newest saved session for the cwd is the preselected row"
    );
    assert!(
        view.notice.contains("newest00000000000000000000000001"),
        "the notice names the candidate: {}",
        view.notice
    );
    assert!(
        candidate.exists(),
        "resolution only reads the candidate file"
    );
}

#[test]
fn continue_recent_without_a_candidate_opens_a_fresh_session() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let session_dir = agent_dir.join("sessions");
    let other_cwd = tempfile::TempDir::new().expect("other cwd");
    seed_saved_session(
        &session_dir,
        "foreign0000000000000000000000001",
        other_cwd.path(),
    );

    let mut options = run_options_for_continue(dir.path());
    options.session.continue_recent = true;
    options.session.session_dir = Some(session_dir);
    assert!(
        continue_recent_view(&options, false).is_none(),
        "no candidate: the launch opens a fresh session, not the view"
    );
}

#[test]
fn continue_recent_defers_to_resume_and_no_session() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let session_dir = agent_dir.join("sessions");
    seed_saved_session(&session_dir, "newest00000000000000000000000001", dir.path());

    let mut options = run_options_for_continue(dir.path());
    options.session.session_dir = Some(session_dir);
    options.session.continue_recent = true;
    assert!(
        continue_recent_view(&options, false).is_some(),
        "a bare --continue opens the view preselected on the candidate"
    );
    options.session.resume = Some("named-session".to_string());
    assert!(
        continue_recent_view(&options, false).is_none(),
        "an explicit --resume selector wins over --continue"
    );
    options.session.resume = None;
    options.session.no_session = true;
    assert!(
        continue_recent_view(&options, false).is_none(),
        "--no-session owns the launch: nothing to continue"
    );
}

#[test]
fn continue_recent_view_gates_on_onboarding_and_the_flag() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let session_dir = agent_dir.join("sessions");
    seed_saved_session(&session_dir, "newest00000000000000000000000001", dir.path());

    let mut options = run_options_for_continue(dir.path());
    options.session.session_dir = Some(session_dir);
    assert!(
        continue_recent_view(&options, false).is_none(),
        "a non-continue launch never opens the agents view through this path"
    );
    options.session.continue_recent = true;
    assert!(
        continue_recent_view(&options, true).is_none(),
        "a pending onboarding keeps the first-run startup"
    );
    assert!(
        continue_recent_view(&options, false).is_some(),
        "a completed onboarding opens the view preselected on the candidate"
    );
}

#[test]
fn onboarding_gate_follows_settings_and_auth() {
    use crate::mode::{AppMode, RuntimeConfig};

    fn run_options(dir: &std::path::Path) -> RunOptions {
        RunOptions {
            app_mode: AppMode::Interactive,
            config: RuntimeConfig {
                cwd: dir.to_path_buf(),
                agent_dir: dir.join("agent"),
                ..Default::default()
            },
            session: crate::mode::SessionOptions::default(),
            messages: Vec::new(),
            file_args: Vec::new(),
            daemon_socket: None,
            list_models: None,
            initial_message: None,
            initial_images: Vec::new(),
            verbose: false,
            offline: false,
            agents_view_requested: false,
            attach_agent: None,
        }
    }

    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");

    let mut settings = pa_core::settings::SettingsManager::create(dir.path(), &agent);
    settings.set_onboarding_shown(true).expect("set flag");
    let options = run_options(dir.path());
    assert!(onboarding_task(&options, None).is_none());

    settings.set_onboarding_shown(false).expect("reset flag");

    // Flagless launch: a models.json provider key + saved default model resolve the
    // startup model, so the onboarding task mounts; a fresh home completes silently.
    std::fs::write(
        agent.join("models.json"),
        r#"{ "providers": {
                "onboard-test": {
                    "baseUrl": "https://onboard.test", "apiKey": "sk-onboard",
                    "api": "openai-completions",
                    "models": [ { "id": "m1", "name": "M1" } ]
                },
                "onboard-naked": {
                    "baseUrl": "https://naked.test",
                    // Present (custom models require "apiKey", same as TS)
                    // but the `!command` resolves to no credential, so the
                    // provider stays unauthorized.
                    "apiKey": "!exit 1",
                    "api": "openai-completions",
                    "models": [ { "id": "m2", "name": "M2" } ]
                }
            } }"#,
    )
    .expect("models.json");
    let mut settings = pa_core::settings::SettingsManager::create(dir.path(), &agent);
    settings
        .set_default_model_and_provider("onboard-test", "m1")
        .expect("saved default");
    let options = run_options(dir.path());
    let task = onboarding_task(&options, None).expect("the ready home mounts the flow");
    assert!(
        (task.model_ready)(),
        "the configured default model is ready (the question flow)"
    );

    // Explicit flags that resolve to a provider without configured auth mount the
    // task too — the full sign-in flow, not the question.
    let mut options = run_options(dir.path());
    options.config.provider = Some("onboard-naked".into());
    options.config.model = Some("m2".into());
    let task = onboarding_task(&options, None).expect("the flag alone mounts the flow");
    assert!(
        !(task.model_ready)(),
        "the naked provider leaves the model not ready (the full flow)"
    );
    assert!(
        task.current_model
            .as_ref()
            .is_some_and(|model| model.id == "m2"),
        "the resolved startup model rides the task (TS getCurrentModel)"
    );
}

#[test]
fn settings_sink_persists_completion() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let sink = SettingsOnboardingSink {
        cwd: dir.path().to_path_buf(),
        agent_dir: agent_dir.clone(),
    };
    assert!(!sink.onboarding_shown());
    sink.mark_onboarding_complete()
        .expect("complete onboarding");
    let settings = pa_core::settings::SettingsManager::create(dir.path(), &agent_dir);
    assert!(settings.get_onboarding_shown());
}

#[test]
fn build_tui_options_reads_code_block_indent_settings() {
    // `markdown.codeBlockIndent` rides InteractiveOptions at startup; a non-default
    // value reaches the TUI, no setting keeps the default two spaces.
    fn run_options(dir: &std::path::Path) -> RunOptions {
        RunOptions {
            app_mode: crate::mode::AppMode::Interactive,
            config: crate::mode::RuntimeConfig {
                cwd: dir.to_path_buf(),
                agent_dir: dir.join("agent"),
                ..Default::default()
            },
            session: crate::mode::SessionOptions::default(),
            messages: Vec::new(),
            file_args: Vec::new(),
            daemon_socket: None,
            list_models: None,
            initial_message: None,
            initial_images: Vec::new(),
            verbose: false,
            offline: false,
            agents_view_requested: false,
            attach_agent: None,
        }
    }

    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::write(
        agent.join("settings.json"),
        r#"{ "markdown": { "codeBlockIndent": "    " } }"#,
    )
    .expect("settings.json");
    let options = build_tui_options(
        &run_options(dir.path()),
        dir.path().join("d.sock"),
        std::sync::Arc::default(),
    )
    .expect("options");
    assert_eq!(options.code_block_indent, "    ");

    let bare = tempfile::TempDir::new().expect("temp dir");
    std::fs::create_dir_all(bare.path().join("agent")).expect("agent dir");
    let options = build_tui_options(
        &run_options(bare.path()),
        bare.path().join("d.sock"),
        std::sync::Arc::default(),
    )
    .expect("options");
    assert_eq!(options.code_block_indent, "  ");
}

/// A session with one user/assistant exchange, in the same shape
/// `SessionManager::persisted` + appends produce.
fn seed_session(
    session_dir: &std::path::Path,
    cwd: &std::path::Path,
    user_text: &str,
) -> (std::path::PathBuf, String) {
    use pa_types::ai::{AssistantMessage, StopReason, Usage, UserContent, UserMessage};
    use pa_types::session::AgentMessage;
    let mut session = pa_core::session::manager::SessionManager::persisted(cwd, session_dir);
    session
        .append_message(AgentMessage::User(UserMessage {
            // The block shape a real run writes, so the assertions read the shape
            // the shipped sessions carry.
            content: UserContent::Blocks(vec![pa_types::ai::UserContentBlock::Text(
                pa_types::ai::TextContent {
                    text: user_text.to_string(),
                    text_signature: None,
                    rest: Map::default(),
                },
            )]),
            timestamp: 0,
            rest: Map::default(),
        }))
        .expect("write user message");
    session
        .append_message(AgentMessage::Assistant(AssistantMessage {
            content: vec![pa_types::ai::AssistantContentBlock::Text(
                pa_types::ai::TextContent {
                    text: "the answer".to_string(),
                    text_signature: None,
                    rest: Map::default(),
                },
            )],
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            model: "gpt-x".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: Map::default(),
        }))
        .expect("write assistant message");
    let id = session.get_session_id().to_string();
    (
        session
            .get_session_file()
            .expect("session file")
            .to_path_buf(),
        id,
    )
}

fn read_jsonl(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .expect("read session file")
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .expect("valid session entries")
}

#[test]
fn fork_startup_selection_copies_the_source_under_a_fresh_header() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let cwd = dir.path().join("project");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("sessions dir");
    let (source, id) = seed_session(&session_dir, &cwd, "original question");
    let before = std::fs::read(&source).expect("read source");

    let selection = fork_startup_selection(&id, &cwd, Some(&session_dir)).expect("fork startup");
    let SessionSelection::Resume(fork) = &selection else {
        panic!("the fork opens as a resume of the forked file, got {selection:?}");
    };

    assert!(fork.is_file(), "the fork file landed on disk");
    assert_ne!(fork, &source, "the fork is a new session file");
    assert!(
        fork.starts_with(&session_dir),
        "the fork lives in the session dir"
    );
    let fork_entries = read_jsonl(fork);
    let header = &fork_entries[0];
    assert_eq!(header["type"], "session");
    assert_ne!(header["id"].as_str(), Some(id.as_str()));
    assert_eq!(
        header["parentSession"].as_str(),
        Some(source.display().to_string().as_str()),
        "the fork header parents at the source"
    );
    assert_eq!(
        header["cwd"].as_str(),
        Some(cwd.display().to_string().as_str())
    );
    let texts: Vec<&str> = fork_entries
        .iter()
        .filter(|entry| entry["type"] == "message")
        .filter_map(|entry| entry["message"]["content"][0]["text"].as_str())
        .collect();
    assert!(texts.contains(&"original question"), "texts: {texts:?}");
    assert!(texts.contains(&"the answer"), "texts: {texts:?}");

    let after = std::fs::read(&source).expect("read source");
    assert_eq!(before, after, "the source file is unchanged");
}

#[test]
fn fork_startup_selection_imports_a_global_session_into_this_cwd() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let other_project = dir.path().join("other-project");
    let this_project = dir.path().join("this-project");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&other_project).expect("other project");
    std::fs::create_dir_all(&this_project).expect("this project");
    std::fs::create_dir_all(&session_dir).expect("sessions dir");
    let (source, id) = seed_session(&session_dir, &other_project, "global question");
    let before = std::fs::read(&source).expect("read source");

    let selection = fork_startup_selection(&id, &this_project, Some(&session_dir))
        .expect("a GLOBAL session is exactly what --fork is for");
    let SessionSelection::Resume(fork) = &selection else {
        panic!("the fork opens as a resume of the forked file, got {selection:?}");
    };

    let fork_entries = read_jsonl(fork);
    assert_eq!(
        fork_entries[0]["cwd"].as_str(),
        Some(this_project.display().to_string().as_str()),
        "the fork adopts the TARGET cwd"
    );
    assert_eq!(
        fork_entries[0]["parentSession"].as_str(),
        Some(source.display().to_string().as_str())
    );
    let after = std::fs::read(&source).expect("read source");
    assert_eq!(before, after, "the source file is unchanged");
}

#[test]
fn fork_startup_selection_reports_the_ts_contracts() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let cwd = dir.path().join("project");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("sessions dir");

    // An empty source file: the `forkFrom` failure contract.
    let empty = session_dir.join("empty.jsonl");
    std::fs::write(&empty, "").expect("write empty file");
    let error =
        fork_startup_selection(empty.to_str().expect("utf8 path"), &cwd, Some(&session_dir))
            .expect_err("an empty source cannot fork");
    assert!(
        error.to_string().contains(&format!(
            "Cannot fork: source session file is empty or invalid: {}",
            empty.display()
        )),
        "unexpected error: {error:#}"
    );

    // A source file with only an unreadable row: the loader finalizes
    // it to zero entries (the `forkFrom` contract).
    let torn = session_dir.join("torn.jsonl");
    std::fs::write(&torn, "{\"type\":\"message\"}\n").expect("write torn file");
    let error = fork_startup_selection(torn.to_str().expect("utf8 path"), &cwd, Some(&session_dir))
        .expect_err("a source with no readable rows cannot fork");
    assert!(
        error
            .to_string()
            .contains("Cannot fork: source session file is empty or invalid: "),
        "unexpected error: {error:#}"
    );

    // A parseable but headerless source file: the loader finalizes it to zero
    // entries — never a half-copied fork.
    let headerless = session_dir.join("headerless.jsonl");
    std::fs::write(
        &headerless,
        "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[],\"timestamp\":0},\"id\":\"aaaa1\",\"parentId\":null}\n",
    )
    .expect("write headerless file");
    let error = fork_startup_selection(
        headerless.to_str().expect("utf8 path"),
        &cwd,
        Some(&session_dir),
    )
    .expect_err("a headerless source cannot fork");
    assert!(
        error
            .to_string()
            .contains("Cannot fork: source session file is empty or invalid: "),
        "unexpected error: {error:#}"
    );

    let error = fork_startup_selection("does-not-exist", &cwd, Some(&session_dir))
        .expect_err("an unknown selector cannot fork");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("No session found matching 'does-not-exist'"),
        "unexpected error: {rendered}"
    );
    assert!(
        rendered.contains("Open prime-agent and press left-arrow to browse sessions."),
        "unexpected error: {rendered}"
    );
}

// The tilde selector expands a unix HOME path.
#[cfg(unix)]
#[test]
fn fork_startup_selection_expands_a_tilde_selector() {
    // The resume selector's convention: a leading `~` resolves against the home
    // dir, so forking a home-located session by that path opens it.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&session_dir).expect("sessions dir");
    let (source, id) = seed_session(&session_dir, &project, "tilde question");
    let home_sessions = home.join("sessions");
    std::fs::create_dir_all(&home_sessions).expect("home sessions dir");
    let home_file = home_sessions.join(format!("{id}.jsonl"));
    std::fs::rename(&source, &home_file).expect("move the source under home");
    let _env = crate::config::env_lock();
    let previous_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", &home);
    let selector = format!("~/sessions/{id}.jsonl");
    let selection = fork_startup_selection(&selector, &project, Some(&session_dir));
    match previous_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    let selection = selection.expect("the tilde selector forks");
    let SessionSelection::Resume(fork) = &selection else {
        panic!("the fork opens as a resume of the forked file, got {selection:?}");
    };
    assert_ne!(fork, &home_file, "the fork is a new session file");
    assert!(
        fork.starts_with(&session_dir),
        "the fork lands in the requested session dir"
    );
    let fork_entries = read_jsonl(fork);
    assert_eq!(
        fork_entries[0]["parentSession"].as_str(),
        Some(home_file.display().to_string().as_str()),
        "the fork header parents at the tilde-resolved source"
    );
}

#[cfg(unix)]
#[test]
fn fork_startup_selection_rejects_a_fifo_source_without_hanging() {
    // A FIFO with no writer blocks the copy's read forever; the guard
    // rejects it before any open.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let project = dir.path().join("project");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&session_dir).expect("sessions dir");
    let fifo = session_dir.join("pipe.jsonl");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).expect("mkfifo");
    let error = fork_startup_selection(
        fifo.to_str().expect("utf8 path"),
        &project,
        Some(&session_dir),
    )
    .expect_err("a FIFO source cannot fork");
    assert!(
        error.to_string().contains(&format!(
            "Cannot fork: source session file is not a regular file: {}",
            fifo.display()
        )),
        "unexpected error: {error:#}"
    );
}

#[test]
fn build_tui_options_opens_a_fork_as_the_startup_session() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let cwd = dir.path().join("project");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("sessions dir");
    let (source, id) = seed_session(&session_dir, &cwd, "interactive question");

    let mut options = run_options_for_continue(dir.path());
    options.config.cwd = cwd;
    options.config.agent_dir = dir.path().join("agent");
    options.session.fork = Some(id);
    options.session.session_dir = Some(session_dir.clone());
    let tui = build_tui_options(
        &options,
        dir.path().join("d.sock"),
        std::sync::Arc::default(),
    )
    .expect("the interactive launch forks instead of refusing");

    let SessionSelection::Resume(fork) = &tui.session else {
        panic!(
            "the fork opens as a resume of the forked file, got {:?}",
            tui.session
        );
    };
    assert!(
        fork.starts_with(&session_dir),
        "the fork honors --session-dir"
    );
    assert_ne!(fork, &source, "the fork is a new session file");
    let fork_entries = read_jsonl(fork);
    assert_eq!(
        fork_entries[0]["parentSession"].as_str(),
        Some(source.display().to_string().as_str())
    );
}

#[test]
fn a_fork_launch_never_opens_the_agents_view() {
    // `--fork` opens its target directly — even alongside an explicit
    // `agents` request.
    let mut options = run_options_for_continue(std::path::Path::new("/does/not/matter"));
    options.agents_view_requested = true;
    assert!(
        should_open_agents_view(&options, /*onboarding_pending*/ false, /*continue_view*/ false,),
        "an explicit agents request still opens the view"
    );
    options.session.resume_bare = true;
    assert!(
        should_open_agents_view(&options, /*onboarding_pending*/ false, /*continue_view*/ false,),
        "a bare --resume still opens the view"
    );
    options.session.resume_bare = false;
    options.session.fork = Some("source".to_string());
    assert!(
        !should_open_agents_view(&options, /*onboarding_pending*/ false, /*continue_view*/ false,),
        "a fork opens its target, never the agents view"
    );
    assert!(
        should_open_agents_view(&options, /*onboarding_pending*/ false, /*continue_view*/ true,),
        "a --continue with a saved candidate still opens the view"
    );
}

#[test]
fn interactive_resume_restores_saved_cwd_and_project_settings_unless_overridden() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("saved-project");
    std::fs::create_dir_all(project.join(".prime/agent")).unwrap();
    std::fs::write(
        project.join(".prime/agent/settings.json"),
        r#"{"markdown":{"codeBlockIndent":"    "}}"#,
    )
    .unwrap();
    let mut options = run_options_for_continue(dir.path());
    let saved = seed_saved_session(
        &options.config.agent_dir.join("sessions"),
        "cwdresume",
        &project,
    );
    for explicit in [true, false] {
        options.session.resume = Some(saved.to_string_lossy().into_owned());
        options.session.cwd_from_flag = explicit;
        let tui = build_tui_options(
            &options,
            dir.path().join("d.sock"),
            std::sync::Arc::default(),
        )
        .unwrap();
        let expected = if explicit {
            dir.path()
        } else {
            project.as_path()
        };
        assert_eq!(
            (&tui.cwd, tui.create_config()["cwd"].clone(), tui.session),
            (
                &expected.to_path_buf(),
                serde_json::json!(expected),
                SessionSelection::Resume(saved.clone())
            )
        );
        assert_eq!(tui.code_block_indent, if explicit { "  " } else { "    " });
    }
}

#[test]
fn onboarding_auth_readiness_resolves_commands_and_explicit_runtime_keys() {
    let dir = tempfile::TempDir::new().unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let cases = [
        ("failed", "!exit 1", false),
        ("failed-output", "!printf 'not-a-usable-key'; exit 1", false),
        ("empty", "!printf ''", false),
        ("whitespace", "!printf '  '", false),
        ("command", "!printf 'local-onboarding-key'", true),
        ("literal", "local-literal-key", true),
    ];
    let mut providers = serde_json::Map::new();
    for (name, key, _) in cases {
        providers.insert(format!("onboarding-{name}"),serde_json::json!({"baseUrl":"https://onboarding.invalid","api":"openai-completions","apiKey":key,"models":[{"id":"m","name":"M"}]}));
    }
    providers.insert("onboarding-header".into(),serde_json::json!({"baseUrl":"https://onboarding.invalid","api":"openai-completions","apiKey":"!exit 1","headers":{"Authorization":"Bearer local-header-key"},"models":[{"id":"m","name":"M"}]}));
    providers.insert("onboarding-empty-headers".into(),serde_json::json!({"baseUrl":"https://onboarding.invalid","api":"openai-completions","apiKey":"!exit 1","headers":{},"models":[{"id":"m","name":"M"}]}));
    providers.insert("onboarding-blank-header".into(),serde_json::json!({"baseUrl":"https://onboarding.invalid","api":"openai-completions","apiKey":"!exit 1","headers":{"Authorization":"  "},"models":[{"id":"m","name":"M"}]}));

    std::fs::write(
        agent.join("models.json"),
        serde_json::json!({"providers":providers}).to_string(),
    )
    .unwrap();
    for (name, _, expected) in cases {
        let mut options = run_options_for_continue(dir.path());
        options.config.provider = Some(format!("onboarding-{name}"));
        options.config.model = Some("m".into());
        let task = onboarding_task(&options, None).unwrap();
        assert!(
            task.current_model
                .as_ref()
                .is_some_and(
                    |model| model.provider == format!("onboarding-{name}") && model.id == "m"
                )
        );
        assert_eq!((task.model_ready)(), expected, "credential case {name}");
    }
    let mut options = run_options_for_continue(dir.path());
    options.config.provider = Some("onboarding-header".into());
    options.config.model = Some("m".into());
    assert!(
        (onboarding_task(&options, None).unwrap().model_ready)(),
        "resolved header authentication remains usable"
    );
    for provider in ["onboarding-empty-headers", "onboarding-blank-header"] {
        options.config.provider = Some(provider.into());
        assert!(
            !(onboarding_task(&options, None).unwrap().model_ready)(),
            "empty headers do not authorize a failed command key"
        );
    }
    options.config.provider = Some("onboarding-failed".into());
    for key in ["", "  ", "explicit-runtime-key"] {
        options.config.api_key = Some(key.into());
        assert_eq!(
            (onboarding_task(&options, None).unwrap().model_ready)(),
            !key.trim().is_empty(),
            "runtime key readiness must match a nonempty explicit credential"
        );
    }
}

#[test]
fn onboarding_readiness_rechecks_changed_credentials_in_the_live_task() {
    let dir = tempfile::TempDir::new().unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let write_key = |key: &str| {
        std::fs::write(agent.join("models.json"),serde_json::json!({"providers":{"onboarding-live":{"baseUrl":"https://onboarding.invalid","api":"openai-completions","apiKey":key,"models":[{"id":"m","name":"M"}]}}}).to_string()).unwrap();
    };
    write_key("!exit 1");
    let mut options = run_options_for_continue(dir.path());
    options.config.provider = Some("onboarding-live".into());
    options.config.model = Some("m".into());
    let task = onboarding_task(&options, None).unwrap();
    assert!(!(task.model_ready)());
    write_key("local-new-credential");
    assert!((task.model_ready)());
    write_key("!printf ''");
    assert!(!(task.model_ready)());
}

#[test]
fn onboarding_transport_headers_without_a_credential_do_not_authorize() {
    let dir = tempfile::TempDir::new().unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("models.json"),serde_json::json!({"providers":{"onboarding-transport-only":{"baseUrl":"https://onboarding.invalid","api":"openai-completions","apiKey":"","models":[{"id":"m","name":"M","headers":{"Content-Type":"application/json"}}]}}}).to_string()).unwrap();
    let mut options = run_options_for_continue(dir.path());
    options.config.provider = Some("onboarding-transport-only".into());
    options.config.model = Some("m".into());
    let task = onboarding_task(&options, None).unwrap();
    assert!(task
        .current_model
        .as_ref()
        .is_some_and(|model| model.id == "m"));
    assert!(
        !(task.model_ready)(),
        "noncredential transport headers cannot make an empty configured key usable"
    );
}
