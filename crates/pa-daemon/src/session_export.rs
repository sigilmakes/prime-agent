//! Session export commands: the worker-side handlers for `export_html` and `export_jsonl`
//! (TS `session.exportToHtml` / `exportToJsonl`). The HTML file is built by pa-core's
//! exporter (the embedded template plus the session data); the JSONL export is the current
//! branch re-chained into a linear file.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::engine::SessionEngine;
use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::SessionCore;

/// The `export_*` command set: the engine (system prompt), the session store
/// (entries), the settings dirs (theme resolution), the session cwd.
pub(crate) struct ExportCommands {
    engine: Arc<dyn SessionEngine>,
    core: Arc<Mutex<SessionCore>>,
    agent_dir: PathBuf,
}

impl ExportCommands {
    pub(crate) fn new(
        engine: Arc<dyn SessionEngine>,
        core: Arc<Mutex<SessionCore>>,
        agent_dir: PathBuf,
    ) -> Self {
        ExportCommands {
            engine,
            core,
            agent_dir,
        }
    }

    /// `export_html`: render the session to a standalone HTML file; the
    /// response carries the written path (TS `{ path }`).
    pub(crate) async fn export_html(&self, payload: &Value) -> DaemonResponse {
        let output_path = payload
            .get("outputPath")
            .and_then(Value::as_str)
            .map(str::to_string);
        match self.export_html_impl(output_path.as_deref()).await {
            Ok(path) => response_success(None, "export_html", Some(json!({ "path": path }))),
            Err(error) => response_failure(None, "export_html", &format!("{error:#}"), None),
        }
    }

    async fn export_html_impl(&self, output_path: Option<&str>) -> Result<String> {
        // The store snapshot under the core lock: the std guard is not `Send`, so the
        // engine reads below await outside the lock.
        let (header, entries, leaf_id, session_file, theme_name) = {
            let core = self.core.lock().unwrap();
            let store = core
                .store
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Session is still initializing"))?;
            let theme_name = self
                .resolved_theme_name(&core)
                .context("resolving the export theme")?;
            let header = serde_json::to_value(&store.header)?;
            let header = match header {
                Value::Object(map) => {
                    // The export's header is the `type: "session"` file line, not just
                    // the typed struct.
                    let mut with_type = serde_json::Map::new();
                    with_type.insert("type".to_string(), Value::String("session".to_string()));
                    with_type.extend(map);
                    Value::Object(with_type)
                }
                other => other,
            };
            let entries: Vec<Value> = store
                .entries()
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<_, serde_json::Error>>()
                .context("serializing session entries")?;
            (
                header,
                entries,
                store.leaf_id().map(str::to_string),
                store.path.clone(),
                theme_name,
            )
        };
        // The tools read comes first: an absent session builds here (the TS state
        // exists from create), so the best-effort prompt read below then sees it too.
        let tools = self.engine.export_tools().await;
        let rendered_tools = self.engine.export_rendered_tools(&entries).await;
        let data = pa_core::export_html::SessionExportData {
            header,
            entries,
            leaf_id,
            system_prompt: self.engine.export_system_prompt(),
            tools,
            rendered_tools,
        };
        pa_core::export_html::export_session_to_html(
            &data,
            theme_name.as_deref(),
            &self.agent_dir,
            &session_file,
            output_path,
        )
    }

