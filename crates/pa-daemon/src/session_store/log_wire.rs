//! The log-transported half of `SessionFile`: the write arm the canonical
//! paths bypass. Everything here serves the port's minimum end-to-end path
//! against the session-log core; the switch lives in
//! `crate::session_transport`.

use anyhow::{Context, Result};
use serde_json::Value;

use super::SessionEntry;
use super::SessionFile;
use crate::session_transport::LogWire;

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
        let row = serde_json::to_value(&entry).context("encode the session row")?;
        self.flush_log_tail_from(0, std::slice::from_ref(&row))?;
        let id = entry.id.clone();
        self.push_index(entry);
        Ok(id)
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
        let start = wire
            .flushed_entries()
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
        wire.flush(&session_id, &header_row, &rows)
            .map_err(|error| anyhow::Error::from(error).context("the session log rejected a row"))
    }
}
