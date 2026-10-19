//! Auth storage backends: locked JSON file (0o600, atomic writes) and
//! in-memory (tests, embedded hosts).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;

use super::types::AuthStorageData;

/// Locked read/modify/write over the auth document. `update` returns
/// `(result, next)`; `next: Some` writes it back atomically.
pub trait AuthStorageBackend: Send + Sync {
    /// The document's current content, exactly what [`Self::with_lock`]'s read
    /// arm would deliver, without a write-back channel. Writers must still go
    /// through [`Self::with_lock`].
    ///
    /// # Errors
    ///
    /// Returns an error when preparing or locking the document fails.
    fn read(&self) -> Result<Option<String>> {
        let mut content = None;
        self.with_lock(&mut |current| {
            content = current;
            Ok(((), None))
        })?;
        Ok(content)
    }

    /// # Errors
    ///
    /// Returns an error when locking, reading, updating or writing fails.
    /// An unreadable document must not be treated as an absent credential store.
    fn with_lock(
        &self,
        update: &mut dyn FnMut(Option<String>) -> Result<((), Option<String>)>,
    ) -> Result<()>;

    /// Run a rotating-token refresh with the same document lock, allowing a
    /// bounded longer acquisition wait. The callback owns the authoritative
    /// snapshot until its result is committed, including the token request.
    ///
    /// # Errors
    /// Returns lock, callback, compromise or persistence failures.
    fn with_refresh_lock(
        &self,
        update: &mut dyn FnMut(Option<String>) -> Result<((), Option<String>)>,
    ) -> Result<()> {
        self.with_lock(update)
    }
}

use super::document_lock::{AuthDocumentGuard, TransactionKind};

pub struct FileAuthStorageBackend {
    auth_path: PathBuf,
}

impl FileAuthStorageBackend {
    pub fn new(auth_path: impl Into<PathBuf>) -> Self {
        FileAuthStorageBackend {
            auth_path: auth_path.into(),
        }
    }

    fn ensure_parent_dir(&self) -> Result<()> {
        if let Some(dir) = self.auth_path.parent() {
            if !dir.exists() {
                fs::create_dir_all(dir)?;
                crate::platform::perms::restrict_dir(dir)?;
            }
        }
        Ok(())
    }

    /// Exclusive create: a racing initializer must never replace saved
    /// credentials.
    fn ensure_file_exists(&self) -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        crate::platform::perms::set_private_mode(&mut options);
        match options.open(&self.auth_path) {
            Ok(mut file) => {
                // The TS initializer writes exactly "{}".
                use std::io::Write;
                file.write_all(b"{}")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn canonical_path(&self) -> Result<PathBuf> {
        self.ensure_parent_dir()?;
        self.ensure_file_exists()?;
        // TS async locking and atomic writes resolve the leaf target. Lock,
        // read and rename that same document; never replace an auth symlink.
        Ok(fs::canonicalize(&self.auth_path)?)
    }

    fn transact(
        &self,
        update: &mut dyn FnMut(Option<String>) -> Result<((), Option<String>)>,
        kind: TransactionKind,
    ) -> Result<()> {
        let path = self.canonical_path()?;
        let guard = AuthDocumentGuard::acquire(&path, kind)?;
        let identity = fs::metadata(&path)
            .ok()
            .and_then(|meta| stat_identity(&meta));
        let current = fs::read_to_string(&path)?;
        let ((), next) = update(Some(current.clone()))?;
        guard.ensure_owned()?;
        if let Some(next) = next {
            // Reject bypass changes already visible here, including logout or
            // grant replacement during the token fetch. This check and rename
            // are not an atomic compare-and-swap against arbitrary writers.
            anyhow::ensure!(
                identity
                    == fs::metadata(&path)
                        .ok()
                        .and_then(|meta| stat_identity(&meta))
                    && fs::read_to_string(&path).ok().as_deref() == Some(current.as_str()),
                "auth document changed during transaction"
            );
            guard.ensure_owned()?;
            super::super::settings::storage::atomic_write_with(
                &path,
                &next,
                super::super::settings::storage::AtomicWriteOptions { fsync: true },
            )?;
        }
        guard.ensure_owned()?;
        Ok(())
    }
}

/// The stat identity a cached read is validated against: every writer renames the document in
/// place, so a matching identity means the cached content is byte-identical to a locked read now.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
    len: u64,
}

// The fallible non-Unix twin pins the Option shape across
// platforms - unwrapping only this arm would split the contract.
#[allow(clippy::unnecessary_wraps)]
#[cfg(unix)]
fn stat_identity(metadata: &fs::Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        mtime_sec: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
        len: metadata.len(),
    })
}

