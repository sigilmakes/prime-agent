//! Durable manager writes through the session-log core only.
use super::{FileEntry, SessionManager, SessionPersistListener};
use crate::session_log::{Outcome, ReadCursor, SessionLog};
use std::io;
use std::time::Instant;

pub(super) struct StandaloneLog {
    log: SessionLog,
    cursor: ReadCursor,
    seq: u64,
    pending: Option<(u64, FileEntry)>,
}

impl SessionManager {
    /// Route every new row through the host's durable log.
    pub fn set_entry_writer(&mut self, writer: std::sync::Arc<dyn super::SessionEntryWriter>) {
        self.entry_writer = Some(writer);
    }

    pub(super) fn refresh_has_assistant_entry(&mut self, entries: &[FileEntry]) {
        self.has_assistant_entry = entries.iter().any(|entry| {
            matches!(
                entry,
                FileEntry::Message {
                    message: super::AgentMessage::Assistant(_),
                    ..
                }
            )
        });
    }

    pub(super) fn build_index(&mut self) {
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        for (index, entry) in self.file_entries.iter().enumerate() {
            if let Some(id) = entry.id() {
                self.by_id.insert(id.to_owned(), index);
                self.leaf_id = Some(id.to_owned());
            }
            if let FileEntry::Label { payload, .. } = entry {
                if let Some(label) = &payload.label {
                    self.labels_by_id
                        .insert(payload.target_id.clone(), label.clone());
                    self.label_timestamps_by_id
                        .insert(payload.target_id.clone(), entry.timestamp().to_owned());
                } else {
                    self.labels_by_id.remove(&payload.target_id);
                    self.label_timestamps_by_id.remove(&payload.target_id);
                }
            }
        }
    }

    fn notify_persist_listeners(&self) {
        if let Some(path) = &self.session_file {
            for listener in &self.persist_listeners {
                listener(path);
            }
        }
    }

    pub fn on_persist(&mut self, listener: SessionPersistListener) {
        self.persist_listeners.push(listener);
    }

    #[must_use]
    pub fn is_persisted(&self) -> bool {
        self.persist || self.entry_writer.is_some()
    }

    /// Bind the independent cursor captured with the authoritative startup seed.
    pub fn set_committed_cursor(&mut self, cursor: ReadCursor) {
        self.read_cursor = cursor;
    }

    /// Refresh committed history before inference or choosing an append parent.
    ///
    /// # Errors
    /// Returns log contention, replacement, checksum, ordering or I/O failures.
    pub fn refresh_committed_entries(&mut self) -> io::Result<()> {
        let entries = if let Some(writer) = &self.entry_writer {
            writer.read_tail(&mut self.read_cursor)?
        } else if self.persist {
            self.flush_now()?;
            let state = self.standalone_log.as_mut().expect("flush initialized log");
            let rows = state
                .log
                .read_tail(&mut state.cursor, Instant::now())
                .map_err(io::Error::other)?;
            if let Some(row) = rows.last() {
                state.seq = row.seq;
            }
            rows.into_iter()
                .map(|row| serde_json::from_value(row.body).map_err(io::Error::from))
                .collect::<io::Result<Vec<_>>>()?
        } else {
            return Ok(());
        };
        let mut changed = false;
        for entry in entries {
            if matches!(entry, FileEntry::Header { .. })
                || entry.id().is_some_and(|id| self.by_id.contains_key(id))
            {
                continue;
            }
            if let Some(window) = &mut self.window {
                window.append_entry(entry.clone());
            }
            self.file_entries.push(entry);
            changed = true;
        }
        if changed {
            super::super::apply_child_usage_attributions(&mut self.file_entries);
            self.build_index();
        }
        Ok(())
    }

