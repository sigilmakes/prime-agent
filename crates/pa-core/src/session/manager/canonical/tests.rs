//! Bounded no-I/O adoption regressions. Window fixture I/O ends before adoption.
use super::*;
use crate::session::canonical::{CanonicalFrontier, CanonicalOwnerScope};
use std::path::Path;

// Fixture builders intentionally take temporary JSON values by value.
#[allow(clippy::needless_pass_by_value)]
fn row(value: serde_json::Value, offset: u64, ordinal: u64) -> CanonicalJournalRow {
    let mut raw = serde_json::to_vec(&value).unwrap();
    raw.push(b'\n');
    CanonicalJournalRow::parse(Arc::from(raw), offset, ordinal).unwrap()
}

fn checkpoint(values: Vec<serde_json::Value>, selected: Option<&str>) -> CanonicalCheckpoint {
    let header = row(
        serde_json::json!({"type":"session","id":"canonical-session","version":3,
        "timestamp":"literal-header-time","cwd":"/literal/../cwd","futureHeader":{"native":"AA=="}}),
        0,
        0,
    );
    let mut rows = Vec::new();
    let mut bytes = u64::try_from(header.raw().len()).unwrap();
    let mut hash = Sha256::new();
    hash.update(header.raw());
    for (i, value) in values.into_iter().enumerate() {
        let next = row(value, bytes, u64::try_from(i).unwrap() + 1);
        bytes += u64::try_from(next.raw().len()).unwrap();
        hash.update(next.raw());
        rows.push(next);
    }
    let cursor = CanonicalCursor {
        owner: CanonicalOwnerScope::new(),
        session_id: "canonical-session".into(),
        source_path: "/no/such/canonical/journal.jsonl".into(),
        raw_source_generation: [3; 32],
        header_certificate: Sha256::digest(header.raw()).into(),
        canonical_revision: 1,
        branch_revision: 1,
        selected_leaf: selected.map(str::to_owned),
        frontier: CanonicalFrontier {
            bytes,
            rows: u64::try_from(rows.len()).unwrap() + 1,
            prefix_certificate: hash.finalize().into(),
        },
    };
    CanonicalCheckpoint {
        cursor,
        header,
        source: CanonicalSource::Full(rows),
    }
}

fn custom(id: &str, parent: Option<&str>) -> serde_json::Value {
    serde_json::json!({"type":"custom","id":id,"parentId":parent,"timestamp":"raw-time",
        "customType":"opaque","data":{"bytes":"AAE=","id":"native-id"},"unknown":{"x":[1,true]}})
}

fn user(id: &str, parent: Option<&str>) -> serde_json::Value {
    serde_json::json!({"type":"message","id":id,"parentId":parent,"timestamp":"raw-time",
        "message":{"role":"user","content":"same body","timestamp":9}})
}

fn manager(cp: &CanonicalCheckpoint) -> SessionManager {
    let FileEntry::Header { header } = cp.header.entry() else {
        panic!("fixture header")
    };
    SessionManager::in_memory_from_header(
        Path::new("/runtime"),
        Path::new("/artifacts"),
        header.clone(),
    )
    .unwrap()
}

fn install(manager: &mut SessionManager, cp: CanonicalCheckpoint) -> CanonicalAdoptionCertificate {
    let revision = manager.canonical_local_revision();
    let binding = manager.canonical_binding().cloned();
    manager
        .install_canonical_checkpoint(
            &CanonicalOperation::new(),
            revision,
            binding.as_ref(),
            cp,
            CanonicalLocalAdoption::preserve(),
        )
        .unwrap()
}

fn snapshot(manager: &SessionManager) -> serde_json::Value {
    serde_json::json!({"entries":manager.file_entries,"by_id":manager.by_id,
        "labels":manager.labels_by_id,"timestamps":manager.label_timestamps_by_id,
        "leaf":manager.leaf_id,"visible":manager.canonical_visible_leaf(),
        "queue":manager.admitted_user_rows,"revision":manager.canonical_local_revision(),
        "raw":manager.canonical_raw_retained_rows().map(|rows| rows.iter().map(|r|
            (r.raw().to_vec(),r.byte_offset(),r.physical_row())).collect::<Vec<_>>()),
        "header":manager.canonical_raw_header().map(|r|r.raw().to_vec()),
        "flushed":manager.flushed,"assistant":manager.has_assistant_entry})
}

