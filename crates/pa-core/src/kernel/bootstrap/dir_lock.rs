//! Cross-process bootstrap lock for the kernel venv, published atomically
//! with `link(2)`. Existing locks are never reclaimed automatically: even a
//! dead parent PID cannot prove that its runtime-writing children have stopped.
//! Older binaries that reclaim locks do not participate in this guarantee.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::venv::{BOOTSTRAP_LOCK_NAME, BOOTSTRAP_LOCK_RETRY_MS};

// Bound contention instead of removing an incumbent based on a stale path
// observation. Interrupted setup requires recovery with all writers stopped.
const BOOTSTRAP_LOCK_WAIT: Duration = Duration::from_secs(120);

/// A `link(2)`-published lock file: born with owner content, EEXIST the only
/// collision signal.
pub(crate) enum DirLockAttempt {
    Acquired,
    Held,
}

fn try_acquire_dir_lock(lock_path: &Path) -> anyhow::Result<DirLockAttempt> {
    std::fs::create_dir_all(lock_path.parent().unwrap_or(Path::new("/")))?;
    let token = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
    let temp_path = lock_path.with_file_name(format!(
        "{}.candidate-{}",
        lock_path.file_name().unwrap_or_default().to_string_lossy(),
        token
    ));
    {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        crate::platform::perms::set_private_mode(&mut options);
        let mut file = options.open(&temp_path)?;
        writeln!(file, "{}", std::process::id())?;
    }
    // Publishing is the only acquisition signal. Never rename or remove an
    // incumbent, including a legacy directory or a lock naming a dead PID.
    let published = std::fs::hard_link(&temp_path, lock_path);
    let _ = std::fs::remove_file(&temp_path);
    match published {
        Ok(()) => Ok(DirLockAttempt::Acquired),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(DirLockAttempt::Held),
        Err(error) => Err(error.into()),
    }
}

