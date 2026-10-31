//! Durable log append boundary for the serving conversation fold.

use anyhow::{Context, Result};
use serde_json::Value;

use super::SessionEntry;
use super::SessionFile;
use crate::session_transport::LogWire;

#[derive(Debug, Default)]
pub(super) struct LogProgress(std::sync::Mutex<ReadPosition>);

#[derive(Debug, Clone, Default)]
struct ReadPosition {
    cursor: pa_core::session_log::ReadCursor,
    flushed_entries: usize,
    observed_entries: usize,
}

impl Clone for LogProgress {
    fn clone(&self) -> Self {
        Self(std::sync::Mutex::new(
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        ))
    }
}

impl LogProgress {
    pub(super) fn opened(cursor: pa_core::session_log::ReadCursor, flushed_entries: usize) -> Self {
        Self(std::sync::Mutex::new(ReadPosition {
            cursor,
            flushed_entries,
            observed_entries: flushed_entries,
        }))
    }
}

impl SessionFile {
    /// Whether this store is served by the log transport.
    pub(crate) fn is_log_served(&self) -> bool {
        self.log_transport.is_some()
    }

    /// Attach the log transport (the worker create arms' wire).
    pub(crate) fn attach_log_wire(&mut self, wire: LogWire) {
        self.log_transport = Some(wire);
    }

    /// The log transport's identity rule: the caller's session file path
    /// names the session, so the header (row one at the first flush) re-keys
    /// to the path's id.
    ///
    /// # Errors
    ///
    /// A store not served by the log transport, or a path with no id name.
    pub(crate) fn rekey_log_session_id(&mut self, session_path: &std::path::Path) -> Result<()> {
        anyhow::ensure!(
            self.log_transport.is_some(),
            "log re-key applies to log-transported sessions"
        );
        let Some(session_id) = crate::session_transport::session_id_of(session_path) else {
            anyhow::bail!(
                "session file path has no id name: {}",
                session_path.display()
            );
        };
        self.header.id = session_id;
        Ok(())
    }

    /// THE funnel for one built entry's durable row on the log transport.
    ///
    /// The entry joins the fold only after the log committed its row, so the
    /// fold and the log never disagree: a refused write is not indexed, and
    /// a reload never finds it as durable.
    ///
    /// # Errors
    ///
    /// The log's boundary policy (a foreign `Busy` holder, a refused row, or
    /// an undecided write), plus the row's encoding.
    pub(crate) fn persist_built_log_entry(&mut self, entry: SessionEntry) -> Result<String> {
        // An undecided caller intent retains its full original envelope even if
        // the public retry rebuilt its ID and timestamp.
        let entry = self
            .pending_log_entry(&entry.type_, &entry.fields)
            .unwrap_or(entry);
        let old_leaf = self.leaf_id.clone();
        self.observe_log_tail()?;
        let id = entry.id.clone();
        if self.by_id.contains_key(&id) {
            return Ok(id);
        }
        let mut entry = entry;
        if entry.parent_id == old_leaf {
            entry.parent_id.clone_from(&self.leaf_id);
        }
        let row = serde_json::to_value(&entry).context("encode the session row")?;
        self.flush_log_tail_from(0, std::slice::from_ref(&row))?;
        self.observe_log_tail()?;
        Ok(id)
    }

    pub(super) fn pending_log_entry(
        &self,
        entry_type: &str,
        fields: &Value,
    ) -> Option<SessionEntry> {
        let Some(row) = self.log_transport.as_ref().and_then(LogWire::pending_row) else {
            return None;
        };
        let Ok(entry) = serde_json::from_value::<SessionEntry>(row) else {
            return None;
        };
        (entry.type_ == entry_type && entry.fields == *fields).then_some(entry)
    }

