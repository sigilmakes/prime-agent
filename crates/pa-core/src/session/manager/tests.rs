//! Manager unit battery: persistence bootstrap, crash repair, ISO round-trip, fork contract.

use super::*;

fn canonical_in_memory_header_fixture() -> SessionHeader {
    serde_json::from_value(serde_json::json!({
        "version": 17,
        "id": "canonical-mirror-session",
        "timestamp": "2001-02-03T04:05:06.789Z",
        "cwd": "/literal/source/project",
        "parentSession": "/literal/source/parent.jsonl",
        "rlmDepth": 4,
        "git": {
            "repoUrl": "ssh://example.invalid/exact/repo",
            "commit": "abc123",
            "branch": "original-branch",
            "futureGit": {"nested": [null, true, {"opaque": "git bytes"}]}
        },
        "futureSession": {"nested": [false, 42, {"opaque": "header bytes"}]}
    }))
    .unwrap()
}

#[test]
fn in_memory_from_header_preserves_authoritative_header_without_io() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("canonical.jsonl");
    let header = canonical_in_memory_header_fixture();
    let expected = serde_json::to_value(&header).unwrap();
    let original = format!(
        "  {}\n",
        serde_json::to_string(&FileEntry::Header {
            header: header.clone()
        })
        .unwrap()
    );
    std::fs::write(&source, &original).unwrap();
    let runtime_cwd = tmp.path().join("runtime-project");
    let session_dir = tmp.path().join("session-owned-artifacts");
    let mut manager =
        SessionManager::in_memory_from_header(&runtime_cwd, &session_dir, header).unwrap();

    assert_eq!(manager.get_session_id(), "canonical-mirror-session");
    assert_eq!(
        serde_json::to_value(manager.get_header()).unwrap(),
        expected
    );
    assert_eq!(manager.get_cwd(), runtime_cwd);
    assert_eq!(manager.get_session_dir(), session_dir);
    assert!(manager.has_session_dir());
    assert!(!manager.persist);
    assert!(manager.get_session_file().is_none());
    assert_eq!(manager.append_ownership, window::AppendOwnership::Unleased);
    assert_eq!(manager.file_entries.len(), 1);
    assert!(matches!(manager.file_entries[0], FileEntry::Header { .. }));
    assert!(manager.get_leaf_id().is_none());
    assert!(manager.by_id.is_empty());
    assert!(manager.labels_by_id.is_empty());
    assert!(manager.label_timestamps_by_id.is_empty());
    assert!(manager.admitted_user_rows.is_empty());
    assert!(manager.persist_listeners.is_empty());
    assert!(manager.window.is_none());

    manager
        .append_message(AgentMessage::User(pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text("first mirrored prompt".into()),
            timestamp: 0,
            rest: pa_types::JsonMap::new(),
        }))
        .unwrap();
    manager.flush_now().unwrap();
    assert_eq!(manager.get_session_id(), "canonical-mirror-session");
    assert_eq!(
        serde_json::to_value(manager.get_header()).unwrap(),
        expected
    );
    assert_eq!(std::fs::read(&source).unwrap(), original.as_bytes());
    assert!(!runtime_cwd.exists());
    assert!(!session_dir.exists());
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
}

#[test]
fn in_memory_from_header_accepts_a_header_only_session() {
    let tmp = tempfile::tempdir().unwrap();
    let header = canonical_in_memory_header_fixture();
    let manager = SessionManager::in_memory_from_header(tmp.path(), tmp.path(), header).unwrap();
    assert!(manager.get_entries().is_empty());
    assert!(manager.active_context().messages.is_empty());
    assert!(manager.active_context().archive.is_none());
    assert!(manager.get_leaf_id().is_none());
    assert!(manager.get_session_file().is_none());
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
}

#[test]
fn in_memory_from_header_rejects_an_empty_id_without_io() {
    let tmp = tempfile::tempdir().unwrap();
    let mut header = canonical_in_memory_header_fixture();
    header.id.clear();
    let error = SessionManager::in_memory_from_header(tmp.path(), tmp.path(), header)
        .err()
        .expect("an empty authoritative id is not a request to invent one");
    assert_eq!(error, "canonical session header id is empty");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
}

