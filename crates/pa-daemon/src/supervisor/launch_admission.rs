//! Admission covers a physical launch through publication or verified rollback.
use super::{anyhow, Arc, AtomicBool, Ordering, Result, Supervisor};

#[derive(Debug)]
pub(super) struct LaunchAdmission {
    _guard: tokio::sync::OwnedRwLockReadGuard<()>,
    pub(super) cleanup_uncertain: Arc<AtomicBool>,
}

impl Supervisor {
    pub(super) fn acquire_launch_admission(&self) -> Result<Arc<LaunchAdmission>> {
        // Fast refusal also keeps shutdown callbacks out of a queued writer.
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(anyhow!("Supervisor is shutting down"));
        }
        #[cfg(test)]
        self.pause_admission_lock();
        // A terminal writer must never wait for a launch whose callback is
        // queued behind that same writer while holding an adoption guard.
        let guard = Arc::clone(&self.launch_admission)
            .try_read_owned()
            .map_err(|_| anyhow!("supervisor launch admission is closed"))?;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(anyhow!("Supervisor is shutting down"));
        }
        Ok(Arc::new(LaunchAdmission {
            _guard: guard,
            cleanup_uncertain: Arc::clone(&self.launch_cleanup_uncertain),
        }))
    }
}

// An aborted cleanup task must record debt BEFORE releasing its admission.
pub(super) struct CleanupProof {
    pub(super) uncertain: Arc<AtomicBool>,
    pub(super) verified: bool,
}

impl Drop for CleanupProof {
    fn drop(&mut self) {
        if !self.verified {
            self.uncertain.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
type LaunchPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(test)]
fn pauses() -> &'static std::sync::Mutex<
    std::collections::HashMap<(std::path::PathBuf, &'static str), LaunchPause>,
> {
    static PAUSES: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<(std::path::PathBuf, &'static str), LaunchPause>,
        >,
    > = std::sync::OnceLock::new();
    PAUSES.get_or_init(Default::default)
}

#[cfg(test)]
pub(super) fn pause_at(
    supervisor: &Supervisor,
    seam: &'static str,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    assert!(pauses()
        .lock()
        .expect("launch pauses")
        .insert(
            (supervisor.descriptor_dir.clone(), seam),
            (entered, released)
        )
        .is_none());
    (entry, release)
}

#[cfg(test)]
impl Supervisor {
    pub(super) async fn pause_launch_at(&self, seam: &'static str) {
        let pause = pauses()
            .lock()
            .expect("launch pauses")
            .remove(&(self.descriptor_dir.clone(), seam));
        if let Some((entered, released)) = pause {
            let _ = entered.send(());
            let _ = released.await;
        }
    }
}

#[cfg(test)]
type AdmissionPause = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[cfg(test)]
type AdmissionPauses =
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, AdmissionPause>>;

#[cfg(test)]
fn admission_pauses() -> &'static AdmissionPauses {
    static PAUSES: std::sync::OnceLock<AdmissionPauses> = std::sync::OnceLock::new();
    PAUSES.get_or_init(Default::default)
}

#[cfg(test)]
pub(super) fn pause_admission_at(
    supervisor: &Supervisor,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    assert!(admission_pauses()
        .lock()
        .expect("admission pauses")
        .insert(supervisor.descriptor_dir.clone(), (entered, released))
        .is_none());
    (entry, release)
}

#[cfg(test)]
impl Supervisor {
    // Installed only for admission invoked on a fixture spawn_blocking thread.
    // No Tokio executor thread may enter this synchronous test pause.
    fn pause_admission_lock(&self) {
        let pause = admission_pauses()
            .lock()
            .expect("admission pauses")
            .remove(&self.descriptor_dir);
        if let Some((entered, released)) = pause {
            let _ = entered.send(());
            let _ = released.recv();
        }
    }
}