fn suffix(
    manager: &SessionManager,
    value: serde_json::Value,
    selected: Option<&str>,
) -> CanonicalAppendReceipt {
    let from = manager.canonical_binding().unwrap().clone();
    let row = row(value, from.frontier.bytes, from.frontier.rows);
    let mut to = from.clone();
    to.canonical_revision += 1;
    to.raw_source_generation =
        Sha256::digest([from.raw_source_generation.as_slice(), row.raw()].concat()).into();
    to.selected_leaf = selected.map(str::to_owned);
    to.frontier.bytes += u64::try_from(row.raw().len()).unwrap();
    to.frontier.rows += 1;
    let mut hash = Sha256::new();
    hash.update(manager.canonical_raw_header().unwrap().raw());
    for prior in manager.canonical_raw_full_rows().unwrap() {
        hash.update(prior.raw());
    }
    hash.update(row.raw());
    to.frontier.prefix_certificate = hash.finalize().into();
    CanonicalAppendReceipt {
        operation: CanonicalOperation::new(),
        from,
        to,
        committed_entry_id: row.entry().id().map(str::to_owned),
        rows: vec![row],
        replacement_window: None,
    }
}

// This is a private test-only construction of a coordinator-approved plan.
// Production factories are crate-sealed and require the actual matching owner prepare.
fn approved(
    manager: &SessionManager,
    operation: &CanonicalOperation,
    cursor: &CanonicalCursor,
    policy: CanonicalAdmissionPolicy,
    run: &Arc<()>,
    visible: Option<&str>,
) -> CanonicalLocalAdoption {
    CanonicalLocalAdoption {
        approved: Some(PreparedApproval {
            operation: operation.clone(),
            cursor: cursor.clone(),
            policy,
            serving: Arc::downgrade(run),
            local_revision: manager.canonical_local_revision(),
            previous_visible: manager.canonical_visible_leaf().map(str::to_owned),
            visible: visible.map(str::to_owned),
        }),
    }
}

#[test]
fn full_raw_all_branches_unknown_maps_and_header_are_exact_and_not_visible_by_default() {
    let cp = checkpoint(
        vec![
            custom("a", None),
            custom("side", Some("a")),
            serde_json::json!({"type":"native_future","id":"opaque","parentId":"a","native":{"bytes":"//8="}}),
            serde_json::json!({"type":"future_no_id","arbitrary":{"bytes":"AA=="}}),
        ],
        Some("a"),
    );
    let original = match &cp.source {
        CanonicalSource::Full(rows) => rows.clone(),
        CanonicalSource::Window(_) => unreachable!(),
    };
    let header = cp.header.raw().to_vec();
    let mut m = manager(&cp);
    let cert = install(&mut m, cp);
    assert_eq!(cert.cursor(), m.canonical_binding().unwrap());
    assert_eq!(m.canonical_raw_coverage(), Some(CanonicalRawCoverage::Full));
    assert_eq!(m.canonical_raw_header().unwrap().raw(), header);
    for (a, b) in original.iter().zip(m.canonical_raw_full_rows().unwrap()) {
        assert_eq!(a.raw(), b.raw());
        assert_eq!(a.entry(), b.entry());
    }
    assert_eq!(
        m.canonical_binding().unwrap().selected_leaf.as_deref(),
        Some("a")
    );
    assert_eq!(m.canonical_visible_leaf(), None);
    assert!(m.canonical_visible_context().messages.is_empty());
    assert_eq!(
        m.append_ownership,
        crate::session::window::AppendOwnership::Unleased
    );
    assert!(!m.persist);
    assert!(m.session_file.is_none());
}

#[test]
fn bad_first_bind_does_not_discard_local_history_or_different_header() {
    let cp = checkpoint(vec![custom("a", None)], Some("a"));
    let mut m = manager(&cp);
    m.file_entries
        .push(serde_json::from_value(custom("local", None)).unwrap());
    let before = snapshot(&m);
    assert!(m
        .install_canonical_checkpoint(
            &CanonicalOperation::new(),
            0,
            None,
            cp,
            CanonicalLocalAdoption::preserve()
        )
        .is_err());
    assert_eq!(snapshot(&m), before);
    let cp = checkpoint(vec![], None);
    let mut m = manager(&cp);
    if let FileEntry::Header { header } = &mut m.file_entries[0] {
        header.cwd = "/different".into();
    }
    let before = snapshot(&m);
    assert!(m
        .install_canonical_checkpoint(
            &CanonicalOperation::new(),
            0,
            None,
            cp,
            CanonicalLocalAdoption::preserve()
        )
        .is_err());
    assert_eq!(snapshot(&m), before);
}