#[cfg(windows)]
fn stat_identity(metadata: &fs::Metadata) -> Option<FileIdentity> {
    let modified = metadata.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(FileIdentity {
        dev: 0,
        ino: 0,
        mtime_sec: since.as_secs() as i64,
        mtime_nsec: i64::from(since.subsec_nanos()),
        len: metadata.len(),
    })
}

#[cfg(not(any(unix, windows)))]
fn stat_identity(_metadata: &fs::Metadata) -> Option<FileIdentity> {
    None
}

/// One validated document read held in the process-wide read-through cache.
struct CachedRead {
    identity: FileIdentity,
    content: String,
}

/// Validated content per auth document: the read-through cache for
/// [`FileAuthStorageBackend::read`]. A stat identity that no longer matches simply misses.
static READ_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedRead>>> = OnceLock::new();

fn read_cache() -> &'static Mutex<HashMap<PathBuf, CachedRead>> {
    READ_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

impl AuthStorageBackend for FileAuthStorageBackend {
    fn read(&self) -> Result<Option<String>> {
        let path = self.canonical_path()?;
        let now_identity = fs::metadata(&path)
            .ok()
            .and_then(|meta| stat_identity(&meta));
        // A validated cached snapshot is safe while a refresh holds the document
        // lock. Do not block ordinary status reads behind the token transport.
        if let Some(identity) = now_identity {
            let cache = read_cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = cache.get(&path) {
                if entry.identity == identity {
                    return Ok(Some(entry.content.clone()));
                }
            }
        }
        let guard = AuthDocumentGuard::acquire(&path, TransactionKind::Ordinary)?;
        let identity = fs::metadata(&path)
            .ok()
            .and_then(|meta| stat_identity(&meta));
        let content = fs::read_to_string(&path)?;
        guard.ensure_owned()?;
        if let Some(identity) = identity {
            read_cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    path,
                    CachedRead {
                        identity,
                        content: content.clone(),
                    },
                );
        }
        Ok(Some(content))
    }

    fn with_lock(
        &self,
        update: &mut dyn FnMut(Option<String>) -> Result<((), Option<String>)>,
    ) -> Result<()> {
        self.transact(update, TransactionKind::Ordinary)
    }

    fn with_refresh_lock(
        &self,
        update: &mut dyn FnMut(Option<String>) -> Result<((), Option<String>)>,
    ) -> Result<()> {
        self.transact(update, TransactionKind::Refresh)
    }
}

#[derive(Default)]
pub struct InMemoryAuthStorageBackend {
    value: Mutex<Option<String>>,
}

impl AuthStorageBackend for InMemoryAuthStorageBackend {
    fn with_lock(
        &self,
        update: &mut dyn FnMut(Option<String>) -> Result<((), Option<String>)>,
    ) -> Result<()> {
        let mut guard = self.value.lock().unwrap();
        let ((), next) = update(guard.clone())?;
        if let Some(next) = next {
            *guard = Some(next);
        }
        Ok(())
    }
}