    /// Persist the initial history without rewriting any existing row.
    ///
    /// # Errors
    /// Returns log contention, damage, mismatch or I/O errors; never repairs.
    pub fn flush_now(&mut self) -> io::Result<()> {
        if !self.persist || self.entry_writer.is_some() || self.standalone_log.is_some() {
            return Ok(());
        }
        let path = self.session_file.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "persisted manager has no session path",
            )
        })?;
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "session path has no parent")
        })?;
        let id = path.file_stem().and_then(|id| id.to_str()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "session path has no UTF-8 id")
        })?;
        std::fs::create_dir_all(dir)?;
        let mut log = SessionLog::open(dir, id, Instant::now()).map_err(io::Error::other)?;
        let mut cursor = ReadCursor::default();
        let rows = log
            .read_tail(&mut cursor, Instant::now())
            .map_err(io::Error::other)?;
        let mut existing = rows
            .iter()
            .map(|row| serde_json::from_value(row.body.clone()).map_err(io::Error::from))
            .collect::<io::Result<Vec<FileEntry>>>()?;
        super::super::apply_child_usage_attributions(&mut existing);
        if self.window.is_some() {
            if existing.first().map(serde_json::to_value).transpose()?
                != self
                    .file_entries
                    .first()
                    .map(serde_json::to_value)
                    .transpose()?
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session window header differs from durable log",
                ));
            }
            self.standalone_log = Some(StandaloneLog {
                seq: rows.last().map_or(0, |row| row.seq),
                log,
                cursor,
                pending: None,
            });
            return Ok(());
        }
        let shared = existing.len().min(self.file_entries.len());
        if serde_json::to_value(&existing[..shared])?
            != serde_json::to_value(&self.file_entries[..shared])?
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "existing session log differs from manager history",
            ));
        }
        if existing.len() > self.file_entries.len() {
            self.file_entries
                .extend(existing.into_iter().skip(self.file_entries.len()));
            self.build_index();
        }
        let mut state = StandaloneLog {
            seq: rows.last().map_or(0, |row| row.seq),
            log,
            cursor,
            pending: None,
        };
        for entry in self.file_entries.iter().skip(rows.len()) {
            let seq = state.seq + 1;
            let outcome = state
                .log
                .append_at_checked(seq, entry, &state.cursor, Instant::now())
                .map_err(io::Error::other)?;
            committed(outcome)?;
            let rows = state
                .log
                .read_tail(&mut state.cursor, Instant::now())
                .map_err(io::Error::other)?;
            state.seq = rows.last().map_or(seq, |row| row.seq);
        }
        self.standalone_log = Some(state);
        self.flushed = true;
        self.notify_persist_listeners();
        Ok(())
    }

    pub(super) fn persist_entry(&mut self, entry: &mut FileEntry) -> io::Result<()> {
        if let Some(writer) = &self.entry_writer {
            return writer.append(entry);
        }
        if !self.persist {
            return Ok(());
        }
        self.flush_now()?;
        let mut state = self
            .standalone_log
            .take()
            .expect("flush initialized standalone log");
        let result = (|| {
            let rows = state
                .log
                .read_tail(&mut state.cursor, Instant::now())
                .map_err(io::Error::other)?;
            let mut pending_found = false;
            let mut changed = false;
            for row in rows {
                state.seq = row.seq;
                if state
                    .pending
                    .as_ref()
                    .is_some_and(|(seq, _)| *seq == row.seq)
                {
                    let pending = &state.pending.as_ref().expect("pending exists").1;
                    if serde_json::to_value(pending)? != row.body {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "pending session row was replaced",
                        ));
                    }
                    pending_found = true;
                } else {
                    let foreign: FileEntry = serde_json::from_value(row.body)?;
                    if let Some(window) = &mut self.window {
                        window.append_entry(foreign.clone());
                    }
                    self.file_entries.push(foreign);
                    changed = true;
                }
            }
            if changed {
                self.build_index();
            }
            if let Some((_, pending)) = &state.pending {
                let payload = |row: &FileEntry| -> io::Result<serde_json::Value> {
                    let mut value = serde_json::to_value(row)?;
                    if let Some(object) = value.as_object_mut() {
                        for key in ["id", "parentId", "timestamp"] {
                            object.remove(key);
                        }
                    }
                    Ok(value)
                };
                if payload(pending)? != payload(entry)? {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "a different session append is pending; retry it first",
                    ));
                }
                *entry = pending.clone();
            } else {
                state.pending = Some((state.seq + 1, entry.clone()));
            }
            if !pending_found {
                let seq = state.pending.as_ref().expect("pending exists").0;
                let outcome = state
                    .log
                    .append_at_checked(seq, entry, &state.cursor, Instant::now())
                    .map_err(io::Error::other)?;
                committed(outcome)?;
                let rows = state
                    .log
                    .read_tail(&mut state.cursor, Instant::now())
                    .map_err(io::Error::other)?;
                state.seq = rows.last().map_or(seq, |row| row.seq);
            }
            state.pending = None;
            Ok(())
        })();
        self.standalone_log = Some(state);
        if result.is_ok() {
            if let Some(path) = &self.session_file {
                super::super::window_cache::committed(path, entry, self.append_ownership);
            }
            self.notify_persist_listeners();
        }
        result
    }
}

fn committed(outcome: Outcome) -> io::Result<()> {
    match outcome {
        Outcome::Committed { .. } => Ok(()),
        Outcome::Unknown { recover } => {
            match recover.recover(Instant::now()).map_err(io::Error::other)? {
                Outcome::Committed { .. } => Ok(()),
                other => Err(io::Error::other(format!(
                    "session append recovery: {other:?}"
                ))),
            }
        }
        Outcome::Busy { .. } => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "session log is busy",
        )),
        Outcome::NotWritten { reason } => Err(io::Error::other(format!(
            "session append not written: {reason:?}"
        ))),
    }
}
