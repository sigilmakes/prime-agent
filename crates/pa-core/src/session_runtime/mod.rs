//! The session runtime: one log, one in-memory index, one selected branch.
//!
//! The log core stores rows and knows nothing about sessions. This runtime
//! replays those rows into the index a session needs -- id, parent id, leaves,
//! selection, timestamps -- and projects the selected branch into provider
//! messages.
//!
//! Rows are opaque except for the fields the session layer writes: `id`,
//! `parentId`, `timestamp`, and for messages `type: "message"` with
//! `message.role` and `message.content`.
//!
//! No epoch, no acknowledgement, no lease, no retry policy. The index changes
//! when the log says [`Outcome::Committed`], and on [`Outcome::Unknown`] the
//! runtime believes the log, never the intent. A branch selection is an
//! ordinary [`SELECTION`] row, so replay restores it.

mod index;
mod projection;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{json, Value};
use uuid::Uuid;

// The log core. It ships as the `pa-session-log` crate; while its module path
// is still `pa_core::session_log`, this alias is the one line that moves.
pub(crate) use crate::session_log as log_core;
use log_core::{Outcome, RepairReport, Row, SessionLog};

use index::{Applied, Index};

pub use index::Entry;
pub use projection::ProviderMessage;

/// The row `type` a branch selection is written as.
pub(crate) const SELECTION: &str = "branch_selection";

