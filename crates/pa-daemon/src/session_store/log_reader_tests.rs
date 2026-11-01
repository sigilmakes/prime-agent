//! Reader tests use real checksummed logs, not legacy JSONL fixtures.

use super::*;
use pa_core::session_log::{Outcome, SessionLog};
use std::io::Write;

fn append(log: &mut SessionLog, row: &Value) {
    assert!(matches!(
        log.append(row, crate::session_transport::deadline()),
        Outcome::Committed { .. }
    ));
}

pub(crate) fn write_log_fixture(path: &Path, session: &SessionFile) {
    let mut log = SessionLog::open(
        path.parent().unwrap(),
        path.file_stem().unwrap().to_str().unwrap(),
        crate::session_transport::deadline(),
    )
    .unwrap();
    append(&mut log, &session_header_line(&session.header));
    for entry in &session.entries {
        append(&mut log, &serde_json::to_value(entry).unwrap());
    }
}

fn fixture(dir: &Path, id: &str) -> (SessionLog, PathBuf) {
    fs::create_dir_all(dir).unwrap();
    let mut log = SessionLog::open(dir, id, crate::session_transport::deadline()).unwrap();
    append(
        &mut log,
        &json!({
            "type": "session", "id": id, "cwd": "/fixture",
            "timestamp": "2026-10-31T00:00:00.000Z"
        }),
    );
    append(
        &mut log,
        &json!({
            "type": "message", "id": "user", "timestamp": "2026-10-31T00:00:01.000Z",
            "message": { "role": "user", "content": "first prompt", "timestamp": 1_793_404_801_000_u64 }
        }),
    );
    (log, dir.join(session_file_name(id)))
}

#[test]
fn log_readers_list_headers_and_resume_a_sidecar_from_real_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (mut log, path) = fixture(dir.path(), "reader");
    let initial = read_session_info(&path).unwrap();
    assert_eq!(initial.message_count, 1);
    assert_eq!(initial.first_message, "first prompt");
    assert_eq!(read_session_header(&path).unwrap().id, "reader");
    assert_eq!(read_session_header_bounded(&path).unwrap().id, "reader");
    assert_eq!(list_sessions(dir.path()), vec![initial.clone()]);
    persist_info_sidecar(&path);
    session_info_cache().lock().unwrap().drop_state(&path);
    append(
        &mut log,
        &json!({
            "type": "session_info", "id": "name", "timestamp": "2026-10-31T00:00:02.000Z",
            "name": "saved name"
        }),
    );
    let resumed = read_session_info(&path).unwrap();
    let mut expected = initial;
    expected.name = Some("saved name".to_string());
    assert_eq!(resumed, expected);
    // Re-reading the same generation never folds its messages twice.
    assert_eq!(read_session_info(&path), Some(resumed));
}

#[test]
fn log_reader_ignores_a_torn_append_without_mutating_or_locking() {
    let dir = tempfile::tempdir().unwrap();
    let (_log, path) = fixture(dir.path(), "reader");
    let before = read_session_info(&path).unwrap();
    let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writer
        .write_all(br#"{"seq":3,"row":{"type":"message"}}"#)
        .unwrap();
    let bytes = fs::read(&path).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert_eq!(read_session_info(&path), Some(before));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn log_reader_rejects_corruption_and_legacy_rows_without_repair() {
    let dir = tempfile::tempdir().unwrap();
    let (_log, path) = fixture(dir.path(), "reader");
    let bytes = fs::read_to_string(&path).unwrap();
    fs::write(&path, bytes.replace("first prompt", "false prompt")).unwrap();
    assert!(read_session_info(&path).is_none());
    assert!(list_sessions(dir.path()).is_empty());
    fs::write(&path, "{\"type\":\"session\",\"id\":\"old\",\"cwd\":\"/old\",\"timestamp\":\"2026-01-01T00:00:00Z\"}\n").unwrap();
    assert!(read_session_info(&path).is_none());
    assert!(read_session_header(&path).is_none());
    assert!(list_sessions(dir.path()).is_empty());
}

#[test]
fn log_reader_rejects_repeated_sequence_even_with_a_valid_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let (_log, path) = fixture(dir.path(), "reader");
    let bytes = fs::read_to_string(&path).unwrap();
    let last = bytes.lines().last().unwrap();
    let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(writer, "{last}").unwrap();
    assert!(read_session_info(&path).is_none());
    assert!(list_sessions(dir.path()).is_empty());
}

#[test]
fn log_archive_round_trip_preserves_bytes_and_catalog_identity() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("session-logs");
    let archive = dir.path().join("sessions-archive");
    let (_log, path) = fixture(&sessions, "reader");
    let bytes = fs::read(&path).unwrap();
    let archived = crate::session_archive::sweep_sessions(
        dir.path(),
        &sessions,
        &archive,
        &std::collections::HashSet::new(),
        &pa_core::settings::SessionArchivePolicy {
            max_age_days: None,
            max_sessions: Some(0),
        },
        std::time::SystemTime::now(),
    )
    .unwrap();
    assert_eq!(archived, vec!["reader"]);
    assert!(!path.exists());
    let resolved = crate::session_catalog::resolve_saved_session(
        dir.path(),
        &sessions,
        &archive,
        "reader",
        "/fixture",
    )
    .unwrap()
    .unwrap();
    assert_eq!(resolved.path, path);
    assert_eq!(fs::read(&resolved.path).unwrap(), bytes);
    assert!(!archive.join("reader.jsonl").exists());
    assert_eq!(list_sessions(&sessions), vec![resolved]);
}