#[test]
fn suffix_tamper_cases_leave_every_field_and_cursor_unchanged() {
    let cp = checkpoint(vec![custom("a", None)], Some("a"));
    let mut m = manager(&cp);
    install(&mut m, cp);
    for case in 0..9 {
        let mut r = suffix(&m, custom("b", Some("a")), Some("b"));
        let mut revision = m.canonical_local_revision();
        match case {
            0 => r.to.frontier.prefix_certificate[0] ^= 1,
            1 => r.to.header_certificate[0] ^= 1,
            2 => {
                r.rows[0] = row(
                    custom("b", Some("a")),
                    r.from.frontier.bytes + 1,
                    r.from.frontier.rows,
                );
            }
            3 => {
                r.rows[0] = row(
                    custom("b", Some("a")),
                    r.from.frontier.bytes,
                    r.from.frontier.rows + 1,
                );
            }
            4 => r.to.owner = CanonicalOwnerScope::new(),
            5 => r.from.branch_revision += 2,
            6 => revision += 1,
            7 => r.to.canonical_revision = r.from.canonical_revision,
            8 => r.committed_entry_id = Some("foreign".into()),
            _ => unreachable!(),
        }
        let before = snapshot(&m);
        let binding = m.canonical_binding().cloned();
        assert!(
            m.apply_canonical_suffix(revision, r, CanonicalLocalAdoption::preserve())
                .is_err(),
            "case {case}"
        );
        assert_eq!(snapshot(&m), before, "case {case}");
        assert_eq!(m.canonical_binding(), binding.as_ref());
    }
    for value in [
        custom("a", Some("a")),
        custom("b", Some("missing")),
        custom("b", Some("b")),
    ] {
        let r = suffix(&m, value, Some("b"));
        let before = snapshot(&m);
        assert!(m
            .apply_canonical_suffix(
                m.canonical_local_revision(),
                r,
                CanonicalLocalAdoption::preserve()
            )
            .is_err());
        assert_eq!(snapshot(&m), before);
    }
}

#[test]
fn full_checkpoint_tamper_and_overflow_are_atomic() {
    for case in 0..6 {
        let mut cp = checkpoint(vec![custom("a", None)], Some("a"));
        let mut m = manager(&cp);
        match case {
            0 => cp.cursor.frontier.prefix_certificate[1] ^= 1,
            1 => cp.cursor.header_certificate[1] ^= 1,
            2 => cp.cursor.frontier.rows += 1,
            3 => cp.cursor.frontier.bytes += 1,
            4 => cp.cursor.selected_leaf = Some("absent".into()),
            5 => m.canonical_local_revision = u64::MAX,
            _ => unreachable!(),
        }
        let before = snapshot(&m);
        assert!(m
            .install_canonical_checkpoint(
                &CanonicalOperation::new(),
                m.canonical_local_revision(),
                None,
                cp,
                CanonicalLocalAdoption::preserve()
            )
            .is_err());
        assert_eq!(snapshot(&m), before);
    }
}

#[test]
fn fifo_append_preserve_partial_consumption_seen_rejects_historical_replay() {
    let cp = checkpoint(vec![user("u1", None), user("u2", Some("u1"))], Some("u2"));
    let mut m = manager(&cp);
    let run = Arc::new(());
    let op = CanonicalOperation::new();
    let plan = approved(
        &m,
        &op,
        &cp.cursor,
        CanonicalAdmissionPolicy::Replace(vec!["u1".into(), "u2".into()]),
        &run,
        Some("u1"),
    );
    m.install_canonical_checkpoint(&op, 0, None, cp, plan)
        .unwrap();
    assert_eq!(m.canonical_pending_admitted_users(), vec!["u1", "u2"]);
    assert_eq!(m.admitted_user_rows.pop_front().as_deref(), Some("u1"));
    m.invalidate_canonical_local_revision().unwrap();
    let r = suffix(&m, user("u3", Some("u2")), Some("u3"));
    let plan = approved(
        &m,
        &r.operation,
        &r.to,
        CanonicalAdmissionPolicy::Append(vec!["u3".into()]),
        &run,
        Some("u1"),
    );
    let cert = m
        .apply_canonical_suffix(m.canonical_local_revision(), r, plan)
        .unwrap();
    assert_eq!(m.canonical_pending_admitted_users(), vec!["u2", "u3"]);
    assert_eq!(m.canonical_visible_leaf(), Some("u1"));
    assert_eq!(
        m.canonical_binding().unwrap().selected_leaf.as_deref(),
        Some("u3")
    );
    let r = suffix(&m, custom("metadata", Some("u3")), Some("metadata"));
    m.apply_canonical_suffix(
        m.canonical_local_revision(),
        r,
        CanonicalLocalAdoption::preserve(),
    )
    .unwrap();
    assert_eq!(m.canonical_pending_admitted_users(), vec!["u2", "u3"]);
    let r = suffix(&m, custom("more", Some("metadata")), Some("more"));
    let plan = approved(
        &m,
        &r.operation,
        &r.to,
        CanonicalAdmissionPolicy::Append(vec!["u1".into()]),
        &run,
        Some("u1"),
    );
    let before = snapshot(&m);
    assert!(m
        .apply_canonical_suffix(m.canonical_local_revision(), r, plan)
        .is_err());
    assert_eq!(snapshot(&m), before);
    assert_eq!(cert.local_revision(), 3);
}