#[test]
fn in_memory_from_header_keeps_metadata_after_verified_window_adoption() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("canonical.jsonl");
    let mut header = canonical_in_memory_header_fixture();
    header.version = Some(3);
    let expected = serde_json::to_value(&header).unwrap();
    let bytes = format!(
        "  {}\n{}\n",
        serde_json::to_string(&FileEntry::Header { header }).unwrap(),
        serde_json::json!({
            "type": "message", "id": "canonical-user", "parentId": null,
            "timestamp": "2001-02-03T04:05:07.000Z",
            "message": {"role": "user", "content": "canonical question", "timestamp": 0}
        })
    );
    write_log_fixture(&path, &bytes).unwrap();
    let bytes = std::fs::read_to_string(&path).unwrap();
    let window = window::WindowedSessionStore::open(&path)
        .unwrap()
        .expect("a verified canonical window");
    let FileEntry::Header { header } = &window.entries()[0] else {
        panic!("the verified window starts with its canonical header");
    };
    let mut manager =
        SessionManager::in_memory_from_header(tmp.path(), tmp.path(), header.clone()).unwrap();
    manager.adopt_window(window);
    assert_eq!(manager.get_session_id(), "canonical-mirror-session");
    assert_eq!(
        serde_json::to_value(manager.get_header()).unwrap(),
        expected
    );
    assert_eq!(manager.get_leaf_id(), Some("canonical-user"));
    assert_eq!(manager.active_context().messages.len(), 1);
    assert!(!manager.persist);
    assert!(manager.get_session_file().is_none());
    assert_eq!(manager.append_ownership, window::AppendOwnership::Unleased);
    manager.append_custom_entry("mirror-only", None).unwrap();
    manager.flush_now().unwrap();
    assert_eq!(
        serde_json::to_value(manager.get_header()).unwrap(),
        expected
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes.as_bytes());
}

#[test]
fn generated_entry_ids_are_unique_and_link_to_previous_entry() {
    let mut manager = SessionManager::in_memory(Path::new("/tmp"));
    let mut previous = None;
    for _ in 0..1_000 {
        let id = manager.append_custom_entry("test", None).unwrap();
        let entry = manager.get_all_entries().last().unwrap();
        assert_eq!(entry.id(), Some(id.as_str()));
        assert_eq!(entry.parent_id(), previous.as_deref());
        assert_eq!(id.len(), 8);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        previous = Some(id);
    }
    assert_eq!(manager.by_id.len(), 1_000);
    assert_eq!(manager.get_leaf_id(), previous.as_deref());
}

/// The durable compaction line is the full TS `CompactionEntry` record:
/// `fromHook: false` is present (never a missing key), and the details
/// and usage ride along.
#[test]
fn append_compaction_serializes_the_full_ts_record() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::in_memory(tmp.path());
    manager
        .append_compaction(pa_types::session::CompactionEntry {
            context_archive: None,
            summary: "the overflow summary".to_string(),
            first_kept_entry_id: Some("e4".to_string()),
            tokens_before: 214,
            details: Some(serde_json::json!({
                "readFiles": [],
                "modifiedFiles": [],
            })),
            from_hook: Some(false),
            custom_instructions: None,
            usage: Some(pa_types::ai::Usage {
                rest: pa_types::JsonMap::new(),
                input: 20,
                output: 10,
                cache_read: 80,
                cache_write: 0,
                total_tokens: 110,
                cost: pa_types::ai::UsageCost::default(),
            }),
        })
        .unwrap();
    let line = serde_json::to_string(
        manager
            .get_entries()
            .iter()
            .rev()
            .find(|entry| matches!(entry, FileEntry::Compaction { .. }))
            .expect("compaction entry appended"),
    )
    .unwrap();
    assert!(line.contains("\"fromHook\":false"));
    assert!(line.contains("\"tokensBefore\":214"));
    assert!(line.contains("\"usage\":"));
}

