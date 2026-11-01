//! Process-lifetime session execution exclusion, separate from the session log's
//! per-append serialization. A held file lock prevents duplicate resident workers;
//! its courtesy header identifies a holder but never establishes ownership.

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const SESSION_LEASE_OWNER_ID_ENV: &str = "PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LockHolder {
    pid: u32,
    active_session_id: Option<String>,
}

/// Error matching the TS wire shape (`session_already_active`): `Display`
/// keeps the TS message byte-identical; the rendering lives in
/// [`crate::hold_refusal`].
#[derive(Debug, thiserror::Error)]
#[error("Session is already active in {owner}: {session_path}")]
pub struct SessionAlreadyActiveError {
    pub session_path: String,
    pub active_session_id: Option<String>,
    pub owner: String,
    /// The live holder's pid: what the refusal's holder classification
    /// resolves into a product flavor.
    pub holder_pid: Option<u32>,
}

impl SessionAlreadyActiveError {
    /// The typed wire info for the refusal (`session_already_active`,
    /// the TS `serializeDaemonError` shape).
    #[must_use]
    pub fn error_info(&self) -> pa_types::daemon::DaemonErrorInfo {
        pa_types::daemon::DaemonErrorInfo::SessionAlreadyActive {
            session_path: self.session_path.clone(),
            active_session_id: self.active_session_id.clone(),
        }
    }

    fn for_owner(session_path: &str, owner: Option<&LockHolder>) -> Self {
        SessionAlreadyActiveError {
            session_path: session_path.to_string(),
            active_session_id: owner
                .and_then(|o| o.active_session_id.clone())
                .filter(|id| !id.is_empty()),
            // An owner without a session id is still identifiable by its pid.
            owner: owner
                .and_then(|o| o.active_session_id.clone())
                .filter(|id| !id.is_empty())
                .or_else(|| owner.map(|o| format!("another process (pid {})", o.pid)))
                .unwrap_or_else(|| "another process".to_string()),
            holder_pid: owner.map(|o| o.pid),
        }
    }
}

pub fn canonical_session_path(path: &Path) -> PathBuf {
    match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(_) => match path.parent().map(std::path::Path::canonicalize) {
            Some(Ok(parent)) => parent.join(path.file_name().unwrap_or_default()),
            _ => path.to_path_buf(),
        },
    }
}

/// `proc:<starttime>` start identity (TS `getProcessStartId`); shared with
/// pa-core through `pa_types::platform`.
#[must_use]
pub fn get_process_start_id(pid: u32) -> Option<String> {
    pa_types::platform::process::process_start_id(pid)
}

/// True only for a process that is actually running: zombies do not count.
///
/// # Errors
///
/// Returns an error when the platform cannot answer the liveness check.
pub fn is_process_alive(pid: u32) -> anyhow::Result<bool> {
    pa_types::platform::process::is_process_alive(pid)
}

fn lock_path(agent_dir: &Path, session_path: &Path) -> PathBuf {
    let canonical = canonical_session_path(session_path);
    let key = Sha256::digest(canonical.to_string_lossy().as_bytes())
        .iter()
        .fold(String::new(), |mut key, b| {
            use std::fmt::Write;
            write!(key, "{b:02x}").expect("write to String");
            key
        });
    agent_dir.join("session-leases").join(format!("{key}.lock"))
}

/// A session's resident execution lock. The file stays on disk permanently:
/// unlinking it could let a successor lock a different inode concurrently.
#[derive(Debug)]
pub struct SessionLease {
    pub session_path: PathBuf,
    opened_path: PathBuf,
    agent_dir: PathBuf,
    file: Mutex<Option<File>>,
}

impl SessionLease {
    pub(crate) fn write_sidecars(&self) {
        let _ = pa_core::session::window::flush_cache(&self.opened_path);
        let _ = crate::session_store::read_session_info(&self.opened_path);
        crate::session_store::persist_info_sidecar(&self.opened_path);
    }