#[test]
fn approval_missing_wrong_operation_closed_run_old_revision_and_duplicate_ids_refuse_atomically() {
    for case in 0..5 {
        let cp = checkpoint(vec![user("u", None)], Some("u"));
        let mut m = manager(&cp);
        let run = Arc::new(());
        let op = CanonicalOperation::new();
        let mut plan = approved(
            &m,
            &op,
            &cp.cursor,
            CanonicalAdmissionPolicy::Append(vec!["u".into()]),
            &run,
            Some("u"),
        );
        match case {
            0 => plan.approved.as_mut().unwrap().operation = CanonicalOperation::new(),
            1 => plan.approved.as_mut().unwrap().local_revision = 9,
            2 => plan.approved.as_mut().unwrap().previous_visible = Some("old".into()),
            3 => {
                plan.approved.as_mut().unwrap().policy =
                    CanonicalAdmissionPolicy::Append(vec!["u".into(), "u".into()]);
            }
            4 => drop(run),
            _ => unreachable!(),
        }
        let before = snapshot(&m);
        assert!(m
            .install_canonical_checkpoint(&op, 0, None, cp, plan)
            .is_err());
        assert_eq!(snapshot(&m), before);
    }
}

#[test]
fn usage_last_physical_aggregate_changes_only_derived_clone_not_raw_rows() {
    let assistant = assistant("a", None, 1);
    let attribution = |id: &str, parent: &str, input: u64| {
        serde_json::json!({"type":"child_usage_attributed",
        "id":id,"parentId":parent,"targetId":"a","childSessionId":"child","childAgentId":"agent",
        "origin":"spawn_task","childUsage":usage(input),"aggregateUsage":usage(input)})
    };
    let cp = checkpoint(
        vec![
            assistant,
            attribution("attr1", "a", 7),
            attribution("attr2", "attr1", 13),
        ],
        Some("a"),
    );
    let mut m = manager(&cp);
    install(&mut m, cp);
    let FileEntry::Message {
        message: AgentMessage::Assistant(raw),
        ..
    } = m.canonical_raw_entry("a").unwrap().entry()
    else {
        panic!("assistant")
    };
    let FileEntry::Message {
        message: AgentMessage::Assistant(derived),
        ..
    } = m.get_entry_by_id("a").unwrap()
    else {
        panic!("assistant")
    };
    assert_eq!(raw.usage.input, 1);
    assert_eq!(derived.usage.input, 13);
}

fn usage(input: u64) -> serde_json::Value {
    serde_json::to_value(pa_types::ai::Usage {
        input,
        ..pa_types::ai::Usage::default()
    })
    .unwrap()
}

fn assistant(id: &str, parent: Option<&str>, input: u64) -> serde_json::Value {
    serde_json::json!({"type":"message","id":id,"parentId":parent,
        "message":{"role":"assistant","content":[],"api":"openai-responses","provider":"test",
        "model":"model","usage":usage(input),"stopReason":"stop","timestamp":0}})
}

fn sparse_fixture(cp: CanonicalCheckpoint, path: &Path) -> CanonicalCheckpoint {
    let CanonicalSource::Full(all) = cp.source else {
        panic!("full fixture")
    };
    let mut bytes = cp.header.raw().to_vec();
    for row in &all {
        bytes.extend_from_slice(row.raw());
    }
    // Fixture setup only. The actual adoption tests close/remove this directory first.
    std::fs::write(path, bytes).unwrap();
    let window = WindowedSessionStore::open(path)
        .unwrap()
        .expect("valid fixture window");
    assert!(!window.retained_whole_file());
    let rows = all
        .into_iter()
        .filter(|row| {
            window
                .raw_entries()
                .iter()
                .chain(window.metadata_entries())
                .any(|raw| row.raw().strip_suffix(b"\n") == Some(raw.as_bytes()))
        })
        .collect();
    let mut cursor = cp.cursor;
    cursor.source_path = path.to_owned();
    CanonicalCheckpoint {
        cursor: cursor.clone(),
        header: cp.header,
        source: CanonicalSource::Window(Box::new(crate::session::canonical::CanonicalWindow {
            cursor,
            rows,
            derived: window,
        })),
    }
}

fn sparse_values() -> Vec<serde_json::Value> {
    vec![
        assistant("old-a", None, 1),
        user("kept", Some("old-a")),
        serde_json::json!({"type":"compaction","id":"compact","parentId":"kept",
            "summary":"summary","firstKeptEntryId":"kept","tokensBefore":10}),
        user("leaf", Some("compact")),
    ]
}

#[test]
fn sparse_checkpoint_works_without_source_io_and_never_claims_full_or_default_visible() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.jsonl");
    let cp = sparse_fixture(checkpoint(sparse_values(), Some("leaf")), &path);
    let mut m = manager(&cp);
    dir.close().unwrap();
    assert!(!path.exists());
    install(&mut m, cp);
    assert_eq!(
        m.canonical_raw_coverage(),
        Some(CanonicalRawCoverage::OwnerVerifiedSparse)
    );
    assert!(m.canonical_raw_full_rows().is_none());
    assert!(m.canonical_raw_entry("old-a").is_none());
    assert!(m.canonical_raw_entry("kept").is_some());
    assert!(m.canonical_visible_context().messages.is_empty());
    assert!(!path.exists());
}