#[test]
fn persist_appends_before_first_assistant() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("sessions");
    let mut manager = SessionManager::persisted(tmp.path(), &dir);
    // Every row is durable before publication, including before inference.
    let first = manager.append_thinking_level_change("high");
    assert!(manager.get_session_file().unwrap().exists());
    manager
        .append_message(AgentMessage::Assistant(pa_types::ai::AssistantMessage {
            content: vec![],
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-x".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: pa_types::ai::Usage::default(),
            stop_reason: pa_types::ai::StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: serde_json::Map::default(),
        }))
        .unwrap();
    let file = manager.get_session_file().unwrap().to_path_buf();
    assert!(file.exists());
    // The assistant append rewrote the whole file, including the earlier entry.
    let content = std::fs::read_to_string(&file).unwrap();
    assert!(content.contains("thinking_level_change"));
    assert!(content.contains("claude-x"));
    assert_eq!(
        manager.get_leaf_id(),
        Some(manager.get_entries().last().and_then(|e| e.id()).unwrap())
    );
    let _ = first;
}

#[test]
fn open_rejects_torn_tail_without_modifying_source() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let mut manager = SessionManager::persisted(tmp.path(), &dir);
    let assistant = AgentMessage::Assistant(pa_types::ai::AssistantMessage {
        content: vec![],
        api: "openai-completions".to_string(),
        provider: "openai".to_string(),
        model: "gpt-x".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pa_types::ai::Usage::default(),
        stop_reason: pa_types::ai::StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
        rest: serde_json::Map::default(),
    });
    manager.append_message(assistant).unwrap();
    let file = manager.get_session_file().unwrap().to_path_buf();
    let content = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, content.trim_end()).unwrap();
    assert!(SessionManager::open(tmp.path(), &dir, &file).is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), content.trim_end());
}

#[test]
fn flush_now_durability_without_assistant() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("sessions");
    let mut manager = SessionManager::persisted(tmp.path(), &dir);
    manager.append_session_info("my session").unwrap();
    assert!(manager.get_session_file().unwrap().exists());
    assert_eq!(manager.get_session_name().as_deref(), Some("my session"));
}

#[test]
fn iso_format_round_trips() {
    let stamp = format_iso(1_704_067_200_012);
    assert_eq!(stamp, "2024-01-01T00:00:00.012Z");
    assert_eq!(super::super::timestamp_to_millis(&stamp), 1_704_067_200_012);
}

