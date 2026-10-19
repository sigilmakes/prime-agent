// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines/casts).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! Verifier integration tests for the session-creation kernel prewarm: a main session with the
//! config flag boots its kernel at creation (so a compaction with no `ipython` use still lands the
//! `ipython_state` notice); a depth-1 subagent session keeps the lazy first-call start.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use pa_core::session_engine::compact_session::CompactOutcome;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_core::session_engine::provider_adapter::{json_round_trip, real_stream_fn};
use pa_core::session_engine::{PromptOptions, PromptOutcome};
use pa_types::session::FileEntry;

/// The faux provider registry is process-global and both tests drive it:
/// the std lock serializes them (the only contenders).
static FAUX_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The kernel Python with prime-agent-runtime installed (the interpreter the session-path
/// provisioner resolves). Skipped (with a note) on machines without a live install; set
/// `PA_CORE_KERNEL_PYTHON` to point at an explicit interpreter instead.
fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        let explicit = PathBuf::from(explicit);
        assert!(
            explicit.exists(),
            "PA_CORE_KERNEL_PYTHON {} not found",
            explicit.display()
        );
        return Some(explicit);
    }
    let candidate = PathBuf::from(std::env::var("HOME").map_or_else(
        |_| "/home/ubuntu/.prime/agent/kernel-venv/bin/python".to_string(),
        |home| format!("{home}/.prime/agent/kernel-venv/bin/python"),
    ));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live prewarm test",
        candidate.display()
    );
    None
}

/// One faux provider session: the model, its agent-loop shape, and the
/// stream function, with the scripted responses queued.
struct FauxSession {
    model: pa_types::ai::Model,
    stream_fn: pa_agent::stream::StreamFn,
    registration: pa_ai::faux::FauxProviderRegistration,
}

fn faux_session(responses: Vec<String>) -> FauxSession {
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "faux-1".to_string(),
                name: Some("Faux".to_string()),
                reasoning: Some(false),
                input: Some(vec![pa_types::ai::ModelInput::Text]),
                cost: None,
                context_window: Some(100_000),
                max_tokens: Some(4_096),
            }]),
            ..Default::default()
        });
    registration.set_responses(
        responses
            .into_iter()
            .map(|text| {
                pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
                    &text,
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ))
            })
            .collect(),
    );
    let model = registration.get_model();
    let stream_fn = real_stream_fn(None, model.clone());
    FauxSession {
        model,
        stream_fn,
        registration,
    }
}

/// The agent-loop model shape the engine config takes.
fn agent_model(model: &pa_types::ai::Model) -> pa_agent::types::Model {
    json_round_trip(model).expect("model conversion")
}

