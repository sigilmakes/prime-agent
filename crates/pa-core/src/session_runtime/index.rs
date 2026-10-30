//! The index the runtime keeps over the log: ids, parents, leaves, stamps.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::{envelope, Error, SELECTION};

/// One indexed row: the fields the runtime must know, plus the body.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The row's position in the log.
    pub seq: u64,
    /// The writer's stamp from the row envelope, as the log recorded it.
    pub ts: String,
    /// The row's own id, unique within the session.
    pub id: String,
    /// The id of the row this one follows, if any.
    pub parent: Option<String>,
    /// The row's ISO-8601 stamp, when it carries a string one.
    pub timestamp: Option<String>,
    /// The row body, opaque to the runtime.
    pub body: Value,
}

/// What indexing one row did to the tree.
pub(crate) enum Applied {
    /// A branch node; `parent` is where it hangs.
    Tree { id: String, parent: Option<String> },
    /// A selection marker naming the branch leaf to select.
    Selection { leaf: String },
    /// A row the index already holds.
    Already,
}

/// Rows by seq, by id, and the parent/child edges of the session tree.
///
/// Selection markers are indexed (they are rows with ids and seqs) but they are
/// not tree nodes: they neither become leaves nor give their target children.
#[derive(Debug, Default)]
pub(crate) struct Index {
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
    children: HashMap<String, usize>,
    markers: HashSet<usize>,
    last_seq: u64,
}

impl Index {
    /// Index one row, or report that its seq is already held.
    pub(crate) fn insert(&mut self, seq: u64, ts: String, body: Value) -> Result<Applied, Error> {
        if self.by_seq(seq).is_some() {
            return Ok(Applied::Already);
        }
        let envelope = envelope(&body)?;
        if self.by_id.contains_key(&envelope.id) {
            return Err(Error::DuplicateId { id: envelope.id });
        }
        let entry = Entry {
            seq,
            ts,
            id: envelope.id,
            parent: envelope.parent,
            timestamp: envelope.timestamp,
            body,
        };
        let index = self.entries.len();
        let applied = if entry.body.get("type").and_then(Value::as_str) == Some(SELECTION) {
            let leaf = entry
                .body
                .get("leafId")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Malformed {
                    field: "leafId",
                    detail: entry.body.to_string(),
                })?
                .to_owned();
            self.markers.insert(index);
            Applied::Selection { leaf }
        } else {
            if let Some(parent) = &entry.parent {
                *self.children.entry(parent.clone()).or_insert(0) += 1;
            }
            Applied::Tree {
                id: entry.id.clone(),
                parent: entry.parent.clone(),
            }
        };
        self.by_id.insert(entry.id.clone(), index);
        self.last_seq = self.last_seq.max(seq);
        self.entries.push(entry);
        Ok(applied)
    }

    pub(crate) fn rows(&self) -> &[Entry] {
        &self.entries
    }

    pub(crate) fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Entry> {
        self.by_id.get(id).map(|&index| &self.entries[index])
    }

    pub(crate) fn by_seq(&self, seq: u64) -> Option<&Entry> {
        self.entries
            .binary_search_by_key(&seq, |entry| entry.seq)
            .ok()
            .map(|index| &self.entries[index])
    }

    /// A branch may end here: a tree node with no children.
    pub(crate) fn is_leaf(&self, id: &str) -> bool {
        let Some(&index) = self.by_id.get(id) else {
            return false;
        };
        !self.markers.contains(&index) && !self.children.contains_key(id)
    }

    pub(crate) fn leaves(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| self.is_leaf(&entry.id))
            .map(|entry| entry.id.as_str())
            .collect()
    }

    /// The newest leaf, for a log whose first rows start no branch.
    pub(crate) fn tip(&self) -> Option<String> {
        self.entries
            .iter()
            .rev()
            .find(|entry| self.is_leaf(&entry.id))
            .map(|entry| entry.id.clone())
    }

    /// The chain from the root to `leaf`.
    pub(crate) fn branch(&self, leaf: &str) -> Vec<&Entry> {
        let mut chain = Vec::new();
        let mut cursor = self.get(leaf);
        while let Some(entry) = cursor {
            // A parent chain that loops is cut rather than followed forever.
            if chain.len() > self.entries.len() {
                break;
            }
            chain.push(entry);
            cursor = entry.parent.as_deref().and_then(|parent| self.get(parent));
        }
        chain.reverse();
        chain
    }
}