#[test]
fn fork_from_copies_the_branch_under_a_fresh_header() {
    let tmp = tempfile::tempdir().unwrap();
    let source_cwd = tmp.path().join("source-project");
    let source_dir = tmp.path().join("source-sessions");
    std::fs::create_dir_all(&source_cwd).unwrap();
    let mut source = SessionManager::persisted(&source_cwd, &source_dir);
    source
        .append_message(AgentMessage::User(pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text("original question".to_string()),
            timestamp: 0,
            rest: serde_json::Map::default(),
        }))
        .unwrap();
    let user_id = source.get_leaf_id().unwrap().to_string();
    let assistant = AgentMessage::Assistant(pa_types::ai::AssistantMessage {
        content: vec![],
        api: "openai-completions".to_string(),
        provider: "openai".to_string(),
        model: "gpt-x".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pa_types::ai::Usage::default(),
        stop_reason: pa_types::ai::StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
        rest: serde_json::Map::default(),
    });
    source.append_message(assistant).unwrap();
    let assistant_id = source.get_leaf_id().unwrap().to_string();
    source
        .append_entry(FileEntry::GitState {
            payload: pa_types::session::GitStateEntry {
                git: GitContext::default(),
            },
            base: EntryBase {
                id: Some("gitstate1".to_string()),
                parent_id: Some(assistant_id.clone()),
                timestamp: Some(format_iso_now()),
                rest: serde_json::Map::default(),
            },
        })
        .unwrap();
    source
        .append_custom_entry("after-git-state", Some(serde_json::json!({ "keep": true })))
        .unwrap();
    let trailing_id = source.get_leaf_id().unwrap().to_string();
    let source_file = source.get_session_file().unwrap().to_path_buf();

    let target_cwd = tmp.path().join("target-project");
    let target_dir = tmp.path().join("target-sessions");
    let forked = SessionManager::fork_from(&source_file, &target_cwd, &target_dir)
        .expect("fork copies the file");
    let fork_file = forked.get_session_file().unwrap().to_path_buf();
    assert!(fork_file.exists(), "the fork file landed on disk");
    assert!(fork_file != source_file, "the fork is a new session file");
    assert!(
        fork_file.starts_with(&target_dir),
        "the fork file lives in the target session dir"
    );

    let header = forked.get_header().unwrap();
    assert_ne!(header.id, source.get_session_id());
    assert_eq!(header.cwd, target_cwd.display().to_string());
    assert_eq!(
        header.parent_session.as_deref(),
        Some(source_file.display().to_string().as_str())
    );
    assert_eq!(header.rlm_depth, source.get_header().unwrap().rlm_depth);

    let entries = forked.get_all_entries();
    assert!(entries
        .iter()
        .any(|entry| entry.id() == Some(user_id.as_str())));
    assert!(entries
        .iter()
        .any(|entry| entry.id() == Some(assistant_id.as_str())));
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, FileEntry::GitState { .. })),
        "git_state rows drop out of the fork"
    );
    let trailing = entries
        .iter()
        .find(|entry| entry.id() == Some(trailing_id.as_str()))
        .expect("the git_state child copied");
    assert_eq!(trailing.parent_id(), Some(assistant_id.as_str()));

    let before = std::fs::read_to_string(&fork_file).unwrap();
    let trailing_line = before
        .lines()
        .find(|line| line.contains(&trailing_id))
        .expect("the copied rows are on disk");
    assert!(trailing_line.contains("\"keep\":true"));
    let mut forked = forked;
    forked
        .append_message(AgentMessage::User(pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text("follow up".to_string()),
            timestamp: 1,
            rest: serde_json::Map::default(),
        }))
        .unwrap();
    let after = std::fs::read_to_string(&fork_file).unwrap();
    assert!(after.contains("follow up"), "appends extend the fork file");
    assert!(after.lines().count() > before.lines().count());
}

#[test]
fn fork_from_never_rewrites_the_source() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("torn.jsonl");
    let content = "{\"type\":\"session\",\"version\":3,\"id\":\"torn-head\",\"timestamp\":\"2024-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n{\"type\":\"custom\",\"customType\":\"kept\",\"data\":{},\"id\":\"keep1\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:00.000Z\"}\n{\"type\":\"custo";
    std::fs::write(&file, content).unwrap();
    let target_dir = tmp.path().join("fork-sessions");
    assert!(SessionManager::fork_from(&file, tmp.path(), &target_dir).is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), content);
}

#[test]
fn fork_from_terminates_on_cyclic_git_state_parents() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    // A hand-written source: a valid header, two git_state rows that
    // parent at each other, and a surviving custom row under one of them.
    let file = dir.join("cyclic.jsonl");
    let header = "{\"type\":\"session\",\"version\":3,\"id\":\"cyc-head\",\"timestamp\":\"2024-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}";
    let git_a = "{\"type\":\"git_state\",\"git\":{},\"id\":\"cyc01\",\"parentId\":\"cyc02\",\"timestamp\":\"2024-01-01T00:00:00.000Z\"}";
    let git_b = "{\"type\":\"git_state\",\"git\":{},\"id\":\"cyc02\",\"parentId\":\"cyc01\",\"timestamp\":\"2024-01-01T00:00:00.000Z\"}";
    let custom = "{\"type\":\"custom\",\"customType\":\"survivor\",\"data\":{},\"id\":\"cyc03\",\"parentId\":\"cyc01\",\"timestamp\":\"2024-01-01T00:00:00.000Z\"}";
    write_log_fixture(&file, format!("{header}\n{git_a}\n{git_b}\n{custom}\n")).unwrap();

    let target_dir = tmp.path().join("fork-sessions");
    let forked = SessionManager::fork_from(&file, tmp.path(), &target_dir)
        .expect("the cycle terminates and the fork completes");
    let entries = forked.get_all_entries();
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, FileEntry::GitState { .. })),
        "the git_state rows dropped"
    );
    let survivor = entries
        .iter()
        .find(|entry| entry.id() == Some("cyc03"))
        .expect("the surviving custom row copied");
    // The walk stopped at the first repeated id (cyc01), so the survivor
    // keeps its (dropped) parent instead of spinning on the cycle.
    assert_eq!(survivor.parent_id(), Some("cyc01"));
}

