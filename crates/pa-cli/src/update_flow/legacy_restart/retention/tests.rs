use super::*;
use sha2::{Digest, Sha256};

struct Fixture {
    directory: tempfile::TempDir,
    socket: std::path::PathBuf,
    source: std::path::PathBuf,
    attempt: std::path::PathBuf,
    checkpoint: std::path::PathBuf,
    status: std::path::PathBuf,
    source_text: String,
    attempt_text: String,
    roster: UpdateRoster,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let source = directory.path().join("source.json");
        let attempt = directory.path().join("attempt.json");
        let checkpoint = directory.path().join("checkpoint.json");
        let status = directory.path().join("status.json");
        let source_text = "{\r\n  \"sessions\": [], \"formatVersion\": 1, \"createdAt\": \"now\", \"unknown\": {\"é\": 1e-05}\r\n}\n\n".to_string();
        let attempt_text = format!(
            "{}\r\n\n",
            serde_json::to_string_pretty(&json!({
                "socket": socket, "version": crate::config::version(),
                "hello": {"supervisorPid":42,"supervisorProcessStartId":"birth",
                    "supervisorGeneration":"generation", "appVersion":"0.9.8"},
                "startedAt": 0, "previousDigest": null, "updateId":"adopted-request",
                "origin":"adopted-origin", "unknown":"raw UTF-8 雪",
            }))
            .unwrap()
        );
        std::fs::write(&source, &source_text).unwrap();
        std::fs::write(&attempt, &attempt_text).unwrap();
        let roster = reconvert(&source_text, &attempt_text, &socket).unwrap();
        super::super::persist(&checkpoint, &serde_json::to_value(&roster).unwrap()).unwrap();
        Self {
            directory,
            socket,
            source,
            attempt,
            checkpoint,
            status,
            source_text,
            attempt_text,
            roster,
        }
    }

    fn publish(&self) -> Result<()> {
        publish(
            &self.status,
            &UpdateId::from("current-request".to_string()),
            Some("current-origin"),
            &self.roster,
            &self.attempt,
            &self.source,
            /*prepared_source*/ None,
        )
    }

    fn read_record(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.status.with_extension("recovery.json")).unwrap())
            .unwrap()
    }

    fn write_record(&self, record: &Value) {
        // Deliberate corruption of an already published record, never producer behavior.
        std::fs::write(
            self.status.with_extension("recovery.json"),
            serde_json::to_vec(record).unwrap(),
        )
        .unwrap();
    }

    fn verify(&self) -> Result<()> {
        verify(
            &self.status,
            &json!("current-request"),
            Some("current-origin"),
            &self.roster,
        )
    }

    fn recover(&self) -> Result<Option<UpdateRoster>> {
        super::super::recover_prepared(
            &self.checkpoint,
            &self.attempt,
            &self.source,
            &self.socket,
            /*live_hello*/ None,
        )
    }
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[test]
fn exact_raw_retention_is_self_contained_with_distinct_current_and_adopted_requests() {
    let fixture = Fixture::new();
    fixture.publish().unwrap();
    let normalized = serde_json::to_value(&fixture.roster).unwrap();
    let roster_text = serde_json::to_string(&normalized).unwrap();
    assert_eq!(
        fixture.read_record(),
        json!({
            "formatVersion":1,"currentRequestId":"current-request",
            "adoptedUpdateId":"adopted-request","socketPath":fixture.socket,
            "origin":"current-origin","targetVersion":crate::config::version(),
            "predecessor":{"pid":42,"processStartId":"birth","supervisorGeneration":"generation"},
            "sourceManifestText":fixture.source_text,"sourceSHA256":digest(&fixture.source_text),
            "attemptText":fixture.attempt_text,"attemptSHA256":digest(&fixture.attempt_text),
            "roster":normalized,"rosterText":roster_text,"rosterSHA256":digest(&roster_text),
        })
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(fixture.status.with_extension("recovery.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    std::fs::remove_file(&fixture.source).unwrap();
    std::fs::remove_file(&fixture.attempt).unwrap();
    std::fs::remove_file(&fixture.checkpoint).unwrap();
    fixture.verify().unwrap();
    assert_eq!(
        std::fs::read_dir(fixture.directory.path()).unwrap().count(),
        1
    );
}

#[test]
fn immutable_publication_rejects_retries_and_collisions_without_replacing_bytes() {
    let fixture = Fixture::new();
    fixture.publish().unwrap();
    let path = fixture.status.with_extension("recovery.json");
    let original = std::fs::read(&path).unwrap();
    assert!(fixture.publish().is_err());
    assert!(publish(
        &fixture.status,
        &UpdateId::from("another-current-request".to_string()),
        Some("another-origin"),
        &fixture.roster,
        &fixture.attempt,
        &fixture.source,
        /*prepared_source*/ None,
    )
    .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    fixture.verify().unwrap();
    assert!(verify(
        &fixture.status,
        &json!("another-current-request"),
        Some("current-origin"),
        &fixture.roster
    )
    .is_err());
    assert!(std::fs::read_dir(fixture.directory.path())
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
}

#[cfg(unix)]
#[test]
fn immutable_publication_does_not_follow_an_existing_symlink() {
    let fixture = Fixture::new();
    let target = fixture.directory.path().join("untouched");
    std::fs::write(&target, b"keep these bytes").unwrap();
    std::os::unix::fs::symlink(&target, fixture.status.with_extension("recovery.json")).unwrap();
    assert!(fixture.publish().is_err());
    assert_eq!(std::fs::read(target).unwrap(), b"keep these bytes");
    assert!(fixture.verify().is_err());
}

#[test]
fn corrupt_raw_source_attempt_and_roster_text_fail_digest_verification() {
    for field in ["sourceManifestText", "attemptText", "rosterText"] {
        let fixture = Fixture::new();
        fixture.publish().unwrap();
        let mut record = fixture.read_record();
        record[field] = json!("corrupt");
        fixture.write_record(&record);
        assert!(fixture.verify().is_err(), "{field}");
    }
}

#[test]
fn self_consistent_hashes_do_not_hide_source_attempt_or_roster_mismatch() {
    for (field, digest_field, value_field, replacement) in [
        (
            "sourceManifestText",
            "sourceSHA256",
            "createdAt",
            "different-source",
        ),
        (
            "attemptText",
            "attemptSHA256",
            "updateId",
            "different-owner-request",
        ),
        (
            "rosterText",
            "rosterSHA256",
            "created_at",
            "different-roster",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.publish().unwrap();
        let mut record = fixture.read_record();
        let mut value: Value = serde_json::from_str(record[field].as_str().unwrap()).unwrap();
        value[value_field] = json!(replacement);
        let text = serde_json::to_string(&value).unwrap();
        record[digest_field] = json!(digest(&text));
        record[field] = json!(text);
        if field == "rosterText" {
            record["roster"] = value;
        }
        fixture.write_record(&record);
        assert!(fixture.verify().is_err(), "{field}");
    }
}

#[test]
fn binding_mismatches_are_not_accepted() {
    for field in [
        "formatVersion",
        "currentRequestId",
        "adoptedUpdateId",
        "socketPath",
        "targetVersion",
        "predecessor",
        "origin",
    ] {
        let fixture = Fixture::new();
        fixture.publish().unwrap();
        let mut record = fixture.read_record();
        record[field] = json!("wrong");
        fixture.write_record(&record);
        assert!(fixture.verify().is_err(), "{field}");
    }
}

#[test]
fn publication_rejects_changed_or_invalid_source_and_attempt_without_removing_scratch() {
    for field in ["source", "attempt", "checkpoint", "invalid-utf8"] {
        let fixture = Fixture::new();
        if field == "source" {
            let mut source: Value = serde_json::from_str(&fixture.source_text).unwrap();
            source["createdAt"] = json!("not-the-checkpoint");
            std::fs::write(&fixture.source, serde_json::to_vec(&source).unwrap()).unwrap();
        } else if field == "attempt" {
            let mut attempt: Value = serde_json::from_str(&fixture.attempt_text).unwrap();
            attempt["updateId"] = json!("not-the-checkpoint");
            std::fs::write(&fixture.attempt, serde_json::to_vec(&attempt).unwrap()).unwrap();
        } else if field == "checkpoint" {
            let mut roster = fixture.roster.clone();
            roster.created_at = "not-the-source".to_string();
            super::super::persist(&fixture.checkpoint, &serde_json::to_value(&roster).unwrap())
                .unwrap();
            assert!(fixture.recover().is_err());
            continue;
        } else {
            std::fs::write(&fixture.source, [0xff]).unwrap();
        }
        assert!(fixture.publish().is_err(), "{field}");
        assert!(fixture.recover().is_err(), "{field}");
        assert!(!fixture.status.with_extension("recovery.json").exists());
        assert!(fixture.source.exists() && fixture.attempt.exists() && fixture.checkpoint.exists());
    }
}

#[test]
fn existing_checkpoint_requires_actual_source_and_attempt_provenance() {
    for missing in ["source", "attempt"] {
        let fixture = Fixture::new();
        assert!(fixture.recover().unwrap().is_some());
        std::fs::remove_file(if missing == "source" {
            &fixture.source
        } else {
            &fixture.attempt
        })
        .unwrap();
        assert!(fixture.recover().is_err(), "{missing}");
        assert!(fixture.checkpoint.exists());
    }
}

#[test]
fn successful_prepare_raw_source_cannot_change_before_publication() {
    let fixture = Fixture::new();
    let changed_formatting = format!("{}\n", fixture.source_text);
    std::fs::write(&fixture.source, changed_formatting).unwrap();
    assert!(publish(
        &fixture.status,
        &UpdateId::from("current-request".to_string()),
        Some("current-origin"),
        &fixture.roster,
        &fixture.attempt,
        &fixture.source,
        Some(&fixture.source_text),
    )
    .is_err());
    assert!(!fixture.status.with_extension("recovery.json").exists());
}

#[tokio::test]
async fn missing_or_corrupt_sidecar_fences_replacement_boot_before_status_transition() {
    for corrupt in [false, true] {
        let fixture = Fixture::new();
        if corrupt {
            fixture.publish().unwrap();
            let mut record = fixture.read_record();
            record["sourceSHA256"] = json!("bad-digest");
            fixture.write_record(&record);
        }
        let status = std::sync::Arc::new(tokio::sync::Mutex::new(json!({
            "phase":"starting","requestId":"current-request"
        })));
        let outcome = super::super::boot_roster(
            &fixture.socket,
            &fixture.status,
            &fixture.checkpoint,
            &fixture.roster,
            Some("current-origin"),
            &status,
            &pa_types::daemon::update_flow::UpdateTimeoutBudget::from_env(),
        )
        .await;
        assert!(outcome.is_err());
        assert_eq!(
            *status.lock().await,
            json!({"phase":"starting","requestId":"current-request"})
        );
        assert!(!fixture.status.exists());
        assert!(fixture.checkpoint.exists() && fixture.source.exists() && fixture.attempt.exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_reply_requires_durable_matching_source_and_archive_before_any_shutdown_connection()
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    for failure in ["sidecar-collision", "source-mismatch", "source-missing"] {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("old.sock");
        let status_path = directory.path().join("status.json");
        let sidecar = status_path.with_extension("recovery.json");
        let source_dir = directory.path().join("daemon-update-restarts");
        std::fs::create_dir(&source_dir).unwrap();
        let key = pa_daemon::paths::hash_key(&socket.to_string_lossy(), 64);
        let source_path = source_dir.join(format!("{key}.json"));
        let checkpoint = directory
            .path()
            .join("legacy-update-recovery")
            .join(format!("{key}.json"));
        if failure == "sidecar-collision" {
            std::fs::write(&sidecar, b"immutable earlier evidence").unwrap();
        }
        let source = source_path.clone();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (finished, mut completion) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let hello = json!({"type":"daemon_hello","supervisorPid":std::process::id(),
                "supervisorProcessStartId":"birth","supervisorGeneration":"generation",
                "appVersion":"0.9.8","protocol":{"name":"prime-agent.daemon","version":7}});
            write
                .write_all(format!("{hello}\n").as_bytes())
                .await
                .unwrap();
            let mut lines = BufReader::new(read).lines();
            let command: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(command["command"]["type"], "prepare_update_restart");
            let manifest = json!({"formatVersion":1,"createdAt":"reply","sessions":[]});
            if failure != "source-missing" {
                let mut durable = manifest.clone();
                if failure == "source-mismatch" {
                    durable["createdAt"] = json!("not-the-reply");
                }
                std::fs::write(&source, serde_json::to_string_pretty(&durable).unwrap()).unwrap();
            }
            let response = json!({"type":"response","id":command["id"],
                "command":"prepare_update_restart","success":true,"data":manifest});
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            while let Some(line) = lines.next_line().await.unwrap() {
                assert!(
                    line.is_empty(),
                    "no further command after retention failure: {line}"
                );
            }
            tokio::select! {
                biased;
                connection = listener.accept() => panic!("no shutdown reconnect after retention failure: {connection:?}"),
                _ = &mut completion => {}
            }
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::super::run_in(
                &socket,
                &status_path,
                directory.path(),
                Some("old-handle"),
                &[directory.path().join("owners")],
            ),
        )
        .await
        .expect("retention failure must finish without shutdown or boot")
        .unwrap();
        finished.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, 1, "{failure}");
        let status: Value = serde_json::from_slice(&std::fs::read(&status_path).unwrap()).unwrap();
        assert_eq!(status["phase"], "failed");
        assert!(checkpoint.with_extension("attempt.json").exists());
        if failure == "sidecar-collision" {
            assert!(checkpoint.exists() && source_path.exists());
            assert_eq!(
                std::fs::read(&sidecar).unwrap(),
                b"immutable earlier evidence"
            );
        } else {
            assert!(!checkpoint.exists() && !sidecar.exists());
        }
    }
}

#[tokio::test]
async fn absent_predecessor_checkpoint_recovery_must_archive_before_replacement_boot() {
    let fixture = Fixture::new();
    let key = pa_daemon::paths::hash_key(&fixture.socket.to_string_lossy(), 64);
    let active = fixture.directory.path().join("legacy-update-recovery");
    let sources = fixture.directory.path().join("daemon-update-restarts");
    std::fs::create_dir(&active).unwrap();
    std::fs::create_dir(&sources).unwrap();
    let checkpoint = active.join(format!("{key}.json"));
    std::fs::copy(&fixture.checkpoint, &checkpoint).unwrap();
    std::fs::copy(&fixture.attempt, checkpoint.with_extension("attempt.json")).unwrap();
    std::fs::copy(&fixture.source, sources.join(format!("{key}.json"))).unwrap();
    let sidecar = fixture.status.with_extension("recovery.json");
    std::fs::write(&sidecar, b"earlier immutable archive").unwrap();
    assert_eq!(
        super::super::run_in(
            &fixture.socket,
            &fixture.status,
            fixture.directory.path(),
            Some("current-origin"),
            &[fixture.directory.path().join("owners")],
        )
        .await
        .unwrap(),
        1
    );
    let status: Value = serde_json::from_slice(&std::fs::read(&fixture.status).unwrap()).unwrap();
    assert_eq!(status["phase"], "failed");
    assert!(status["message"]
        .as_str()
        .unwrap()
        .contains("publish immutable legacy recovery sidecar"));
    assert_eq!(
        std::fs::read(&sidecar).unwrap(),
        b"earlier immutable archive"
    );
    assert!(checkpoint.exists() && checkpoint.with_extension("attempt.json").exists());
}

#[test]
fn a_fresh_status_gets_a_new_bound_sidecar_without_rewriting_prior_evidence() {
    let fixture = Fixture::new();
    fixture.publish().unwrap();
    let original = fixture.read_record();
    let next_status = fixture.directory.path().join("retry-status.json");
    publish(
        &next_status,
        &UpdateId::from("retry-current-request".to_string()),
        Some("retry-origin"),
        &fixture.roster,
        &fixture.attempt,
        &fixture.source,
        /*prepared_source*/ None,
    )
    .unwrap();
    let next: Value = serde_json::from_slice(
        &std::fs::read(next_status.with_extension("recovery.json")).unwrap(),
    )
    .unwrap();
    let mut expected = original.clone();
    expected["currentRequestId"] = json!("retry-current-request");
    expected["origin"] = json!("retry-origin");
    assert_eq!(next, expected);
    assert_eq!(fixture.read_record(), original);
    verify(
        &next_status,
        &json!("retry-current-request"),
        Some("retry-origin"),
        &fixture.roster,
    )
    .unwrap();
}

#[test]
fn current_origin_is_bound_to_the_coordinator_not_the_adopted_attempt() {
    let fixture = Fixture::new();
    fixture.publish().unwrap();
    fixture.verify().unwrap();
    assert!(verify(
        &fixture.status,
        &json!("current-request"),
        Some("adopted-origin"),
        &fixture.roster
    )
    .is_err());
    assert!(verify(
        &fixture.status,
        &json!("current-request"),
        /*origin*/ None,
        &fixture.roster
    )
    .is_err());
}