#[test]
fn sparse_omitted_attribution_target_needs_owner_replacement_and_folds_older_stats() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.jsonl");
    let initial = checkpoint(sparse_values(), Some("leaf"));
    let all = match &initial.source {
        CanonicalSource::Full(rows) => rows.clone(),
        CanonicalSource::Window(_) => unreachable!(),
    };
    let old_cursor = initial.cursor.clone();
    let mut cp = sparse_fixture(initial, &path);
    let mut m = manager(&cp);
    let from = cp.cursor.clone();
    let attr = row(
        serde_json::json!({"type":"child_usage_attributed","id":"attr","parentId":"leaf",
        "targetId":"old-a","childUsage":usage(9),"aggregateUsage":usage(10)}),
        from.frontier.bytes,
        from.frontier.rows,
    );
    let mut to = from.clone();
    to.canonical_revision += 1;
    to.raw_source_generation =
        Sha256::digest([from.raw_source_generation.as_slice(), attr.raw()].concat()).into();
    to.selected_leaf = Some("attr".into());
    to.frontier.bytes += u64::try_from(attr.raw().len()).unwrap();
    to.frontier.rows += 1;
    let mut hash = Sha256::new();
    hash.update(cp.header.raw());
    for row in &all {
        hash.update(row.raw());
    }
    hash.update(attr.raw());
    to.frontier.prefix_certificate = hash.finalize().into();
    let mut next_values = sparse_values();
    next_values.push(serde_json::from_slice(attr.raw()).unwrap());
    let mut next = checkpoint(next_values, Some("attr"));
    next.cursor = to.clone();
    let replacement = sparse_fixture(next, &path);
    let CanonicalSource::Window(replacement) = replacement.source else {
        unreachable!()
    };
    assert_eq!(replacement.derived.older_path_stats().input, 10);
    // Keep the old owner/path binding; no source read is needed after fixture destruction.
    cp.cursor.owner = old_cursor.owner;
    dir.close().unwrap();
    install(&mut m, cp);
    let receipt = CanonicalAppendReceipt {
        operation: CanonicalOperation::new(),
        from: from.clone(),
        to: to.clone(),
        rows: vec![attr.clone()],
        committed_entry_id: Some("attr".into()),
        replacement_window: None,
    };
    let before = snapshot(&m);
    assert!(m
        .apply_canonical_suffix(
            m.canonical_local_revision(),
            receipt,
            CanonicalLocalAdoption::preserve()
        )
        .is_err());
    assert_eq!(snapshot(&m), before);
    let receipt = CanonicalAppendReceipt {
        operation: CanonicalOperation::new(),
        from,
        to,
        rows: vec![attr],
        committed_entry_id: Some("attr".into()),
        replacement_window: Some(replacement),
    };
    m.apply_canonical_suffix(
        m.canonical_local_revision(),
        receipt,
        CanonicalLocalAdoption::preserve(),
    )
    .unwrap();
    assert_eq!(m.window.as_ref().unwrap().older_path_stats().input, 10);
    assert!(m.canonical_raw_entry("old-a").is_none());
    assert!(m.canonical_raw_full_rows().is_none());
    assert!(!path.exists());
}

#[test]
fn branch_aba_and_foreign_checkpoint_owner_fail_with_exact_binding_cas() {
    let cp = checkpoint(vec![custom("a", None)], Some("a"));
    let mut m = manager(&cp);
    install(&mut m, cp);
    let stale = suffix(&m, custom("b", Some("a")), Some("b"));
    let rows = m.canonical_raw_full_rows().unwrap().to_vec();
    let mut cursor = m.canonical_binding().unwrap().clone();
    cursor.branch_revision += 2;
    let cp = CanonicalCheckpoint {
        cursor,
        header: m.canonical_raw_header().unwrap().clone(),
        source: CanonicalSource::Full(rows),
    };
    let run = Arc::new(());
    let op = CanonicalOperation::new();
    let plan = approved(
        &m,
        &op,
        &cp.cursor,
        CanonicalAdmissionPolicy::Replace(vec![]),
        &run,
        None,
    );
    let binding = m.canonical_binding().cloned();
    m.install_canonical_checkpoint(
        &op,
        m.canonical_local_revision(),
        binding.as_ref(),
        cp,
        plan,
    )
    .unwrap();
    let before = snapshot(&m);
    assert!(m
        .apply_canonical_suffix(
            m.canonical_local_revision(),
            stale,
            CanonicalLocalAdoption::preserve()
        )
        .is_err());
    assert_eq!(snapshot(&m), before);
    let mut cursor = m.canonical_binding().unwrap().clone();
    cursor.owner = CanonicalOwnerScope::new();
    let cp = CanonicalCheckpoint {
        cursor,
        header: m.canonical_raw_header().unwrap().clone(),
        source: CanonicalSource::Full(m.canonical_raw_full_rows().unwrap().to_vec()),
    };
    let binding = m.canonical_binding().unwrap().clone();
    assert!(m
        .install_canonical_checkpoint(
            &CanonicalOperation::new(),
            m.canonical_local_revision(),
            Some(&binding),
            cp,
            CanonicalLocalAdoption::preserve()
        )
        .is_err());
    assert_eq!(snapshot(&m), before);
}

