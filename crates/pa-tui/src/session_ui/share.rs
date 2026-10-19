//! The `/update`, `/copy`, `/export`, and `/share` runs — the
//! spawned one-way tasks and their note outcomes.
use super::{
    export_share, key_event_to_id, AgentView, DaemonCommand, Duration, GhAuthStatus, GistOutcome,
    KeyEvent, Map, Result, SessionUi, ShareLoader, Value, UI_REQUEST_TIMEOUT_MS,
};

/// The `/share` upload task's report: the created gist or the failure
/// message.
pub(crate) type ShareNote = Result<GistOutcome, String>;

/// The background `/update` run's report: the new build's version line, or
/// the failure message.
pub(crate) type UpdateNote = std::result::Result<String, String>;

/// A `/share` upload in flight: the abortable task and the temp export.
pub(crate) struct ShareRun {
    /// The upload task; aborting it kills `gh` (`kill_on_drop`).
    task: tokio::task::JoinHandle<()>,
    /// The temp HTML export `gh gist create` uploads (removed on settle).
    tmp_file: std::path::PathBuf,
}

impl SessionUi {
    // Update (/update)

    /// The confirmed update: spawn the download+install OUT-OF-BAND. The run replaces only the
    /// on-disk binary (the running binary is in-memory), so the TUI stays mounted, the daemon keeps
    /// running, and the outcome lands as a row through [`Self::update_notes`].
    pub(super) fn spawn_update(&mut self, view: &mut AgentView) {
        let Some(update) = self.update_commands.clone() else {
            self.note("/update is not available in this client yet", view);
            return;
        };
        let notes = self.update_notes.clone();
        self.update_in_flight = true;
        self.note(
            "Updating — downloading and installing the latest Rust build…",
            view,
        );
        tokio::spawn(async move {
            let outcome = update.0.run_update().await;
            let _ = notes.send(outcome);
        });
    }

    /// The landed outcome: the success row names the new build (restart runs
    /// it), else the error row carries the installer's failure.
    pub(crate) fn apply_update_note(&mut self, outcome: UpdateNote, view: &mut AgentView) {
        self.update_in_flight = false;
        match outcome {
            Ok(version) => {
                self.note(
                    &format!("updated to {version} — restart prime-agent to run it"),
                    view,
                );
            }
            Err(message) => {
                self.error_row(&message, view);
            }
        }
    }

    // Clipboard (/copy)

