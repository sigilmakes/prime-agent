//! Auth's long-lived `proper-lockfile` transaction guard. The directory must
//! remain empty so the TypeScript backend can reclaim a crashed holder.
//! Legacy regular-file and other non-directory artifacts are unsupported, not stale leases.
//! Their transition requires quiescent offline resolution: never probe or remove an artifact
//! while an older writer may retain its inode.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

const STALE_AFTER: Duration = Duration::from_secs(30);
const HEARTBEAT: Duration = Duration::from_secs(1);

/// Ordinary operations retain the existing portable lock protocol. Rotating
/// refresh requires the verified identity/heartbeat guard before token I/O.
#[derive(Clone, Copy)]
pub(super) enum TransactionKind {
    Ordinary,
    Refresh,
}

pub(super) enum AuthDocumentGuard {
    Verified(AuthDocumentLock),
    #[cfg(not(unix))]
    Ordinary(crate::platform::lock_dir::LockDir),
}

impl AuthDocumentGuard {
    pub(super) fn acquire(file: &Path, kind: TransactionKind) -> io::Result<Self> {
        let wait = match kind {
            TransactionKind::Ordinary => Duration::from_millis(200),
            TransactionKind::Refresh => Duration::from_secs(60),
        };
        #[cfg(not(unix))]
        if matches!(kind, TransactionKind::Ordinary) {
            // Preserve ordinary API-key reads/set/delete on unsupported platforms.
            // This is the pre-existing portable protocol, not a rotating-refresh
            // interoperation claim. Canonical paths are supplied by the backend.
            return crate::platform::lock_dir::LockDir::acquire_retrying(
                file,
                STALE_AFTER,
                10,
                Duration::from_millis(20),
            )
            .map(Self::Ordinary);
        }
        AuthDocumentLock::acquire(file, wait).map(Self::Verified)
    }

    pub(super) fn ensure_owned(&self) -> io::Result<()> {
        match self {
            Self::Verified(guard) => guard.ensure_owned(),
            #[cfg(not(unix))]
            Self::Ordinary(_guard) => Ok(()),
        }
    }
}

/// One filesystem lock, retained through the complete refresh transaction.
/// An unsupported identity check fails closed instead of guessing ownership.
pub(super) struct AuthDocumentLock {
    path: PathBuf,
    directory: File,
    lost: Arc<AtomicBool>,
    stop: mpsc::Sender<()>,
    heartbeat: Option<JoinHandle<()>>,
}

#[cfg(unix)]
fn same_directory(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.is_dir() && right.is_dir() && left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_directory(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    false
}

fn still_owned(path: &Path, directory: &File) -> bool {
    match (fs::symlink_metadata(path), directory.metadata()) {
        (Ok(named), Ok(held)) => same_directory(&named, &held),
        _ => false,
    }
}

impl AuthDocumentLock {
    pub(super) fn acquire(file: &Path, wait: Duration) -> io::Result<Self> {
        // A held directory handle is the generation identity. Portable filesystems
        // without it must not silently fall back to an unsafe mtime-only owner.
        if !cfg!(unix) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "safe cross-runtime auth locking requires directory identity support",
            ));
        }
        let mut name = file.as_os_str().to_os_string();
        name.push(".lock");
        let path = PathBuf::from(name);
        let deadline = Instant::now() + wait;
        loop {
            match fs::create_dir(&path) {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    // The TS async backend uses stale=30s and updates every 15s.
                    // Never apply the shorter sync backend's 10s stale window here.
                    let metadata = fs::symlink_metadata(&path).ok();
                    if metadata.as_ref().is_some_and(|meta| !meta.is_dir()) {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "unsupported auth lock artifact: expected a directory; automatic recovery disabled; stop all auth writers and resolve the artifact offline",
                        ));
                    }
                    let stale = metadata
                        .filter(fs::Metadata::is_dir)
                        .and_then(|meta| meta.modified().ok())
                        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
                        .is_some_and(|age| age > STALE_AFTER);
                    if stale {
                        // Like proper-lockfile, only an EMPTY stale directory is
                        // reclaimable. An owner marker would break TS recovery.
                        let _ = fs::remove_dir(&path);
                    }
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "auth document lock is held by another transaction",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error),
            }
        }
        let directory = match File::open(&path) {
            Ok(directory) => directory,
            Err(error) => {
                // Without a retained generation identity, leave cleanup to stale
                // recovery. Removing this path could remove a successor's lock.
                return Err(error);
            }
        };
        let (stop, receive) = mpsc::channel();
        // Once the generation handle exists, every fallible setup step is
        // guarded by ownership-safe drop, including heartbeat thread creation.
        let mut guard = Self {
            path,
            directory,
            lost: Arc::new(AtomicBool::new(false)),
            stop,
            heartbeat: None,
        };
        guard.ensure_owned()?;
        guard.directory.set_modified(SystemTime::now())?;
        let thread_path = guard.path.clone();
        let thread_directory = guard.directory.try_clone()?;
        let thread_lost = Arc::clone(&guard.lost);
        guard.heartbeat = Some(
            std::thread::Builder::new()
                .name("auth-lock-heartbeat".into())
                .spawn(move || {
                    while matches!(
                        receive.recv_timeout(HEARTBEAT),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        if !still_owned(&thread_path, &thread_directory)
                            || thread_directory.set_modified(SystemTime::now()).is_err()
                        {
                            thread_lost.store(true, Ordering::Release);
                            break;
                        }
                    }
                })?,
        );
        guard.ensure_owned()?;
        Ok(guard)
    }

    pub(super) fn ensure_owned(&self) -> io::Result<()> {
        if self.lost.load(Ordering::Acquire) || !still_owned(&self.path, &self.directory) {
            self.lost.store(true, Ordering::Release);
            return Err(io::Error::other("auth document lock was compromised"));
        }
        Ok(())
    }
}

impl Drop for AuthDocumentLock {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if self
            .heartbeat
            .take()
            .is_some_and(|thread| thread.join().is_err())
        {
            self.lost.store(true, Ordering::Release);
        }
        // A compromised holder must never remove the replacement directory.
        if self.ensure_owned().is_ok() {
            let _ = fs::remove_dir(&self.path);
        }
    }
}
