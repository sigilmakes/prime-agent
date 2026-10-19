//! Interactive agent session over the daemon: attach to a live session, send prompts, render
//! streamed assistant output, and switch sessions. The session loop keeps running in the daemon
//! worker, so closing the UI detaches instead of stopping the session. Two UI sources drive the
//! same loop: a crossterm terminal and a headless plan (the verifier seam: the identical path
//! without a TTY).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use crate::daemon_client::DaemonClient;
use crate::daemon_client::DaemonClientEvent;
use crate::daemon_reconnect::RecoveryKind;
use crate::exit_guard::ExitGuard;
use crate::keybindings::KeybindingsManager;
use crate::session_ui::SessionUi;
use crate::view::AgentView;

mod onboarding;

use onboarding::{run_onboarding_phase, PaneDrive};

mod headless;

use headless::HeadlessSettle;

mod render;

pub use headless::{HeadlessPlan, HeadlessStep, UiMode};
pub use onboarding::{ModelReadiness, OnboardingSink, OnboardingTask};
pub(crate) use render::write_flush_rows;
use render::{
    apply_startup_chrome, check_tmux_keyboard_setup, spawn_session_reader, Renderer,
    TerminalHandoff,
};

mod reconnect;

use reconnect::{
    arm_shutdown_recovery, ReconnectConnect, ReconnectLoop, SessionReconnect,
    SESSION_RECONNECT_ATTEMPT_TIMEOUT_S,
};
#[cfg(test)]
use reconnect::{DAEMON_SHUTDOWN_RECONNECT_WINDOW, SHUTDOWN_RECONNECT_RETRY};

mod run;

pub use run::{run_interactive, run_interactive_agents_view_open};

use crossterm::event::KeyEvent;
use crossterm::terminal;
use ratatui::Terminal;
use tokio::sync::mpsc;

#[cfg(test)]
pub(crate) mod tests;

/// Which session the interactive run opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSelection {
    New,
    /// Create a fresh session bound under a parent session (the scoped
    /// agents view's new action): the create records `parent_session_file`
    /// as the session's `parentSessionPath` and runs it at `rlm_depth`
    /// (one below the parent), so every roster surface nests it under the
    /// parent.
    NewChild {
        parent_session_file: PathBuf,
        rlm_depth: u32,
    },
    /// Attach an existing live session by active session id.
    Attach(String),
    Resume(PathBuf),
}

/// Explicit model selection carried into every `create` config: the CLI
/// `--provider`/`--model`/`--api-key`/`--thinking` flags, authoritative end-to-end — the
/// daemon worker resolves its session model and thinking level from this selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// The requested thinking level (`--thinking`); the worker clamps it to the supported levels.
    pub thinking: Option<pa_types::ai::ModelThinkingLevel>,
}