    /// Extend this serving fold from validated durable rows, without sharing a
    /// read cursor with clones or indexing an unresolved durability outcome.
    pub(crate) fn observe_log_tail(&mut self) -> Result<()> {
        let Some(wire) = self.log_transport.clone() else {
            return Ok(());
        };
        let mut position = self
            .log_progress
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let rows = wire.observe(&self.header.id, &mut position.cursor)?;
        if rows.is_empty() {
            return Ok(());
        }
        // Nonfallible creation builders may already have an unflushed tail.
        // Foreign durable rows precede that tail, never suppress its flush.
        let local_tail = self
            .entries
            .split_off(position.observed_entries.min(self.entries.len()));
        for entry in &local_tail {
            self.by_id.remove(&entry.id);
        }
        for row in rows {
            match row.body.get("type").and_then(Value::as_str) {
                Some("session") => {
                    let pa_types::session::FileEntry::Header { header } =
                        serde_json::from_value(row.body).context("invalid session header")?
                    else {
                        anyhow::bail!("invalid session header")
                    };
                    anyhow::ensure!(
                        header.id == self.header.id,
                        "session header identity changed; reopen the session"
                    );
                    self.header = header;
                }
                Some("branch_selection") => {
                    self.leaf_id = match row.body.get("selectedLeaf") {
                        Some(Value::Null) => None,
                        Some(Value::String(id)) if self.by_id.contains_key(id) => Some(id.clone()),
                        _ => anyhow::bail!("branch selection names an absent entry"),
                    };
                }
                _ => match serde_json::from_value::<SessionEntry>(row.body) {
                    Ok(entry) => {
                        if self.by_id.contains_key(&entry.id) {
                            self.leaf_id = Some(entry.id);
                        } else {
                            self.push_index(entry);
                        }
                    }
                    Err(_) => self.skipped_lines += 1,
                },
            }
        }
        position.flushed_entries = self.entries.len();
        position.observed_entries = self.entries.len();
        for entry in local_tail {
            if !self.by_id.contains_key(&entry.id) {
                self.push_index(entry);
            }
        }
        self.log_progress = LogProgress(std::sync::Mutex::new(position));
        super::fold_child_usage_attributions(&mut self.entries);
        self.hydrate_anthropic_warning_flag();
        Ok(())
    }

    /// Selection is durable state but not an entry in the conversation tree.
    pub(crate) fn persist_branch_selection(&self, leaf: Option<&str>) -> Result<()> {
        self.flush_unmaterialized_log_tail(0)?;
        let wire = self
            .log_transport
            .as_ref()
            .context("session has no log writer")?;
        let position = self
            .log_progress
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        wire.flush_selection(
            &self.header.id,
            &super::session_header_line(&self.header),
            leaf,
            &position.cursor,
        )
        .map_err(anyhow::Error::from)
    }

    /// Flush every entry row the log does not hold yet, or (for
    /// `persist_appended`) every row from the caller's start, whichever is
    /// later. The flush's rows join the fold's index through the same paths
    /// that put them in memory, so this only moves durability.
    pub(crate) fn flush_unmaterialized_log_tail(&self, requested_start: usize) -> Result<()> {
        self.flush_log_tail_from(requested_start, &[])
    }

    fn flush_log_tail_from(&self, requested_start: usize, extra_row: &[Value]) -> Result<()> {
        let Some(wire) = self.log_transport.as_ref() else {
            anyhow::bail!("not a log-transported session");
        };
        let session_id = self.header.id.clone();
        anyhow::ensure!(
            !session_id.is_empty(),
            "log-transported flush needs a session id"
        );
        let header_row = super::session_header_line(&self.header);
        let mut position = self
            .log_progress
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = position
            .flushed_entries
            .max(requested_start)
            .min(self.entries.len());
        let mut rows: Vec<Value> = Vec::with_capacity(extra_row.len() + self.entries.len() - start);
        rows.extend(
            self.entries[start..]
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?,
        );
        rows.extend(extra_row.iter().cloned());
        let cursor = position.cursor.clone();
        wire.flush(
            &session_id,
            &header_row,
            &rows,
            &mut position.flushed_entries,
            &cursor,
        )
        .map_err(|error| anyhow::Error::from(error).context("the session log rejected a row"))
    }
}