/// Wait for the prewarmed boot to finish (a background task): poll the
/// session's kernel provisioner until a kernel runs.
async fn wait_for_kernel_boot(engine: &pa_core::session_engine::engine::SessionEngine) {
    let deadline = Instant::now() + Duration::from_mins(2);
    loop {
        if engine
            .kernel_provisioner_weak()
            .upgrade()
            .is_some_and(|provisioner| provisioner.has_running_kernel())
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the prewarmed kernel never booted"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The prewarm fires at creation (no `ipython` tool use anywhere): the
/// background boot completes, the session's compaction sees
/// a running kernel, and the hidden `ipython_state` notice lands on the
/// durable entries — with the empty-namespace arm, because the model never
/// ran a cell.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn prewarmed_kernel_lands_compaction_notice_without_tool_use() {
    let Some(_kernel_python) = kernel_python() else {
        return;
    };
    let _guard = FAUX_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::json!({
            "compaction": { "enabled": true, "reserveTokens": 1000, "keepRecentTokens": 10 }
        })
        .to_string(),
    )
    .expect("settings");

    // Two plain text turns (a single turn is too short for a cut). The tiny
    // keep-recent budget splits the second turn, so compaction needs separate
    // history and turn-prefix replies. No `ipython` tool call anywhere.
    let faux = faux_session(vec![
        "history one noted".to_string(),
        "history two noted".to_string(),
        "the compaction summary".to_string(),
        "the retained turn prefix".to_string(),
    ]);
    let engine = create_session(SessionEngineConfig {
        cron_store: None,
        steering_mode: None,
        follow_up_mode: None,
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        model: Some(agent_model(&faux.model)),
        stream_fn: Some(faux.stream_fn),
        tools: Vec::new(),
        prewarm_ipython_kernel: Some(true),
        ..Default::default()
    })
    .await
    .expect("create the prewarmed session");

    // The prewarm's boot, without a single ipython tool call.
    wait_for_kernel_boot(&engine).await;

    for turn in ["history turn one", "history turn two"] {
        let outcome = tokio::time::timeout(
            Duration::from_secs(15),
            engine.prompt(turn, PromptOptions::default()),
        )
        .await
        .expect("the scripted history prompt must settle")
        .expect("prompt");
        assert_eq!(outcome, PromptOutcome::Prompt);
        engine.session.agent().wait_for_idle().await;
    }

    let compacted = tokio::time::timeout(
        Duration::from_secs(15),
        engine.session.compact(None, &faux.model, None, None),
    )
    .await
    .expect("compaction must settle within its finite response budget")
    .expect("compact");
    let run = match compacted {
        CompactOutcome::Ran(run) => run,
        CompactOutcome::Skipped(reason) => panic!("compaction must run, skipped: {reason}"),
    };
    assert_eq!(
        faux.registration.call_count(),
        4,
        "two turns and two summaries"
    );
    assert!(run.result.summary.contains("the compaction summary"));
    assert!(run.result.summary.contains("the retained turn prefix"));
    let notice = run
        .ipython_state
        .expect("the prewarmed kernel must land the ipython_state notice");
    assert_eq!(notice.custom_type, "ipython_state");
    assert!(!notice.display, "the notice is never rendered");
    let pa_types::ai::UserContent::Text(content) = &notice.content else {
        panic!("the notice content is text");
    };
    assert!(content.contains("[python-state]"), "{content}");
    assert!(
        content.contains("Your Python kernel persisted through compaction"),
        "{content}"
    );
    // The live-names detail arm is environment-dependent (the bootstrap pre-imports the installed
    // Python skills as live names), so only the persistence sentence is pinned here — same scoping
    // as the battery's kernel-notice differential.
    let entries = engine.session.entries().await;
    assert!(
        entries.iter().any(|entry| match entry {
            FileEntry::CustomMessage { payload, .. } => {
                payload.custom_type == "ipython_state"
            }
            _ => false,
        }),
        "the notice must be durable"
    );
}

/// The TS depth gate: subagent sessions (rlmDepth > 0) keep the lazy
/// first-call start even when the runtime factory passes
/// `prewarmIpythonKernel: true` — no boot.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn subagent_sessions_stay_lazy_despite_the_prewarm_flag() {
    let Some(_kernel_python) = kernel_python() else {
        return;
    };
    let _guard = FAUX_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("cwd");

    let faux = faux_session(vec!["ok".to_string()]);
    let engine = create_session(SessionEngineConfig {
        cron_store: None,
        steering_mode: None,
        follow_up_mode: None,
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        model: Some(agent_model(&faux.model)),
        stream_fn: Some(faux.stream_fn),
        tools: Vec::new(),
        rlm_depth: Some(1),
        prewarm_ipython_kernel: Some(true),
        ..Default::default()
    })
    .await
    .expect("create the subagent session");

    // The depth-1 session still carries the kernel-backed `ipython` tool
    // (the lazy first-call start, just not prewarmed).
    let tool_names: Vec<String> = engine
        .session
        .agent()
        .state()
        .await
        .tools
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    assert!(
        tool_names.iter().any(|name| name == "python_repl"),
        "the subagent keeps the lazy python_repl tool: {tool_names:?}"
    );

    // Long enough for a wrongly-fired prewarm to boot and report; the lazy session reports nothing.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        assert!(
            !engine
                .kernel_provisioner_weak()
                .upgrade()
                .is_some_and(|provisioner| provisioner.has_running_kernel()),
            "a depth-1 session must not prewarm"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
