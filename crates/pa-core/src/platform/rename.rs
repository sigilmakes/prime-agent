//! Rename-onto-destination with a bounded Windows retry.
//!
//! Windows raises transient EPERM/EACCES/EBUSY when the rename destination
//! is held open (antivirus, search indexer); Unix never retries.

use std::fs;
use std::io;
use std::path::Path;
use std::thread;
use std::time::Duration;

/// Total attempts before a transient rename failure surfaces.
const WIN32_RENAME_ATTEMPTS: u32 = 5;

/// Destination-busy failure classes that deserve a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenameFailure {
    /// The access-denied family: std decodes it to `PermissionDenied`
    /// (libuv surfaces the same codes as EPERM/EACCES).
    AccessDenied,
    /// The destination is held with sharing restrictions: libuv EBUSY.
    Busy,
    /// Any other failure: surface immediately.
    Fatal,
}

/// Whether a failed rename deserves another attempt on the given platform;
/// `None` means the error surfaces. Attempts are 1-based.
pub(crate) fn retry_delay_ms(
    platform_windows: bool,
    failure: RenameFailure,
    attempt: u32,
) -> Option<u64> {
    if !platform_windows || failure == RenameFailure::Fatal || attempt >= WIN32_RENAME_ATTEMPTS {
        return None;
    }
    Some(10 * u64::from(attempt))
}

/// Classify a rename failure by its decoded kind and raw OS error code
/// (TS `renameOntoSync` decodes on every platform): classification runs
/// everywhere; `retry_delay_ms` gates retries to win32, so Unix EPERM/EACCES
/// still surface immediately there.
fn classify_kind(kind: io::ErrorKind, raw_os_error: Option<i32>) -> RenameFailure {
    // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION (libuv EBUSY) stay
    // raw in std; the rest decodes to PermissionDenied.
    if matches!(raw_os_error, Some(32 | 33)) {
        return RenameFailure::Busy;
    }
    if kind == io::ErrorKind::PermissionDenied {
        return RenameFailure::AccessDenied;
    }
    RenameFailure::Fatal
}

fn classify(error: &io::Error) -> RenameFailure {
    classify_kind(error.kind(), error.raw_os_error())
}

/// Rename `from` onto `to`, replacing an existing destination.
///
/// On Windows, retries destination-busy failures (EPERM/EACCES/EBUSY) up to
/// [`WIN32_RENAME_ATTEMPTS`] total attempts; every other failure - and every
/// failure on Unix - surfaces immediately.
///
/// # Errors
///
/// Returns the underlying `std::fs::rename` error (the final retry's error on Windows).
pub fn rename_onto(from: &Path, to: &Path) -> io::Result<()> {
    let platform_windows = cfg!(windows);
    let mut attempt = 1;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) => match retry_delay_ms(platform_windows, classify(&error), attempt) {
                Some(delay) => {
                    thread::sleep(Duration::from_millis(delay));
                    attempt += 1;
                }
                None => return Err(error),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_retries_busy_failures_with_linear_backoff_until_cap() {
        for failure in [RenameFailure::AccessDenied, RenameFailure::Busy] {
            for attempt in 1..WIN32_RENAME_ATTEMPTS {
                assert_eq!(
                    retry_delay_ms(true, failure, attempt),
                    Some(10 * u64::from(attempt)),
                    "failure={failure:?} attempt={attempt}"
                );
            }
            assert_eq!(
                retry_delay_ms(true, failure, WIN32_RENAME_ATTEMPTS),
                None,
                "failure={failure:?}"
            );
            assert_eq!(
                retry_delay_ms(true, failure, WIN32_RENAME_ATTEMPTS + 3),
                None,
                "failure={failure:?}"
            );
        }
    }

    #[test]
    fn unix_and_fatal_failures_never_retry() {
        for failure in [
            RenameFailure::AccessDenied,
            RenameFailure::Busy,
            RenameFailure::Fatal,
        ] {
            for attempt in 1..=WIN32_RENAME_ATTEMPTS {
                assert_eq!(
                    retry_delay_ms(false, failure, attempt),
                    None,
                    "failure={failure:?} attempt={attempt}"
                );
            }
        }
        for attempt in 1..=WIN32_RENAME_ATTEMPTS {
            assert_eq!(retry_delay_ms(true, RenameFailure::Fatal, attempt), None);
        }
    }

    #[test]
    fn rename_onto_replaces_destination() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from.json");
        let to = dir.path().join("to.json");
        std::fs::write(&from, b"next").unwrap();
        std::fs::write(&to, b"previous").unwrap();
        rename_onto(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "next");
        assert!(!from.exists());
    }

    #[test]
    fn rename_onto_missing_source_surfaces_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("missing");
        let to = dir.path().join("to.json");
        let error = rename_onto(&from, &to).expect_err("rename of a missing source must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn classification_marks_only_the_win32_busy_family_transient() {
        use io::ErrorKind;
        assert_eq!(
            classify_kind(ErrorKind::PermissionDenied, None),
            RenameFailure::AccessDenied
        );
        // The raw code wins over the decoded kind.
        assert_eq!(
            classify_kind(ErrorKind::Other, Some(32)),
            RenameFailure::Busy
        );
        assert_eq!(
            classify_kind(ErrorKind::Other, Some(33)),
            RenameFailure::Busy
        );
        assert_eq!(
            classify_kind(ErrorKind::PermissionDenied, Some(32)),
            RenameFailure::Busy
        );
        assert_eq!(
            classify_kind(ErrorKind::NotFound, None),
            RenameFailure::Fatal
        );
        assert_eq!(
            classify_kind(ErrorKind::Other, Some(5)),
            RenameFailure::Fatal
        );
    }

    #[test]
    #[cfg(unix)]
    fn unix_surfaces_real_permission_failures() {
        let denied = io::Error::from_raw_os_error(13); // EACCES
        assert_eq!(classify(&denied), RenameFailure::AccessDenied);
        assert_eq!(retry_delay_ms(false, classify(&denied), 1), None);
    }
}

