// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines/casts).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! Lock-artifact compatibility through the production call sites: the backends must leave the TS
//! `proper-lockfile` artifacts — an empty DIRECTORY at `{file}.lock` (a holder removes it on
//! release, a crashed holder leaves it stale, a contender judges staleness).
//! Unix auth deliberately rejects legacy regular FILE artifacts, even after a flock is released.
//! Transition from that legacy Rust protocol requires quiescent offline resolution, never automatic
//! artifact healing: a momentarily unheld flock does not exclude an older process with an open inode.
//! Settings retain their separate shared-lock behavior.
#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use pa_core::auth::{AuthStorageBackend, FileAuthStorageBackend};
use pa_core::settings::{FileSettingsStorage, SettingsScope, SettingsStorage};

/// Every staleness threshold in play is far below this.
const STALE: Duration = Duration::from_secs(10);

fn lock_path(file: &Path) -> std::path::PathBuf {
    pa_core::platform::LockDir::path_for(file)
}

/// Set a path's mtime far in the past, the way a crashed holder's artifact looks after a while.
fn age(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path to cstring");
    let ancient = libc::timespec {
        tv_sec: 1_000_000_000,
        tv_nsec: 0,
    };
    let times = [ancient, ancient];
    let result = unsafe { libc::utimensat(-1, c_path.as_ptr(), times.as_ptr(), 0) };
    assert_eq!(result, 0, "aging {}", path.display());
}

/// Hold a pre-compat flock on the artifact FILE the way the legacy Rust builds did.
struct Flock {
    file: std::fs::File,
}

impl Flock {
    fn hold(path: &Path) -> Self {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open legacy lock file");
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(result, 0, "taking the legacy flock");
        Flock { file }
    }
}

impl Drop for Flock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[test]
fn settings_write_leaves_no_lock_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let storage = FileSettingsStorage::new(dir.path().join("cwd"), dir.path().join("agent"));
    storage
        .with_lock(SettingsScope::Global, &mut |current| {
            assert_eq!(current, None);
            Some(r#"{ "defaultProvider": "prime-inference" }"#.to_string())
        })
        .unwrap();
    let settings = dir.path().join("agent").join("settings.json");
    assert!(settings.is_file());
    assert!(
        !lock_path(&settings).exists(),
        "a released write leaves no lock artifact"
    );
}

#[test]
fn settings_write_reclaims_a_crashed_holders_lock_directory() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("agent").join("settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let lock = lock_path(&settings);
    std::fs::create_dir(&lock).unwrap();
    age(&lock);
    let storage = FileSettingsStorage::new(dir.path().join("cwd"), dir.path().join("agent"));
    storage
        .with_lock(SettingsScope::Global, &mut |current| {
            assert_eq!(current, None);
            Some("{}".to_string())
        })
        .unwrap();
    assert!(settings.is_file(), "the stale lock did not block the write");
    assert!(!lock.exists(), "the write released its own lock");
}

#[test]
fn settings_write_reports_contention_on_a_live_lock_directory() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("agent").join("settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let guard = pa_core::platform::LockDir::acquire(&settings, STALE).unwrap();
    let storage = FileSettingsStorage::new(dir.path().join("cwd"), dir.path().join("agent"));
    let error = storage
        .with_lock(SettingsScope::Global, &mut |_| Some("{}".to_string()))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Failed to acquire settings lock"),
        "contention surfaces as a lock error: {error}"
    );
    assert!(!settings.exists());
    assert!(lock_path(&settings).exists(), "the live lock is intact");
    drop(guard);
}

/// Existing source auth and foreign artifact identity must survive a failed acquisition.
/// Reads may update atime; it is not part of the lock ownership or preservation contract.
#[derive(Debug, PartialEq, Eq)]
struct AuthArtifacts {
    auth: Vec<u8>,
    lock: Vec<u8>,
    lock_identity: (u64, u64, u32, i64, i64),
}

fn artifacts(auth: &Path, lock: &Path) -> AuthArtifacts {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(lock).unwrap();
    AuthArtifacts {
        auth: std::fs::read(auth).unwrap(),
        lock: std::fs::read(lock).unwrap(),
        lock_identity: (
            metadata.dev(),
            metadata.ino(),
            metadata.mode(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        ),
    }
}

fn assert_legacy_refused(auth: &Path, lock: &Path, expected: &AuthArtifacts) {
    #[derive(Debug)]
    enum Transaction {
        Ordinary,
        Refresh,
    }
    let backend = FileAuthStorageBackend::new(auth);
    for transaction in [Transaction::Ordinary, Transaction::Refresh] {
        let mut called = false;
        let mut update = |_| {
            called = true;
            Ok(((), Some("{}".to_string())))
        };
        let result = match transaction {
            Transaction::Ordinary => backend.with_lock(&mut update),
            Transaction::Refresh => backend.with_refresh_lock(&mut update),
        };
        assert!(
            !called,
            "unsupported {transaction:?} lock entered update callback"
        );
        assert_eq!(
            &artifacts(auth, lock),
            expected,
            "unsupported {transaction:?} lock changed source auth or foreign artifact"
        );
        let error = result.unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::Unsupported),
            "legacy {transaction:?} artifacts require offline resolution, not contention: {error}"
        );
    }
}

const SOURCE_AUTH: &str = r#"{
    "fixture": { "type": "api_key", "key": "fixture-only-never-real",
        "futureMetadata": { "nested": [null, false, { "keep": "exact" }] } },
    "unknown-provider": { "type": "future_kind", "opaque": [null, { "keep": 17 }] }
}
"#;

#[test]
fn auth_write_rejects_a_stale_legacy_file_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let auth = dir.path().join("auth.json");
    let lock = lock_path(&auth);
    std::fs::write(&auth, SOURCE_AUTH).unwrap();
    std::fs::write(&lock, "pre-compat flock artifact").unwrap();
    age(&lock);
    let expected = artifacts(&auth, &lock);
    assert_legacy_refused(&auth, &lock, &expected);
}

#[test]
fn auth_write_rejects_a_held_and_released_legacy_flock_without_mutation() {
    // Even a released flock cannot prove that no legacy writer already has
    // this inode open. Never unlink it or silently switch locking protocols.
    let dir = tempfile::tempdir().unwrap();
    let auth = dir.path().join("auth.json");
    let lock = lock_path(&auth);
    std::fs::write(&auth, SOURCE_AUTH).unwrap();
    std::fs::write(&lock, "pre-compat flock artifact").unwrap();
    age(&lock);
    let expected = artifacts(&auth, &lock);
    let flock = Flock::hold(&lock);
    assert_legacy_refused(&auth, &lock, &expected);
    drop(flock);
    assert_legacy_refused(&auth, &lock, &expected);
}