    /// Release execution ownership after the caller has stopped session work.
    /// Explicit unlock also releases descriptors inherited by a child before exec.
    /// Process exit releases the lock when the last inherited descriptor closes.
    pub fn release(&self) {
        let mut file = self.file.lock().expect("session execution lock");
        if file.is_some() {
            self.write_sidecars();
            if let Some(file) = file.take() {
                let _ = file.unlock();
            }
        }
    }

    pub(crate) fn acquire_target(&self, path: &Path) -> Result<std::sync::Arc<Self>> {
        acquire_runtime_session_lease(path, &self.agent_dir).map(std::sync::Arc::new)
    }

    /// Validate a captured execution source. This does not serialize log writes.
    pub(crate) fn ensure_held(&self, path: &Path) -> Result<()> {
        ensure!(
            canonical_session_path(path) == self.session_path
                && self.file.lock().expect("session execution lock").is_some(),
            "session execution lock is not held for target"
        );
        Ok(())
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// Informational identity of a process holding the execution flock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveLeaseOwner {
    /// Zero means that the courtesy header could not identify the holder.
    pub pid: u32,
    pub active_session_id: Option<String>,
}

fn read_holder(file: &mut File) -> Option<LockHolder> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut content = String::new();
    file.take(4096).read_to_string(&mut content).ok()?;
    serde_json::from_str(&content).ok()
}

/// Probe the OS lock, not the courtesy header or a PID liveness check.
/// An unreadable header on a held lock still reports an anonymous live holder.
#[must_use]
pub fn live_lease_owner(agent_dir: &Path, session_path: &Path) -> Option<LiveLeaseOwner> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path(agent_dir, session_path))
        .ok()?;
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            None
        }
        Err(TryLockError::WouldBlock) => {
            let holder = read_holder(&mut file);
            Some(LiveLeaseOwner {
                pid: holder.as_ref().map_or(0, |holder| holder.pid),
                active_session_id: holder.and_then(|holder| holder.active_session_id),
            })
        }
        Err(TryLockError::Error(_)) => None,
    }
}