#[test]
fn log_archive_does_not_move_a_locked_or_protected_session() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("session-logs");
    let archive = dir.path().join("sessions-archive");
    let (_log, path) = fixture(&sessions, "reader");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let policy = pa_core::settings::SessionArchivePolicy {
        max_age_days: None,
        max_sessions: Some(0),
    };
    assert!(crate::session_archive::sweep_sessions(
        dir.path(),
        &sessions,
        &archive,
        &std::collections::HashSet::new(),
        &policy,
        std::time::SystemTime::now()
    )
    .unwrap()
    .is_empty());
    lock.unlock().unwrap();
    let protected = std::collections::HashSet::from([crate::lease::canonical_session_path(&path)]);
    assert!(crate::session_archive::sweep_sessions(
        dir.path(),
        &sessions,
        &archive,
        &protected,
        &policy,
        std::time::SystemTime::now()
    )
    .unwrap()
    .is_empty());
    assert!(path.is_file());
}

#[test]
#[ignore = "subprocess execution-lock holder for the archive regression"]
fn archive_execution_holder() {
    let path = PathBuf::from(std::env::var_os("PA_ARCHIVE_TEST_SESSION").unwrap());
    let agent_dir = PathBuf::from(std::env::var_os("PA_ARCHIVE_TEST_AGENT_DIR").unwrap());
    let _execution = crate::lease::acquire_runtime_session_lease(&path, &agent_dir).unwrap();
    println!("ARCHIVE_EXECUTION_READY");
    std::io::stdout().flush().unwrap();
    let mut release = String::new();
    std::io::stdin().read_line(&mut release).unwrap();
}

#[test]
fn log_archive_spares_an_execution_owned_by_a_foreign_process() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("session-logs");
    let archive = dir.path().join("sessions-archive");
    let (_log, path) = fixture(&sessions, "foreign");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "session_store::log_reader_tests::archive_execution_holder",
            "--ignored",
            "--nocapture",
        ])
        .env("PA_ARCHIVE_TEST_SESSION", &path)
        .env("PA_ARCHIVE_TEST_AGENT_DIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "execution holder exited before readiness"
        );
        if line.contains("ARCHIVE_EXECUTION_READY") {
            break;
        }
    }
    // There is no local registry/protected row, and no append is running.
    let held = crate::lease::live_lease_owner(dir.path(), &path).unwrap();
    assert_eq!(held.pid, child.id());
    let policy = pa_core::settings::SessionArchivePolicy {
        max_age_days: None,
        max_sessions: Some(0),
    };
    let archived = crate::session_archive::sweep_sessions(
        dir.path(),
        &sessions,
        &archive,
        &std::collections::HashSet::new(),
        &policy,
        std::time::SystemTime::now(),
    )
    .unwrap();
    assert!(archived.is_empty());
    assert!(path.is_file());
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    assert_eq!(
        crate::session_archive::sweep_sessions(
            dir.path(),
            &sessions,
            &archive,
            &std::collections::HashSet::new(),
            &policy,
            std::time::SystemTime::now(),
        )
        .unwrap(),
        vec!["foreign"]
    );
}

#[test]
fn log_restore_refuses_a_held_source_or_live_target_execution_lock() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("session-logs");
    let archive = dir.path().join("sessions-archive");
    let (_log, source) = fixture(&archive, "restore");
    fs::create_dir_all(&sessions).unwrap();
    let destination = sessions.join("restore.jsonl");
    for lock_path in [&source, &destination] {
        let execution = crate::lease::acquire_runtime_session_lease(lock_path, dir.path()).unwrap();
        let error =
            crate::session_archive::restore_session(dir.path(), &archive, &sessions, "restore")
                .unwrap_err();
        assert!(error
            .downcast_ref::<crate::lease::SessionAlreadyActiveError>()
            .is_some());
        assert!(source.is_file());
        assert!(!destination.exists());
        drop(execution);
    }
    assert_eq!(
        crate::session_archive::restore_session(dir.path(), &archive, &sessions, "restore")
            .unwrap(),
        destination
    );
}