/// Why the runtime could not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The log core failed or refused the operation.
    #[error("session log: {0}")]
    Log(#[from] log_core::Error),
    /// The row's id is already in the session.
    #[error("row id {id:?} is already in the session")]
    DuplicateId { id: String },
    /// A row is missing a field the index needs.
    #[error("row field {field} is not usable: {detail}")]
    Malformed { field: &'static str, detail: String },
    /// A branch must end at a leaf.
    #[error("row {id:?} has children and is not a branch leaf")]
    NotALeaf { id: String },
    /// No row has that id.
    #[error("no row with id {id:?}")]
    NoSuchRow { id: String },
    /// The log said the row committed but does not hold it.
    #[error("the log reported seq {seq} committed but does not hold it")]
    MissingRow { seq: u64 },
    /// The core's verdict decided nothing, so there is nothing to apply.
    #[error("the log could not decide the undecided append")]
    Undecided,
}

/// One opened session: the replayed runtime and the repair `open` performed.
pub struct Opened {
    /// The replayed runtime.
    pub runtime: SessionRuntime,
    /// The repair applied to a torn or corrupt tail, when there was one.
    pub repair: Option<RepairReport>,
}

/// A session's in-memory view over its log.
pub struct SessionRuntime {
    dir: PathBuf,
    id: String,
    log: SessionLog,
    index: Index,
    selected: Option<String>,
    pending: Option<Pending>,
}

/// An append whose durability the log has not decided.
///
/// The seq survives a failed `recover()`, so the retry is pinned to it and the
/// core answers from the log's bytes: a write that landed is reported instead
/// of repeated under the same id.
struct Pending {
    seq: u64,
    id: String,
}

impl SessionRuntime {
    /// Open (creating when absent) session `id` in `dir` and replay its log.
    ///
    /// A torn or corrupt tail is repaired before the replay and returned, so
    /// the caller can report it.
    ///
    /// # Errors
    ///
    /// [`Error::Log`] when the lock is held, the file cannot be read, or the
    /// tail could not be repaired.
    pub fn open(dir: &Path, id: &str, deadline: Instant) -> Result<Opened, Error> {
        match SessionLog::open(dir, id, deadline) {
            Ok(log) => Self::replay(dir, id, log, None),
            Err(log_core::Error::Damaged { .. } | log_core::Error::Corrupt { .. }) => {
                let repair = SessionLog::repair(dir, id, deadline)?;
                tracing::warn!(
                    session = id,
                    last_good_seq = repair.last_good_seq,
                    truncated_bytes = repair.truncated_bytes,
                    "repaired a torn session log tail on open"
                );
                let log = SessionLog::open(dir, id, deadline)?;
                Self::replay(dir, id, log, Some(repair))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn replay(
        dir: &Path,
        id: &str,
        log: SessionLog,
        repair: Option<RepairReport>,
    ) -> Result<Opened, Error> {
        let mut runtime = Self {
            dir: dir.to_path_buf(),
            id: id.to_owned(),
            log,
            index: Index::default(),
            selected: None,
            pending: None,
        };
        for row in runtime.log.iter_since(0)? {
            let row = row?;
            runtime.absorb(row.seq, row.ts, row.body)?;
        }
        if runtime.selected.is_none() {
            runtime.selected = runtime.index.tip();
        }
        Ok(Opened { runtime, repair })
    }

    /// Append `row` at the log's next seq.
    ///
    /// A row the session already holds is a retry and resolves to its seq, even
    /// after an undecided attempt. Otherwise the next seq is pinned with
    /// `append_at`; a seq another writer took costs one tail read and one
    /// retry, never a duplicate id.
    ///
    /// A commit is read back from the file, so rows other writers committed are
    /// indexed too. On [`Outcome::Unknown`] the core decides; if that decision
    /// fails, the undecided seq stays pending for the retry.
    ///
    /// # Errors
    ///
    /// [`Error::DuplicateId`] for a different row with that id,
    /// [`Error::Malformed`] for an unusable row, [`Error::Log`] for log
    /// failures, and the error of a `recover()` that could not run.
    pub fn append(&mut self, row: &Value, deadline: Instant) -> Result<Outcome, Error> {
        let envelope = envelope(row)?;
        if let Some(outcome) = self.retried(&envelope.id, row)? {
            return Ok(outcome);
        }
        if let Some(pending) = &self.pending {
            if pending.id == envelope.id {
                // Finish the undecided write at its own seq before anything else.
                let seq = pending.seq;
                let outcome = self.log.append_at(seq, row, deadline);
                return self.commit(outcome, row, deadline);
            }
        }
        let mut outcome = self.log.append_at(self.index.last_seq() + 1, row, deadline);
        if is_seq_conflict(&outcome) {
            // Another writer took the seq we pinned: read its rows, then retry.
            self.sync_tail(deadline)?;
            if let Some(outcome) = self.retried(&envelope.id, row)? {
                return Ok(outcome);
            }
            outcome = self.log.append_at(self.index.last_seq() + 1, row, deadline);
        }
        self.commit(outcome, row, deadline)
    }

    /// The outcome a retry of `row` resolves to when the session already holds
    /// its id, or `None` when the id is free.
    fn retried(&mut self, id: &str, row: &Value) -> Result<Option<Outcome>, Error> {
        let Some(held) = self.index.get(id) else {
            return Ok(None);
        };
        if held.body != *row {
            return Err(Error::DuplicateId { id: id.to_owned() });
        }
        let (seq, ts) = (held.seq, held.ts.clone());
        self.resolve_pending(id);
        Ok(Some(Outcome::Committed { seq, ts }))
    }

    /// Append `row` at `seq`, for a caller retrying an undecided write.
    ///
    /// The core answers from the log when `seq` already holds the same body, so
    /// the retry is idempotent without an acknowledgement.
    ///
    /// # Errors
    ///
    /// [`Error::DuplicateId`] when the id sits at a different seq,
    /// [`Error::Malformed`], and [`Error::Log`].
    pub fn append_at(
        &mut self,
        seq: u64,
        row: &Value,
        deadline: Instant,
    ) -> Result<Outcome, Error> {
        let envelope = envelope(row)?;
        if self
            .index
            .get(&envelope.id)
            .is_some_and(|held| held.seq != seq)
        {
            return Err(Error::DuplicateId { id: envelope.id });
        }
        let outcome = self.log.append_at(seq, row, deadline);
        self.commit(outcome, row, deadline)
    }

    /// Select the branch that ends at `leaf`, by appending a selection row.
    ///
    /// The switch takes effect when the log commits it, so it survives a
    /// restart the way every other row does.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchRow`], [`Error::NotALeaf`], and [`Error::Log`].
    pub fn select(&mut self, leaf: &str, deadline: Instant) -> Result<Outcome, Error> {
        if self.index.get(leaf).is_none() {
            return Err(Error::NoSuchRow {
                id: leaf.to_owned(),
            });
        }
        if !self.index.is_leaf(leaf) {
            return Err(Error::NotALeaf {
                id: leaf.to_owned(),
            });
        }
        let marker = format!("selection-{}", Uuid::new_v4());
        let row = json!({
            "type": SELECTION,
            "id": marker,
            "parentId": leaf,
            "leafId": leaf,
        });
        let mut outcome = self
            .log
            .append_at(self.index.last_seq() + 1, &row, deadline);
        if is_seq_conflict(&outcome) {
            self.sync_tail(deadline)?;
            outcome = self
                .log
                .append_at(self.index.last_seq() + 1, &row, deadline);
        }
        self.commit(outcome, &row, deadline)
    }

    /// Committed rows with `seq > since`, read from the log itself.
    ///
    /// # Errors
    ///
    /// [`Error::Log`] for I/O failures.
    pub fn read_since(&self, since: u64) -> Result<Vec<Row>, Error> {
        Ok(self.log.read_since(since)?)
    }

    /// The id of the row the selected branch ends at, when the session has one.
    #[must_use]
    pub fn leaf(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Every row id a branch could end at, in log order.
    #[must_use]
    pub fn leaves(&self) -> Vec<&str> {
        self.index.leaves()
    }

    /// The selected branch, root first.
    #[must_use]
    pub fn branch(&self) -> Vec<&Entry> {
        self.leaf()
            .map_or_else(Vec::new, |leaf| self.index.branch(leaf))
    }

    /// The provider message list for the selected branch, root first.
    ///
    /// Rebuilt on every call: nothing is cached, so a branch switch or an
    /// append is visible immediately.
    #[must_use]
    pub fn projection(&self) -> Vec<ProviderMessage> {
        projection::messages(&self.branch())
    }

    /// Every indexed row, in log order.
    #[must_use]
    pub fn rows(&self) -> &[Entry] {
        self.index.rows()
    }

    /// The indexed row with `id`.
    #[must_use]
    pub fn row(&self, id: &str) -> Option<&Entry> {
        self.index.get(id)
    }

    /// The seq of an append whose durability is still undecided, if any.
    ///
    /// A caller can retry the same row with [`append_at`](Self::append_at) at
    /// this seq; the core answers from the log, so no second row can appear.
    #[must_use]
    pub fn pending_seq(&self) -> Option<u64> {
        self.pending.as_ref().map(|pending| pending.seq)
    }

    /// The seq of the newest row the index holds; zero for an empty session.
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.index.last_seq()
    }

    /// Apply a committed row, or reconcile what the log decided.
    ///
    /// Not applied from the intent: the tail is read first, so rows this
    /// runtime did not write are indexed too and the row at `seq` is the file's.
    fn commit(
        &mut self,
        outcome: Outcome,
        intended: &Value,
        deadline: Instant,
    ) -> Result<Outcome, Error> {
        let id = envelope(intended)?.id;
        match outcome {
            Outcome::Committed { seq, ts } => {
                self.resolve_pending(&id);
                self.reconcile(Outcome::Committed { seq, ts }, deadline)
            }
            Outcome::Unknown { recover } => {
                self.pending = Some(Pending {
                    seq: recover.seq(),
                    id,
                });
                // A failed decision propagates with the seq still pending, so a
                // retry resolves it against the log.
                let decided = recover.recover(deadline)?;
                let resolved = self.reconcile(decided, deadline)?;
                self.pending = None;
                Ok(resolved)
            }
            other @ (Outcome::NotWritten { .. } | Outcome::Busy { .. }) => {
                self.resolve_pending(&id);
                Ok(other)
            }
        }
    }

    fn resolve_pending(&mut self, id: &str) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == id)
        {
            self.pending = None;
        }
    }

    /// Turn the core's verdict on an undecided append into an indexed outcome.
    ///
    /// A `Committed` verdict is checked against the log before anything is
    /// applied: the index learns the row from the file, not from the intent.
    pub(crate) fn reconcile(
        &mut self,
        decided: Outcome,
        deadline: Instant,
    ) -> Result<Outcome, Error> {
        match decided {
            Outcome::Committed { seq, .. } => {
                self.sync_tail(deadline)?;
                let entry = self.index.by_seq(seq).ok_or(Error::MissingRow { seq })?;
                Ok(Outcome::Committed {
                    seq,
                    ts: entry.ts.clone(),
                })
            }
            Outcome::NotWritten { reason } => Ok(Outcome::NotWritten { reason }),
            Outcome::Busy { until, holder } => Ok(Outcome::Busy { until, holder }),
            Outcome::Unknown { .. } => Err(Error::Undecided),
        }
    }

    /// Read what the log holds beyond the index's newest row.
    ///
    /// A complete but invalid line is damage, not a verdict: the tail is
    /// repaired and reading resumes from the index's newest row.
    pub(crate) fn sync_tail(&mut self, deadline: Instant) -> Result<(), Error> {
        loop {
            let newest = self.index.last_seq();
            let rows = self.log.iter_since(newest)?;
            let mut corrupt = false;
            for row in rows {
                match row {
                    Ok(row) => self.absorb(row.seq, row.ts, row.body)?,
                    Err(log_core::Error::Corrupt { after_seq }) => {
                        tracing::warn!(
                            session = self.id,
                            after_seq,
                            "repaired a corrupt session log line"
                        );
                        corrupt = true;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            if !corrupt {
                return Ok(());
            }
            SessionLog::repair(&self.dir, &self.id, deadline)?;
            self.log = SessionLog::open(&self.dir, &self.id, deadline)?;
        }
    }

    /// Index one row and follow the branch selection it implies.
    fn absorb(&mut self, seq: u64, ts: String, body: Value) -> Result<(), Error> {
        match self.index.insert(seq, ts, body)? {
            Applied::Tree { id, parent } => {
                let extends_selection = match &self.selected {
                    Some(selected) => parent.as_deref() == Some(selected.as_str()),
                    None => parent.is_none(),
                };
                if extends_selection {
                    self.selected = Some(id);
                }
            }
            Applied::Selection { leaf } => {
                if self.index.get(&leaf).is_some() {
                    self.selected = Some(leaf);
                }
            }
            Applied::Already => {}
        }
        Ok(())
    }
}

/// Whether the log refused the pinned seq because another row names it.
fn is_seq_conflict(outcome: &Outcome) -> bool {
    matches!(
        outcome,
        Outcome::NotWritten {
            reason: log_core::NotWritten::SeqConflict { .. },
        }
    )
}

/// The fields of a row the index reads.
pub(crate) struct Envelope {
    pub(crate) id: String,
    pub(crate) parent: Option<String>,
    pub(crate) timestamp: Option<String>,
}

/// Read the indexed fields of `body`.
pub(crate) fn envelope(body: &Value) -> Result<Envelope, Error> {
    let unusable = |field| Error::Malformed {
        field,
        detail: body.to_string(),
    };
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| unusable("id"))?
        .to_owned();
    let parent = match body.get("parentId") {
        None | Some(Value::Null) => None,
        Some(Value::String(parent)) => Some(parent.clone()),
        Some(_) => return Err(unusable("parentId")),
    };
    // Stamps are informational: a row that carries something else is indexed
    // without one instead of making the session unopenable.
    let timestamp = body
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Envelope {
        id,
        parent,
        timestamp,
    })
}
