//! `SessionEngine` assembly: build a running agent session from a config.
//! The session subscribes persistence listeners on the caller's reactor, so
//! `create_session` is async.

use std::path::PathBuf;
use std::sync::Arc;

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::stream::StreamFn;
use pa_agent::types::{Model, ThinkingLevel};
use pa_types::ai::PYTHON_TOOL_NAME;

use crate::resources::{load_resources, ResourceLoaderOptions};
use crate::session::manager::SessionManager;
use crate::skills::PromptTemplate;

use super::{AgentSession, PromptOptions, PromptOutcome};

/// The background MCP settle's per-server open bound (the kernel's
/// `mcp_status` listing opens each not-yet-connected server bounded per
/// server; the settle is off every user-visible path, so the bound only
/// keeps the background task from outliving a wedged server forever).
const MCP_SETTLE_PER_SERVER_TIMEOUT_MS: u64 = 10_000;

/// Acknowledges the already validated and pinned context strategy through an
/// external canonical writer. The callback cannot replace the selected route.
/// It runs without the SDK session mutex, before initial strategy work, for
/// both fresh choices and saved-choice revalidation.
pub type InitialContextSelectionCommit = Arc<
    dyn Fn(
            pa_types::session::ContextSelection,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'static>,
        > + Send
        + Sync,
>;

/// Everything needed to assemble a session.
#[derive(Default)]
pub struct SessionEngineConfig {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub stream_fn: Option<StreamFn>,
    /// Shared session-local uncapped baselines for host-owned model targets.
    pub ollama_cloud: Option<Arc<super::ollama_cloud::SessionOllamaCloud>>,
    pub tools: Vec<Arc<dyn pa_agent::types::AgentTool>>,
    pub custom_system_prompt: Option<String>,
    /// Explicit append-system-prompt sources from the host; overrides file discovery.
    pub prompt_guidelines: Vec<String>,
    pub generic_mcp_servers: Vec<String>,
    pub allow_recursion: Option<bool>,
    /// In-memory when None.
    pub session_manager: Option<SessionManager>,
    /// Actual external file/lease owner; None preserves standalone persistence.
    pub canonical_live_owner: Option<Arc<dyn super::canonical_live::CanonicalLiveOwner>>,
    /// Restrict initial strategy work to the verified pre-admission prefix.
    /// The complete canonical manager and durable strategy selection stay intact.
    pub context_startup_boundary: super::ContextStartupBoundary,
    /// Exact ordered canonical user-row IDs after a non-Current boundary.
    /// These prove admission identity; content and timestamps are not deduplicated.
    pub context_startup_admitted_user_ids: Vec<String>,
    /// Acknowledge fresh or saved selection through the embedding's canonical
    /// writer before initial strategy work. When set, the SDK does not mint a
    /// provisional selection row; the host must adopt its verified real row.
    pub on_initial_context_selection: Option<InitialContextSelectionCommit>,
    /// Merged over the built-in goal/heartbeat registrations.
    pub extra_host_handlers: Option<crate::kernel::shared::HostRequestHandlers>,
    /// Conversation-log path for the system prompt when the caller owns
    /// persistence outside the session manager.
    pub conversation_log_path: Option<PathBuf>,
    pub additional_skill_paths: Vec<String>,
    pub additional_prompt_paths: Vec<String>,
    pub extra_builtin_skill_overrides: Vec<String>,
    pub rlm_subagent_host: Option<Arc<dyn super::rlm_host::RlmSubagentHost>>,
    pub rlm_usage_store: Option<Arc<dyn super::rlm_usage::RlmChildUsageStore>>,
    /// Announces every accepted `rlm.progress.note`; `None` keeps the store
    /// the only surface.
    pub progress_note_emit: Option<super::rlm_host::RlmProgressNoteEmit>,
    /// The session's depth in the RLM recursion tree (0 for top-level
    /// sessions); gates the `refine.*` host requests.
    pub rlm_depth: Option<u32>,
    /// Whether the session runs without a daemon: nothing outside the
    /// session itself (no scheduler, no bash-completion wake) starts a
    /// turn. Set explicitly by the embedding; print sessions are
    /// daemonless with or without an inline-subagent host, daemon
    /// workers are not.
    pub daemonless: bool,
    /// The full registry model (input modalities for `model.info`); the
    /// engine derives minimal facts from `model` when absent.
    pub model_info: Option<pa_types::ai::Model>,
    /// Invoked at the goal pause/clear/start command sites and after a
    /// kernel `goal.complete` settles the goal.
    pub queued_goal_context_purge: Option<super::runtime::QueuedGoalContextPurge>,
    /// `true` while a steering-lane action is queued or mid-selection: the
    /// running turn stops at the next boundary and the queued steer delivers.
    pub queued_steering_probe: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Embedding-owned turn boundary, consulted after queued steering.
    /// Returning true yields the run; the embedding owns any continuation.
    pub should_stop_after_turn: Option<pa_agent::agent_loop::ShouldStopAfterTurnFn>,
    /// `None` keeps the TS default ("one-at-a-time").
    pub steering_mode: Option<pa_agent::agent::QueueMode>,
    pub follow_up_mode: Option<pa_agent::agent::QueueMode>,
    /// Boot the kernel in the background at creation, depth-0 gated; boot
    /// failures surface on the next `ensure()`.
    pub prewarm_ipython_kernel: Option<bool>,
    /// Fires when the kernel's last live background `bash()` handle
    /// settles; `None` installs nothing.
    pub on_background_work_settled: Option<crate::kernel::shared::BackgroundWorkSettledCallback>,
    /// An externally owned MCP manager: the engine adopts it instead of
    /// building its own, so ACP-admitted servers reach the prompt's MCP gating.
    pub mcp_manager: Option<std::sync::Arc<std::sync::Mutex<crate::mcp::McpManager>>>,
    /// The kernel's `rlm_heartbeat.*` host requests write and read this
    /// embedding-owned store instead of the engine-private `cron-jobs.json`.
    pub cron_store: Option<super::runtime_wiring::KernelCronWiring>,
    /// The image-model routing host seam: the headless surfaces install
    /// theirs; the daemon worker stays `None` (its dispatch owns routing).
    pub image_model_router: Option<super::image_model_routing::ImageModelRouter>,
    /// The session's semantic-edge identity (TS
    /// `semanticEdgeLedgerPath` + `semanticParentSessionId` +
    /// `semanticSpawnedByRequestId`): the recorder's ledger location and
    /// spawn provenance. `None` keeps the session off the ledger (no
    /// request ids on the wire).
    pub semantic_edges: Option<super::semantic_edges::SemanticEdgeIdentity>,
    pub on_late_sent_agent_message: Option<crate::tools::ipython::LateSentAgentMessageHandler>,
}

pub struct SessionEngine {
    pub session: AgentSession,
    pub skills: Vec<crate::skills::Skill>,
    /// Skill-loading diagnostics; the connection resource snapshot
    /// surfaces them.
    pub skill_diagnostics: Vec<crate::skills::ResourceDiagnostic>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub agents_files: Vec<crate::resources::ContextFile>,
    pub system_prompt: String,
    /// The same instance the kernel `goal.*` host handlers reach, so `/goal`
    /// and `goal.complete()` observe one state machine.
    pub goal_driver: std::sync::Arc<tokio::sync::Mutex<super::goal_driver::GoalDriver>>,
    /// `/goal` pause/clear/start and the kernel's `goal.complete` withdraw
    /// queued goal-context turns through it; `None` when no queue.
    pub queued_goal_context_purge: Option<super::runtime::QueuedGoalContextPurge>,
    /// Auth gating and the source the `mcp.*` kernel host handlers resolve
    /// against; `replace_acp_mcp_servers` reaches it through this handle.
    pub mcp_manager: std::sync::Arc<std::sync::Mutex<crate::mcp::McpManager>>,
    pub turn_boundary: std::sync::Arc<super::turn_boundary::TurnBoundaryRequests>,
    /// The kernel `rlm.spawn` handler registers spawn targets into it; the
    /// embedding wires the child-observation sink onto it after the build.
    pub rlm_usage: std::sync::Arc<super::rlm_usage::RlmChildUsageAttributions>,
    /// The progress-note store the `rlm.progress.note` handler throttles
    /// into; the embedding mirrors the Arc for sync reads.
    pub rlm_progress_notes: std::sync::Arc<super::rlm_host::RlmProgressNotes>,
    /// The session's kernel provisioner. The engine is the STRONG owner on
    /// purpose: the `ipython` tool on the agent and the compaction
    /// kernel-state probe on the session hold weak references, because the
    /// kernel's host handlers reach back into the session (goal state,
    /// turn-boundary requests) — a strong edge anywhere on that return path
    /// loops the graph and keeps a dropped session's kernel process alive
    /// until the process exits.
    pub(crate) provisioner: std::sync::Arc<crate::kernel::provisioner::IpythonKernelProvisioner>,
}

/// Skill overrides for built-in integrations the user is not logged into,
/// plus the enabled persistent generic servers.
async fn mcp_gating(
    settings: &crate::settings::SettingsManager,
    agent_dir: std::path::PathBuf,
) -> anyhow::Result<(Vec<String>, Vec<String>, crate::mcp::McpManager)> {
    let user_servers = settings
        .settings()
        .mcp_servers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(server, config)| {
            serde_json::from_value(config)
                .ok()
                .map(|parsed| (server, parsed))
        })
        .collect::<std::collections::HashMap<String, crate::mcp::McpServerConfig>>();
    // The MCP manager snapshots auth with a blocking lock; run it off the
    // async runtime.
    tokio::task::spawn_blocking(move || mcp_gating_blocking(user_servers, &agent_dir))
        .await
        .map_err(|error| anyhow::anyhow!("MCP gating task failed: {error}"))
}

