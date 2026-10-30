//! The filesystem seam: real files in production, fault-injected files in
//! tests. Every syscall the log depends on is one method here, so the crash and
//! fsync-failure tests exercise the same code path as production.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Every filesystem operation the session log performs.
pub trait Disk: Send + Sync {
    /// Open the log for appending, creating it when absent; `true` when this
    /// call created the file.
    fn open_log(&self, path: &Path) -> io::Result<(File, bool)>;
    /// Open (creating) the lock file.
    fn open_lock(&self, path: &Path) -> io::Result<File>;
    /// Open the log for reading and truncating.
    fn open_rw(&self, path: &Path) -> io::Result<File>;
    /// Open the log for reading only.
    fn open_read(&self, path: &Path) -> io::Result<File>;
    /// Append `bytes`.
    fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()>;
    /// Flush the file's bytes and metadata to stable storage.
    fn fsync(&self, file: &File) -> io::Result<()>;
    /// Flush the directory `dir` itself, making its entries durable.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
    /// Current file length.
    fn len(&self, file: &File) -> io::Result<u64>;
    /// Truncate to `len` bytes.
    fn truncate(&self, file: &File, len: u64) -> io::Result<()>;
    /// Try to take the exclusive lock; `false` means another writer holds it.
    fn try_lock(&self, file: &File) -> io::Result<bool>;
    /// Release the exclusive lock.
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
