//! Explicit, lossless import of legacy harness record snapshots.
//!
//! Migration is not scope enrollment. Global data enters the named
//! `legacy_global` scope; local data enters only its declared session scope.
//! Original source bytes are never changed. Receipts and imported records commit
//! together, after rechecking every source's SHA-256 under the store lock.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::store::{Document, ExpectedRevision, RecordIdentity, RecordValue, Store};

/// Explicit inputs supplied by the caller, never ambient live-state discovery.
#[derive(Debug, Clone)]
pub struct LegacySource {
    scope: String,
    state_path: Option<PathBuf>,
}

impl LegacySource {
    #[must_use]
    pub fn global(state_path: Option<PathBuf>) -> Self {
        Self {
            scope: "legacy_global".to_owned(),
            state_path,
        }
    }

    /// Bind local state to one durable session.
    ///
    /// # Errors
    /// Rejects an empty session identity.
    pub fn session(session_id: &str, state_path: Option<PathBuf>) -> Result<Self> {
        ensure!(
            !session_id.is_empty(),
            "migration requires an explicit session identity"
        );
        Ok(Self {
            scope: format!("session:{session_id}"),
            state_path,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceSnapshot {
    identity: String,
    scope: String,
    format: String,
    path: Option<PathBuf>,
    sha256: Option<String>,
    // Exact UTF-8 input, including whitespace and line endings.
    raw: Option<String>,
    entries: Vec<ImportedRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportedRecord {
    identity: RecordIdentity,
    value: Value,
}

/// Frozen source bytes; commit rejects any source added, removed or changed later.
#[derive(Debug, Clone)]
pub struct MigrationPlan {
    sources: Vec<SourceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationConflict {
    pub source: String,
    pub identity: RecordIdentity,
    pub current: RecordValue,
    pub incoming: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MigrationReport {
    pub imported: usize,
    pub unchanged: usize,
    pub conflicts: Vec<MigrationConflict>,
}

fn fingerprint(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn resolved_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    match fs::canonicalize(&absolute) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if let Some((parent, name)) = absolute
                .parent()
                .zip(absolute.file_name())
                .filter(|(parent, _)| parent.exists())
            {
                return Ok(fs::canonicalize(parent)?.join(name));
            }
            Ok(absolute)
        }
        Err(error) => Err(error).context("resolve legacy migration source"),
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            String::from_utf8(bytes).context("legacy source is not UTF-8")?,
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read explicit legacy source {}", path.display()))
        }
    }
}

fn file_snapshot(scope: &str, path: &Path, format: &str) -> Result<SourceSnapshot> {
    let canonical = resolved_path(path)?;
    let identity = serde_json::to_string(&(scope, format, &canonical))?;
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let raw = read_optional(&path)?;
    Ok(SourceSnapshot {
        identity,
        scope: scope.to_owned(),
        format: format.to_owned(),
        path: Some(path),
        sha256: raw.as_ref().map(|raw| fingerprint(raw.as_bytes())),
        raw,
        entries: Vec::new(),
    })
}

/// Read only explicitly supplied paths and preserve their complete raw contents.
///
/// # Errors
/// Rejects corrupt JSON, invalid entry collections, duplicate sources or I/O failures.
pub fn prepare_migration(inputs: &[LegacySource]) -> Result<MigrationPlan> {
    let mut sources = Vec::new();
    for input in inputs {
        if let Some(path) = &input.state_path {
            let mut snapshot = file_snapshot(&input.scope, path, "harness_state")?;
            if let Some(raw) = &snapshot.raw {
                let state: Value =
                    serde_json::from_str(raw).context("parse legacy harness state")?;
                ensure!(state.is_object(), "legacy harness state must be an object");
                if let Some(schema) = state.get("schema") {
                    ensure!(schema.is_number(), "legacy schema must be numeric");
                }
                let entries = state
                    .get("entries")
                    .and_then(Value::as_object)
                    .context("legacy entries must be an object")?;
                for (kind, rows) in entries {
                    let rows = rows
                        .as_object()
                        .context("legacy kind entries must be an object")?;
                    for (id, value) in rows {
                        let identity = RecordIdentity {
                            scope: input.scope.clone(),
                            kind: kind.clone(),
                            id: id.clone(),
                        };
                        identity.validate()?;
                        snapshot.entries.push(ImportedRecord {
                            identity,
                            value: value.clone(),
                        });
                    }
                }
            }
            sources.push(snapshot);
        }
    }
    let mut identities = BTreeSet::new();
    for source in &sources {
        ensure!(
            identities.insert(&source.identity),
            "duplicate explicit migration source"
        );
    }
    Ok(MigrationPlan { sources })
}

fn verify_sources(plan: &MigrationPlan, target: &Path) -> Result<()> {
    let target = resolved_path(target)?;
    for source in &plan.sources {
        if let Some(path) = &source.path {
            ensure!(
                resolved_path(path)? != target,
                "migration target must not replace a legacy source"
            );
            let current = read_optional(path)?;
            let sha256 = current.as_ref().map(|raw| fingerprint(raw.as_bytes()));
            ensure!(
                sha256 == source.sha256,
                "legacy source changed before migration commit: {}",
                path.display()
            );
        }
    }
    Ok(())
}

// A receipt includes each source's entire JSON snapshot, not just recognized
// entries. Unknown top-level state fields and future history fields survive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    source: SourceSnapshot,
    imported: Vec<ImportedRecord>,
}

pub(super) fn validate_receipts(values: &[Value]) -> Result<()> {
    for value in values {
        let receipt: Receipt =
            serde_json::from_value(value.clone()).context("corrupt native migration receipt")?;
        let source = &receipt.source;
        ensure!(
            !source.identity.is_empty() && !source.scope.is_empty(),
            "invalid migration source identity"
        );
        ensure!(
            source.sha256 == source.raw.as_ref().map(|raw| fingerprint(raw.as_bytes())),
            "migration receipt hash mismatch"
        );
        for entry in source.entries.iter().chain(&receipt.imported) {
            entry.identity.validate()?;
            ensure!(
                entry.identity.scope == source.scope,
                "migration receipt scope mismatch"
            );
        }
    }
    Ok(())
}

fn apply_source(
    document: &mut Document,
    source: &SourceSnapshot,
    report: &mut MigrationReport,
) -> Result<()> {
    let receipts: Vec<Receipt> = document
        .migrations
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()
        .context("corrupt native migration receipt")?;
    let prior = receipts
        .iter()
        .rev()
        .find(|receipt| receipt.source.identity == source.identity);
    let mut imported = Vec::new();
    for incoming in &source.entries {
        let current = document
            .slot(&incoming.identity)
            .map_or(RecordValue::Deleted, |slot| slot.value.clone());
        let baseline = receipts
            .iter()
            .rev()
            .filter(|receipt| receipt.source.identity == source.identity)
            .flat_map(|receipt| &receipt.imported)
            .find(|row| row.identity == incoming.identity);
        if current == RecordValue::Present(incoming.value.clone()) {
            report.unchanged += 1;
            imported.push(incoming.clone());
        } else if (current == RecordValue::Deleted
            && baseline.is_none()
            && document.slot(&incoming.identity).is_none())
            || baseline
                .is_some_and(|baseline| current == RecordValue::Present(baseline.value.clone()))
        {
            document.change(
                &incoming.identity,
                RecordValue::Present(incoming.value.clone()),
                ExpectedRevision::Any,
                "migration",
            )?;
            report.imported += 1;
            imported.push(incoming.clone());
        } else {
            report.conflicts.push(MigrationConflict {
                source: source.identity.clone(),
                identity: incoming.identity.clone(),
                current,
                incoming: incoming.value.clone(),
            });
        }
    }
    // Repeating an identical import does not rewrite the native store. Conflict
    // snapshots are still archived, without claiming that conflicting rows landed.
    let receipt = Receipt {
        source: source.clone(),
        imported,
    };
    let value = serde_json::to_value(&receipt)?;
    if prior.is_none_or(|prior| prior.source.sha256 != source.sha256)
        || !document.migrations.contains(&value)
    {
        document.migrations.push(value);
    }
    Ok(())
}

/// Commit all sources and receipts in one locked native transaction.
/// Conflicting native edits are reported and retained, never overwritten.
///
/// # Errors
/// Fails on changed source hashes, corrupt native data, invalid receipts or I/O.
pub fn commit_migration(store: &Store, plan: &MigrationPlan) -> Result<MigrationReport> {
    // This initial check also rejects aliases before the native lock file opens.
    verify_sources(plan, store.path())?;
    store.transact_checked(
        |document| {
            let mut report = MigrationReport::default();
            for source in &plan.sources {
                apply_source(document, source, &mut report)?;
            }
            Ok(report)
        },
        || verify_sources(plan, store.path()),
    )
}

/// Prepare and commit explicit sources without modifying their original bytes.
///
/// # Errors
/// Fails if preparation or the transactional migration fails.
pub fn migrate(store: &Store, sources: &[LegacySource]) -> Result<MigrationReport> {
    commit_migration(store, &prepare_migration(sources)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn public_import_preserves_originals_raw_fields_history_and_isolation() -> Result<()> {
        let private = tempfile::tempdir()?;
        let global_path = private.path().join("global.json");
        let local_path = private.path().join("local.json");
        let raw_entry = json!({"id":"different_inner_id","scope":null,"kind":"opaque","reference":null,
            "arguments":{"null":null},"metadata":null,"version":7,"future":{"opaque":[1,null]}});
        let state = json!({"schema":99,"future_top":null,"entries":{"opaque":{"same":raw_entry},"future_kind":{"same":null}}});
        let global_bytes = format!("  {}\n", serde_json::to_string(&state)?);
        let local_bytes = serde_json::to_string(&state)?;
        fs::write(&global_path, &global_bytes)?;
        fs::write(&local_path, &local_bytes)?;
        let sources = vec![
            LegacySource::global(Some(global_path.clone())),
            LegacySource::session("local", Some(local_path.clone()))?,
        ];
        let originals: Vec<_> = [&global_path, &local_path]
            .into_iter()
            .map(fs::read)
            .collect::<std::io::Result<_>>()?;
        let target = private.path().join("native.json");
        let store = Store::new(&target);
        let report = migrate(&store, &sources)?;
        assert_eq!(report.imported, 4);
        assert!(report.conflicts.is_empty());
        let bytes = fs::read(&target)?;
        let repeated = migrate(&store, &sources)?;
        assert_eq!(repeated.imported, 0);
        assert_eq!(repeated.unchanged, 4);
        assert_eq!(fs::read(&target)?, bytes);
        for (index, path) in [&global_path, &local_path].iter().enumerate() {
            assert_eq!(fs::read(path)?, originals[index]);
        }
        let global = RecordIdentity {
            scope: "legacy_global".into(),
            kind: "opaque".into(),
            id: "same".into(),
        };
        assert_eq!(store.read(&global)?.map(|row| row.value), Some(raw_entry));
        assert_eq!(store.enumerate(Some("session:fresh"))?, vec![]);
        let inspection = store.inspect()?;
        assert_eq!(inspection["migrations"][0]["source"]["raw"], global_bytes);
        assert_eq!(inspection["migrations"][1]["source"]["raw"], local_bytes);
        let revision = store.read(&global)?.context("imported global")?.revision;
        let changed = store.put(
            &global,
            json!({"native":null}),
            ExpectedRevision::Revision(revision),
        )?;
        store.rollback(
            &global,
            changed.revision,
            ExpectedRevision::Revision(changed.revision),
        )?;
        assert_eq!(
            store.read(&global)?.map(|row| row.value),
            Some(state["entries"]["opaque"]["same"].clone())
        );
        assert_eq!(store.inspect()?["migrations"], inspection["migrations"]);
        Ok(())
    }

    #[test]
    fn source_rehash_and_divergent_target_conflicts_are_transactional() -> Result<()> {
        let private = tempfile::tempdir()?;
        let path = private.path().join("legacy.json");
        let state = json!({"entries":{"memory":{"id":{"future":null}}}});
        fs::write(&path, serde_json::to_vec(&state)?)?;
        let source = LegacySource::session("one", Some(path.clone()))?;
        let target = private.path().join("native.json");
        let store = Store::new(&target);
        let plan = prepare_migration(std::slice::from_ref(&source))?;
        fs::write(&path, b"{}")?;
        assert!(commit_migration(&store, &plan).is_err());
        assert!(!target.exists());
        fs::write(&path, serde_json::to_vec(&state)?)?;
        migrate(&store, std::slice::from_ref(&source))?;
        let key = RecordIdentity {
            scope: "session:one".into(),
            kind: "memory".into(),
            id: "id".into(),
        };
        store.put(&key, json!({"edited":true}), ExpectedRevision::Any)?;
        let report = migrate(&store, std::slice::from_ref(&source))?;
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(
            store.read(&key)?.map(|row| row.value),
            Some(json!({"edited":true}))
        );
        store.delete(&key, ExpectedRevision::Any)?;
        assert_eq!(
            migrate(&store, std::slice::from_ref(&source))?
                .conflicts
                .len(),
            1
        );
        assert_eq!(store.read(&key)?, None);
        // An input cannot be overwritten even when explicitly passed as target.
        let same = Store::new(&path);
        assert!(migrate(&same, &[source]).is_err());
        assert_eq!(fs::read(path)?, serde_json::to_vec(&state)?);
        Ok(())
    }

    #[test]
    fn corrupt_archived_receipt_fails_all_native_reads_closed() -> Result<()> {
        let private = tempfile::tempdir()?;
        let path = private.path().join("legacy.json");
        fs::write(&path, b"{\"entries\":{\"opaque\":{\"id\":null}}}")?;
        let target = private.path().join("native.json");
        let store = Store::new(&target);
        migrate(&store, &[LegacySource::global(Some(path))])?;
        let mut native = store.inspect()?;
        native["migrations"][0]["source"]["sha256"] = json!("forged");
        let bytes = serde_json::to_vec(&native)?;
        fs::write(&target, &bytes)?;
        assert!(store.enumerate(None).is_err());
        assert!(store.inspect().is_err());
        assert_eq!(fs::read(target)?, bytes);
        Ok(())
    }

    #[test]
    fn corrupt_sources_and_missing_source_changes_fail_closed() -> Result<()> {
        let private = tempfile::tempdir()?;
        let path = private.path().join("missing.json");
        let source = LegacySource::global(Some(path.clone()));
        let plan = prepare_migration(&[source])?;
        fs::write(&path, b"{}")?;
        let store = Store::new(private.path().join("native.json"));
        assert!(commit_migration(&store, &plan).is_err());
        for bytes in [
            b"{".as_slice(),
            b"null",
            b"{}",
            b"{\"entries\":null}",
            b"{\"schema\":null,\"entries\":{}}",
        ] {
            fs::write(&path, bytes)?;
            assert!(prepare_migration(&[LegacySource::global(Some(path.clone()))]).is_err());
        }
        Ok(())
    }
}