fn mcp_gating_blocking(
    user_servers: std::collections::HashMap<String, crate::mcp::McpServerConfig>,
    agent_dir: &std::path::Path,
) -> (Vec<String>, Vec<String>, crate::mcp::McpManager) {
    crate::mcp::McpManager::prompt_gating(user_servers, agent_dir)
}

/// Assemble a session: load resources, build the system prompt, and start the
/// loop with persistence wiring.
///
/// # Errors
///
/// Returns an error when the MCP gating task fails, when session resources
/// cannot be loaded, or when the runtime bootstrap fails.
///
/// # Panics
///
/// Panics if the shared MCP manager mutex is poisoned.
pub async fn create_session(config: SessionEngineConfig) -> anyhow::Result<SessionEngine> {
    // Keep the startup state machine off every embedding caller's future stack.
    Box::pin(create_session_inner(config)).await
}

async fn create_session_inner(mut config: SessionEngineConfig) -> anyhow::Result<SessionEngine> {
    anyhow::ensure!(
        config.canonical_live_owner.is_none()
            || config.rlm_subagent_host.is_none()
            || config.rlm_usage_store.is_some(),
        "canonical live sessions with subagents require an owner-backed RLM usage store"
    );
    let cwd = config.cwd.clone();
    // Session persistence first: the conversation-log path and the resume
    // context both come from the session manager.
    let session_manager = config
        .session_manager
        .unwrap_or_else(|| SessionManager::in_memory(&cwd));
    // This future captures retained state and hydrates any old selection off lock.
    let stored_context_selection = session_manager.context_selection_snapshot().await?;
    let conversation_log = {
        let session = &session_manager;
        session
            .get_session_file()
            .map(|path| path.display().to_string())
            .or_else(|| {
                config
                    .conversation_log_path
                    .as_ref()
                    .map(|path| path.display().to_string())
            })
    };
    let settings = crate::settings::SettingsManager::create(&cwd, &config.agent_dir);
    // Resolve the kernel shell once, before any side effects: an invalid
    // `shellPath` must fail session creation with a clear config error
    // naming the setting, not every kernel `bash()` call later.
    let kernel_shell_path = crate::platform::shell::resolve_kernel_bash_shell(
        settings.settings().shell_path.as_deref(),
    )?
    .map(std::path::PathBuf::from);
    // TS injects the prefix only when truthy (an empty string stays unset).
    let kernel_command_prefix = settings
        .settings()
        .shell_command_prefix
        .clone()
        .filter(|prefix| !prefix.is_empty());
    let wiring = super::runtime_wiring::wire_session_runtime(
        session_manager,
        &config.agent_dir,
        super::runtime_wiring::RlmWiring {
            model_registry: None,
            subagent_host: config.rlm_subagent_host.clone(),
            usage_store: config.rlm_usage_store.clone(),
            progress_note_emit: config.progress_note_emit.clone(),
        },
        config.queued_goal_context_purge.clone(),
        config.cron_store.clone(),
    );

    let canonical_live = config.canonical_live_owner.take().map(|owner| {
        super::canonical_live::CanonicalLiveSession::new(owner, Arc::clone(&wiring.session))
    });
    if let Some(live) = &canonical_live {
        live.attach_metadata_writer().await;
    }
    let service_tier_preference = settings.get_default_service_tier();
    // Captured before `settings` moves into the resource loader: the
    // compaction budget.
    let compaction_settings = settings.settings().compaction.clone().unwrap_or_default();
    let compaction_threshold_percent = settings.get_compaction_threshold_percent();
    // Request timing: the settings half of the flag is read once here
    // (`settings` moves into the loader); the `PI_REQUEST_TIMING` half stays live.
    let request_timing_settings = settings.get_request_timing();
    // Captured before `settings` moves into the resource loader: the
    // factory host bridge's preflight facts (the daemon `allowedModels`
    // pin), like the request-timing snapshot above.
    let (mcp_skill_overrides, mcp_generic_servers, built_manager) =
        mcp_gating(&settings, config.agent_dir.clone()).await?;
    let mcp_manager = config
        .mcp_manager
        .take()
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(built_manager)));
    let mut extra_builtin_skill_overrides = config.extra_builtin_skill_overrides.clone();
    extra_builtin_skill_overrides.extend(mcp_skill_overrides);
    let mut generic_mcp_servers = config.generic_mcp_servers.clone();
    for server in mcp_generic_servers {
        if !generic_mcp_servers.contains(&server) {
            generic_mcp_servers.push(server);
        }
    }
    let resources = load_resources(ResourceLoaderOptions {
        cwd: cwd.clone(),
        agent_dir: config.agent_dir.clone(),
        settings: Some(settings),
        extra_builtin_skill_overrides,
        additional_skill_paths: config.additional_skill_paths.clone(),
        additional_prompt_paths: config.additional_prompt_paths.clone(),
        no_skills: false,
        no_prompt_templates: false,
        no_context_files: false,
        system_prompt: config.custom_system_prompt.clone(),
        append_system_prompt: config.prompt_guidelines.clone(),
        ..Default::default()
    })?;

    let ollama_cloud = config.ollama_cloud.take().unwrap_or_default();
    let model = config
        .model
        .ok_or_else(|| anyhow::anyhow!("a resolved model is required"))?;
    let model = ollama_cloud.configure_agent_model(&cwd, &config.agent_dir, &model);
    config.model_info = config
        .model_info
        .map(|model| ollama_cloud.configure_model(&cwd, &config.agent_dir, &model));
    let stream_fn = config
        .stream_fn
        .ok_or_else(|| anyhow::anyhow!("a provider stream_fn is required"))?;
    // `model.info` facts, captured before the model moves into the loop.
    let model_info = match config.model_info.clone() {
        Some(full) => super::turn_boundary::ModelInfo {
            id: full.id,
            provider: full.provider,
            input: full.input,
        },
        None => super::turn_boundary::ModelInfo {
            id: model.id.clone(),
            provider: model.provider.clone(),
            input: Vec::new(),
        },
    };
    let model_context_window = model.context_window;

    let python_skills = super::runtime_wiring::kernel_python_skills(&resources.skills);
    let session_id = wiring.session.lock().await.get_session_id().to_string();
    let plugins = Arc::new(super::plugins::SessionPlugins::new(
        session_id.clone(),
        cwd.clone(),
        config.agent_dir.clone(),
        config.rlm_depth.unwrap_or(0),
    ));
    plugins.register_optional_builtin("optchat", include_str!("../plugins/builtins/optchat.lua"));
    let optchat_artifacts = wiring.session.lock().await.get_session_artifact_dir();
    plugins.set_capability_delegate(
        "optchat.",
        Arc::new(super::optchat::OptChatServices::new(
            wiring.session.clone(),
            &plugins.host(),
            optchat_artifacts,
        )),
    );
    plugins.pin_context_strategy(
        stored_context_selection
            .as_ref()
            .map(|selection| selection.strategy.clone()),
    );
    let inference_capabilities = Arc::new(
        super::inference_capabilities::InferenceCapabilities::new(session_id.clone()),
    );
    plugins.set_native_capabilities(inference_capabilities.clone());
    let auxiliary_inference = Arc::new(super::inference_router::AuxiliaryInference::new(
        plugins.host(),
        Arc::clone(&inference_capabilities),
    ));
    let stream_fn =
        super::inference_router::wrap_stream_fn(plugins.host(), inference_capabilities, stream_fn);
    if let Err(error) = plugins.reload().await {
        let activation_settings = crate::settings::SettingsManager::create(&cwd, &config.agent_dir);
        let settings = activation_settings.settings();
        if stored_context_selection.is_none()
            && settings
                .extra
                .get("contextStrategy")
                .is_some_and(|requested| {
                    requested.as_str() != Some(crate::plugins::context::DEFAULT_STRATEGY)
                })
        {
            return Err(error.context("selected context strategy activation failed; re-enable or repair its plugin, select another strategy, or start a new session"));
        }
        tracing::warn!(%error, "plugin activation failed; keeping ordinary session startup");
    }
    let selected = plugins.context_selection()?;
    let selected_route = format!("{}/{}", selected.plugin, selected.name);
    if stored_context_selection
        .as_ref()
        .is_some_and(|selection| selection.strategy != selected_route)
    {
        anyhow::bail!("the session's recorded context strategy is unavailable");
    }
    plugins.pin_context_strategy(Some(selected_route.clone()));
    // Validate and capture the explicit prefix before asking an external owner
    // to commit. No SDK session lock may cross the acknowledged writer await.
    // The default SDK path retains its original later snapshot and append.
    let acknowledged_context = if let Some(commit) = &config.on_initial_context_selection {
        let snapshot = {
            let session = wiring.session.lock().await;
            super::context_strategy::startup::snapshot_at(
                &plugins,
                &session,
                &config.context_startup_boundary,
                &config.context_startup_admitted_user_ids,
            )?
        };
        let captured = snapshot.await?;
        commit(pa_types::session::ContextSelection {
            version: 1,
            strategy: selected_route.clone(),
        })
        .await?;
        if !plugins.context_is_current(&selected) {
            return Err(crate::plugins::PluginError::Retired.into());
        }
        Some(captured)
    } else {
        None
    };
    // The initial selection's real row is durable before binding the live owner.
    if let Some(live) = &canonical_live {
        live.observe().await?;
    }
    let mut handlers = wiring.handlers.clone();
    if let Some(extra) = config.extra_host_handlers.clone() {
        handlers.merge(extra);
    }
    // Plugin routes cannot be replaced by an extra handler that bypasses generation fences.
    handlers.merge(plugins.handlers());
    // The registration takes the shared manager: the inventory handlers
    // serve live views per request.
    crate::mcp::McpManager::register_host_handlers(&mcp_manager, &mut handlers);
    let turn_boundary = Arc::new(super::turn_boundary::TurnBoundaryRequests::new());
    turn_boundary.register_model_info_handler(&mut handlers, model_info.clone());
    let keep_recent_tokens = compaction_settings
        .keep_recent_tokens
        .unwrap_or(super::compaction::DEFAULT_KEEP_RECENT_TOKENS);
    if compaction_settings.agent_callable.unwrap_or(true) {
        turn_boundary.register_compact_handlers(&mut handlers, keep_recent_tokens);
    }
    // The daemon worker owns persistence outside the session manager, so
    // its conversation-log path implies the same artifact tree.
    let session_artifact_dir: Option<PathBuf> = wiring
        .session
        .lock()
        .await
        .get_session_artifact_dir()
        .or_else(|| {
            config
                .conversation_log_path
                .as_deref()
                .and_then(super::harness_digest::session_artifact_dir_for_log)
        });
    // A snapshot in the artifact dir revives the namespace on the next boot
    // of the same session; `has_snapshot` drives the resume prewarm below.
    let has_snapshot = session_artifact_dir
        .as_ref()
        .is_some_and(|dir| crate::kernel::state_snapshot::snapshot_path_in(dir).exists());
    // A boot's `onRestore`/`onUnavailableSkills` can settle before the AgentSession
    // exists, so the rows park in a mailbox it adopts once constructed.
    let boot_notice_rows = std::sync::Arc::new(std::sync::Mutex::new(Vec::<
        pa_types::session::CustomMessage,
    >::new()));
    let on_restore = {
        let boot_notice_rows = std::sync::Arc::clone(&boot_notice_rows);
        Some(std::sync::Arc::new(
            move |result: &crate::kernel::state_snapshot::RestoreResult| {
                // Only a genuine revive fires the notice (the provisioner
                // suppresses the callback when no snapshot existed).
                let row = super::state_restore_notice::notice_message(result);
                boot_notice_rows
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(row);
            },
        ) as crate::kernel::provisioner::RestoreCallback)
    };
    let on_unavailable_skills = {
        let boot_notice_rows = std::sync::Arc::clone(&boot_notice_rows);
        Some(std::sync::Arc::new(
            move |errors: &crate::kernel::bootstrap::UnavailablePythonSkills| {
                // The broken skills and their import errors ride the next
                // admitted turn, so the model learns before its first call.
                let row = super::skills_unavailable_notice::notice_message(errors);
                boot_notice_rows
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(row);
            },
        )
            as crate::kernel::provisioner::UnavailableSkillsCallback)
    };
    // TS resolves the serper credential on the host: `!command` key refs
    // cannot run in-kernel (the websearch skill's auth.json fallback reads
    // literals and env-var names, not commands). The credential is read
    // under the auth lock, then resolved off the async runtime (a `!command`
    // key can block).
    let serper_api_key = {
        let credential = {
            let auth = mcp_manager.lock().unwrap().auth_storage_handle();
            let guard = auth.lock().await;
            guard
                .get_all()
                .credential(crate::auth::SERPER_CREDENTIAL_ID)
        };
        let skills = resources.skills.clone();
        tokio::task::spawn_blocking(move || {
            super::runtime_wiring::websearch_api_key(&skills, credential)
        })
        .await
        .map_err(|join| anyhow::anyhow!("websearch key resolve join failed: {join}"))?
    };
    let provisioner = super::runtime_wiring::kernel_provisioner(
        session_id,
        handlers,
        python_skills,
        cwd.clone(),
        &config.agent_dir,
        kernel_shell_path,
        kernel_command_prefix,
        serper_api_key,
        session_artifact_dir.clone(),
        on_restore,
        config.on_background_work_settled.clone(),
        on_unavailable_skills,
    );
    let mut tools = config.tools.clone();
    if !tools.iter().any(|tool| tool.name() == PYTHON_TOOL_NAME) {
        let definition = crate::tools::ipython::create_ipython_tool_definition(
            &cwd.to_string_lossy(),
            super::runtime_wiring::ipython_tool_options(
                provisioner.clone(),
                config.on_late_sent_agent_message.clone(),
            ),
        );
        tools.push(Arc::new(
            crate::session_engine::tool_bridge::ToolDefinitionBridge::new(definition),
        ));
    }
    let active_tool_names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();

    // The snapshot arm is not depth-gated: a resumed session prewarms even
    // when the config flag is off, so its namespace revives before the first turn.
    let prewarm_configured =
        config.prewarm_ipython_kernel.unwrap_or(false) && config.rlm_depth.unwrap_or(0) == 0;
    if (prewarm_configured || has_snapshot)
        && active_tool_names
            .iter()
            .any(|name| name == PYTHON_TOOL_NAME)
    {
        provisioner.prewarm();
        // The MCP settle rides the same background posture (the
        // parallel-startup rule: nothing user-visible waits on the MCP
        // spawn/settle; the join point is first use): once the kernel is
        // up, the configured generic servers open in the background via
        // the bounded `mcp_status` listing the connections view uses, so
        // tool discovery is warm by the first turn. A tool call or
        // listing that arrives first is never raced to an error — the
        // runtime's registry serializes per-server opens on its lock and
        // then reuses the open connection, so the earliest user of a
        // server joins the settle's in-flight open and succeeds.
        let settle_servers = generic_mcp_servers.clone();
        let settle_provisioner = provisioner.clone();
        tokio::spawn(async move {
            if settle_provisioner.ensure(None, None).await.is_ok() {
                if let Some(manager) = settle_provisioner.manager() {
                    let _ = manager
                        .mcp_tool_listing(&settle_servers, MCP_SETTLE_PER_SERVER_TIMEOUT_MS)
                        .await;
                }
            }
        });
    }

    let prompt_model_selector = format!("{}/{}", model_info.provider, model_info.id);
    let prompt_vision_capable = Some(model_info.input.contains(&pa_types::ai::ModelInput::Image));
    let model_prompts = crate::prompts::model_prompts::load_model_prompts(
        Some(&prompt_model_selector),
        &config.agent_dir,
    );
    if !model_prompts.errors.is_empty() {
        boot_notice_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(crate::prompts::model_prompts::model_prompt_error_message(
                &model_prompts.errors,
            ));
    }
    let system_prompt = crate::prompts::system_prompt::build_system_prompt(
        &crate::prompts::system_prompt::BuildSystemPromptOptions {
            custom_prompt: resources.system_prompt.clone(),
            model_prompt_extras: model_prompts.extras.as_deref(),
            vision_capable: prompt_vision_capable,
            cwd: cwd.display().to_string(),
            messages_path: conversation_log.clone(),
            context_files: resources
                .agents_files
                .iter()
                .map(|file| (file.path.display().to_string(), file.content.clone()))
                .collect(),
            skills: resources.skills.clone(),
            selected_tools: Some(
                active_tool_names
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            ),
            allow_recursion: config.allow_recursion,
            // A spawned child's prompt must read "depth: N (not root)" with
            // the child-agent reply doctrine, never the root identity.
            rlm_depth: config.rlm_depth,
            daemonless: config.daemonless,
            can_spawn: config.rlm_subagent_host.is_some(),
            generic_mcp_servers,
            append_system_prompt: Some(resources.append_system_prompt.join("\n\n")),
            ..Default::default()
        },
    );

    let (context_snapshot, has_thinking_entry, has_service_tier_entry) = {
        let session = wiring.session.lock().await;
        (
            if acknowledged_context.is_none() {
                Some(super::context_strategy::startup::snapshot_at(
                    &plugins,
                    &session,
                    &config.context_startup_boundary,
                    &config.context_startup_admitted_user_ids,
                )?)
            } else {
                None
            },
            session.has_thinking_level(),
            session.has_service_tier(),
        )
    };
    let (context, startup_leaf) = match (acknowledged_context, context_snapshot) {
        (Some(captured), None) => captured,
        (None, Some(snapshot)) => snapshot.await?,
        _ => unreachable!("one initial context snapshot"),
    };
    let context_archive = context.archive;
    let existing_messages = super::refinement_gate::without_refinements(context.messages);
    let full_model: Option<pa_types::ai::Model> = config
        .model_info
        .clone()
        .or_else(|| super::provider_adapter::native_model(&model).ok());
    let context_archive_state = Arc::new(std::sync::RwLock::new(context_archive.clone()));
    if let Some(live) = &canonical_live {
        live.bind_archive(Arc::clone(&context_archive_state))?;
    }
    let thinking_level = config.thinking_level.unwrap_or(ThinkingLevel::Off);
    if let Some(live) = &canonical_live {
        use crate::session::canonical::CanonicalMutationKind::OwnerMetadata;
        use pa_types::session::{
            FileEntry, ModelChangeEntry, ServiceTierChangeEntry, ThinkingLevelChangeEntry,
        };
        if existing_messages.is_empty() {
            live.append(
                OwnerMetadata,
                FileEntry::ModelChange {
                    base: super::canonical_live::draft_base(),
                    payload: ModelChangeEntry {
                        provider: model.provider.clone(),
                        model_id: model.id.clone(),
                    },
                },
            )
            .await?;
        }
        if existing_messages.is_empty() || !has_thinking_entry {
            live.append(
                OwnerMetadata,
                FileEntry::ThinkingLevelChange {
                    base: super::canonical_live::draft_base(),
                    payload: ThinkingLevelChangeEntry {
                        thinking_level: format!("{thinking_level:?}").to_lowercase(),
                    },
                },
            )
            .await?;
        }
        if existing_messages.is_empty() || !has_service_tier_entry {
            live.append(
                OwnerMetadata,
                FileEntry::ServiceTierChange {
                    base: super::canonical_live::draft_base(),
                    payload: ServiceTierChangeEntry {
                        service_tier: Some(service_tier_preference),
                    },
                },
            )
            .await?;
        }
    } else {
        let mut session = wiring.session.lock().await;
        if existing_messages.is_empty() {
            session.append_model_change(&model.provider, &model.id)?;
            session.append_thinking_level_change(&format!("{thinking_level:?}").to_lowercase())?;
        } else if !has_thinking_entry {
            session.append_thinking_level_change(&format!("{thinking_level:?}").to_lowercase())?;
        }
        if existing_messages.is_empty() || !has_service_tier_entry {
            session.append_service_tier_change(Some(service_tier_preference))?;
        }
    }
    // Session entries cross through the shared wire shape (same conversion
    // the compaction rebuild uses).
    let initial_messages = if existing_messages.is_empty() {
        None
    } else {
        Some(
            existing_messages
                .iter()
                .filter_map(super::provider_adapter::json_round_trip)
                .collect(),
        )
    };

    // One wiring per session owns the flag probe, the JSONL log, and the prompt-build
    // correlation state; the wrappers pass straight through while the flag is off.
    let request_timing_wiring = std::sync::Arc::new(
        super::request_timing::RequestTimingWiring::new(
            std::sync::Arc::new(move || {
                super::request_timing::is_request_timing_enabled(request_timing_settings)
            }),
            super::request_timing::RequestTimingLog::new(&config.agent_dir),
        )
        .with_payload_capture(super::request_timing::RequestPayloadCapture::new(
            &config.agent_dir,
        )),
    );
    // Semantic edges (TS `semantic-edges.ts`): the recorder opens this
    // session's request-id ledger, and its stream wrapper goes OUTERMOST
    // over the timing-instrumented fn (TS `sdk.ts` instruments first, the
    // `AgentSession` constructor wraps semantic edges over it). The side
    // question keeps the pre-semantic fn, so its calls carry no id.
    let timing_stream_fn = super::request_timing::instrument_stream_fn(
        std::sync::Arc::clone(&request_timing_wiring),
        super::ollama_cloud::wrap_stream_fn(
            cwd.clone(),
            config.agent_dir.clone(),
            Arc::clone(&ollama_cloud),
            super::plugin_context::wrap_stream_fn(
                Arc::clone(&plugins),
                super::plugins::wrap_stream_fn(
                    Arc::clone(&plugins),
                    super::context_strategy::wrap_stream_fn(
                        Arc::clone(&plugins),
                        Arc::clone(&context_archive_state),
                        stream_fn,
                    ),
                ),
            ),
        ),
    );
    let side_question_stream_fn = std::sync::Arc::clone(&timing_stream_fn);
    let semantic_recorder = config.semantic_edges.take().map(|identity| {
        std::sync::Arc::new(super::semantic_edges::SemanticEdgeRecorder::open(identity))
    });
    let agent_stream_fn = match &semantic_recorder {
        Some(recorder) => {
            super::semantic_edges::wrap_stream_fn(std::sync::Arc::clone(recorder), timing_stream_fn)
        }
        None => timing_stream_fn,
    };
    let agent_stream_fn = canonical_live.as_ref().map_or_else(
        || Arc::clone(&agent_stream_fn),
        |live| {
            super::canonical_live::wrap_stream_fn(Arc::clone(live), Arc::clone(&agent_stream_fn))
        },
    );
    let agent = Agent::new(AgentOptions {
        before_event: canonical_live.as_ref().map(|live| {
            super::canonical_live::before_event(Arc::clone(live), Arc::clone(&wiring.session))
        }),
        initial_state: AgentInitialState {
            system_prompt: Some(system_prompt.clone()),
            model: Some(model),
            thinking_level: Some(thinking_level),
            tools: Some(tools),
            messages: initial_messages,
        },
        stream_fn: Some(agent_stream_fn),
        // The session conversion rules apply at the loop's LLM boundary
        // (TS `convertToLlm`): bookkeeping and legacy harness rows drop.
        // Active plugin context arrives through explicit contributions.
        convert_to_llm: Some(super::request_timing::instrument_convert_to_llm(
            std::sync::Arc::clone(&request_timing_wiring),
            super::refinement_gate::convert_to_llm(),
        )),
        // The Rust engine wires no transform, so the instrumented seam
        // wraps a pass-through that marks the turn's dispatch moment.
        transform_context: Some(super::request_timing::instrument_transform_context(
            std::sync::Arc::clone(&request_timing_wiring),
            canonical_live.as_ref().map_or_else(
                super::request_timing::pass_through_transform,
                |live| {
                    super::canonical_live::wrap_transform_context(
                        Arc::clone(live),
                        super::request_timing::pass_through_transform(),
                    )
                },
            ),
        )),
        should_stop_after_turn: {
            let steering = config.queued_steering_probe.take();
            let boundary = config.should_stop_after_turn.take();
            Some(std::sync::Arc::new(move |context| {
                let steering = steering.clone();
                let boundary = boundary.clone();
                Box::pin(async move {
                    if steering.as_ref().is_some_and(|probe| probe()) {
                        return Ok(true);
                    }
                    match boundary {
                        Some(hook) => hook(context).await,
                        None => Ok(false),
                    }
                })
            }))
        },
        should_stop_before_turn: config.queued_steering_probe.clone(),
        steering_mode: config.steering_mode,
        follow_up_mode: config.follow_up_mode,
        ..Default::default()
    });

    let agent = Arc::new(agent);
    let services = Arc::new(super::plugin_services::PluginServices::new(
        wiring.session.clone(),
        &agent,
        cwd.clone(),
        config.agent_dir.clone(),
        &auxiliary_inference,
    ));
    plugins.set_capability_delegate("session.snapshot", services.clone());
    plugins.set_capability_delegate("inference.", services);
    let plugin_command_barrier = Arc::new(tokio::sync::Mutex::new(()));
    // The host installs the turn-activity probe after the build (the daemon
    // worker owns the turn queue); until then a job's run-idle wait is the
    // whole turn, which is the pre-existing behavior.
    let turn_activity = Arc::new(super::turn_activity::TurnActivitySignal::new());
    let jobs = Arc::new(super::plugin_jobs::PluginJobs::new(
        &plugins.host(),
        &agent,
        wiring.session.clone(),
        Arc::clone(&plugin_command_barrier),
        Arc::clone(&turn_activity),
    ));
    plugins.set_capability_delegate("jobs.", jobs.clone());
    plugins.set_capability_delegate(
        "harness.store.",
        Arc::new(crate::harness_plugin::HarnessStorage::new(
            &config.agent_dir,
            session_artifact_dir
                .as_ref()
                .map(|path| path.join("harness")),
            wiring.session.lock().await.get_session_id().to_owned(),
            wiring.session.clone(),
            &plugins.host(),
            &agent,
        )),
    );
    // Install native/auxiliary services that need AgentWeak here. This Agent
    // remains private until its selected strategy has built a valid live view.
    let startup_leaf = if matches!(
        config.context_startup_boundary,
        super::ContextStartupBoundary::Current
    ) {
        wiring.session.lock().await.get_leaf_id().map(str::to_owned)
    } else {
        startup_leaf
    };
    let assembled_messages = super::context_strategy::assemble_at(
        &plugins,
        existing_messages,
        context_archive.as_ref(),
        full_model.as_ref(),
        startup_leaf.as_deref(),
    )
    .await?;
    if stored_context_selection.is_none() && config.on_initial_context_selection.is_none() {
        let selection = pa_types::session::ContextSelection {
            version: 1,
            strategy: selected_route,
        };
        wiring.session.lock().await.append_custom_entry(
            pa_types::session::CONTEXT_SELECTION_CUSTOM_TYPE,
            Some(serde_json::to_value(selection)?),
        )?;
    }
    agent
        .set_messages(
            assembled_messages
                .into_iter()
                .filter_map(|message| super::provider_adapter::json_round_trip(&message))
                .collect(),
        )
        .await;
    // TS `_startRlmChildRun`'s spawn anchor: the `rlm.spawn` host handler
    // names the parent's in-flight turn through this weak seam (the
    // bridge is built before the agent exists, and a strong edge would
    // cycle the bridge -> agent -> kernel -> bridge graph).
    if let Some(recorder) = &semantic_recorder {
        let _ = wiring
            .rlm
            .semantic_spawn
            .set(super::rlm_host::SemanticSpawnAnchor {
                agent: Arc::downgrade(&agent),
                recorder: std::sync::Arc::clone(recorder),
            });
    }
    let mut session = AgentSession::from_session_arc_live(
        agent.clone(),
        wiring.session.clone(),
        resources.prompts.clone(),
        canonical_live,
    )
    .await?;
    session.plugins = Some(Arc::clone(&plugins));
    session.plugin_command_barrier = plugin_command_barrier;
    session.plugin_jobs = Some(jobs);
    session.turn_activity = turn_activity;
    session.context_archive = Arc::clone(&context_archive_state);
    session.context_model = std::sync::RwLock::new(full_model);
    super::plugins::observe_turns(&agent, Arc::clone(&plugins)).await;
    super::plugin_observers::observe_runs(&agent, plugins, Arc::clone(&wiring.session)).await;
    session.ollama_cloud = ollama_cloud;
    session.set_agent_dir(config.agent_dir.clone());
    // Every compaction path reads the session's resolved compaction
    // settings (TS `getCompactionSettings`): `/compact` matches the
    // `compact.*` turn-boundary tool's `keepRecentTokens`/`reserveTokens`.
    session.set_compaction_settings(crate::session_engine::compaction::CompactionSettings {
        threshold_percent: compaction_threshold_percent,
        enabled: compaction_settings.enabled.unwrap_or(true),
        reserve_tokens: compaction_settings
            .reserve_tokens
            .unwrap_or(crate::session_engine::compaction::DEFAULT_RESERVE_TOKENS),
        keep_recent_tokens: compaction_settings
            .keep_recent_tokens
            .unwrap_or(crate::session_engine::compaction::DEFAULT_KEEP_RECENT_TOKENS),
    });
    // Summarizer passes resolve their model through the `auxiliaryModel` setting
    // with the session model as fallback, so one-off prompts stay off the prompt-cache prefix.
    session.set_auxiliary_model_context(
        crate::session_engine::auxiliary_model::AuxiliaryModelContext {
            cwd,
            agent_dir: config.agent_dir.clone(),
        },
    );
    // The probe holds a WEAK reference: the engine owns the provisioner,
    // and a strong edge here would loop the graph.
    let kernel_state_probe: std::sync::Arc<
        dyn crate::session_engine::ipython_state::CompactionKernelProbe,
    > = std::sync::Arc::new(crate::session_engine::ipython_state::EngineOwnedProbe::new(
        std::sync::Arc::downgrade(&provisioner),
    ));
    session.set_kernel_state_probe(Some(kernel_state_probe));
    session.set_skills(resources.skills.clone());
    session.set_image_model_router(config.image_model_router.clone());
    // The semantic-edge handoff: the daemon's child registry and retry
    // park read the recorder; the side question keeps the pre-semantic
    // fn so its calls carry no id.
    session.set_semantic_edges(semantic_recorder);
    session.set_side_question_stream_fn(side_question_stream_fn);
    session.auxiliary_inference = Some(auxiliary_inference);
    // The armed image route never outlives the run that armed it (TS
    // `_clearModelOverrideWhenIdle`: the override drops once the turn is
    // idle, so a picker switch between turns is live immediately — the
    // settle's still-routed guard leaves the switched slot). The settle
    // here is idempotent: an un-armed episode's swap restores the slot
    // it already holds, and the next admission's own settle re-reads
    // fresh state either way.
    if let Some(router) = config.image_model_router.clone() {
        // A weak agent reference: the listener lives ON the agent, so a
        // strong edge would cycle and pin a dropped session's agent.
        let agent_at_end = std::sync::Arc::downgrade(&agent);
        agent
            .subscribe(move |event, _signal| {
                let router = router.clone();
                let agent_at_end = agent_at_end.clone();
                Box::pin(async move {
                    if matches!(event, pa_agent::types::AgentEvent::AgentEnd { .. }) {
                        (router.swap_target)(None);
                        // A leftover override would leak into a later
                        // `continue_run`'s loop config.
                        if let Some(agent) = agent_at_end.upgrade() {
                            agent.set_model_override(None);
                        }
                    }
                    Ok(())
                })
            })
            .await;
    }
    // Rows parked by a boot that settled mid-build merge in; later boots
    // push straight into the live session's queue.
    session.adopt_next_turn_rows(boot_notice_rows);

    turn_boundary.bind(super::turn_boundary::TurnBoundaryRuntime {
        agent,
        session: wiring.session.clone(),
        context_window: (model_context_window > 0).then_some(model_context_window),
        model_info,
    });

    let goal_driver = wiring.runtime.goal_driver().clone();
    Ok(SessionEngine {
        session,
        skills: resources.skills,
        skill_diagnostics: resources.skill_diagnostics,
        prompt_templates: resources.prompts,
        agents_files: resources.agents_files,
        system_prompt,
        goal_driver,
        queued_goal_context_purge: config.queued_goal_context_purge.clone(),
        mcp_manager,
        turn_boundary,
        rlm_usage: wiring.rlm_usage,
        rlm_progress_notes: Arc::clone(&wiring.rlm.notes),
        provisioner,
    })
}

