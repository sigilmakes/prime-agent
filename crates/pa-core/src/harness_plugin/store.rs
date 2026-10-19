//! Transactional, raw-JSON persistence owned by the optional harness plugin.
//!
//! A single atomic document contains records, tombstones, history and migration
//! receipts. Readers never turn a corrupt or unreadable document into empty state.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A record's identity is independent of the fields in its raw JSON payload.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordIdentity {
    pub scope: String,
    pub kind: String,
    pub id: String,
}

impl RecordIdentity {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            !self.scope.is_empty() && !self.kind.is_empty() && !self.id.is_empty(),
            "scope, kind and id must not be empty"
        );
        Ok(())
    }
}

/// An explicit tombstone avoids conflating a stored JSON null with deletion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", deny_unknown_fields)]
pub enum RecordValue {
    Present(Value),
    Deleted,
}

/// A live record with its native compare-and-swap revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub identity: RecordIdentity,
    pub value: Value,
    pub revision: u64,
}

/// Write precondition, checked while holding the native store's exclusive lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedRevision {
    Any,
    Absent,
    Revision(u64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub revision: u64,
    pub previous: RecordValue,
    pub current: RecordValue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    pub identity: RecordIdentity,
    pub revision: u64,
    pub before: RecordValue,
    pub after: RecordValue,
    pub operation: String,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Slot {
    pub identity: RecordIdentity,
    pub value: RecordValue,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Document {
    pub schema: u64,
    pub revision: u64,
    pub records: Vec<Slot>,
    pub history: Vec<HistoryEntry>,
    // Receipts retain original UTF-8 source bytes, raw rows and their identities.
    pub migrations: Vec<Value>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            schema: 1,
            revision: 0,
            records: Vec::new(),
            history: Vec::new(),
            migrations: Vec::new(),
        }
    }
}

impl Document {
    pub(super) fn slot(&self, identity: &RecordIdentity) -> Option<&Slot> {
        self.records.iter().find(|slot| slot.identity == *identity)
    }

    pub(super) fn change(
        &mut self,
        identity: &RecordIdentity,
        value: RecordValue,
        expected: ExpectedRevision,
        operation: &str,
    ) -> Result<Change> {
        identity.validate()?;
        let slot = self.slot(identity);
        let revision = slot.map(|slot| slot.revision);
        let previous = slot.map_or(RecordValue::Deleted, |slot| slot.value.clone());
        let allowed = match expected {
            ExpectedRevision::Any => true,
            ExpectedRevision::Absent => previous == RecordValue::Deleted,
            ExpectedRevision::Revision(expected) => revision == Some(expected),
        };
        ensure!(
            allowed,
            "revision conflict for {identity:?}: current {revision:?}"
        );
        self.revision = self.revision.checked_add(1).context("revision overflow")?;
        self.history.push(HistoryEntry {
            identity: identity.clone(),
            revision: self.revision,
            before: previous.clone(),
            after: value.clone(),
            operation: operation.to_owned(),
            metadata: Value::Null,
        });
        if let Some(slot) = self
            .records
            .iter_mut()
            .find(|slot| slot.identity == *identity)
        {
            slot.value = value.clone();
            slot.revision = self.revision;
        } else {
            self.records.push(Slot {
                identity: identity.clone(),
                value: value.clone(),
                revision: self.revision,
            });
        }
        Ok(Change {
            revision: self.revision,
            previous,
            current: value,
        })
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported native harness store schema");
        let mut seen = BTreeSet::new();
        for slot in &self.records {
            slot.identity.validate()?;
            ensure!(
                seen.insert(&slot.identity),
                "duplicate native harness identity"
            );
            ensure!(
                slot.revision > 0 && slot.revision <= self.revision,
                "invalid record revision"
            );
        }
        let mut revision = 0_u64;
        let mut replay = std::collections::BTreeMap::new();
        for event in &self.history {
            event.identity.validate()?;
            if event.operation == "rollback_legacy" {
                let rollback_id = event
                    .metadata
                    .get("rollbackOf")
                    .and_then(Value::as_str)
                    .context("legacy rollback history is missing rollbackOf")?;
                ensure!(!rollback_id.is_empty(), "invalid legacy rollback result ID");
                ensure!(
                    event.metadata.get("scope").and_then(Value::as_str)
                        == Some(event.identity.scope.as_str()),
                    "legacy rollback history scope mismatch"
                );
                let sources = event
                    .metadata
                    .get("legacySources")
                    .and_then(Value::as_array)
                    .context("legacy rollback history is missing source identities")?;
                ensure!(
                    !sources.is_empty(),
                    "legacy rollback history has no sources"
                );
                for source in sources {
                    let source_id = source
                        .as_str()
                        .context("invalid legacy rollback source identity")?;
                    ensure!(
                        self.migrations.iter().any(|receipt| {
                            receipt.get("source").is_some_and(|source| {
                                source.get("identity").and_then(Value::as_str) == Some(source_id)
                                    && source.get("scope").and_then(Value::as_str)
                                        == Some(event.identity.scope.as_str())
                            })
                        }),
                        "legacy rollback history references unknown source"
                    );
                }
            }
            ensure!(
                Some(event.revision) == revision.checked_add(1) && event.revision <= self.revision,
                "invalid history revision"
            );
            let previous = replay
                .get(&event.identity)
                .map_or(&RecordValue::Deleted, |(value, _)| value);
            ensure!(*previous == event.before, "history before/after mismatch");
            replay.insert(
                event.identity.clone(),
                (event.after.clone(), event.revision),
            );
            revision = event.revision;
        }
        // A truncated document must not silently discard its rollback ledger.
        ensure!(
            revision == self.revision,
            "incomplete native harness history"
        );
        ensure!(
            replay.len() == self.records.len(),
            "incomplete native record collection"
        );
        for slot in &self.records {
            ensure!(
                replay.get(&slot.identity) == Some(&(slot.value.clone(), slot.revision)),
                "record/history mismatch"
            );
        }
        super::migration::validate_receipts(&self.migrations)?;
        Ok(())
    }
}