/// Rename `from` onto `to` and prove the DIRECTORY ENTRY is durable.
///
/// [`rename_onto`] alone only makes the rename visible: POSIX persists a new or
/// replaced name only after the containing directory is fsynced. A caller that
/// reports "durable" from a bare rename therefore claims a Known the crash can
/// undo - the exact false-`Known` class the canonical writer classifies as
/// `Uncertain`. This wrapper fsyncs `to`'s parent after a successful rename.
///
/// On Windows a directory cannot be opened as a file for fsync; the flush
/// semantics there already journal the rename, so the parent sync is a no-op
/// there and the rename result is returned unchanged.
///
/// # Errors
///
/// Returns the rename error, or the parent-directory open/sync error. A caller
/// must treat both as "the outcome is not proven", never as success.
pub fn rename_onto_durable(from: &Path, to: &Path) -> io::Result<()> {
    rename_onto(from, to)?;
    sync_parent_directory(to)
}

/// Fsync the directory that holds `path`, making its entry durable.
///
/// A bare relative name resolves to the current directory, which is exactly the
/// directory holding it, so it is synced rather than refused.
///
/// # Errors
///
/// Returns the open or sync error. A missing or unreadable directory surfaces the
/// error instead of being treated as durable.
pub fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        // A bare relative name has `Some("")` as its parent, and for such a name
        // the containing directory IS the current directory: syncing "." is the
        // honest proof, not a fallback. Refusing here would turn a call that
        // succeeded before into an error without making anything more durable.
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod durable_tests {
    //! The directory-entry proof: a rename alone is not durability.
    use super::*;
    use std::io::Write;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pa-rename-durable-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn durable_rename_replaces_the_destination_and_keeps_its_bytes() {
        let dir = scratch("replace");
        let from = dir.join("tmp");
        let to = dir.join("final.jsonl");
        fs::write(&to, b"old").expect("seed destination");
        let mut file = fs::File::create(&from).expect("temp file");
        file.write_all(b"new").expect("write temp");
        file.sync_all().expect("sync temp");
        drop(file);
        rename_onto_durable(&from, &to).expect("durable rename");
        assert_eq!(fs::read(&to).expect("bytes"), b"new");
        assert!(!from.exists(), "the temp name is gone");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn durable_rename_creates_a_new_name_without_a_prior_destination() {
        let dir = scratch("create");
        let from = dir.join("tmp");
        let to = dir.join("fresh.jsonl");
        fs::write(&from, b"fresh").expect("temp file");
        rename_onto_durable(&from, &to).expect("durable create");
        assert_eq!(fs::read(&to).expect("bytes"), b"fresh");
        fs::remove_dir_all(&dir).ok();
    }

    /// A bare relative name syncs the current directory: that IS its containing
    /// directory, so the proof is real rather than silently skipped. This matches
    /// the append path's normalisation, so the two durability helpers agree.
    #[test]
    fn bare_relative_name_syncs_the_current_directory() {
        sync_parent_directory(Path::new("bare-name.jsonl")).expect("current directory sync");
    }

    #[test]
    fn a_missing_parent_directory_surfaces_the_open_error() {
        let dir = scratch("missing-parent");
        let error =
            sync_parent_directory(&dir.join("nested").join("x.jsonl")).expect_err("no such parent");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        fs::remove_dir_all(&dir).ok();
    }
}