impl SessionEngine {
    /// After a model switch, the `model.info` handler and the usage
    /// estimate's context window follow the model the session now runs.
    pub fn update_model_facts(&self, model: &pa_types::ai::Model) {
        super::turn_boundary::TurnBoundaryRequests::rebind_model_facts(
            &self.turn_boundary,
            super::turn_boundary::ModelInfo {
                id: model.id.clone(),
                provider: model.provider.clone(),
                input: model.input.clone(),
            },
            (model.context_window > 0).then_some(model.context_window),
        );
    }

    /// A weak reference for lock-free kernel liveness probes without
    /// joining the strong ownership graph.
    pub fn kernel_provisioner_weak(
        &self,
    ) -> std::sync::Weak<crate::kernel::provisioner::IpythonKernelProvisioner> {
        std::sync::Arc::downgrade(&self.provisioner)
    }

    /// Expand a `/skill:<name>` submission into its `<skill>` block for
    /// the accepted-turn row (TS `_expandSkillCommand`; the row the daemon
    /// emits before admission must match the text the model turn
    /// receives). Non-skill inputs pass through unchanged; the admitted
    /// turn's own expansion is idempotent over the block.
    pub fn expand_skill_submission(&self, text: &str) -> String {
        crate::skills::expand_skill_command(text, &self.skills).0
    }

