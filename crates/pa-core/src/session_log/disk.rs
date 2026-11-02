//! The filesystem seam: real files in production, fault-injected files in
//! tests. Every syscall the log depends on is one method here, so the crash and
//! fsync-failure tests exercise the same code path as production.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Filesystem operations used by the log, shared by production and fault tests.
///
/// Implementations must preserve ordinary file and lock semantics. A failed
/// write may leave a partial tail; a successful sync must make prior writes
/// durable. Wrappers may inject errors to exercise recovery.
pub trait Disk: Send + Sync {
    /// Open for appending, creating when absent; report whether it was created.
    ///
    /// # Errors
    /// Returns the filesystem error if opening or creating fails.
    fn open_log(&self, path: &Path) -> io::Result<(File, bool)>;
    /// Open or create the lock file without truncating its holder note.
    ///
    /// # Errors
    /// Returns the filesystem error if opening or creating fails.
    fn open_lock(&self, path: &Path) -> io::Result<File>;
    /// Open the log for reading and truncating.
    ///
    /// # Errors
    /// Returns the filesystem error if opening fails.
    fn open_rw(&self, path: &Path) -> io::Result<File>;
    /// Open the log for reading only.
    ///
    /// # Errors
    /// Returns the filesystem error if opening fails.
    fn open_read(&self, path: &Path) -> io::Result<File>;
    /// Append all of `bytes`.
    ///
    /// # Errors
    /// Returns a write error, which may leave a partial tail.
    fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()>;
    /// Flush the file's bytes and metadata to stable storage.
    ///
    /// # Errors
    /// Returns a sync error; prior writes may already be durable.
    fn fsync(&self, file: &File) -> io::Result<()>;
    /// Flush directory entries to stable storage.
    ///
    /// # Errors
    /// Returns the directory open or sync error.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
    /// Read the current file length.
    ///
    /// # Errors
    /// Returns the metadata read error.
    fn len(&self, file: &File) -> io::Result<u64>;
    /// Truncate to `len` bytes.
    ///
    /// # Errors
    /// Returns the truncate error; callers must still sync on success.
    fn truncate(&self, file: &File, len: u64) -> io::Result<()>;
    /// Try the exclusive lock; `false` means another writer holds it.
    ///
    /// # Errors
    /// Returns a lock error other than contention.
    fn try_lock(&self, file: &File) -> io::Result<bool>;
    /// Release the exclusive lock.
    ///
    /// # Errors
    /// Returns the unlock error.
    fn unlock(&self, file: &File) -> io::Result<()>;
}

/// The production disk.
pub struct RealDisk;

impl Disk for RealDisk {
    fn open_log(&self, path: &Path) -> io::Result<(File, bool)> {
        let open = || OpenOptions::new().read(true).append(true).open(path);
        match open() {
            Ok(file) => Ok((file, false)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match OpenOptions::new()
                    .read(true)
                    .append(true)
                    .create_new(true)
                    .open(path)
                {
                    Ok(file) => Ok((file, true)),
                    // Another writer created it first: theirs is the log.
                    Err(race) if race.kind() == io::ErrorKind::AlreadyExists => {
                        open().map(|file| (file, false))
                    }
                    Err(race) => Err(race),
                }
            }
            Err(error) => Err(error),
        }
    }

    fn open_lock(&self, path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    }

    fn open_rw(&self, path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(path)
    }

    fn open_read(&self, path: &Path) -> io::Result<File> {
        File::open(path)
    }

    fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }

    fn fsync(&self, file: &File) -> io::Result<()> {
        crate::platform::fsync(file)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        crate::platform::fs::sync_directory(dir)
    }

    fn len(&self, file: &File) -> io::Result<u64> {
        Ok(file.metadata()?.len())
    }

    fn truncate(&self, file: &File, len: u64) -> io::Result<()> {
        file.set_len(len)
    }

    fn try_lock(&self, file: &File) -> io::Result<bool> {
        match file.try_lock() {
            Ok(()) => Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    fn unlock(&self, file: &File) -> io::Result<()> {
        file.unlock()
    }
}