/// The loader returns no entries for a missing file, an empty file, AND a
/// file without a valid leading header (the no-header error stays as
/// TS-faithful defense-in-depth).
#[test]
fn fork_from_rejects_empty_and_headerless_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("sessions");
    let empty = tmp.path().join("empty.jsonl");
    std::fs::write(&empty, "").unwrap();
    assert!(SessionManager::fork_from(&empty, tmp.path(), &target).is_err());
    assert_eq!(std::fs::read(&empty).unwrap(), b"");
    let headerless = tmp.path().join("headerless.jsonl");
    write_log_fixture(&headerless, "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[],\"timestamp\":0},\"id\":\"aaaa1\",\"parentId\":null}\n").unwrap();
    let before = std::fs::read(&headerless).unwrap();
    assert!(SessionManager::fork_from(&headerless, tmp.path(), &target).is_err());
    assert_eq!(std::fs::read(&headerless).unwrap(), before);
    let missing = tmp.path().join("absent.jsonl");
    assert!(SessionManager::fork_from(&missing, tmp.path(), &target).is_err());
    assert!(!missing.exists());
    assert!(!target.exists());
}

#[test]
fn fork_from_rejects_a_non_regular_source() {
    let tmp = tempfile::tempdir().unwrap();
    let dir_source = tmp.path().join("not-a-session");
    std::fs::create_dir_all(&dir_source).unwrap();
    let error = SessionManager::fork_from(&dir_source, tmp.path(), &tmp.path().join("sessions"))
        .err()
        .expect("fork rejects a non-regular source");
    assert_eq!(
        error,
        format!(
            "Cannot fork: source session file is not a regular file: {}",
            dir_source.display()
        )
    );
}

#[test]
fn open_treats_a_path_shaped_header_id_as_corrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("crafted.jsonl");
    let bytes = b"unsupported old session format\n";
    std::fs::write(&file, bytes).unwrap();
    assert!(SessionManager::open(tmp.path(), &dir, &file).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
}

/// Replay validation must stay on the retained index: the full-history query
/// panics after window adoption. Replaying a registered row once does not
/// suppress a later legitimate submission of the same text.
#[test]
fn replay_admitted_user_rows_stays_windowed_and_reuses_only_registered_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut rows = vec![serde_json::json!({
        "type":"session", "id":"session", "version":3, "cwd":"/tmp",
        "timestamp":"2026-01-01T00:00:00Z",
    })];
    let mut parent = None;
    for index in 0..220 {
        let id = format!("user-{index}");
        rows.push(serde_json::json!({
            "type":"message", "id":id, "parentId":parent,
            "message":{"role":"user", "content":"repeat this prompt", "timestamp":index},
        }));
        parent = Some(id);
    }
    rows.push(serde_json::json!({
        "type":"compaction", "id":"compacted", "parentId":parent,
        "summary":"earlier turns", "firstKeptEntryId":"user-215", "tokensBefore":9000,
    }));
    rows.push(serde_json::json!({
        "type":"message", "id":"accepted", "parentId":"compacted",
        "message":{"role":"user", "content":"repeat this prompt", "timestamp":220},
    }));
    let bytes = rows
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    write_log_fixture(&path, &bytes).unwrap();
    let bytes = std::fs::read_to_string(&path).unwrap();
    let window = super::super::window::WindowedSessionStore::open(&path)
        .unwrap()
        .expect("windowed transcript");
    let mut manager = SessionManager::in_memory(dir.path());
    manager.adopt_window(window);
    assert!(manager.retained_entries().len() < rows.len());
    let retained = manager.retained_entries().to_vec();
    let message: AgentMessage = serde_json::from_value(serde_json::json!({
        "role":"user", "content":"repeat this prompt", "timestamp":221,
    }))
    .unwrap();
    manager
        .replay_admitted_user_rows(vec!["accepted".to_string()])
        .unwrap();
    let id = manager.append_message_retained(message.clone()).unwrap();
    assert_eq!(id, "accepted");
    assert_eq!(manager.retained_entries(), retained);
    let next_id = manager.append_message_retained(message).unwrap();
    assert_ne!(next_id, "accepted");
    assert_eq!(manager.retained_entries().len(), retained.len() + 1);
    assert!(manager.window.is_some(), "no full-history hydration");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        bytes,
        "the worker-owned durable file is untouched by mirrored replay"
    );
}

