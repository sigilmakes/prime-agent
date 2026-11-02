//! Live model catalog subsystem for Prime Agent.
//! Everything the catalog can do is select among transports the client
//! compiled in; it can never introduce a transport or change what a
//! request sends. Depends on pa-ai and pa-types; nothing above pa-agent.

pub mod bundled;
pub mod cache;
pub mod compat;
pub mod fetch;
pub mod offline;
pub mod ollama_cloud;
pub mod pinning;
pub mod prime_inference;
mod provider_catalogs;
#[cfg(test)]
mod provider_catalogs_tests;
pub mod provider_discovery;
pub mod schema;
pub mod transports;

mod chain;
mod discovery;
pub use discovery::{
    ApplyPublication, CatalogFormat, CatalogInputs, CatalogSnapshot, RefreshPolicy,
    RefreshPublication, RefreshRequest, ResolvedSource, SnapshotRequirement,
};

pub use chain::ModelCatalog;
pub use pa_types::ai::Model;
pub use provider_catalogs::DEFAULT_MODEL_CATALOG_URL;

pub const CATALOG_REFRESH_INTERVAL_MS: u64 = 60 * 60_000;
