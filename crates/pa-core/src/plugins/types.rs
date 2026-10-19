use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Complete desired source of one enabled plugin. Omission means disabled, not deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginSource {
    pub name: String,
    pub source: String,
    /// Owned JSON exposed as `config` in this generation's private Lua environment.
    pub config: Value,
}

/// Callback forms actually implemented by the host. Engine hook-stage policy stays outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContributionKind {
    Export,
    Command,
    Hook,
    ContextStrategy,
    Compactor,
    Provider,
}

/// A unary callback cannot emit updates. Streaming callbacks have bounded backpressure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationMode {
    Unary,
    Streaming,
}

/// Explicit data contract for context strategy and compactor callbacks.
/// The host remains JSON-only and never changes invocation inputs to fit a contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContextDataMode {
    /// The existing full JSON message and canonical-entry contract.
    #[default]
    Json,
    /// Opt in to bounded, invocation-scoped context references supplied and
    /// resolved by the engine instead of materializing full bodies in Lua.
    RefsV1,
}

/// An immutable registration descriptor, also the generation-bound invocation target.
/// Export routes are `plugin.<plugin>.<name>`; command routes are `/<name>`.
/// Hook names are scoped to their plugin, not globally dispatched stages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contribution {
    pub plugin: String,
    pub generation: Uuid,
    pub kind: ContributionKind,
    pub name: String,
    pub description: String,
    pub mode: InvocationMode,
    /// Explicit context integration contract. Non-context contributions use `Json`;
    /// the Lua host does not rewrite inputs or resolve references itself.
    pub context_data: ContextDataMode,
}

/// An owned snapshot cannot mutate the host registry. Old descriptors never retarget a reload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContributionSnapshot {
    pub revision: u64,
    pub contributions: Vec<Contribution>,
}

/// Host failures remain distinct from successful callback data and intentional hook decisions.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum PluginError {
    #[error("invalid plugin declaration: {0}")]
    Invalid(String),
    #[error("plugin Lua error: {0}")]
    Lua(String),
    #[error("plugin generation retired")]
    Retired,
    #[error("plugin invocation cancelled")]
    Cancelled,
    #[error("contribution is not in the active snapshot")]
    Missing,
    #[error("another reconcile committed; retry against its snapshot")]
    ReconcileConflict,
    #[error("plugin invocation requires a Tokio runtime")]
    NoRuntime,
    #[error("plugin invocation ended without an outcome")]
    Interrupted,
}

/// JSON-only request at the async capability seam. One host belongs to one session.
/// The adapter must authorize operations, use explicit private runtime paths, and
/// honor cancellation. The host also drops its future on cancellation/retirement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityRequest {
    pub session: String,
    pub plugin: String,
    pub generation: Uuid,
    /// Stable across capability calls in one callback, distinct from its generation.
    pub invocation: Uuid,
    pub operation: String,
    pub input: Value,
}

/// Boxed only at this dynamically supplied adapter seam, not for internal async work.
pub type CapabilityFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'static>>;

/// Adapter for explicitly supplied host operations (for example auxiliary inference).
/// No default filesystem, network, credentials, providers, or session access exists.
/// Implementations must not block in `call` or retain authority after cancellation.
pub trait HostCapabilities: Send + Sync + 'static {
    fn call(&self, request: CapabilityRequest, cancellation: CancellationToken)
        -> CapabilityFuture;
}

/// The terminal callback result carries the declared delivery mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvocationOutcome {
    Unary(Value),
    Streamed(Value),
}

/// Exactly one terminal event follows zero or more updates, unless the consumer drops.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvocationEvent {
    Update(Value),
    Finished(Result<InvocationOutcome, PluginError>),
}

/// A native async consumer. Dropping it cancels work. Updates and terminal results
/// are fenced again at delivery, including results queued before retirement.
pub struct Invocation {
    pub(super) receiver: mpsc::Receiver<InvocationEvent>,
    pub(super) cancellation: CancellationToken,
    pub(super) retirement: CancellationToken,
    // Keep the generation alive while its invocation is observed.
    pub(super) _generation: Arc<super::runtime::Generation>,
    pub(super) finished: bool,
}

impl Invocation {
    /// Receive the next update or terminal outcome. After the terminal event, returns `None`.
    pub async fn next(&mut self) -> Option<InvocationEvent> {
        if self.finished {
            return None;
        }
        let event = tokio::select! {
            biased;
            () = self.retirement.cancelled() => InvocationEvent::Finished(Err(PluginError::Retired)),
            () = self.cancellation.cancelled() => InvocationEvent::Finished(Err(PluginError::Cancelled)),
            event = self.receiver.recv() => event.unwrap_or(
                InvocationEvent::Finished(Err(PluginError::Interrupted))
            ),
        };
        // A producer can win the channel race with retirement; never publish that stale value.
        let event = if self.retirement.is_cancelled() {
            InvocationEvent::Finished(Err(PluginError::Retired))
        } else if self.cancellation.is_cancelled() {
            InvocationEvent::Finished(Err(PluginError::Cancelled))
        } else {
            event
        };
        self.finished = matches!(event, InvocationEvent::Finished(_));
        if self.finished {
            self.receiver.close();
        }
        Some(event)
    }
}

impl Drop for Invocation {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