/// Serialize concurrent bootstraps across processes on the same venv.
pub(crate) async fn acquire_bootstrap_lock(venv: &Path) -> anyhow::Result<impl Drop> {
    let lock_dir = venv.with_file_name(format!(
        "{}{}",
        venv.file_name().unwrap_or_default().to_string_lossy(),
        BOOTSTRAP_LOCK_NAME
    ));
    std::fs::create_dir_all(lock_dir.parent().unwrap_or(Path::new("/")))?;
    let deadline = tokio::time::Instant::now() + BOOTSTRAP_LOCK_WAIT;
    loop {
        match try_acquire_dir_lock(&lock_dir)? {
            DirLockAttempt::Acquired => {
                struct Guard(PathBuf);
                impl Drop for Guard {
                    fn drop(&mut self) {
                        let _ = std::fs::remove_file(&self.0);
                    }
                }
                return Ok(Guard(lock_dir));
            }
            DirLockAttempt::Held => {
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "Timed out after {} seconds waiting for the Python kernel bootstrap lock at {}. \
                         Another Prime Agent process may still be setting up this runtime. \
                         Retry after it finishes. If setup was interrupted, verify that no process \
                         is setting up this runtime before removing this lock and retrying.",
                        BOOTSTRAP_LOCK_WAIT.as_secs(),
                        lock_dir.display()
                    );
                }
                tokio::time::sleep_until(deadline.min(
                    tokio::time::Instant::now() + Duration::from_millis(BOOTSTRAP_LOCK_RETRY_MS),
                ))
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn reaped_parent_pid_times_out_without_reclaiming_the_lock() {
        use std::os::unix::fs::MetadataExt;

        let root = tempfile::tempdir().unwrap();
        let venv = root.path().join("kernel-venv");
        let lock = root.path().join("kernel-venv.bootstrap.lock");
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success());
        assert!(!crate::platform::process::pid_exists(pid));
        let owner = format!("{pid}\n");
        std::fs::write(&lock, &owner).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        std::fs::File::open(&lock)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        let before = std::fs::metadata(&lock).unwrap();

        let result = tokio::time::timeout(
            BOOTSTRAP_LOCK_WAIT + Duration::from_secs(1),
            acquire_bootstrap_lock(&venv),
        )
        .await
        .expect("a dead parent must not cause an unbounded wait");
        let error = match result {
            Ok(_) => panic!("a dead parent cannot authorize reclaiming its lock"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("Timed out after 120 seconds"), "{error}");
        assert!(error.contains("verify that no process"), "{error}");
        let after = std::fs::metadata(&lock).unwrap();
        assert_eq!(
            (after.dev(), after.ino(), after.modified().unwrap()),
            (before.dev(), before.ino(), before.modified().unwrap())
        );
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), owner);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn old_unknown_owner_files_time_out_without_mutating_the_lock() {
        for owner in [
            b"".as_slice(),
            b"corrupt\n",
            b"0\n",
            b"4294967296\n",
            b"\xff",
        ] {
            let root = tempfile::tempdir().unwrap();
            let venv = root.path().join("kernel-venv");
            let lock = root.path().join("kernel-venv.bootstrap.lock");
            std::fs::write(&lock, owner).unwrap();
            let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
            std::fs::File::open(&lock)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();

            let result = tokio::time::timeout(
                BOOTSTRAP_LOCK_WAIT + Duration::from_secs(1),
                acquire_bootstrap_lock(&venv),
            )
            .await
            .expect("unknown owner must have a bounded wait");
            let error = match result {
                Ok(_) => panic!("must not acquire an unknown owner's lock"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains("Timed out after 120 seconds"), "{error}");
            assert!(error.contains("verify that no process"), "{error}");
            assert_eq!(std::fs::read(&lock).unwrap(), owner);
            assert_eq!(std::fs::metadata(&lock).unwrap().modified().unwrap(), old);
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn old_legacy_directory_without_pid_times_out_without_moving_the_lock() {
        let root = tempfile::tempdir().unwrap();
        let venv = root.path().join("kernel-venv");
        let lock = root.path().join("kernel-venv.bootstrap.lock");
        std::fs::create_dir(&lock).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        std::fs::File::open(&lock)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();

        let result = tokio::time::timeout(
            BOOTSTRAP_LOCK_WAIT + Duration::from_secs(1),
            acquire_bootstrap_lock(&venv),
        )
        .await
        .expect("unknown legacy owner must have a bounded wait");
        assert!(result.is_err());
        assert!(lock.is_dir());
        assert_eq!(std::fs::metadata(&lock).unwrap().modified().unwrap(), old);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn live_pid_file_has_a_bounded_wait_without_removing_the_owner() {
        let root = tempfile::tempdir().unwrap();
        let venv = root.path().join("kernel-venv");
        let lock = root.path().join("kernel-venv.bootstrap.lock");
        // An unrelated live PID is indistinguishable from a recycled owner.
        let owner = format!("{}\n", std::process::id());
        std::fs::write(&lock, &owner).unwrap();

        let result = tokio::time::timeout(
            BOOTSTRAP_LOCK_WAIT + Duration::from_secs(1),
            acquire_bootstrap_lock(&venv),
        )
        .await
        .expect("lock acquisition must return an error rather than wait forever");
        let error = match result {
            Ok(_) => panic!("must not acquire a live owner's lock"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("Timed out"), "{error}");
        assert!(error.contains(&lock.display().to_string()), "{error}");
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), owner);
    }

    #[tokio::test(start_paused = true)]
    async fn legacy_directory_has_a_bounded_wait_without_removing_the_owner() {
        let root = tempfile::tempdir().unwrap();
        let venv = root.path().join("kernel-venv");
        let lock = root.path().join("kernel-venv.bootstrap.lock");
        std::fs::create_dir(&lock).unwrap();
        let owner = format!("{}\n", std::process::id());
        std::fs::write(lock.join("pid"), &owner).unwrap();

        let result = tokio::time::timeout(
            BOOTSTRAP_LOCK_WAIT + Duration::from_secs(1),
            acquire_bootstrap_lock(&venv),
        )
        .await
        .expect("legacy lock acquisition must also have a deadline");
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(lock.join("pid")).unwrap(), owner);
    }

    #[tokio::test(start_paused = true)]
    async fn an_owner_releasing_before_the_deadline_allows_acquisition() {
        let root = tempfile::tempdir().unwrap();
        let venv = root.path().join("kernel-venv");
        let lock = root.path().join("kernel-venv.bootstrap.lock");
        let owner = acquire_bootstrap_lock(&venv).await.unwrap();
        let mut waiter = Box::pin(acquire_bootstrap_lock(&venv));
        assert!(matches!(
            futures::poll!(&mut waiter),
            std::task::Poll::Pending
        ));
        drop(owner);
        let successor = waiter.await.unwrap();
        assert!(lock.exists());
        drop(successor);
        assert!(!lock.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_waiter_preserves_the_owner_and_cleans_candidates() {
        let root = tempfile::tempdir().unwrap();
        let venv = root.path().join("kernel-venv");
        let lock = root.path().join("kernel-venv.bootstrap.lock");
        let owner = acquire_bootstrap_lock(&venv).await.unwrap();
        let mut waiter = Box::pin(acquire_bootstrap_lock(&venv));
        assert!(matches!(
            futures::poll!(&mut waiter),
            std::task::Poll::Pending
        ));
        drop(waiter);
        assert!(lock.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        drop(owner);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