    /// Fetch the last assistant text from the daemon and copy it to the
    /// clipboard.
    pub(crate) async fn handle_copy_command(&mut self, view: &mut AgentView) -> Result<()> {
        let data = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetLastAssistantText {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Map::default(),
                },
            )
            .await?;
        let text = data
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        let Some(text) = text else {
            self.error_row("No agent messages to copy yet.", view);
            return Ok(());
        };
        match crate::clipboard::copy_to_clipboard(&text, &mut self.osc_sink) {
            Ok(crate::clipboard::CopyOutcome::Confirmed) => {
                self.toast("Copied last agent message to clipboard", view);
            }
            Ok(crate::clipboard::CopyOutcome::Requested) => {
                self.toast(crate::clipboard::CLIPBOARD_REQUESTED, view);
            }
            Err(message) => self.error_row(&message, view),
        }
        Ok(())
    }

    // Session export and share (/export, /share)

    /// Export the session to HTML — or, for an explicit `.jsonl` path, the
    /// current branch as a JSONL file — and report the written path.
    pub(crate) async fn handle_export_command(
        &mut self,
        resolved: &pa_types::slash_commands::ResolvedSlashCommand,
        view: &mut AgentView,
    ) -> Result<()> {
        let command_text = if resolved.args.is_empty() {
            "/export".to_string()
        } else {
            format!("/export {}", resolved.args)
        };
        let output_path = export_share::path_command_argument(&command_text, "/export");
        let request = if output_path.as_deref().is_some_and(|path| {
            std::path::Path::new(path)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        }) {
            DaemonCommand::ExportJsonl {
                id: None,
                active_session_id: self.active_session_id.clone(),
                output_path,
                rest: Map::default(),
            }
        } else {
            DaemonCommand::ExportHtml {
                id: None,
                active_session_id: self.active_session_id.clone(),
                output_path,
                rest: Map::default(),
            }
        };
        match self
            .bounded_request(Duration::from_millis(UI_REQUEST_TIMEOUT_MS), request)
            .await
        {
            Ok(data) => {
                let path = data.get("path").and_then(Value::as_str).unwrap_or_default();
                self.note(&format!("Session exported to: {path}"), view);
            }
            Err(error) => {
                self.error_row(&format!("Failed to export session: {error:#}"), view);
            }
        }
        Ok(())
    }

    /// Gate on the GitHub CLI, export the session to a temp file, then upload it
    /// as a secret gist while the cancellable loader replaces the editor.
    pub(crate) async fn handle_share_command(&mut self, view: &mut AgentView) -> Result<()> {
        match export_share::probe_gh_auth() {
            GhAuthStatus::NotLoggedIn => {
                self.error_row(
                    "GitHub CLI is not logged in. Run 'gh auth login' first.",
                    view,
                );
                return Ok(());
            }
            GhAuthStatus::NotInstalled => {
                self.error_row(
                    "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/",
                    view,
                );
                return Ok(());
            }
            GhAuthStatus::Ok => {}
        }
        // The temp export `gh` uploads.
        let tmp_file = std::env::temp_dir().join("session.html");
        let export = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::ExportHtml {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    output_path: Some(tmp_file.to_string_lossy().into_owned()),
                    rest: Map::default(),
                },
            )
            .await;
        if let Err(error) = export {
            self.error_row(&format!("Failed to export session: {error:#}"), view);
            return Ok(());
        }
        // The upload runs in the background: the loader keeps the UI live,
        // the run loop folds the outcome in when it lands.
        let child = match export_share::spawn_gist_create(&tmp_file) {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_file(&tmp_file);
                self.error_row(&format!("Failed to create gist: {error}"), view);
                return Ok(());
            }
        };
        let notes = self.share_notes.clone();
        let task = tokio::spawn(async move {
            let outcome = export_share::gist_outcome(child).await;
            let _ = notes.send(outcome);
        });
        self.share = Some(ShareRun {
            task,
            tmp_file: tmp_file.clone(),
        });
        view.share_loader = Some(ShareLoader::new());
        self.dirty = true;
        Ok(())
    }

    /// Whether a `/share` upload is in flight (the run loop must not end
    /// before its outcome row lands).
    pub(crate) fn share_pending(&self) -> bool {
        self.share.is_some()
    }

    /// A `/share` upload settled: drop the loader, clean the temp file, and surface the share URL,
    /// or the failure. A late outcome after a cancel is ignored.
    pub(crate) fn apply_share_outcome(&mut self, outcome: ShareNote, view: &mut AgentView) {
        let Some(run) = self.share.take() else {
            return;
        };
        view.share_loader = None;
        let _ = std::fs::remove_file(&run.tmp_file);
        match outcome {
            Ok(gist) => {
                self.note(
                    &format!("Share URL: {}\nGist: {}", gist.preview_url, gist.gist_url),
                    view,
                );
            }
            Err(message) => {
                self.error_row(&format!("Failed to create gist: {message}"), view);
            }
        }
        self.dirty = true;
    }

    /// One key press while the `/share` loader is open: the cancel binding aborts
    /// the upload, every other key is the loader's.
    pub(crate) fn handle_share_loader_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        if let Some(id) = key_event_to_id(&key) {
            let kb = view.editor.keybindings();
            if kb.matches(&id, "tui.select.cancel") {
                if let Some(run) = self.share.take() {
                    // Aborting the task drops the child and kills `gh`
                    // (kill-on-drop); the temp file goes with the run.
                    run.task.abort();
                    let _ = std::fs::remove_file(&run.tmp_file);
                }
                view.share_loader = None;
                self.note("Share cancelled", view);
            }
        }
        self.dirty = true;
        Ok(())
    }
}