    /// The session's configured theme name (TS `settingsManager.getTheme()`)
    /// resolved against the session's cwd and the agent dir.
    fn resolved_theme_name(&self, core: &SessionCore) -> Result<Option<String>> {
        let cwd = if core.cwd.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(&core.cwd)
        };
        let settings = pa_core::settings::SettingsManager::create(&cwd, &self.agent_dir);
        Ok(settings.get_theme().map(str::to_string))
    }

    /// `export_jsonl`: the current branch re-chained linearly into a JSONL
    /// file; the response carries the resolved path (TS `{ path }`).
    pub(crate) fn export_jsonl(&self, payload: &Value) -> DaemonResponse {
        let output_path = payload
            .get("outputPath")
            .and_then(Value::as_str)
            .map(str::to_string);
        match self.export_jsonl_impl(output_path.as_deref()) {
            Ok(path) => response_success(None, "export_jsonl", Some(json!({ "path": path }))),
            Err(error) => response_failure(None, "export_jsonl", &format!("{error:#}"), None),
        }
    }

    fn export_jsonl_impl(&self, output_path: Option<&str>) -> Result<String> {
        let core = self.core.lock().unwrap();
        let store = core
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Session is still initializing"))?;
        // A relative (or omitted) output path lands in the daemon process
        // cwd, like the TS `resolve` default.
        let cwd = if core.cwd.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(&core.cwd)
        };
        let file_path = match output_path {
            Some(path) => {
                let absolute = Path::new(path);
                if absolute.is_absolute() {
                    absolute.to_path_buf()
                } else {
                    cwd.join(path)
                }
            }
            None => cwd.join(format!(
                "session-{}.jsonl",
                pa_core::session::manager::format_iso_now().replace([':', '.'], "-")
            )),
        };
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let header = json!({
            "type": "session",
            "version": pa_core::session::CURRENT_SESSION_VERSION,
            "id": store.session_id(),
            "timestamp": pa_core::session::manager::format_iso_now(),
            "cwd": core.cwd,
        });
        let mut rows = Vec::new();
        // The branch walks root-to-leaf; parent ids re-chain linearly so
        // the exported file resumes as one straight session.
        let mut previous: Option<String> = None;
        for entry in store.branch() {
            let mut value = serde_json::to_value(entry)?;
            if let Some(map) = value.as_object_mut() {
                match previous.clone() {
                    Some(parent) => map.insert("parentId".to_string(), Value::String(parent)),
                    None => map.insert("parentId".to_string(), Value::Null),
                };
            }
            previous = value.get("id").and_then(Value::as_str).map(str::to_string);
            rows.push(value);
        }
        // JSONL is importable session history, not the display/RPC row stream.
        // Use the normal log writer so exports obey the same CRC/sequence
        // contract as every strict session reader. Publish only complete bytes.
        let staging = file_path.with_file_name(format!(".session-export-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&staging)?;
        let staged_file = staging.join("export.jsonl");
        let result = (|| -> Result<()> {
            let wire = crate::session_transport::LogWire::fresh(&staged_file);
            let mut flushed = 0;
            wire.flush(
                "export",
                &header,
                &rows,
                &mut flushed,
                &pa_core::session_log::ReadCursor::default(),
            )?;
            drop(wire);
            pa_core::platform::rename_onto_durable(&staged_file, &file_path)
                .with_context(|| format!("writing {}", file_path.display()))?;
            Ok(())
        })();
        // These names belong only to this private staging directory; no
        // execution owner or log reader can retain its lock inode.
        for path in [staged_file, staging.join("export.lock")] {
            if let Err(error) = std::fs::remove_file(&path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error).with_context(|| format!("cleaning {}", path.display()));
                }
            }
        }
        std::fs::remove_dir(&staging)?;
        result?;
        Ok(file_path.to_string_lossy().into_owned())
    }
}

/// The export's custom-tool renderer: resolves a tool by name against the
/// session's live registry. The Rust tool surface carries no render functions,
/// so the export falls back to the template's generic tool rendering.
pub(crate) struct ExportToolRenderer<'a> {
    /// The session's live tool registry (TS `getToolDefinition` source).
    pub tools: &'a [std::sync::Arc<dyn pa_agent::types::AgentTool>],
}