#[test]
fn sparse_cursor_positions_duplicates_and_foreign_owner_fail_atomically_without_journal() {
    for case in 0..4 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        let mut cp = sparse_fixture(checkpoint(sparse_values(), Some("leaf")), &path);
        let mut m = manager(&cp);
        dir.close().unwrap();
        match case {
            0 => cp.cursor.frontier.prefix_certificate[0] ^= 1,
            1 => {
                let CanonicalSource::Window(window) = &mut cp.source else {
                    unreachable!()
                };
                let last = window.rows.last().unwrap();
                let corrupt = CanonicalJournalRow::parse(
                    Arc::from(last.raw()),
                    cp.cursor.frontier.bytes + 1,
                    last.physical_row(),
                )
                .unwrap();
                *window.rows.last_mut().unwrap() = corrupt;
            }
            2 => cp.cursor.owner = CanonicalOwnerScope::new(),
            3 => {
                let CanonicalSource::Window(window) = &mut cp.source else {
                    unreachable!()
                };
                window.rows.push(window.rows.last().unwrap().clone());
            }
            _ => unreachable!(),
        }
        let before = snapshot(&m);
        assert!(m
            .install_canonical_checkpoint(
                &CanonicalOperation::new(),
                0,
                None,
                cp,
                CanonicalLocalAdoption::preserve()
            )
            .is_err());
        assert_eq!(snapshot(&m), before);
        assert!(!path.exists());
    }
}

#[test]
fn already_used_owner_approval_cannot_be_reminted_for_historical_delivery() {
    let cp = checkpoint(vec![user("u", None)], Some("u"));
    let mut m = manager(&cp);
    let run = Arc::new(());
    let op = CanonicalOperation::new();
    let plan = approved(
        &m,
        &op,
        &cp.cursor,
        CanonicalAdmissionPolicy::Replace(vec!["u".into()]),
        &run,
        Some("u"),
    );
    m.install_canonical_checkpoint(&op, 0, None, cp, plan)
        .unwrap();
    let cursor = m.canonical_binding().unwrap().clone();
    let cp = CanonicalCheckpoint {
        cursor: cursor.clone(),
        header: m.canonical_raw_header().unwrap().clone(),
        source: CanonicalSource::Full(m.canonical_raw_full_rows().unwrap().to_vec()),
    };
    let plan = approved(
        &m,
        &op,
        &cursor,
        CanonicalAdmissionPolicy::Replace(vec!["u".into()]),
        &run,
        Some("u"),
    );
    let before = snapshot(&m);
    assert!(m
        .install_canonical_checkpoint(&op, m.canonical_local_revision(), Some(&cursor), cp, plan)
        .is_err());
    assert_eq!(snapshot(&m), before);
}

#[test]
fn same_navigation_append_requires_new_descendants_and_none_root_is_explicit() {
    let cp = checkpoint(vec![custom("a", None), custom("other", None)], Some("a"));
    let mut m = manager(&cp);
    install(&mut m, cp);
    for (value, selected) in [
        (custom("b", Some("other")), Some("b")),
        (custom("metadata", Some("a")), Some("other")),
        (custom("metadata", Some("a")), None),
    ] {
        let r = suffix(&m, value, selected);
        let before = snapshot(&m);
        assert!(m
            .apply_canonical_suffix(
                m.canonical_local_revision(),
                r,
                CanonicalLocalAdoption::preserve()
            )
            .is_err());
        assert_eq!(snapshot(&m), before);
    }
    let mut cursor = m.canonical_binding().unwrap().clone();
    cursor.selected_leaf = Some("other".into());
    let binding = m.canonical_binding().cloned();
    let cp = CanonicalCheckpoint {
        cursor,
        header: m.canonical_raw_header().unwrap().clone(),
        source: CanonicalSource::Full(m.canonical_raw_full_rows().unwrap().to_vec()),
    };
    let before = snapshot(&m);
    assert!(m
        .install_canonical_checkpoint(
            &CanonicalOperation::new(),
            m.canonical_local_revision(),
            binding.as_ref(),
            cp,
            CanonicalLocalAdoption::preserve()
        )
        .is_err());
    assert_eq!(snapshot(&m), before);
    let r = suffix(&m, custom("b", Some("a")), Some("b"));
    assert_eq!(r.from.branch_revision, r.to.branch_revision);
    m.apply_canonical_suffix(
        m.canonical_local_revision(),
        r,
        CanonicalLocalAdoption::preserve(),
    )
    .unwrap();
    assert_eq!(
        m.canonical_binding().unwrap().selected_leaf.as_deref(),
        Some("b")
    );
    assert_eq!(m.canonical_visible_leaf(), None);
    let cp = checkpoint(vec![], None);
    let mut empty = manager(&cp);
    install(&mut empty, cp);
    let r = suffix(&empty, custom("first", None), Some("first"));
    empty
        .apply_canonical_suffix(
            empty.canonical_local_revision(),
            r,
            CanonicalLocalAdoption::preserve(),
        )
        .unwrap();
    assert_eq!(empty.canonical_visible_leaf(), None);
}