/// Options for one interactive run.
#[derive(Clone)]
pub struct InteractiveOptions {
    pub socket_path: PathBuf,
    pub cwd: PathBuf,
    /// The model catalog for the `/model` picker (a startup snapshot resolved by the
    /// composition root; the daemon's `get_model_catalog` refresh replaces it once it lands).
    pub model_catalog: Vec<pa_types::ai::Model>,
    /// Providers with configured auth for the picker's sign-in marking.
    pub model_configured_providers: std::collections::HashSet<String>,
    /// The settings recent-model list (`provider/id` keys, newest first).
    pub model_recent_models: Vec<String>,
    /// The settings default thinking level (the picker's effort seed for non-reasoning models).
    pub default_thinking_level: Option<String>,
    /// Persistence directory for new sessions (defaults to the daemon's sessions dir when `None`).
    pub session_dir: Option<PathBuf>,
    /// Scripted faux-engine script path. Verification seam only; the product never sets it.
    pub script_path: Option<PathBuf>,
    /// Model flags to carry into the create config.
    pub model_selection: ModelSelection,
    /// The `--models` scope patterns: raw strings — `provider/id`, globs, `:thinking`
    /// suffixes — that ride the create config's `models` field; the daemon resolves them into
    /// the session's scoped list. `None` leaves the scope unset.
    pub models: Option<Vec<String>>,
    /// Create without a session file (`--no-session`).
    pub no_session: bool,
    pub session: SessionSelection,
    /// Prompt sent immediately after attach (CLI message arguments).
    pub initial_message: Option<String>,
    /// The `terminal.showImages` setting, default true: whether image blocks render their
    /// metadata rows or the `[Image: ...]` placeholders.
    pub show_images: bool,
    /// The `terminal.fullscreenMouse` setting, default true: whether the fullscreen surface
    /// enables SGR mouse tracking and wheel-scrolls the transcript.
    pub fullscreen_mouse: bool,
    pub theme: String,
    /// The chat markdown fenced-code indent (`markdown.codeBlockIndent`, default two spaces).
    pub code_block_indent: String,
    /// The `/tree` selector's initial filter mode (the `treeFilterMode` setting, default view).
    pub tree_filter_mode: String,
    /// The `branchSummary.skipPrompt` setting: `/tree` navigation skips the "Summarize branch?"
    /// question and navigates with no summary.
    pub branch_summary_skip_prompt: bool,
    /// Product version for the brand splash.
    pub version: String,
    /// Run the first-run onboarding flow before the session screen.
    pub onboarding: Option<OnboardingTask>,
    /// `/mcp login` / `/mcp logout`: the client-side auth flows the composition root provides;
    /// `None` reports the commands as unavailable.
    pub client_auth: Option<crate::client_auth::ClientAuthCommandsHandle>,
    /// `/login` + `/logout`: the provider auth flows the composition root owns (credential
    /// storage, OAuth, the provider catalog); `None` reports the commands as unavailable.
    pub provider_auth: Option<crate::provider_auth::ProviderAuthCommandsHandle>,
    /// `/update`: the CLI child runner + the post-update relaunch the composition root owns;
    /// `None` reports the command as unavailable.
    pub update_commands: Option<crate::update_command::UpdateCommandsHandle>,
    /// The effective keybindings (defaults merged with the user's `keybindings.json`, loaded by the
    /// composition root): every hint and key handler renders and dispatches through this set.
    pub keybindings: KeybindingsManager,
    /// The client-owned prompt stash store shared across the chat views of this TUI process: a
    /// draft left behind on a session switch returns when the session's chat reopens; the
    /// composition root owns one store per process, so the agents-view loop keeps every stashed
    /// draft.
    pub prompt_stash: std::sync::Arc<std::sync::Mutex<crate::prompt_stash::PromptStashStore>>,
    /// The attached session's persisted RLM depth: the agents view passes it when it opens a
    /// row, and a subagent session renders its `depth N` tray label.
    pub session_rlm_depth: Option<u32>,
    /// Whether the opened session had direct children (TS `sessionHasChildren`).
    pub session_has_children: bool,
    /// The agents view handed the pane back from the dock's scoped panel (`scope_back`): the
    /// reopened chat starts with the dock focused on the panel's own group, instead of the prompt
    /// bar.
    pub restore_dock_focus: bool,
    /// The client-process settings the interactive commands read and persist (`/settings`,
    /// `/fullscreen`); `None` reports the commands' persistence as unavailable.
    pub client_settings: Option<std::sync::Arc<dyn crate::client_settings::ClientSettings>>,
}

// The opaque service handles have no Debug surface; the launch-config rows above are the debug
// surface.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for InteractiveOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractiveOptions")
            .field("socket_path", &self.socket_path)
            .field("cwd", &self.cwd)
            .field("session_dir", &self.session_dir)
            .field("script_path", &self.script_path)
            .field("model_selection", &self.model_selection)
            .field("models", &self.models)
            .field("model_catalog", &self.model_catalog)
            .field("no_session", &self.no_session)
            .field("session", &self.session)
            .field("initial_message", &self.initial_message)
            .field("theme", &self.theme)
            .field("code_block_indent", &self.code_block_indent)
            .field("version", &self.version)
            .field("onboarding", &self.onboarding)
            .field("fullscreen_mouse", &self.fullscreen_mouse)
            .field("client_auth", &self.client_auth)
            .field("keybindings", &self.keybindings.get_effective_config())
            .finish()
    }
}