/// Parse an auth document; invalid JSON or a non-object root is a load error
/// (the TS throws too).
///
/// # Errors
///
/// Returns an error when the content is not valid JSON or its root is not a JSON object.
pub fn parse_storage_data(content: Option<&str>) -> Result<AuthStorageData> {
    let content = content.filter(|content| !content.is_empty());
    let Some(content) = content else {
        return Ok(AuthStorageData::default());
    };
    let value: serde_json::Value = serde_json::from_str(content)?;
    let serde_json::Value::Object(map) = value else {
        anyhow::bail!("Invalid auth storage: expected a JSON object");
    };
    Ok(AuthStorageData(map))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(unix))]
    #[test]
    fn ordinary_file_auth_survives_unsupported_rotating_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileAuthStorageBackend::new(dir.path().join("auth.json"));
        backend
            .with_lock(&mut |_| Ok(((), Some("{}".into()))))
            .unwrap();
        let mut entered = false;
        assert!(backend
            .with_refresh_lock(&mut |_| {
                entered = true;
                Ok(((), None))
            })
            .is_err());
        assert!(
            !entered,
            "unsupported rotating refresh reached token callback"
        );
        assert_eq!(backend.read().unwrap().as_deref(), Some("{}"));
    }

    #[test]
    fn file_backend_round_trip_and_modes() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileAuthStorageBackend::new(dir.path().join("auth.json"));
        backend
            .with_lock(&mut |current| {
                assert_eq!(current.as_deref(), Some("{}"));
                Ok((
                    (),
                    Some(
                        r#"{ "prime-inference": { "type": "api_key", "key": "sk" } }"#.to_string(),
                    ),
                ))
            })
            .unwrap();
        let path = dir.path().join("auth.json");
        // Owner-only mode is a Unix guarantee; Windows inherits ACLs.
        #[cfg(unix)]
        assert_eq!(crate::platform::perms::file_mode(&path), Some(0o600));
        let data = parse_storage_data(Some(&fs::read_to_string(&path).unwrap())).unwrap();
        assert!(data.credential("prime-inference").is_some());
    }

    /// The auth save goes through the real `with_lock` writer and takes exactly one fsync branch
    /// landing the exact document bytes.
    #[test]
    fn auth_write_takes_exactly_one_fsync() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileAuthStorageBackend::new(dir.path().join("auth.json"));
        let document = r#"{ "prime-inference": { "type": "api_key", "key": "sk" } }"#;
        let before = crate::settings::storage::opt_in_fsync_calls();
        backend
            .with_lock(&mut |_| Ok(((), Some(document.to_string()))))
            .unwrap();
        assert_eq!(
            crate::settings::storage::opt_in_fsync_calls(),
            before + 1,
            "the auth write must flush the temp file before the rename"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("auth.json")).unwrap(),
            document
        );
    }

    #[test]
    fn parse_rejects_non_object() {
        assert!(parse_storage_data(Some("[1,2]")).is_err());
        assert_eq!(
            parse_storage_data(None).unwrap().keys(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn read_creates_the_document_like_with_lock() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileAuthStorageBackend::new(dir.path().join("auth.json"));
        assert_eq!(backend.read().unwrap().as_deref(), Some("{}"));
        assert!(dir.path().join("auth.json").is_file());
    }

    #[test]
    fn read_hits_do_not_run_the_lock_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let backend = FileAuthStorageBackend::new(&path);
        backend.read().unwrap();
        // The first read populated the process-wide cache, so this
        // second instance's read is a hit.
        let backend2 = FileAuthStorageBackend::new(&path);
        assert_eq!(backend2.read().unwrap().as_deref(), Some("{}"));
        assert!(
            !crate::platform::lock_dir::LockDir::path_for(&path).exists(),
            "a read hit leaves no lock artifact"
        );
    }

    #[test]
    fn read_sees_external_atomic_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let backend = FileAuthStorageBackend::new(&path);
        backend.read().unwrap();
        // The rename changes the inode, so the next read must miss.
        let temp = dir.path().join("external.tmp");
        fs::write(&temp, r#"{ "written": "externally" }"#).unwrap();
        fs::rename(&temp, &path).unwrap();
        assert_eq!(
            backend.read().unwrap().as_deref(),
            Some(r#"{ "written": "externally" }"#)
        );
    }

    #[test]
    fn read_sees_external_in_place_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let backend = FileAuthStorageBackend::new(&path);
        backend.read().unwrap();
        // Keeps the inode but changes the mtime: the identity must not match.
        fs::write(&path, r#"{ "rewritten": "in place" }"#).unwrap();
        assert_eq!(
            backend.read().unwrap().as_deref(),
            Some(r#"{ "rewritten": "in place" }"#)
        );
    }

    #[test]
    fn read_hits_serve_the_validated_copy_under_a_foreign_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let backend = FileAuthStorageBackend::new(&path);
        assert_eq!(backend.read().unwrap().as_deref(), Some("{}"));
        // A foreign lock directory would make `with_lock` retry 10x and
        // fail; a validated hit serves the cached copy instead.
        fs::create_dir(crate::platform::lock_dir::LockDir::path_for(&path)).unwrap();
        assert_eq!(backend.read().unwrap().as_deref(), Some("{}"));
        fs::remove_dir(crate::platform::lock_dir::LockDir::path_for(&path)).unwrap();
    }

    #[test]
    fn read_sees_this_process_with_lock_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let backend = FileAuthStorageBackend::new(&path);
        backend.read().unwrap();
        backend
            .with_lock(&mut |current| {
                assert_eq!(current.as_deref(), Some("{}"));
                Ok(((), Some(r#"{ "written": "locally" }"#.to_string())))
            })
            .unwrap();
        assert_eq!(
            backend.read().unwrap().as_deref(),
            Some(r#"{ "written": "locally" }"#)
        );
    }
}