#[test]
fn append_generation_changes_are_allowed_only_with_verified_physical_growth() {
    let cp = checkpoint(vec![custom("a", None)], Some("a"));
    let mut m = manager(&cp);
    install(&mut m, cp);
    let r = suffix(&m, custom("b", Some("a")), Some("b"));
    assert_ne!(r.from.raw_source_generation, r.to.raw_source_generation);
    let next_generation = r.to.raw_source_generation;
    m.apply_canonical_suffix(
        m.canonical_local_revision(),
        r,
        CanonicalLocalAdoption::preserve(),
    )
    .unwrap();
    assert_eq!(
        m.canonical_binding().unwrap().raw_source_generation,
        next_generation
    );
    let binding = m.canonical_binding().unwrap().clone();
    let mut replaced = binding.clone();
    replaced.raw_source_generation[0] ^= 1;
    replaced.canonical_revision += 1;
    let cp = CanonicalCheckpoint {
        cursor: replaced.clone(),
        header: m.canonical_raw_header().unwrap().clone(),
        source: CanonicalSource::Full(m.canonical_raw_full_rows().unwrap().to_vec()),
    };
    let before = snapshot(&m);
    assert!(m
        .install_canonical_checkpoint(
            &CanonicalOperation::new(),
            m.canonical_local_revision(),
            Some(&binding),
            cp,
            CanonicalLocalAdoption::preserve()
        )
        .is_err());
    assert_eq!(snapshot(&m), before);
    let r = CanonicalAppendReceipt {
        operation: CanonicalOperation::new(),
        from: binding,
        to: replaced,
        rows: vec![],
        committed_entry_id: None,
        replacement_window: None,
    };
    assert!(m
        .apply_canonical_suffix(
            m.canonical_local_revision(),
            r,
            CanonicalLocalAdoption::preserve()
        )
        .is_err());
    assert_eq!(snapshot(&m), before);
    let mut r = suffix(&m, custom("c", Some("b")), Some("c"));
    r.from.raw_source_generation[0] ^= 1;
    assert!(m
        .apply_canonical_suffix(
            m.canonical_local_revision(),
            r,
            CanonicalLocalAdoption::preserve()
        )
        .is_err());
    assert_eq!(snapshot(&m), before);
}

fn immutable_observation(
    checkpoint: CanonicalCheckpoint,
    visible: Option<&str>,
) -> crate::session_engine::canonical_live::CanonicalLiveSnapshot {
    crate::session_engine::canonical_live::CanonicalLiveSnapshot {
        checkpoint,
        admitted_users: HashSet::new(),
        pending_users: VecDeque::new(),
        pending_custom: VecDeque::new(),
        safe_visible_leaf: visible.map(str::to_owned),
    }
}

#[test]
fn immutable_snapshot_installs_without_approvals_and_rejects_whole_candidate_atomically() {
    let cp = checkpoint(vec![custom("a", None)], Some("a"));
    let mut m = manager(&cp);
    let raw_header = cp.header.raw().to_vec();
    let raw_rows = match &cp.source {
        CanonicalSource::Full(rows) => rows.clone(),
        CanonicalSource::Window(_) => unreachable!(),
    };
    m.install_canonical_snapshot(0, None, immutable_observation(cp, Some("a")))
        .unwrap();
    assert_eq!(m.canonical_raw_header().unwrap().raw(), raw_header);
    assert_eq!(
        m.canonical_raw_full_rows()
            .unwrap()
            .iter()
            .map(crate::session::canonical::CanonicalJournalRow::raw)
            .collect::<Vec<_>>(),
        raw_rows
            .iter()
            .map(crate::session::canonical::CanonicalJournalRow::raw)
            .collect::<Vec<_>>()
    );
    assert!(m.canonical_raw.as_ref().unwrap().used_approvals.is_empty());
    assert!(m.canonical_raw.as_ref().unwrap().seen_users.is_empty());
    let before = snapshot(&m);
    let binding = m.canonical_binding().unwrap().clone();
    let bad = CanonicalCheckpoint {
        cursor: binding.clone(),
        header: m.canonical_raw_header().unwrap().clone(),
        source: CanonicalSource::Full(raw_rows),
    };
    assert!(m
        .install_canonical_snapshot(
            m.canonical_local_revision(),
            Some(&binding),
            immutable_observation(bad, Some("absent"))
        )
        .is_err());
    assert_eq!(snapshot(&m), before);
    assert!(m.canonical_raw.as_ref().unwrap().used_approvals.is_empty());
}

