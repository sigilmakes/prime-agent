//! The create path's durability proof, and the settlement that rests on it.
//!
//! `rewrite` creates the session file by renaming a synced temp onto it. A rename
//! only makes the name visible: POSIX persists a new entry when the containing
//! directory is synced, so a caller that reports success from the rename alone can
//! lose the file to a crash it already survived. These tests run the create path in
//! a directory that accepts writes and renames but cannot be opened for reading -
//! the open the directory proof needs - so an unprovable entry is observable
//! without a fault-injection device.
//!
//! The assertions are behavioural: the create path reports the unprovable entry
//! instead of success, and the grant that admitted the row settles uncertain
//! instead of draining the epoch.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_core::session::canonical::{CanonicalCursor, CanonicalFrontier, CanonicalOwnerScope};
use pa_core::session_engine::canonical_coordinator::{CanonicalDrainView, CanonicalEpoch};
use serde_json::json;

use super::canonical_adapter::EpochHandle;
use super::SessionFile;

/// A directory this process may write and rename into but cannot open for
/// reading. Restores a readable mode on drop, so the temp tree stays removable
/// even when an assertion panics.
struct WriteOnlyDirectory(PathBuf);

impl WriteOnlyDirectory {
    /// `None` when this user can read the directory anyway (root): the property
    /// under test is then indistinguishable from a successful sync.
    fn new(path: &Path) -> Option<Self> {
        set_mode(path, 0o300);
        let guard = Self(path.to_owned());
        if std::fs::File::open(path).is_ok() {
            return None;
        }
        Some(guard)
    }
}

impl Drop for WriteOnlyDirectory {
    fn drop(&mut self) {
        set_mode(&self.0, 0o700);
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("directory mode");
}

fn cursor() -> CanonicalCursor {
    CanonicalCursor {
        owner: CanonicalOwnerScope::new(),
        session_id: "durable-create-session".to_owned(),
        source_path: PathBuf::from("/private/durable-create.jsonl"),
        raw_source_generation: [1; 32],
        header_certificate: [2; 32],
        canonical_revision: 0,
        branch_revision: 1,
        selected_leaf: None,
        frontier: CanonicalFrontier {
            bytes: 9,
            rows: 1,
            prefix_certificate: [3; 32],
        },
    }
}

fn epoch() -> Arc<CanonicalEpoch> {
    Arc::new(
        CanonicalEpoch::genesis(
            &cursor(),
            Arc::new(String::from("run")),
            Arc::new(String::from("model")),
        )
        .expect("epoch"),
    )
}

fn user_row() -> serde_json::Value {
    json!({
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
            "timestamp": 1
        }
    })
}

/// A store whose session file does not exist yet, so the next persist creates it.
fn uncreated_store(sessions: &Path) -> SessionFile {
    let mut store = SessionFile::create("/synthetic", None, 0);
    store.set_path(sessions.join("created.jsonl"));
    store
}

/// The create path must not report a durable create it cannot prove: a directory
/// entry whose sync fails is an error, even though the rename itself succeeded.
#[test]
fn the_create_rewrite_reports_a_directory_entry_it_cannot_prove() {
    let dir = tempfile::tempdir().expect("temp dir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let store = uncreated_store(&sessions);

    let Some(_write_only) = WriteOnlyDirectory::new(&sessions) else {
        return;
    };
    assert!(
        store.rewrite().is_err(),
        "an entry the directory cannot be asked to persist is not a durable create"
    );
    // The bytes landed before the proof was attempted, which is exactly why this has
    // to be an error instead of success: only the name is unproven.
    assert!(
        store.path.exists(),
        "the failed proof happens after the rename"
    );
}

/// The same failure, seen from the canonical epoch: the row's grant settles from
/// what the write proved, so an unproven directory entry is `Uncertain` - it
/// poisons the epoch and blocks the drain instead of counting as `Known`.
#[test]
fn an_unproven_directory_entry_settles_the_row_grant_uncertain() {
    let dir = tempfile::tempdir().expect("temp dir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let mut store = uncreated_store(&sessions);
    let epoch = epoch();
    store.epoch = Some(EpochHandle(Arc::clone(&epoch)));

    let Some(_write_only) = WriteOnlyDirectory::new(&sessions) else {
        return;
    };
    assert!(
        store.persist_entry("message", user_row()).is_err(),
        "the row cannot be persisted without a durable create"
    );
    assert!(
        epoch.poisoned(),
        "a directory proof that failed must settle uncertain, never Known"
    );
    assert_eq!(
        epoch.request_close().view(),
        CanonicalDrainView::Blocked,
        "an uncertain write blocks the drain instead of draining it"
    );
}