impl InteractiveOptions {
    /// The `create` config carried on every new-session request (the agents view
    /// reuses it as the base of a resume's config).
    #[must_use]
    pub fn create_config(&self) -> Value {
        let mut config = json!({
            "cwd": self.cwd.display().to_string(),
        });
        if let Some(session_dir) = &self.session_dir {
            config["sessionDir"] = json!(session_dir.display().to_string());
        }
        if let Some(script) = &self.script_path {
            config["script"] = json!(script.display().to_string());
        }
        if let Some(provider) = &self.model_selection.provider {
            config["provider"] = json!(provider);
        }
        if let Some(model) = &self.model_selection.model {
            config["model"] = json!(model);
        }
        if let Some(api_key) = &self.model_selection.api_key {
            config["apiKey"] = json!(api_key);
        }
        if let Some(thinking) = self.model_selection.thinking {
            config["thinking"] = json!(thinking.wire_name());
        }
        // The raw `--models` patterns ride the create config: the daemon resolves them once per
        // create.
        if let Some(models) = &self.models {
            config["models"] = json!(models);
        }
        if let SessionSelection::NewChild {
            parent_session_file,
            rlm_depth,
        } = &self.session
        {
            config["parentSessionPath"] = json!(parent_session_file.to_string_lossy());
            config["rlmDepth"] = json!(rlm_depth);
        }
        config
    }
}

/// Result of an interactive run: session identity plus, in headless mode, the rendered frames.
#[derive(Debug, Clone, Default)]
pub struct InteractiveOutcome {
    pub active_session_id: String,
    pub session_id: String,
    /// The resume-hint line (a resumable, flushed session), for the composition root to print
    /// after the terminal is restored.
    pub resume_hint: Option<String>,
    pub last_assistant_text: Option<String>,
    pub frames: Vec<String>,
    /// Native restored-transcript bytes from the production replay writer. Headless capture
    /// only; terminal runs write directly to stdout and leave this empty.
    pub native_scrollback: Vec<u8>,
    /// OSC 52 clipboard sequences emitted during the run (headless capture only; terminal
    /// runs write them to stdout directly).
    pub clipboard_emissions: Vec<String>,
    /// `/resume` requested the agents view next (return-to-session flow).
    pub return_to_agents_view: bool,
    /// The subagent summary line opened the agents view scoped to this session's subtree;
    /// `None` with `return_to_agents_view` means the plain view.
    pub agents_view_scope: Option<crate::agents_view::AgentsViewScope>,
    /// `/resume <selector>` requested this session next.
    pub selection_request: Option<SessionSelection>,
    /// Texts copied out by finished mouse selections (headless runs have no terminal for
    /// OSC 52; the verifiers read these).
    pub copies: Vec<String>,
    /// Links opened by mouse clicks (headless runs have no terminal to hand a browser to;
    /// the verifiers read these).
    pub opened_urls: Vec<String>,
    /// A startup attach failed on a session that is truly gone: the run hands off to the
    /// agents view and this notice seeds the view's status line.
    pub agents_view_notice: Option<String>,
    /// How many first-draw windows this run served from an adopted cross-view layout handoff
    /// (`view::handoff`): the re-entry's frames are byte-identical either way, so a zero here
    /// is the re-render and a nonzero is the reuse.
    pub handoff_seeds: u32,
}

/// Inputs consumed by the UI loop. Terminal keys arrive one event at a time; headless steps
/// arrive as whole submissions.
enum UiInput {
    Key(KeyEvent),
    Paste(String),
    /// A decoded mouse report (wheel turns; other reports are consumed at the source).
    Mouse(crate::mouse::MouseEvent),
    Submit(String),
    /// The headless `SubmitAndSettle` step (see [`HeadlessStep`]).
    SubmitAndSettle {
        text: String,
        timeout_ms: u64,
    },
    /// One materialized input-idle tick (the headless `SettleIdle` step).
    SettleIdle,
    WaitIdle {
        timeout_ms: u64,
    },
    WaitRender {
        needle: String,
        timeout_ms: u64,
    },
    WaitGone {
        needle: String,
        timeout_ms: u64,
    },
    WaitNativeReplay {
        needle: String,
        timeout_ms: u64,
    },
    ScrollTop,
    Timestamp(std::sync::mpsc::Sender<(Instant, usize)>),
    Resize,
    HeadlessDone,
}
