//! Immutable binding from a built SDK mirror to its actual worker-owned writer.
//! Host locks stay inside blocking owner calls; Core performs all local adoption.

use std::sync::{Arc, Mutex};

use pa_core::session::canonical::{
    CanonicalAcknowledgement, CanonicalCommitDisposition, CanonicalFailureKind, CanonicalFuture,
    CanonicalPrepared, CanonicalRequestedWrite, CanonicalSourceDisposition, CanonicalStoreError,
};
use pa_core::session_engine::canonical_live::{
    CanonicalLiveAcknowledged, CanonicalLiveAppendRequest, CanonicalLiveCustomPrepared,
    CanonicalLiveCustomRequest, CanonicalLiveOwner, CanonicalLivePrepareRequest,
    CanonicalLiveProviderStepRequest, CanonicalLiveRunRequest, CanonicalLiveToolRequest,
    CanonicalLiveUserPrepared, CanonicalLiveUserRequest,
};

use crate::agent_engine::{canonical_mirror_header, ContextSelectionSource};
use crate::lease::canonical_session_path;

use super::SessionCore;

pub(super) struct CoreCanonicalLiveOwner {
    pub(super) core: Arc<Mutex<SessionCore>>,
    pub(super) source: ContextSelectionSource,
    // Captured anew for this SDK adapter, before any attach. An invalid
    // capture remains a refusing live adapter, never standalone fallback.
    pub(super) bootstrap_admission: Result<bool, String>,
}

fn refused(detail: impl std::fmt::Display) -> CanonicalStoreError {
    CanonicalStoreError {
        kind: CanonicalFailureKind::InvalidRequest,
        requested: CanonicalRequestedWrite::NotWritten,
        source: CanonicalSourceDisposition::NeedsReconcile,
        detail: detail.to_string(),
    }
}

fn validate(
    core: &SessionCore,
    source: &ContextSelectionSource,
) -> Result<(), CanonicalStoreError> {
    let expected = source
        .lease
        .upgrade()
        .ok_or_else(|| refused("canonical owner was replaced"))?;
    let store = core
        .store
        .as_ref()
        .ok_or_else(|| refused("canonical store is unavailable"))?;
    let lease = store
        .lease
        .as_ref()
        .ok_or_else(|| refused("canonical lease is unavailable"))?;
    if !Arc::ptr_eq(lease, &expected)
        || canonical_session_path(&store.path) != source.path
        || canonical_mirror_header(&store.header).map_err(refused)? != source.header
    {
        return Err(refused("canonical owner, header or path changed"));
    }
    lease
        .with_owned_mutation(&store.path, || Ok(()))
        .map_err(refused)?;
    let header = crate::session_store::read_session_header(&store.path)
        .ok_or_else(|| refused("physical canonical header is unavailable"))?;
    if canonical_mirror_header(&header).map_err(refused)? != source.header {
        return Err(refused("physical canonical header changed"));
    }
    Ok(())
}

impl CoreCanonicalLiveOwner {
    fn call<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut SessionCore) -> Result<T, CanonicalStoreError> + Send + 'static,
    ) -> CanonicalFuture<T> {
        let core = Arc::clone(&self.core);
        let source = self.source.clone();
        let capture_error = self.bootstrap_admission.as_ref().err().cloned();
        Box::pin(async move {
            if let Some(error) = capture_error {
                return Err(refused(error));
            }
            tokio::task::spawn_blocking(move || {
                let mut core = core.lock().map_err(refused)?;
                validate(&core, &source)?;
                action(&mut core)
            })
            .await
            .map_err(refused)?
        })
    }
}

impl CanonicalLiveOwner for CoreCanonicalLiveOwner {
    fn bootstrap_preserves_owner_admission(&self) -> bool {
        self.bootstrap_admission
            .as_ref()
            .is_ok_and(|preserve| *preserve)
    }

    fn register_run(&self, request: CanonicalLiveRunRequest) -> CanonicalFuture<()> {
        self.call(move |core| {
            core.store
                .as_mut()
                .expect("validated store")
                .register_canonical_run(request)
        })
    }

    fn prepare(&self, request: CanonicalLivePrepareRequest) -> CanonicalFuture<CanonicalPrepared> {
        self.call(move |core| {
            core.store
                .as_mut()
                .expect("validated store")
                .prepare_canonical(request)
        })
    }

    fn append(
        &self,
        request: CanonicalLiveAppendRequest,
    ) -> CanonicalFuture<CanonicalCommitDisposition> {
        self.call(move |core| {
            core.store
                .as_mut()
                .expect("validated store")
                .append_canonical(request)
        })
    }

    fn acknowledge(
        &self,
        request: CanonicalAcknowledgement,
    ) -> CanonicalFuture<CanonicalLiveAcknowledged> {
        self.call(move |core| {
            core.store
                .as_mut()
                .expect("validated store")
                .acknowledge_canonical(request)
        })
    }

    fn admit_user(
        &self,
        request: CanonicalLiveUserRequest,
    ) -> CanonicalFuture<CanonicalLiveUserPrepared> {
        self.call(move |core| {
            core.store
                .as_mut()
                .expect("validated store")
                .admit_canonical_user(request)
        })
    }

    fn admit_custom(
        &self,
        request: CanonicalLiveCustomRequest,
    ) -> CanonicalFuture<CanonicalLiveCustomPrepared> {
        self.call(move |core| {
            core.store
                .as_mut()
                .expect("validated store")
                .admit_canonical_custom(request)
        })
    }

    fn begin_provider_step(
        &self,
        request: CanonicalLiveProviderStepRequest,
    ) -> CanonicalFuture<()> {
        self.call(move |core| {
            if core.shutdown_requested || core.abort_requested || core.retry_abort_requested {
                return Err(refused("new canonical provider work is closing or aborted"));
            }
            core.store
                .as_mut()
                .expect("validated store")
                .begin_canonical_provider_step(request)
        })
    }

    fn admit_tool(&self, request: CanonicalLiveToolRequest) -> CanonicalFuture<()> {
        self.call(move |core| {
            core.store
                .as_mut()
                .expect("validated store")
                .admit_canonical_tool(request)
        })
    }
}
