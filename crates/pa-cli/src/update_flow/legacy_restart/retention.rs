//! Immutable legacy-sidecar-v1 evidence, independent of active restart scratch.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use pa_types::daemon::update_flow::{UpdateId, UpdateRoster};
use serde_json::{json, Value};

use super::identity_from_hello;

/// Rebuild provenance from the actual TS source and recorded prepare owner.
pub(super) fn reconvert(
    source_text: &str,
    attempt_text: &str,
    socket: &Path,
) -> Result<UpdateRoster> {
    let attempt: Value = serde_json::from_str(attempt_text)?;
    anyhow::ensure!(
        attempt["socket"] == socket.to_string_lossy().as_ref()
            && attempt["version"] == crate::config::version(),
        "prepared attempt belongs to another socket or release"
    );
    let owner = identity_from_hello(&attempt["hello"]);
    anyhow::ensure!(
        owner.pid > 0
            && owner
                .process_start_id
                .as_deref()
                .is_some_and(|id| !id.is_empty())
            && owner
                .supervisor_generation
                .as_deref()
                .is_some_and(|id| !id.is_empty()),
        "prepared attempt has no fixed predecessor identity"
    );
    anyhow::ensure!(
        attempt["origin"].is_null() || attempt["origin"].is_string(),
        "prepared attempt has an invalid origin"
    );
    anyhow::ensure!(
        attempt["startedAt"].as_u64().is_some()
            && attempt["previousDigest"] != pa_daemon::paths::hash_key(source_text, 64),
        "prepared attempt has no start time or its source predates preparation"
    );
    let id: UpdateId = serde_json::from_value(attempt["updateId"].clone())?;
    anyhow::ensure!(!id.0.is_empty(), "prepared attempt has no update id");
    let source: Value = serde_json::from_str(source_text)?;
    let roster = super::super::legacy_roster::convert(
        &source,
        &attempt["hello"],
        &id,
        socket,
        attempt["origin"].as_str(),
    )?;
    super::validate_recovery(&roster, socket, crate::config::version())?;
    Ok(roster)
}

pub(super) fn publish(
    status_path: &Path,
    current_id: &UpdateId,
    origin: Option<&str>,
    roster: &UpdateRoster,
    attempt_path: &Path,
    source_path: &Path,
    prepared_source: Option<&str>,
) -> Result<()> {
    let source_text = std::fs::read_to_string(source_path)
        .context("retain the original TypeScript recovery manifest")?;
    let attempt_text =
        std::fs::read_to_string(attempt_path).context("retain the original preparation attempt")?;
    if let Some(expected) = prepared_source {
        anyhow::ensure!(
            source_text == expected,
            "TypeScript source changed after its successful prepare reply"
        );
    }
    let converted = reconvert(&source_text, &attempt_text, Path::new(&roster.socket_path))?;
    let normalized = serde_json::to_value(roster)?;
    anyhow::ensure!(
        serde_json::to_value(&converted)? == normalized,
        "recovery checkpoint differs from its source and attempt"
    );
    let roster_text = serde_json::to_string(&normalized)?;
    let record = json!({
        "formatVersion": 1,
        "currentRequestId": current_id,
        "adoptedUpdateId": roster.update_id,
        "socketPath": roster.socket_path,
        "origin": origin,
        "targetVersion": roster.binary.to_version,
        "predecessor": predecessor(roster),
        "sourceSHA256": pa_daemon::paths::hash_key(&source_text, 64),
        "sourceManifestText": source_text,
        "attemptSHA256": pa_daemon::paths::hash_key(&attempt_text, 64),
        "attemptText": attempt_text,
        "rosterSHA256": pa_daemon::paths::hash_key(&roster_text, 64),
        "rosterText": roster_text,
        "roster": normalized,
    });
    let path = status_path.with_extension("recovery.json");
    publish_immutable(&path, &record)?;
    verify(status_path, &json!(current_id), origin, roster)
}

fn predecessor(roster: &UpdateRoster) -> Value {
    json!({
        "pid": roster.supervisor.pid,
        "processStartId": roster.supervisor.process_start_id,
        "supervisorGeneration": roster.supervisor.generation,
    })
}

/// Every destructive edge requires this independently readable durable record.
pub(super) fn verify(
    status_path: &Path,
    current_id: &Value,
    origin: Option<&str>,
    roster: &UpdateRoster,
) -> Result<()> {
    let path = status_path.with_extension("recovery.json");
    let metadata = std::fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        metadata.is_file(),
        "legacy recovery sidecar is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o777 == 0o600,
            "legacy recovery sidecar is not private (0600)"
        );
    }
    let record: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    anyhow::ensure!(
        record["formatVersion"] == 1
            && current_id.as_str().is_some_and(|id| !id.is_empty())
            && record["currentRequestId"] == *current_id
            && record["adoptedUpdateId"] == json!(roster.update_id)
            && record["socketPath"] == roster.socket_path
            && record["targetVersion"] == roster.binary.to_version
            && record["predecessor"] == predecessor(roster)
            && record["origin"] == json!(origin),
        "legacy recovery sidecar binding mismatch"
    );
    let source = record["sourceManifestText"]
        .as_str()
        .context("sidecar has no raw source")?;
    let attempt = record["attemptText"]
        .as_str()
        .context("sidecar has no raw attempt")?;
    let roster_text = record["rosterText"]
        .as_str()
        .context("sidecar has no roster text")?;
    anyhow::ensure!(
        record["sourceSHA256"] == pa_daemon::paths::hash_key(source, 64)
            && record["attemptSHA256"] == pa_daemon::paths::hash_key(attempt, 64)
            && record["rosterSHA256"] == pa_daemon::paths::hash_key(roster_text, 64),
        "legacy recovery sidecar digest mismatch"
    );
    let normalized = serde_json::to_value(roster)?;
    anyhow::ensure!(
        serde_json::from_str::<Value>(roster_text)? == record["roster"]
            && record["roster"] == normalized
            && serde_json::to_value(reconvert(source, attempt, Path::new(&roster.socket_path))?)?
                == normalized,
        "legacy recovery sidecar source, attempt, and roster disagree"
    );
    Ok(())
}

fn publish_immutable(path: &Path, record: &Value) -> Result<()> {
    let parent = path.parent().context("sidecar has no parent directory")?;
    let temporary = parent.join(format!(".legacy-recovery-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| -> Result<()> {
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        // A same-directory hard link publishes atomically without replacing an
        // existing inode. Rename would silently destroy an earlier sidecar.
        std::fs::hard_link(&temporary, path)
            .context("publish immutable legacy recovery sidecar (never overwrite)")?;
        Ok(())
    })();
    drop(file);
    let cleanup = std::fs::remove_file(&temporary);
    // Flush the publication and temporary-name removal even on a collision.
    let directory_sync = std::fs::File::open(parent)?.sync_all();
    result?;
    cleanup?;
    directory_sync?;
    Ok(())
}

#[cfg(test)]
mod tests;