    /// Withdraw the queued goal-context turns: a minted continuation waiting
    /// in the embedding's queue never runs behind a paused/cleared/replaced goal.
    pub fn purge_queued_goal_contexts(&self) {
        if let Some(purge) = &self.queued_goal_context_purge {
            purge();
        }
    }

    /// Prompt the session (delegates to `AgentSession::prompt`).
    ///
    /// # Errors
    ///
    /// Returns the underlying turn admission error: an invalid prompt, an
    /// already-busy session under its admission rule, or the turn's own failure.
    pub async fn prompt(
        &self,
        text: &str,
        options: PromptOptions,
    ) -> anyhow::Result<PromptOutcome> {
        self.session.prompt(text, options).await
    }

    /// Opaque admitted job IDs for correlating one command or prompt's work.
    pub fn plugin_job_checkpoint(&self) -> Vec<String> {
        self.session.plugin_job_checkpoint()
    }
    /// Wait for admitted jobs; return only jobs admitted after this checkpoint.
    pub async fn drain_plugin_jobs_since(&self, checkpoint: &[String]) -> Vec<serde_json::Value> {
        self.session.drain_plugin_jobs_since(checkpoint).await
    }
    /// Wait for the current run, awaited observers, and admitted jobs.
    pub async fn drain_plugin_jobs(&self) -> Vec<serde_json::Value> {
        self.session.drain_plugin_jobs().await
    }