impl pa_core::export_html::ToolHtmlRenderer for ExportToolRenderer<'_> {
    fn render_call(&self, _tool_call_id: &str, tool_name: &str, _args: &Value) -> Option<String> {
        // Registry lookup first (TS `getToolDefinition`): an unregistered tool never
        // renders; a registered one has no render function.
        self.tools.iter().find(|tool| tool.name() == tool_name)?;
        None
    }

    fn render_result(
        &self,
        _tool_call_id: &str,
        tool_name: &str,
        _result: &[Value],
        _details: &Value,
        _is_error: bool,
    ) -> Option<pa_core::export_html::RenderedToolResult> {
        self.tools.iter().find(|tool| tool.name() == tool_name)?;
        None
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    fn write_session_log(path: impl AsRef<Path>, content: impl AsRef<str>) -> anyhow::Result<()> {
        let path = path.as_ref();
        let directory = path.parent().expect("fixture directory");
        let id = path
            .file_stem()
            .and_then(|name| name.to_str())
            .expect("fixture id");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut log = pa_core::session_log::SessionLog::open(directory, id, deadline)?;
        for line in content
            .as_ref()
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            let row: serde_json::Value = serde_json::from_str(line)?;
            anyhow::ensure!(
                matches!(
                    log.append(&row, deadline),
                    pa_core::session_log::Outcome::Committed { .. }
                ),
                "fixture row must commit"
            );
        }
        Ok(())
    }

    fn core_with_session(dir: &Path) -> SessionCore {
        let file = dir.join("sess.jsonl");
        write_session_log(
            &file,
            concat!(
                r##"{"type":"session","id":"sess-1","version":3,"timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"##,
                "\n",
                r##"{"type":"message","id":"e1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hi"}}"##,
                "\n",
                r##"{"type":"message","id":"e2","parentId":"e1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"##,
                "\n",
            ),
        )
        .expect("write session file");
        let store = crate::session_store::SessionFile::open(&file).expect("open session");
        SessionCore::test_core(Some(store), dir.display().to_string())
    }

    #[tokio::test]
    async fn export_html_writes_the_session_data() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let core = Arc::new(Mutex::new(core_with_session(dir.path())));
        let engine: Arc<dyn SessionEngine> = Arc::new(crate::engine::ScriptedEngine::default());
        let exports = ExportCommands::new(engine, core, dir.path().join("agent"));
        let out = dir.path().join("export.html");
        let payload = json!({ "outputPath": out.display().to_string() });
        let response = exports.export_html(&payload).await;
        assert!(response.success, "export failed: {:?}", response.error);
        let path = response
            .data
            .as_ref()
            .and_then(|data| data.get("path"))
            .and_then(Value::as_str)
            .expect("path in response")
            .to_string();
        assert_eq!(path, out.display().to_string());
        let html = std::fs::read_to_string(&out).expect("read export");
        assert!(html.contains("Session Export"));
        assert!(html.contains("--accent:"));
    }

    #[tokio::test]
    async fn export_jsonl_rechains_the_branch() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let core = Arc::new(Mutex::new(core_with_session(dir.path())));
        let engine: Arc<dyn SessionEngine> = Arc::new(crate::engine::ScriptedEngine::default());
        let exports = ExportCommands::new(engine, core, dir.path().join("agent"));
        let response = exports.export_jsonl(&json!({}));
        assert!(response.success, "export failed: {:?}", response.error);
        let path = response
            .data
            .as_ref()
            .and_then(|data| data.get("path"))
            .and_then(Value::as_str)
            .expect("path in response")
            .to_string();
        let entries = pa_core::session::read::read_entries(Path::new(&path))
            .expect("the JSONL export is a validated session log");
        let lines: Vec<Value> = entries
            .into_iter()
            .map(|entry| serde_json::to_value(entry).expect("export entry"))
            .collect();
        assert_eq!(lines.len(), 3);
        let header = &lines[0];
        assert_eq!(header["type"], "session");
        assert_eq!(header["id"], "sess-1");
        assert_eq!(header["cwd"], dir.path().display().to_string());
        let first = &lines[1];
        assert_eq!(first["id"], "e1");
        assert_eq!(first["parentId"], Value::Null);
        let second = &lines[2];
        assert_eq!(second["id"], "e2");
        assert_eq!(second["parentId"], "e1");
    }

    #[test]
    fn failed_jsonl_export_preserves_the_source_and_existing_destination() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut core = core_with_session(dir.path());
        let store = core.store.as_mut().expect("session store");
        let source = store.path.clone();
        let source_before = std::fs::read(&source).expect("source bytes");
        // The native log refuses a row beyond its 16 MiB read limit. A
        // failed export must not truncate an earlier successful export.
        store.append_message(&json!({ "role": "user", "content": "x".repeat(16 * 1024 * 1024) }));
        let target = dir.path().join("export.jsonl");
        let target_before = b"previous export\n";
        std::fs::write(&target, target_before).expect("existing export");
        let engine: Arc<dyn SessionEngine> = Arc::new(crate::engine::ScriptedEngine::default());
        let exports =
            ExportCommands::new(engine, Arc::new(Mutex::new(core)), dir.path().join("agent"));
        let response = exports.export_jsonl(&json!({ "outputPath": target }));
        assert!(!response.success, "oversized row must be refused");
        assert_eq!(std::fs::read(&target).expect("target bytes"), target_before);
        assert_eq!(std::fs::read(&source).expect("source bytes"), source_before);
        assert!(std::fs::read_dir(dir.path())
            .expect("export directory")
            .all(|entry| !entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".session-export-")));
    }

    #[tokio::test]
    async fn export_requires_the_session_store() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut core = core_with_session(dir.path());
        core.store = None;
        let engine: Arc<dyn SessionEngine> = Arc::new(crate::engine::ScriptedEngine::default());
        let exports =
            ExportCommands::new(engine, Arc::new(Mutex::new(core)), dir.path().join("agent"));
        let response = exports.export_html(&json!({})).await;
        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("Session is still initializing")
        );
    }
}