#[test]
fn host_writer_receives_exact_entry_before_publication() {
    use std::sync::{Arc, Mutex};
    struct Writer(Arc<Mutex<Vec<FileEntry>>>);
    impl super::SessionEntryWriter for Writer {
        fn read_tail(
            &self,
            _: &mut crate::session_log::ReadCursor,
        ) -> std::io::Result<Vec<FileEntry>> {
            Ok(Vec::new())
        }

        fn append(&self, entry: &mut FileEntry) -> std::io::Result<()> {
            self.0.lock().unwrap().push(entry.clone());
            Ok(())
        }
    }
    let rows = Arc::new(Mutex::new(Vec::new()));
    let mut manager = SessionManager::in_memory(Path::new("/tmp"));
    manager.set_entry_writer(Arc::new(Writer(rows.clone())));
    let id = manager
        .append_custom_entry("host-metadata", Some(serde_json::json!({"value":1})))
        .unwrap();
    let entries = manager.retained_entries();
    let entry = entries
        .iter()
        .find(|entry| entry.id() == Some(id.as_str()))
        .unwrap();
    assert_eq!(*rows.lock().unwrap(), vec![entry.clone()]);
}

#[test]
fn host_writer_failure_does_not_publish_an_entry() {
    struct Writer;
    impl super::SessionEntryWriter for Writer {
        fn read_tail(
            &self,
            _: &mut crate::session_log::ReadCursor,
        ) -> std::io::Result<Vec<FileEntry>> {
            Ok(Vec::new())
        }

        fn append(&self, _: &mut FileEntry) -> std::io::Result<()> {
            Err(std::io::Error::other("injected append refusal"))
        }
    }
    let mut manager = SessionManager::in_memory(Path::new("/tmp"));
    let before = manager.retained_entries().to_vec();
    manager.set_entry_writer(std::sync::Arc::new(Writer));
    assert!(manager.append_custom_entry("host-metadata", None).is_err());
    assert_eq!(manager.retained_entries(), before);
}

#[test]
fn host_writer_does_not_repeat_accepted_user_rows() {
    use std::sync::{Arc, Mutex};
    struct Writer(Arc<Mutex<Vec<FileEntry>>>);
    impl super::SessionEntryWriter for Writer {
        fn read_tail(
            &self,
            _: &mut crate::session_log::ReadCursor,
        ) -> std::io::Result<Vec<FileEntry>> {
            Ok(Vec::new())
        }

        fn append(&self, entry: &mut FileEntry) -> std::io::Result<()> {
            self.0.lock().unwrap().push(entry.clone());
            Ok(())
        }
    }
    let mut manager = SessionManager::in_memory(Path::new("/tmp"));
    let message: AgentMessage = serde_json::from_value(serde_json::json!({
        "role":"user", "content":"accepted once", "timestamp":1
    }))
    .unwrap();
    let accepted = manager.append_message(message.clone()).unwrap();
    manager
        .replay_admitted_user_rows(vec![accepted.clone()])
        .unwrap();
    let rows = Arc::new(Mutex::new(Vec::new()));
    manager.set_entry_writer(Arc::new(Writer(rows.clone())));
    assert_eq!(manager.append_message(message).unwrap(), accepted);
    assert!(rows.lock().unwrap().is_empty());
}