#[test]
fn immutable_snapshot_preserves_local_queue_seen_and_actual_approvals_exactly() {
    let cp = checkpoint(vec![user("u1", None), user("u2", Some("u1"))], Some("u2"));
    let mut m = manager(&cp);
    let operation = CanonicalOperation::new();
    let serving = Arc::new(());
    let local = approved(
        &m,
        &operation,
        &cp.cursor,
        CanonicalAdmissionPolicy::Replace(vec!["u2".into(), "u1".into()]),
        &serving,
        None,
    );
    m.install_canonical_checkpoint(&operation, 0, None, cp, local)
        .unwrap();
    let binding = m.canonical_binding().unwrap().clone();
    let queue = m.admitted_user_rows.clone();
    let seen = m.canonical_raw.as_ref().unwrap().seen_users.clone();
    let approvals = m.canonical_raw.as_ref().unwrap().used_approvals.clone();
    let observation = crate::session_engine::canonical_live::CanonicalLiveSnapshot {
        checkpoint: CanonicalCheckpoint {
            cursor: binding.clone(),
            header: m.canonical_raw_header().unwrap().clone(),
            source: CanonicalSource::Full(m.canonical_raw_full_rows().unwrap().to_vec()),
        },
        admitted_users: HashSet::from(["u1".into(), "u2".into()]),
        pending_users: VecDeque::from(["u1".into(), "u2".into()]),
        pending_custom: VecDeque::new(),
        safe_visible_leaf: None,
    };
    m.install_canonical_snapshot(m.canonical_local_revision(), Some(&binding), observation)
        .unwrap();
    assert_eq!(m.admitted_user_rows, queue);
    assert_eq!(m.canonical_raw.as_ref().unwrap().seen_users, seen);
    assert_eq!(m.canonical_raw.as_ref().unwrap().used_approvals, approvals);
}

#[test]
fn immutable_snapshot_rejects_lost_seen_membership_without_changing_manager() {
    let cp = checkpoint(vec![user("u", None)], Some("u"));
    let mut m = manager(&cp);
    let operation = CanonicalOperation::new();
    let serving = Arc::new(());
    let local = approved(
        &m,
        &operation,
        &cp.cursor,
        CanonicalAdmissionPolicy::Replace(vec!["u".into()]),
        &serving,
        None,
    );
    m.install_canonical_checkpoint(&operation, 0, None, cp, local)
        .unwrap();
    // Already consumed locally: queue validation alone must not forget seen membership.
    m.admitted_user_rows.clear();
    let before = snapshot(&m);
    let binding = m.canonical_binding().unwrap().clone();
    let observation = immutable_observation(
        CanonicalCheckpoint {
            cursor: binding.clone(),
            header: m.canonical_raw_header().unwrap().clone(),
            source: CanonicalSource::Full(m.canonical_raw_full_rows().unwrap().to_vec()),
        },
        Some("u"),
    );
    assert!(m
        .install_canonical_snapshot(m.canonical_local_revision(), Some(&binding), observation)
        .is_err());
    assert_eq!(snapshot(&m), before);
}

#[test]
fn immutable_snapshot_rejects_pending_user_missing_from_local_seen_atomically() {
    let cp = checkpoint(vec![user("u", None)], Some("u"));
    let mut m = manager(&cp);
    install(&mut m, cp);
    m.admitted_user_rows.push_back("u".into());
    let before = snapshot(&m);
    let binding = m.canonical_binding().unwrap().clone();
    let observation = crate::session_engine::canonical_live::CanonicalLiveSnapshot {
        checkpoint: CanonicalCheckpoint {
            cursor: binding.clone(),
            header: m.canonical_raw_header().unwrap().clone(),
            source: CanonicalSource::Full(m.canonical_raw_full_rows().unwrap().to_vec()),
        },
        admitted_users: HashSet::from(["u".into()]),
        pending_users: VecDeque::from(["u".into()]),
        pending_custom: VecDeque::new(),
        safe_visible_leaf: None,
    };
    assert!(m
        .install_canonical_snapshot(m.canonical_local_revision(), Some(&binding), observation)
        .is_err());
    assert_eq!(snapshot(&m), before);
}