/// Acquire resident execution ownership without waiting. SessionLog acquires its
/// own distinct append lock per operation; holding this handle cannot block it.
///
/// # Errors
/// Returns the typed session-active refusal on contention, or the underlying IO error.
pub fn acquire_runtime_session_lease(
    session_path: &Path,
    agent_dir: &Path,
) -> Result<SessionLease> {
    let canonical = canonical_session_path(session_path);
    fs::create_dir_all(agent_dir.join("session-leases"))?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(agent_dir, &canonical))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(SessionAlreadyActiveError::for_owner(
                &canonical.to_string_lossy(),
                read_holder(&mut file).as_ref(),
            )
            .into());
        }
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    let holder = LockHolder {
        pid: std::process::id(),
        active_session_id: std::env::var(SESSION_LEASE_OWNER_ID_ENV).ok(),
    };
    // Informational only: a failed header write must not undo a valid flock.
    let header = serde_json::to_vec(&holder)?;
    if file.set_len(0).is_ok() {
        let _ = file.write_all(&header);
    }
    Ok(SessionLease {
        session_path: canonical,
        opened_path: session_path.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
        file: Mutex::new(Some(file)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn execution_lock_conflicts_until_last_owner_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let lease = Arc::new(acquire_runtime_session_lease(&path, dir.path()).unwrap());
        let shared = lease.clone();
        let error = acquire_runtime_session_lease(&path, dir.path()).unwrap_err();
        assert!(error.downcast_ref::<SessionAlreadyActiveError>().is_some());
        assert!(live_lease_owner(dir.path(), &path).is_some());
        drop(lease);
        assert!(acquire_runtime_session_lease(&path, dir.path()).is_err());
        drop(shared);
        assert!(live_lease_owner(dir.path(), &path).is_none());
        let next = acquire_runtime_session_lease(&path, dir.path()).unwrap();
        assert_eq!(next.session_path, canonical_session_path(&path));
    }

    #[test]
    fn release_keeps_the_lock_inode_and_old_handle_cannot_unlock_successor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let first = acquire_runtime_session_lease(&path, dir.path()).unwrap();
        let lock = lock_path(dir.path(), &path);
        let held_inode = File::open(&lock).unwrap();
        // A fork before exec shares the lock's open file description. Retain a
        // duplicate to prove release unlocks ownership, not just this descriptor.
        let inherited = first
            .file
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .try_clone()
            .unwrap();
        first.release();
        assert!(lock.is_file());
        assert!(first.ensure_held(&path).is_err());
        let second = acquire_runtime_session_lease(&path, dir.path()).unwrap();
        drop(inherited);
        first.release();
        drop(first);
        assert!(matches!(
            held_inode.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        assert!(acquire_runtime_session_lease(&path, dir.path()).is_err());
        second.release();
        held_inode.try_lock().unwrap();
    }

    #[test]
    fn courtesy_header_never_decides_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let lease = acquire_runtime_session_lease(&path, dir.path()).unwrap();
        fs::write(lock_path(dir.path(), &path), b"broken header").unwrap();
        assert_eq!(
            live_lease_owner(dir.path(), &path),
            Some(LiveLeaseOwner {
                pid: 0,
                active_session_id: None,
            })
        );
        assert!(acquire_runtime_session_lease(&path, dir.path()).is_err());
        lease.release();
        // A stale header naming a live PID cannot prevent acquisition.
        fs::write(
            lock_path(dir.path(), &path),
            serde_json::to_vec(&LockHolder {
                pid: std::process::id(),
                active_session_id: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(live_lease_owner(dir.path(), &path).is_none());
        acquire_runtime_session_lease(&path, dir.path()).unwrap();
    }

    #[test]
    fn resident_execution_lock_does_not_hold_the_append_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let _lease = acquire_runtime_session_lease(&path, dir.path()).unwrap();
        let mut log =
            pa_core::session_log::SessionLog::open(dir.path(), "session", Instant::now()).unwrap();
        let row = serde_json::json!({"message": "accepted"});
        assert!(matches!(
            log.append(&row, Instant::now()),
            pa_core::session_log::Outcome::Committed { seq: 1, .. }
        ));
        assert!(acquire_runtime_session_lease(&path, dir.path()).is_err());
    }

    // A real process holds the production lock until stdin closes. The parent
    // waits for an explicit ready line, not a timer or PID-based guess.
    #[test]
    fn execution_lock_process_helper() {
        let Some(dir) = std::env::var_os("PA_EXECUTION_LOCK_TEST_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let _lease = acquire_runtime_session_lease(&dir.join("session.jsonl"), &dir).unwrap();
        println!("EXECUTION_LOCK_READY");
        std::io::stdout().flush().unwrap();
        let mut byte = [0];
        let _ = std::io::stdin().read(&mut byte).unwrap();
    }

    #[test]
    fn process_exit_and_crash_release_execution_ownership_immediately() {
        for crash in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.jsonl");
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "lease::tests::execution_lock_process_helper",
                    "--nocapture",
                ])
                .env("PA_EXECUTION_LOCK_TEST_DIR", dir.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdout = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                assert_ne!(
                    stdout.read_line(&mut line).unwrap(),
                    0,
                    "helper exited before holding lock"
                );
                if line.contains("EXECUTION_LOCK_READY") {
                    break;
                }
                line.clear();
            }
            let error = acquire_runtime_session_lease(&path, dir.path()).unwrap_err();
            assert!(error.downcast_ref::<SessionAlreadyActiveError>().is_some());
            if crash {
                child.kill().unwrap();
            }
            drop(child.stdin.take());
            let status = child.wait().unwrap();
            assert_eq!(status.success(), !crash);
            assert!(live_lease_owner(dir.path(), &path).is_none());
            acquire_runtime_session_lease(&path, dir.path()).unwrap();
        }
    }
}