#[test]
fn host_sink_returns_the_committed_identity_and_parent() {
    struct Writer;
    impl super::SessionEntryWriter for Writer {
        fn read_tail(
            &self,
            _: &mut crate::session_log::ReadCursor,
        ) -> std::io::Result<Vec<FileEntry>> {
            Ok(Vec::new())
        }

        fn append(&self, entry: &mut FileEntry) -> std::io::Result<()> {
            let mut value = serde_json::to_value(&*entry)?;
            value["id"] = "committed-id".into();
            value["parentId"] = "committed-parent".into();
            value["timestamp"] = "2001-02-03T04:05:06.789Z".into();
            *entry = serde_json::from_value(value)?;
            Ok(())
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::in_memory(tmp.path());
    manager.set_entry_writer(std::sync::Arc::new(Writer));
    assert!(manager.is_persisted());
    let id = manager.append_session_info("name").unwrap();
    assert_eq!(id, "committed-id");
    assert_eq!(manager.get_leaf_id(), Some(id.as_str()));
    let stored = manager.get_entry_by_id(&id).unwrap();
    assert_eq!(stored.parent_id(), Some("committed-parent"));
    assert_eq!(stored.timestamp(), "2001-02-03T04:05:06.789Z");
}

#[test]
fn admitted_custom_identity_is_consumed_once_without_content_deduplication() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::in_memory(tmp.path());
    let id = manager
        .append_custom_message(
            "accepted",
            pa_types::ai::UserContent::Text("same".into()),
            true,
            None,
        )
        .unwrap();
    manager
        .replay_admitted_custom_rows(vec![id.clone()])
        .unwrap();
    assert_eq!(manager.consume_admitted_custom_row(), Some(id));
    assert_eq!(manager.consume_admitted_custom_row(), None);
    assert!(manager
        .replay_admitted_custom_rows(vec!["missing".into()])
        .is_err());
}

fn write_log_fixture(path: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    let temp = tempfile::tempdir()?;
    let mut log =
        crate::session_log::SessionLog::open(temp.path(), "fixture", std::time::Instant::now())
            .map_err(std::io::Error::other)?;
    for line in content
        .as_ref()
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
    {
        let row: serde_json::Value = serde_json::from_slice(line)?;
        assert!(matches!(
            log.append(&row, std::time::Instant::now()),
            crate::session_log::Outcome::Committed { .. }
        ));
    }
    std::fs::copy(temp.path().join("fixture.jsonl"), path)?;
    Ok(())
}

#[test]
fn external_commits_enter_context_before_inference_and_parent_selection() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::persisted(dir.path(), dir.path());
    manager.append_custom_entry("initial", None).unwrap();
    let path = manager.get_session_file().unwrap().to_owned();
    let id = path.file_stem().unwrap().to_str().unwrap();
    let mut external =
        crate::session_log::SessionLog::open(dir.path(), id, std::time::Instant::now()).unwrap();
    let row = serde_json::json!({"type":"message","id":"foreign-user","parentId":manager.get_leaf_id(),"timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"external prompt","timestamp":0}});
    assert!(matches!(
        external.append(&row, std::time::Instant::now()),
        crate::session_log::Outcome::Committed { .. }
    ));
    manager.refresh_committed_entries().unwrap();
    assert_eq!(manager.active_context().messages.len(), 1);
    let row = serde_json::json!({"type":"custom_message","id":"foreign-custom","parentId":"foreign-user","timestamp":"2026-01-01T00:00:00Z","customType":"external","content":"external notice","display":true});
    assert!(matches!(
        external.append(&row, std::time::Instant::now()),
        crate::session_log::Outcome::Committed { .. }
    ));
    let appended = manager.append_session_info("after external").unwrap();
    assert_eq!(
        manager.get_entry_by_id(&appended).unwrap().parent_id(),
        Some("foreign-custom")
    );
}

#[test]
fn unsupported_and_torn_prefixes_never_repair_or_recreate_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unsupported.jsonl");
    for bytes in [
        b"".as_slice(),
        b"{\"type\":\"session\",\"id\":\"old\"}\n".as_slice(),
        b"{\"seq\":1,\"row\":{".as_slice(),
    ] {
        std::fs::write(&path, bytes).unwrap();
        assert!(SessionManager::open(dir.path(), dir.path(), &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn mirrored_sink_tail_is_visible_before_model_and_next_append() {
    use std::sync::{Arc, Mutex};
    struct Writer(Mutex<crate::session_log::SessionLog>);
    impl SessionEntryWriter for Writer {
        fn read_tail(
            &self,
            cursor: &mut crate::session_log::ReadCursor,
        ) -> std::io::Result<Vec<FileEntry>> {
            self.0
                .lock()
                .unwrap()
                .read_tail(cursor, std::time::Instant::now())
                .map_err(std::io::Error::other)?
                .into_iter()
                .map(|row| serde_json::from_value(row.body).map_err(std::io::Error::from))
                .collect()
        }
        fn append(&self, entry: &mut FileEntry) -> std::io::Result<()> {
            match self
                .0
                .lock()
                .unwrap()
                .append(entry, std::time::Instant::now())
            {
                crate::session_log::Outcome::Committed { .. } => Ok(()),
                other => Err(std::io::Error::other(format!("{other:?}"))),
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::in_memory(dir.path());
    let mut log =
        crate::session_log::SessionLog::open(dir.path(), "mirror", std::time::Instant::now())
            .unwrap();
    assert!(matches!(
        log.append(&manager.file_entries[0], std::time::Instant::now()),
        crate::session_log::Outcome::Committed { .. }
    ));
    let mut cursor = crate::session_log::ReadCursor::default();
    log.read_tail(&mut cursor, std::time::Instant::now())
        .unwrap();
    let writer = Arc::new(Writer(Mutex::new(log)));
    manager.set_committed_cursor(cursor);
    manager.set_entry_writer(writer.clone());
    let mut row: FileEntry = serde_json::from_value(serde_json::json!({"type":"message","id":"foreign","parentId":null,"message":{"role":"user","content":"external input","timestamp":0}})).unwrap();
    writer.append(&mut row).unwrap();
    manager
        .replay_admitted_input_rows(vec!["foreign".to_owned()])
        .unwrap();
    assert_eq!(manager.active_context().messages.len(), 1);
    let FileEntry::Message {
        message: accepted, ..
    } = row.clone()
    else {
        unreachable!()
    };
    assert_eq!(manager.append_message(accepted).unwrap(), "foreign");
    let mut row: FileEntry = serde_json::from_value(serde_json::json!({"type":"custom_message","id":"foreign-custom","parentId":"foreign","customType":"external","content":"external notice","display":true})).unwrap();
    writer.append(&mut row).unwrap();
    let id = manager.append_session_info("after foreign").unwrap();
    assert_eq!(
        manager.get_entry_by_id(&id).unwrap().parent_id(),
        Some("foreign-custom")
    );
    let mut selection: FileEntry = serde_json::from_value(
        serde_json::json!({"type":"branch_selection","selectedLeaf":"foreign"}),
    )
    .unwrap();
    writer.append(&mut selection).unwrap();
    manager.refresh_committed_entries().unwrap();
    assert_eq!(manager.get_leaf_id(), Some("foreign"));
    let next = manager.append_session_info("selected branch").unwrap();
    assert_eq!(
        manager.get_entry_by_id(&next).unwrap().parent_id(),
        Some("foreign")
    );
}

#[test]
fn lazy_new_session_never_adopts_a_precreated_target() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::persisted(dir.path(), dir.path());
    let target = manager.get_session_file().unwrap().to_owned();
    write_log_fixture(
        &target,
        serde_json::to_string(&manager.file_entries[0]).unwrap(),
    )
    .unwrap();
    let before = std::fs::read(&target).unwrap();
    assert_eq!(
        manager.flush_now().unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(std::fs::read(&target).unwrap(), before);
    assert!(manager.append_session_info("cannot overwrite").is_err());
    assert_eq!(std::fs::read(&target).unwrap(), before);
}
