//! Explicit, lossless import of legacy harness snapshots and refinement rows.
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

use super::store::{Change, Document, ExpectedRevision, RecordIdentity, RecordValue, Store};

/// Explicit inputs supplied by the caller, never ambient live-state discovery.
#[derive(Debug, Clone)]
pub struct LegacySource {
    scope: String,
    state_path: Option<PathBuf>,
    history_paths: Vec<PathBuf>,
    canonical_rows: Vec<Value>,
}

impl LegacySource {
    #[must_use]
    pub fn global(state_path: Option<PathBuf>, history_paths: Vec<PathBuf>) -> Self {
        Self {
            scope: "legacy_global".to_owned(),
            state_path,
            history_paths,
            canonical_rows: Vec::new(),
        }
    }

    /// Bind local state and caller-selected custom rows to one durable session.
    ///
    /// # Errors
    /// Rejects an empty session identity.
    pub fn session(
        session_id: &str,
        state_path: Option<PathBuf>,
        history_paths: Vec<PathBuf>,
        canonical_rows: Vec<Value>,
    ) -> Result<Self> {
        ensure!(
            !session_id.is_empty(),
            "migration requires an explicit session identity"
        );
        Ok(Self {
            scope: format!("session:{session_id}"),
            state_path,
            history_paths,
            canonical_rows,
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
    rows: Vec<RawHistoryRow>,
    entries: Vec<ImportedRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHistoryRow {
    identity: String,
    raw: String,
    value: Value,
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
        rows: Vec::new(),
        entries: Vec::new(),
    })
}

/// Read only explicitly supplied paths and preserve their complete raw contents.
/// Both `refinements.jsonl` and `refinement_history.jsonl` can be supplied.
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
                if let Some(rows) = state.get("refinements") {
                    let rows = rows
                        .as_array()
                        .context("legacy refinements must be an array")?;
                    for (index, value) in rows.iter().enumerate() {
                        let raw = serde_json::to_string(value)?;
                        snapshot.rows.push(RawHistoryRow {
                            identity: format!(
                                "{}:refinements:{index}:{}",
                                snapshot.identity,
                                fingerprint(raw.as_bytes())
                            ),
                            raw,
                            value: value.clone(),
                        });
                    }
                }
            }
            sources.push(snapshot);
        }
        for path in &input.history_paths {
            let mut snapshot = file_snapshot(&input.scope, path, "refinement_history")?;
            if let Some(raw) = &snapshot.raw {
                for (index, row) in raw.split_inclusive('\n').enumerate() {
                    if row.trim().is_empty() {
                        continue;
                    }
                    let value: Value = serde_json::from_str(row)
                        .with_context(|| format!("parse legacy history line {}", index + 1))?;
                    snapshot.rows.push(RawHistoryRow {
                        identity: format!(
                            "{}:line:{}:{}",
                            snapshot.identity,
                            index + 1,
                            fingerprint(row.as_bytes())
                        ),
                        raw: row.to_owned(),
                        value,
                    });
                }
            }
            sources.push(snapshot);
        }
        // Rows are already selected from canonical local history by the caller.
        // Never scan arbitrary transcripts or import another session implicitly.
        if !input.canonical_rows.is_empty() {
            let raw = serde_json::to_string(&input.canonical_rows)?;
            let mut snapshot = SourceSnapshot {
                identity: serde_json::to_string(&(&input.scope, "canonical_custom_rows"))?,
                scope: input.scope.clone(),
                format: "canonical_custom_rows".to_owned(),
                path: None,
                sha256: Some(fingerprint(raw.as_bytes())),
                raw: Some(raw),
                rows: Vec::new(),
                entries: Vec::new(),
            };
            for (index, value) in input.canonical_rows.iter().enumerate() {
                let raw = serde_json::to_string(value)?;
                snapshot.rows.push(RawHistoryRow {
                    identity: format!(
                        "{}:{index}:{}",
                        snapshot.identity,
                        fingerprint(raw.as_bytes())
                    ),
                    raw,
                    value: value.clone(),
                });
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
        for row in &source.rows {
            ensure!(
                !row.identity.is_empty(),
                "invalid legacy history row identity"
            );
            ensure!(
                serde_json::from_str::<Value>(&row.raw)? == row.value,
                "legacy raw history row mismatch"
            );
        }
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

/// Reverse an original legacy result by its durable ID within an explicit scope.
/// Source paths embedded in results are inert data, never write destinations.
/// All applied edits restore their complete raw snapshots in reverse order.
///
/// # Errors
/// Rejects missing results, conflicting duplicate IDs, malformed edits, changed
/// current records, corrupt native data and persistence failures. No edit lands
/// unless every reverse edit passes its raw-value compare-and-swap check.
pub fn rollback_legacy(store: &Store, scope: &str, result_id: &str) -> Result<Vec<Change>> {
    ensure!(
        !scope.is_empty() && !result_id.is_empty(),
        "legacy rollback needs an explicit scope and result ID"
    );
    store.transact(|document| {
        let mut result: Option<Value> = None;
        let mut origins = BTreeSet::new();
        for value in &document.migrations {
            let receipt: Receipt = serde_json::from_value(value.clone()).context("corrupt native migration receipt")?;
            if receipt.source.scope != scope { continue; }
            for row in &receipt.source.rows {
                let direct = &row.value;
                let candidate = if direct.get("id").and_then(Value::as_str) == Some(result_id)
                    && (direct.get("appliedEdits").is_some() || direct.get("applied_edits").is_some())
                {
                    direct
                } else if let Some(data) = direct.get("data") {
                    data
                } else {
                    continue;
                };
                if candidate.get("id").and_then(Value::as_str) != Some(result_id)
                    || (candidate.get("appliedEdits").is_none() && candidate.get("applied_edits").is_none())
                { continue; }
                if let Some(prior) = &result {
                    ensure!(prior == candidate, "conflicting legacy result ID {result_id} in scope {scope}");
                } else {
                    result = Some(candidate.clone());
                }
                origins.insert(receipt.source.identity.clone());
            }
        }
        let result = result.context("legacy refinement result not found in requested scope")?;
        let edits = match (result.get("appliedEdits"), result.get("applied_edits")) {
            (Some(camel), Some(snake)) => {
                ensure!(camel == snake, "conflicting legacy applied-edit fields");
                camel
            }
            (Some(edits), None) | (None, Some(edits)) => edits,
            (None, None) => anyhow::bail!("legacy result has no applied edits"),
        }.as_array().context("legacy appliedEdits must be an array")?;
        let mut changes = Vec::new();
        for edit in edits.iter().rev() {
            if edit.get("applied") != Some(&Value::Bool(true)) { continue; }
            let kind = edit.get("kind").and_then(Value::as_str).context("legacy edit kind missing")?;
            let id = edit.get("id").and_then(Value::as_str).context("legacy edit id missing")?;
            let action = edit.get("action").and_then(Value::as_str).context("legacy edit action missing")?;
            let identity = RecordIdentity { scope: scope.to_owned(), kind: kind.to_owned(), id: id.to_owned() };
            identity.validate()?;
            let before = edit.get("before").cloned().map_or(RecordValue::Deleted, RecordValue::Present);
            let after = edit.get("after").cloned().map_or(RecordValue::Deleted, RecordValue::Present);
            match action {
                "create" => ensure!(edit.get("before").is_none() && edit.get("after").is_some(), "invalid legacy create snapshots"),
                "update" => ensure!(edit.get("before").is_some() && edit.get("after").is_some(), "invalid legacy update snapshots"),
                "delete" => ensure!(edit.get("before").is_some() && edit.get("after").is_none(), "invalid legacy delete snapshots"),
                _ => anyhow::bail!("unsupported legacy edit action {action}"),
            }
            let current = document.slot(&identity).map_or(RecordValue::Deleted, |slot| slot.value.clone());
            ensure!(current == after, "legacy rollback conflict for {identity:?}");
            let expected = document.slot(&identity).map_or(ExpectedRevision::Absent, |slot| ExpectedRevision::Revision(slot.revision));
            changes.push(document.change(&identity, before, expected, "rollback_legacy")?);
            if let Some(event) = document.history.last_mut() {
                event.metadata = serde_json::json!({"rollbackOf":result_id,"scope":scope,"legacySources":origins});
            }
        }
        Ok(changes)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn public_import_preserves_originals_raw_fields_histories_and_isolation() -> Result<()> {
        let private = tempfile::tempdir()?;
        let global_path = private.path().join("global.json");
        let local_path = private.path().join("local.json");
        let ts_history = private.path().join("refinements.jsonl");
        let rust_history = private.path().join("refinement_history.jsonl");
        let raw_entry = json!({"id":"different_inner_id","scope":null,"kind":"factory","reference":null,
            "arguments":{"null":null},"metadata":null,"version":7,"future":{"opaque":[1,null]}});
        let state = json!({"schema":99,"future_top":null,"entries":{"factory":{"same":raw_entry},"future_kind":{"same":null}},
            "refinements":[{"id":"state","unknown":null,"created_at":"ts","changes":["factory"]}]});
        let global_bytes = format!("  {}\n", serde_json::to_string(&state)?);
        fs::write(&global_path, &global_bytes)?;
        fs::write(&local_path, serde_json::to_vec(&state)?)?;
        let ts_bytes = " {\"id\":\"duplicate\",\"created_at\":\"ts\",\"future\":null} \r\n";
        let rust_bytes = "{\"id\":\"duplicate\",\"createdAt\":\"rust\",\"future\":{}}\n";
        fs::write(&ts_history, ts_bytes)?;
        fs::write(&rust_history, rust_bytes)?;
        let sources = vec![
            LegacySource::global(
                Some(global_path.clone()),
                vec![ts_history.clone(), rust_history.clone()],
            ),
            LegacySource::session(
                "local",
                Some(local_path.clone()),
                vec![],
                vec![json!({"type":"custom","data":{"unknown":null}})],
            )?,
        ];
        let originals: Vec<_> = [&global_path, &local_path, &ts_history, &rust_history]
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
        for (index, path) in [&global_path, &local_path, &ts_history, &rust_history]
            .iter()
            .enumerate()
        {
            assert_eq!(fs::read(path)?, originals[index]);
        }
        let global = RecordIdentity {
            scope: "legacy_global".into(),
            kind: "factory".into(),
            id: "same".into(),
        };
        assert_eq!(store.read(&global)?.map(|row| row.value), Some(raw_entry));
        assert_eq!(store.enumerate(Some("session:fresh"))?, vec![]);
        let inspection = store.inspect()?;
        assert_eq!(inspection["migrations"][0]["source"]["raw"], global_bytes);
        assert_eq!(
            inspection["migrations"][1]["source"]["rows"][0]["raw"],
            ts_bytes
        );
        assert_eq!(
            inspection["migrations"][2]["source"]["rows"][0]["raw"],
            rust_bytes
        );
        // Both legacy formats survive, independently of the new rollback ledger.
        assert_ne!(
            inspection["migrations"][1]["source"]["rows"][0]["identity"],
            inspection["migrations"][2]["source"]["rows"][0]["identity"]
        );
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
            Some(state["entries"]["factory"]["same"].clone())
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
        let source = LegacySource::session("one", Some(path.clone()), vec![], vec![])?;
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
    fn original_ts_and_rust_results_rollback_full_raw_snapshots_by_id() -> Result<()> {
        for filename in ["refinements.jsonl", "refinement_history.jsonl"] {
            let private = tempfile::tempdir()?;
            let state_path = private.path().join("state.json");
            let history_path = private.path().join(filename);
            let before = json!({"id":"inner","reference":null,"arguments":{"x":null},"metadata":null,"future":[null,{"x":2}],"created_at":"old","version":4});
            let after = json!({"id":"inner","reference":{"callable":"new"},"arguments":null,"future":{"changed":true},"version":9});
            let restored_delete = json!({"content":"deleted","unknown":null,"version":3});
            let state = json!({"schema":1,"entries":{"factory":{"updated":after,"created":null,"null_updated":null}}});
            let result = json!({"id":"original_full_id","summary":"kept verbatim","expectedOutcome":"e","rationale":"r",
            "harnessStatePath":"/never/follow/this/path","future_result":null,"scope":"global",
            "appliedEdits":[
                {"action":"update","kind":"factory","id":"updated","applied":true,"before":before,"after":after,"unknown":null},
                {"action":"create","kind":"factory","id":"created","applied":true,"after":null},
                {"action":"delete","kind":"memory","id":"deleted","applied":true,"before":restored_delete},
                {"action":"update","kind":"factory","id":"null_updated","applied":true,"before":{"old":null},"after":null},
                {"action":"update","kind":"memory","id":"ignored","applied":false,"before":{},"after":{},"error":"not applied"}
            ]});
            fs::write(&state_path, serde_json::to_vec(&state)?)?;
            let raw_history = format!(" {}\r\n", serde_json::to_string(&result)?);
            fs::write(&history_path, &raw_history)?;
            let store = Store::new(private.path().join("native.json"));
            // The embedded global/path fields are not authority: the explicit
            // source binding alone determines the local destination identity.
            let source = LegacySource::session(
                "trusted",
                Some(state_path.clone()),
                vec![history_path.clone()],
                vec![json!({"id":"original_full_id","type":"custom","data":result})],
            )?;
            migrate(&store, &[source])?;
            let archive = store.inspect()?["migrations"].clone();
            let changes = rollback_legacy(&store, "session:trusted", "original_full_id")?;
            assert_eq!(changes.len(), 4);
            let key = |kind: &str, id: &str| RecordIdentity {
                scope: "session:trusted".into(),
                kind: kind.into(),
                id: id.into(),
            };
            assert_eq!(
                store
                    .read(&key("factory", "updated"))?
                    .map(|record| record.value),
                Some(before)
            );
            assert_eq!(store.read(&key("factory", "created"))?, None);
            assert_eq!(
                store
                    .read(&key("memory", "deleted"))?
                    .map(|record| record.value),
                Some(restored_delete)
            );
            assert_eq!(
                store
                    .read(&key("factory", "null_updated"))?
                    .map(|record| record.value),
                Some(json!({"old":null}))
            );
            assert_eq!(store.read(&key("memory", "ignored"))?, None);
            assert_eq!(store.enumerate(Some("legacy_global"))?, vec![]);
            assert_eq!(store.inspect()?["migrations"], archive);
            assert_eq!(fs::read(&history_path)?, raw_history.as_bytes());
            assert_eq!(fs::read(&state_path)?, serde_json::to_vec(&state)?);
            let ledger = store.history(&key("factory", "updated"))?;
            let event = ledger.last().context("rollback ledger")?;
            assert_eq!(event.operation, "rollback_legacy");
            assert_eq!(event.metadata["rollbackOf"], "original_full_id");
            assert_eq!(event.metadata["scope"], "session:trusted");
            assert!(rollback_legacy(&store, "legacy_global", "original_full_id").is_err());
        }
        Ok(())
    }

    #[test]
    fn legacy_rollback_conflicts_abort_all_reverse_edits_and_ambiguous_ids() -> Result<()> {
        let private = tempfile::tempdir()?;
        let path = private.path().join("state.json");
        let history = private.path().join("refinement_history.jsonl");
        fs::write(
            &path,
            b"{\"entries\":{\"memory\":{\"one\":{\"after\":1},\"two\":{\"after\":2}}}}",
        )?;
        let result = json!({"id":"shared","appliedEdits":[
            {"action":"update","kind":"memory","id":"one","applied":true,"before":null,"after":{"after":1}},
            {"action":"update","kind":"memory","id":"two","applied":true,"before":{"before":2},"after":{"after":2}}
        ]});
        fs::write(&history, serde_json::to_vec(&result)?)?;
        let target = private.path().join("native.json");
        let store = Store::new(&target);
        migrate(&store, &[LegacySource::global(Some(path), vec![history])])?;
        let first = RecordIdentity {
            scope: "legacy_global".into(),
            kind: "memory".into(),
            id: "one".into(),
        };
        store.put(
            &first,
            json!({"native":"must survive"}),
            ExpectedRevision::Any,
        )?;
        let before = fs::read(&target)?;
        assert!(rollback_legacy(&store, "legacy_global", "shared").is_err());
        assert_eq!(fs::read(&target)?, before); // reverse edit two must not partially land
        let conflict_path = private.path().join("refinements.jsonl");
        let mut conflicting = result;
        conflicting["future"] = json!("different logical payload");
        fs::write(&conflict_path, serde_json::to_vec(&conflicting)?)?;
        migrate(&store, &[LegacySource::global(None, vec![conflict_path])])?;
        let before = fs::read(&target)?;
        assert!(rollback_legacy(&store, "legacy_global", "shared").is_err());
        assert_eq!(fs::read(&target)?, before);
        Ok(())
    }

    #[test]
    fn legacy_rollback_reverses_multiple_edits_on_same_identity() -> Result<()> {
        let private = tempfile::tempdir()?;
        let path = private.path().join("history.jsonl");
        let result = json!({"id":"chain","appliedEdits":[
            {"action":"create","kind":"factory","id":"id","applied":true,"after":{"v":1}},
            {"action":"update","kind":"factory","id":"id","applied":true,"before":{"v":1},"after":{"v":2}}
        ]});
        fs::write(&path, serde_json::to_vec(&result)?)?;
        let store = Store::new(private.path().join("native.json"));
        migrate(&store, &[LegacySource::global(None, vec![path])])?;
        let key = RecordIdentity {
            scope: "legacy_global".into(),
            kind: "factory".into(),
            id: "id".into(),
        };
        store.put(&key, json!({"v":2}), ExpectedRevision::Absent)?;
        assert_eq!(rollback_legacy(&store, "legacy_global", "chain")?.len(), 2);
        assert_eq!(store.read(&key)?, None);
        Ok(())
    }

    #[test]
    fn malformed_legacy_rollback_metadata_fails_native_reads_closed() -> Result<()> {
        let private = tempfile::tempdir()?;
        let state_path = private.path().join("state.json");
        let history_path = private.path().join("history.jsonl");
        fs::write(&state_path, b"{\"entries\":{\"factory\":{\"id\":null}}}")?;
        fs::write(
            &history_path,
            serde_json::to_vec(&json!({"id":"original","appliedEdits":[
                {"action":"create","kind":"factory","id":"id","applied":true,"after":null}
            ]}))?,
        )?;
        let target = private.path().join("native.json");
        let store = Store::new(&target);
        migrate(
            &store,
            &[LegacySource::global(Some(state_path), vec![history_path])],
        )?;
        rollback_legacy(&store, "legacy_global", "original")?;
        let native = store.inspect()?;
        let metadata = native["history"][1]["metadata"].clone();
        let mut missing_id = metadata.clone();
        missing_id["rollbackOf"] = json!("");
        let mut wrong_scope = metadata.clone();
        wrong_scope["scope"] = json!("session:untrusted");
        let mut no_sources = metadata.clone();
        no_sources["legacySources"] = json!([]);
        let mut unknown_source = metadata;
        unknown_source["legacySources"] = json!(["unknown source"]);
        for invalid in [
            Value::Null,
            missing_id,
            wrong_scope,
            no_sources,
            unknown_source,
        ] {
            let mut corrupted = native.clone();
            corrupted["history"][1]["metadata"] = invalid;
            let bytes = serde_json::to_vec(&corrupted)?;
            fs::write(&target, &bytes)?;
            assert!(store.enumerate(None).is_err());
            assert!(store.inspect().is_err());
            assert_eq!(fs::read(&target)?, bytes);
        }
        Ok(())
    }

    #[test]
    fn corrupt_archived_receipt_fails_all_native_reads_closed() -> Result<()> {
        let private = tempfile::tempdir()?;
        let path = private.path().join("legacy.json");
        fs::write(&path, b"{\"entries\":{\"factory\":{\"id\":null}}}")?;
        let target = private.path().join("native.json");
        let store = Store::new(&target);
        migrate(&store, &[LegacySource::global(Some(path), vec![])])?;
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
        let source = LegacySource::global(Some(path.clone()), vec![]);
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
            assert!(
                prepare_migration(&[LegacySource::global(Some(path.clone()), vec![])]).is_err()
            );
        }
        Ok(())
    }
}
