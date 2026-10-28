use serde::{Deserialize, Serialize};

/// Reason an element is absent from a materialized view.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElisionReason {
    /// The element did not match the active view selectors.
    Excluded,
    /// The element references at least one endpoint outside the view.
    EndpointOutsideView,
    /// The element needs a feature deferred from the v1 view slice.
    NotImplementedYet,
}

/// Diagnostic event emitted while materializing a view.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewEvent {
    /// A canonical node was omitted from the materialized view.
    NodeLeftView {
        /// MMDS node ID from the canonical payload.
        id: String,
        /// Why the node is absent from the view.
        reason: ElisionReason,
    },
    /// A canonical subgraph was omitted from the materialized view.
    SubgraphLeftView {
        /// MMDS subgraph ID from the canonical payload.
        id: String,
        /// Why the subgraph is absent from the view.
        reason: ElisionReason,
    },
    /// A canonical edge was omitted from the materialized view.
    EdgeElided {
        /// Source node ID from the canonical edge.
        source: String,
        /// Target node ID from the canonical edge.
        target: String,
        /// Zero-based ordinal among canonical edges with the same source and target.
        ordinal: usize,
        /// Canonical edge label, when present.
        label: Option<String>,
        /// Why the edge is absent from the view.
        reason: ElisionReason,
    },
}
