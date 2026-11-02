use super::*;
use crate::session_log::{
    disk::{Disk, RealDisk},
    ReadCursor,
};
use std::fs::File;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

struct BarrierDisk {
    real: RealDisk,
    failing: AtomicBool,
    barriers: AtomicUsize,
}
impl Disk for BarrierDisk {
    fn open_log(&self, path: &Path) -> io::Result<(File, bool)> {
        self.real.open_log(path)
    }
    fn open_lock(&self, path: &Path) -> io::Result<File> {
        self.real.open_lock(path)
    }
    fn open_rw(&self, path: &Path) -> io::Result<File> {
        self.real.open_rw(path)
    }
    fn open_read(&self, path: &Path) -> io::Result<File> {
        self.real.open_read(path)
    }
    fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        self.real.write(file, bytes)
    }
    fn fsync(&self, file: &File) -> io::Result<()> {
        self.barriers.fetch_add(1, Ordering::SeqCst);
        if self.failing.load(Ordering::SeqCst) {
            Err(io::Error::other("barrier unavailable"))
        } else {
            self.real.fsync(file)
        }
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.real.sync_dir(path)
    }
    fn len(&self, file: &File) -> io::Result<u64> {
        self.real.len(file)
    }
    fn truncate(&self, file: &File, len: u64) -> io::Result<()> {
        self.real.truncate(file, len)
    }
    fn try_lock(&self, file: &File) -> io::Result<bool> {
        self.real.try_lock(file)
    }
    fn unlock(&self, file: &File) -> io::Result<()> {
        self.real.unlock(file)
    }
}

fn disk() -> Arc<BarrierDisk> {
    Arc::new(BarrierDisk {
        real: RealDisk,
        failing: AtomicBool::new(false),
        barriers: AtomicUsize::new(0),
    })
}

#[test]
fn an_observed_pending_row_requires_a_new_barrier_before_publication() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::persisted(dir.path(), dir.path());
    manager.flush_now().unwrap();
    let path = manager.session_file.clone().unwrap();
    let id = path.file_stem().unwrap().to_str().unwrap();
    let disk = disk();
    manager.standalone_log.as_mut().unwrap().log =
        SessionLog::open_with(disk.clone(), dir.path(), id, Instant::now()).unwrap();
    disk.failing.store(true, Ordering::SeqCst);
    assert!(manager.append_session_info("pending").is_err());
    let pending = manager
        .standalone_log
        .as_ref()
        .unwrap()
        .pending
        .as_ref()
        .unwrap()
        .1
        .clone();
    assert_eq!(
        super::super::super::read::read_entries(&path)
            .unwrap()
            .len(),
        2
    );
    let before = disk.barriers.load(Ordering::SeqCst);
    assert!(manager.refresh_committed_entries().is_err());
    assert!(disk.barriers.load(Ordering::SeqCst) > before);
    assert_eq!(manager.file_entries.len(), 1);
    assert!(manager.get_entry_by_id(pending.id().unwrap()).is_none());
    disk.failing.store(false, Ordering::SeqCst);
    let committed = manager.append_session_info("pending").unwrap();
    assert_eq!(committed, pending.id().unwrap());
    assert_eq!(manager.get_entry_by_id(&committed), Some(&pending));
    assert_eq!(
        super::super::super::read::read_entries(&path)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn initial_unknown_retains_its_exact_row_and_recovery_until_the_barrier_returns() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::persisted(dir.path(), dir.path());
    let path = manager.session_file.clone().unwrap();
    let id = path.file_stem().unwrap().to_str().unwrap();
    let disk = disk();
    let log = SessionLog::open_with(disk.clone(), dir.path(), id, Instant::now()).unwrap();
    manager.standalone_log = Some(StandaloneLog {
        log,
        cursor: ReadCursor::default(),
        seq: 0,
        initialized: 0,
        pending: None,
        initial_recover: None,
        initial_pending: None,
    });
    disk.failing.store(true, Ordering::SeqCst);
    assert!(manager.flush_now().is_err());
    let state = manager.standalone_log.as_ref().unwrap();
    assert!(state.initial_recover.is_some());
    let pending = state.initial_pending.clone().unwrap();
    assert_eq!(state.initialized, 0);
    disk.failing.store(false, Ordering::SeqCst);
    manager.flush_now().unwrap();
    let state = manager.standalone_log.as_ref().unwrap();
    assert!(state.initial_recover.is_none());
    assert!(state.initial_pending.is_none());
    assert_eq!(
        super::super::super::read::read_entries(&path).unwrap(),
        vec![pending.1]
    );
}
