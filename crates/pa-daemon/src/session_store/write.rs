//! Session row builders and durable append publication.

use super::{
    json, new_entry_id, HashMap, PathBuf, Result, SessionEntry, SessionFile, SessionHeader, Value,
};

impl SessionEntry {
    fn new(
        type_: &str,
        parent_id: Option<String>,
        used: &HashMap<String, usize>,
        fields: Value,
        timestamp: &str,
    ) -> Self {
        SessionEntry {
            type_: type_.to_string(),
            id: new_entry_id(used),
            parent_id,
            timestamp: timestamp.to_string(),
            fields,
        }
    }
}

impl SessionFile {
    /// Persist only the newly appended creation records on a resumed file.
    pub(crate) fn persist_appended(&self, start: usize) -> Result<()> {
        self.flush_unmaterialized_log_tail(start)
    }

    pub fn append_entry(&mut self, type_: &str, fields: Value) -> String {
        let parent_id = self.leaf_id.clone();
        let mut entry = SessionEntry::new(
            type_,
            parent_id,
            &self.by_id,
            fields,
            &crate::util::now_iso(),
        );
        if self.window.is_some() {
            entry.id = uuid::Uuid::new_v4().to_string();
        }
        let id = entry.id.clone();
        self.push_index(entry);
        id
    }

    pub fn append_message(&mut self, message: &Value) -> String {
        self.append_entry("message", serde_json::json!({ "message": message }))
    }

    pub fn append_session_state(&mut self, status: &str) -> String {
        self.append_entry(
            "session_state",
            serde_json::json!({ "state": { "status": status } }),
        )
    }

    pub fn append_session_info(&mut self, name: &str) -> String {
        self.append_entry("session_info", serde_json::json!({ "name": name.trim() }))
    }

    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> String {
        self.append_entry(
            "model_change",
            serde_json::json!({ "provider": provider, "modelId": model_id }),
        )
    }

    pub fn append_thinking_level_change(&mut self, level: &str) -> String {
        self.append_entry(
            "thinking_level_change",
            serde_json::json!({ "thinkingLevel": level }),
        )
    }

    /// Materialize the unflushed entry tail in the append-only log.
    ///
    /// # Errors
    /// Returns the log's refusal; an empty path stays in memory.
    pub fn rewrite(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        self.flush_unmaterialized_log_tail(0)
    }

    /// Commit one row before publishing it in the serving fold.
    ///
    /// # Errors
    /// Returns the log's refusal without indexing an uncommitted row.
    pub fn persist_entry(&mut self, entry_type: &str, fields: Value) -> Result<String> {
        self.persist_entry_at(entry_type, fields, &crate::util::now_iso())
    }

    /// Durably mark that this session has drawn the Anthropic subscription
    /// ban-risk warning (the once-per-session-lifecycle gate, operator
    /// directive 2026-09-29): append the
    /// [`pa_core::session::ANTHROPIC_WARNING_SHOWN_CUSTOM_TYPE`] custom row
    /// and arm the in-memory flag, so a reattach, a resume, or a worker
    /// replacement rebuild reads the row and the gate holds.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when the durable append fails; the
    /// in-memory flag stays unset then, so the next open re-warns (a lost
    /// marker costs one repeated warning, never a suppressed one).
    pub fn mark_anthropic_warning_shown(&mut self) -> Result<()> {
        self.persist_entry(
            "custom",
            json!({
                "customType": pa_core::session::ANTHROPIC_WARNING_SHOWN_CUSTOM_TYPE,
                "data": { "shown": true },
            }),
        )?;
        self.anthropic_warning_shown = true;
        Ok(())
    }

    /// Append one entry stamped with the given time. The interrupted-
    /// compaction replay re-stamps the supervisor's declaration, so the
    /// entry's timestamp is the row's stable identity: a replacement that
    /// already persisted the disclosure but died before the supervisor
    /// consumed the record replays the same declaration, and the create
    /// handler recognizes its own row instead of duplicating it.
    ///
    /// # Errors
    ///
    /// Returns an error when the window-backed file is missing or the append
    /// or durability sync fails.
    pub fn persist_entry_at(
        &mut self,
        entry_type: &str,
        fields: Value,
        timestamp: &str,
    ) -> Result<String> {
        let mut entry = SessionEntry::new(
            entry_type,
            self.leaf_id.clone(),
            &self.by_id,
            fields,
            timestamp,
        );
        // A windowed index lacks the pre-window IDs, so the short minted ID could
        // collide with unloaded history; a UUID cannot (same rule as `append_entry`).
        if self.window.is_some() {
            entry.id = uuid::Uuid::new_v4().to_string();
        }
        self.persist_built_entry(entry)
    }

    /// Commit through the log before publishing the row in the serving fold.
    pub(super) fn persist_built_entry(&mut self, entry: SessionEntry) -> Result<String> {
        if self.path.as_os_str().is_empty() {
            let id = entry.id.clone();
            self.push_index(entry);
            return Ok(id);
        }
        self.persist_built_log_entry(entry)
    }

    /// Point the session at a concrete file path (after `create`), preserving entries.
    pub fn set_path(&mut self, path: PathBuf) {
        if self.path != path {
            self.lease = None;
        }
        if self.path != path && !path.as_os_str().is_empty() {
            self.log_transport = Some(crate::session_transport::LogWire::for_path(&path));
            if let Some(id) = crate::session_transport::session_id_of(&path) {
                self.header.id = id;
            }
        }
        self.path = path;
    }
}

/// The stored first line: the typed header plus the `session` type tag. The
/// tag leads the line (TS `SessionHeader` declares `type` first, so the
/// first line starts with `{"type":"session",...}`); `preserve_order` keeps
/// the insertion order, so the tag is rebuilt into the leading slot.
#[must_use]
pub fn session_header_line(header: &SessionHeader) -> Value {
    let value = serde_json::to_value(header).unwrap_or(Value::Null);
    let Some(object) = value.as_object() else {
        return json!({ "type": "session" });
    };
    let mut ordered = serde_json::Map::new();
    ordered.insert("type".to_string(), Value::String("session".to_string()));
    ordered.extend(
        object
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    Value::Object(ordered)
}