    /// Tear the kernel down now with a final namespace snapshot, for a host that
    /// ends the session but keeps the engine alive; dropping the engine tears the kernel down too.
    pub async fn dispose_kernel(&self) {
        self.provisioner.dispose(None).await;
    }

    /// Release the kernel with a final namespace snapshot, revivable: the next
    /// kernel use boots a fresh kernel and revives the flushed snapshot.
    pub async fn stop_kernel_snapshot(&self) {
        self.provisioner
            .stop_kernel(Some(crate::kernel::shared::KernelShutdownOptions {
                snapshot: true,
                drain_host_requests: true,
            }))
            .await;
    }

    /// One factory activity over this session's live kernel. Named specs resolve
    /// in the kernel's machine library. Every child spawn enforces model access.
    ///
    /// # Errors
    /// Returns an error for invalid arguments, an unavailable kernel, or a failed
    /// kernel request. Legacy harness records are never loaded by this path.
    pub async fn factory_activity(
        &self,
        action: &str,
        run_id: Option<&str>,
        spec_id: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<serde_json::Value> {
        let request = super::factory_host::FactoryActivityRequest::parse(
            action, run_id, spec_id, timeout_ms,
        )?;
        let manager = self
            .provisioner
            .manager()
            .ok_or_else(|| anyhow::anyhow!("Kernel is not running"))?;
        manager
            .factory_activity(
                request.action,
                request.run_id.as_deref(),
                request.spec_id.as_deref(),
                request.timeout_ms,
            )
            .await
    }

    /// Out-of-band kernel bash activity, scoped to this session's live kernel.
    ///
    /// # Errors
    ///
    /// Returns an error when the session has no running kernel, or the
    /// kernel's bash-activity validation or request fails.
    pub async fn bash_activity(
        &self,
        action: &str,
        activity_id: Option<&str>,
        lines: usize,
    ) -> anyhow::Result<serde_json::Value> {
        let manager = self
            .provisioner
            .manager()
            .ok_or_else(|| anyhow::anyhow!("Kernel is not running"))?;
        manager.bash_activity(action, activity_id, lines).await
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod append_system_tests;