/// Explicit path only: this backend never discovers or opens live harness paths.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Enumerate live raw records, optionally limited to one explicit scope.
    ///
    /// # Errors
    /// Fails on unreadable, corrupt or unsupported native data.
    pub fn enumerate(&self, scope: Option<&str>) -> Result<Vec<Record>> {
        let document = self.load()?;
        Ok(document
            .records
            .into_iter()
            .filter_map(|slot| {
                if scope.is_some_and(|scope| scope != slot.identity.scope) {
                    return None;
                }
                match slot.value {
                    RecordValue::Present(value) => Some(Record {
                        identity: slot.identity,
                        value,
                        revision: slot.revision,
                    }),
                    RecordValue::Deleted => None,
                }
            })
            .collect())
    }

    /// Read one record. An absent record differs from an explicit JSON null.
    ///
    /// # Errors
    /// Fails on an invalid identity or unreadable/corrupt native data.
    pub fn read(&self, identity: &RecordIdentity) -> Result<Option<Record>> {
        identity.validate()?;
        let document = self.load()?;
        Ok(document.slot(identity).and_then(|slot| match &slot.value {
            RecordValue::Present(value) => Some(Record {
                identity: identity.clone(),
                value: value.clone(),
                revision: slot.revision,
            }),
            RecordValue::Deleted => None,
        }))
    }

    /// Atomically write raw JSON and its rollback event.
    ///
    /// # Errors
    /// Fails on a revision conflict, corrupt data, lock failure or I/O error.
    pub fn put(
        &self,
        identity: &RecordIdentity,
        value: Value,
        expected: ExpectedRevision,
    ) -> Result<Change> {
        self.transact(|document| {
            document.change(identity, RecordValue::Present(value), expected, "put")
        })
    }

    /// Apply a complete refinement in one locked transaction.
    /// A `None` payload means deletion; `Some(Value::Null)` stores JSON null.
    ///
    /// # Errors
    /// Any invalid identity, failed precondition or persistence error aborts all changes.
    pub fn batch(
        &self,
        changes: Vec<(RecordIdentity, Option<Value>, ExpectedRevision)>,
    ) -> Result<Vec<Change>> {
        self.transact(|document| {
            changes
                .into_iter()
                .map(|(identity, value, expected)| {
                    let value = value.map_or(RecordValue::Deleted, RecordValue::Present);
                    document.change(&identity, value, expected, "batch")
                })
                .collect()
        })
    }

    /// Atomically tombstone a record, retaining the full raw rollback history.
    ///
    /// # Errors
    /// Fails on a revision conflict, corrupt data, lock failure or I/O error.
    pub fn delete(&self, identity: &RecordIdentity, expected: ExpectedRevision) -> Result<Change> {
        self.transact(|document| {
            document.change(identity, RecordValue::Deleted, expected, "delete")
        })
    }

    /// Read the native mutation ledger for an identity (including deletions).
    ///
    /// # Errors
    /// Fails on invalid identity or unreadable/corrupt native data.
    pub fn history(&self, identity: &RecordIdentity) -> Result<Vec<HistoryEntry>> {
        identity.validate()?;
        Ok(self
            .load()?
            .history
            .into_iter()
            .filter(|event| event.identity == *identity)
            .collect())
    }

    /// Restore the state before a particular event as a new, CAS-checked event.
    ///
    /// # Errors
    /// Fails if the event is absent, the precondition fails or persistence fails.
    pub fn rollback(
        &self,
        identity: &RecordIdentity,
        history_revision: u64,
        expected: ExpectedRevision,
    ) -> Result<Change> {
        self.transact(|document| {
            let event = document
                .history
                .iter()
                .find(|event| event.identity == *identity && event.revision == history_revision)
                .context("rollback event not found")?;
            let before = event.before.clone();
            document.change(identity, before, expected, "rollback")
        })
    }

    /// Inspect the complete native document, including raw migration receipts.
    ///
    /// # Errors
    /// Fails on unreadable/corrupt data or serialization failure.
    pub fn inspect(&self) -> Result<Value> {
        serde_json::to_value(self.load()?).context("serialize native harness document")
    }

    fn load(&self) -> Result<Document> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Document::default()),
            Err(error) => return Err(error).context("read native harness store"),
        };
        let document: Document =
            serde_json::from_slice(&bytes).context("parse native harness store")?;
        document.validate()?;
        Ok(document)
    }

    pub(super) fn transact<T>(&self, update: impl FnOnce(&mut Document) -> Result<T>) -> Result<T> {
        self.transact_checked(update, || Ok(()))
    }

    pub(super) fn transact_checked<T>(
        &self,
        update: impl FnOnce(&mut Document) -> Result<T>,
        before_commit: impl FnOnce() -> Result<()>,
    ) -> Result<T> {
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(parent)
            .context("create native harness directory")?;
        let name = self
            .path
            .file_name()
            .context("native harness path needs a filename")?;
        let lock_path = parent.join(format!("{}.lock", name.to_string_lossy()));
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options
            .open(&lock_path)
            .context("open native harness lock")?;
        lock.lock().context("lock native harness store")?;
        let mut document = self.load()?;
        let original = serde_json::to_vec(&document)?;
        let result = update(&mut document)?;
        document.validate()?;
        let bytes = serde_json::to_vec(&document)?;
        if bytes == original {
            before_commit()?;
            return Ok(result);
        }
        let temporary = parent.join(format!(
            ".{}.{}.tmp",
            name.to_string_lossy(),
            uuid::Uuid::new_v4()
        ));
        let write_result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary)
                .context("create native harness transaction")?;
            file.write_all(&bytes)
                .context("write native harness transaction")?;
            file.sync_all().context("sync native harness transaction")?;
            before_commit()?;
            fs::rename(&temporary, &self.path).context("commit native harness transaction")?;
            File::open(parent)?
                .sync_all()
                .context("sync native harness directory")?;
            Ok(())
        })();
        if temporary.exists() {
            // Remove only our uncommitted transaction, never source data.
            fs::remove_file(&temporary).context("remove uncommitted native transaction")?;
        }
        write_result?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity(scope: &str, kind: &str) -> RecordIdentity {
        RecordIdentity {
            scope: scope.into(),
            kind: kind.into(),
            id: "same".into(),
        }
    }

    #[test]
    fn public_crud_preserves_raw_values_and_cas_rollback() -> Result<()> {
        let private = tempfile::tempdir()?;
        let store = Store::new(private.path().join("native.json"));
        let key = identity("session:one", "factory");
        let raw = json!({"reference":null,"arguments":{},"future":{"x":[null,1]},"scope":null});
        let created = store.put(&key, raw.clone(), ExpectedRevision::Absent)?;
        assert!(store
            .put(&key, json!({}), ExpectedRevision::Absent)
            .is_err());
        let updated = store.put(
            &key,
            Value::Null,
            ExpectedRevision::Revision(created.revision),
        )?;
        assert_eq!(store.read(&key)?.map(|row| row.value), Some(Value::Null));
        let deleted = store.delete(&key, ExpectedRevision::Revision(updated.revision))?;
        assert_eq!(store.read(&key)?, None);
        store.rollback(
            &key,
            deleted.revision,
            ExpectedRevision::Revision(deleted.revision),
        )?;
        assert_eq!(store.read(&key)?.map(|row| row.value), Some(Value::Null));
        store.rollback(&key, updated.revision, ExpectedRevision::Any)?;
        assert_eq!(store.read(&key)?.map(|row| row.value), Some(raw));
        assert_eq!(store.history(&key)?.len(), 5);
        store.put(
            &identity("session:two", "factory"),
            json!(2),
            ExpectedRevision::Absent,
        )?;
        store.put(
            &identity("session:one", "memory"),
            json!(3),
            ExpectedRevision::Absent,
        )?;
        assert_eq!(store.enumerate(None)?.len(), 3);
        assert_eq!(store.enumerate(Some("session:two"))?.len(), 1);
        Ok(())
    }

    #[test]
    fn corrupt_native_data_never_becomes_empty() -> Result<()> {
        let private = tempfile::tempdir()?;
        let path = private.path().join("native.json");
        let store = Store::new(&path);
        for bytes in [b"{".as_slice(), b"null", b"{}", b"{\"schema\":2}"] {
            fs::write(&path, bytes)?;
            assert!(store.enumerate(None).is_err());
            assert!(store
                .put(&identity("s", "memory"), json!({}), ExpectedRevision::Any)
                .is_err());
            assert_eq!(fs::read(&path)?, bytes);
        }
        Ok(())
    }

    #[test]
    fn batch_aborts_every_change_on_late_cas_conflict() -> Result<()> {
        let private = tempfile::tempdir()?;
        let path = private.path().join("native.json");
        let store = Store::new(&path);
        let first = identity("s", "memory");
        let second = identity("s", "factory");
        store.put(&first, json!({"old":null}), ExpectedRevision::Absent)?;
        let before = fs::read(&path)?;
        assert!(store
            .batch(vec![
                (first.clone(), Some(Value::Null), ExpectedRevision::Any),
                (
                    second.clone(),
                    Some(json!({"future":null})),
                    ExpectedRevision::Revision(99)
                ),
            ])
            .is_err());
        assert_eq!(fs::read(&path)?, before);
        assert_eq!(store.read(&second)?, None);
        store.batch(vec![
            (first.clone(), None, ExpectedRevision::Any),
            (second.clone(), Some(Value::Null), ExpectedRevision::Absent),
        ])?;
        assert_eq!(store.read(&first)?, None);
        assert_eq!(
            store.read(&second)?.map(|record| record.value),
            Some(Value::Null)
        );
        Ok(())
    }

    #[test]
    fn competing_cas_writers_commit_only_once() -> Result<()> {
        let private = tempfile::tempdir()?;
        let store = Store::new(private.path().join("native.json"));
        let key = identity("s", "memory");
        let revision = store
            .put(&key, json!(0), ExpectedRevision::Absent)?
            .revision;
        let threads: Vec<_> = (1..5)
            .map(|n| {
                let store = store.clone();
                let key = key.clone();
                std::thread::spawn(move || {
                    store
                        .put(&key, json!(n), ExpectedRevision::Revision(revision))
                        .is_ok()
                })
            })
            .collect();
        let mut successes = 0;
        for thread in threads {
            if thread.join().expect("writer thread") {
                successes += 1;
            }
        }
        assert_eq!(successes, 1);
        assert_eq!(store.history(&key)?.len(), 2);
        Ok(())
    }
}
