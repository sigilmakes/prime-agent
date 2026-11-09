//! Session-local, reloadable Lua plugin host.
//!
//! Three operations own the lifecycle: reconcile desired sources, read an immutable
//! contribution snapshot, and invoke a snapshot contribution. No Lua values escape.
//! See the crate README for the declaration and capability contracts.

pub(crate) mod context;
mod host;
pub mod manifest;
mod names;
pub(crate) use names::validate_plugin_name;
mod runtime;
pub(crate) mod source;
mod types;

pub use host::PluginHost;
pub use manifest::{ManifestCatalog, ProviderFact};
pub use types::{
    CapabilityFuture, CapabilityRequest, ContextDataMode, Contribution, ContributionKind,
    ContributionSnapshot, HostCapabilities, Invocation, InvocationEvent, InvocationMode,
    InvocationOutcome, PluginError, PluginSource,
};

#[cfg(test)]
mod tests;
