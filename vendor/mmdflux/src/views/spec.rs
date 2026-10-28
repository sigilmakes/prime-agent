use serde::{Deserialize, Serialize};

use crate::graph::Shape;

/// Materialized view request over a canonical MMDS payload.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSpec {
    /// Ordered include/exclude statements used to build the view keep-set.
    ///
    /// Statements are evaluated in order. `Include` adds matching elements and
    /// `Exclude` removes matching elements from the accumulated keep-set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub statements: Vec<ViewStatement>,
    /// Layout policy for the returned view payload.
    ///
    /// V1 supports [`LayoutMode::SharedCoordinates`] only.
    #[serde(default)]
    pub layout: LayoutMode,
    /// Policy for edges that touch nodes outside the view.
    ///
    /// V1 supports [`BoundaryPolicy::Omit`] only; omitted edges are reported
    /// as [`crate::views::ViewEvent::EdgeElided`].
    #[serde(default)]
    pub boundary: BoundaryPolicy,
    /// Policy for retained subgraph structure.
    ///
    /// V1 supports [`CompoundPolicy::Preserve`] only.
    #[serde(default)]
    pub compound: CompoundPolicy,
}

impl ViewSpec {
    /// Build a view spec from the ordered selector statements that define it.
    pub fn new(statements: Vec<ViewStatement>) -> Self {
        Self {
            statements,
            ..Self::default()
        }
    }
}

/// Include or exclude a selector from the view keep-set.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewStatement {
    /// Add the selector result to the current keep-set.
    Include(Selector),
    /// Remove the selector result from the current keep-set.
    Exclude(Selector),
}

/// Selector expression for v1 and forward-compatible follow-up view slices.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Selector {
    /// Select every node and subgraph in the payload.
    All,
    /// Select one anchor.
    Anchor(AnchorRef),
    /// Select a node anchor plus graph neighbors within `hops` edge hops.
    Traversal {
        /// Starting anchor for the traversal.
        ///
        /// V1 supports node anchors only for traversal.
        anchor: AnchorRef,
        /// Direction to follow through the graph edge topology.
        direction: TraversalDirection,
        /// Maximum number of edge hops from the anchor.
        hops: u32,
    },
    /// Select nodes matching a node predicate.
    Predicate(NodePredicate),
    /// Recursively include a subgraph, child subgraphs, and descendant nodes.
    SubgraphDescendants(String),
}

/// Stable anchor reference used by view selectors.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnchorRef {
    /// Anchor on a node ID.
    Node(String),
    /// Select only the subgraph container. Use `Selector::SubgraphDescendants`
    /// when the view should include the subgraph contents.
    Subgraph(String),
    /// Anchor on an edge identity tuple.
    ///
    /// Reserved for a later edge-aware view slice.
    Edge(EdgeAnchor),
}

/// Edge anchor shape reserved for a later edge-aware view slice.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeAnchor {
    /// Source node ID.
    pub source: String,
    /// Target node ID.
    pub target: String,
    /// Zero-based ordinal among edges with the same source and target.
    pub ordinal: usize,
    /// Optional edge label used to disambiguate human-authored anchors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Direction used by hop traversal selectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraversalDirection {
    /// Follow incoming edges toward dependencies or callers.
    Upstream,
    /// Follow outgoing edges toward dependents or callees.
    Downstream,
    /// Follow both incoming and outgoing edges.
    Neighbors,
}

/// Node predicate for selector filters.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodePredicate {
    /// Select nodes whose MMDS `shape` equals the supplied graph shape.
    Shape(Shape),
    /// Select nodes whose `parent` subgraph ID equals the supplied value.
    Parent(String),
    /// Select nodes by tag metadata.
    ///
    /// Reserved for a later tag-aware view slice.
    Tag(String),
}

/// Layout policy for the materialized view.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutMode {
    /// Preserve canonical coordinates and mark the payload as a shared-coordinate view.
    #[default]
    SharedCoordinates,
    /// Repack the retained subgraph into a compact layout.
    ///
    /// Reserved for a later layout slice.
    Compact,
    /// Reflow locally around the selected anchor.
    ///
    /// Reserved for a later layout slice.
    LocalReflow,
    /// Update a prior view incrementally.
    ///
    /// Reserved for a later layout slice.
    Incremental,
}

/// Boundary policy for elements connected to elided endpoints.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundaryPolicy {
    /// Drop edges whose source or target node is outside the view.
    #[default]
    Omit,
    /// Replace omitted boundary edges with aggregate stubs.
    ///
    /// Reserved for a later boundary slice.
    Stub {
        /// Minimum number of omitted edges before a grouped stub may be used.
        aggregate_threshold: u32,
    },
}

/// Compound/subgraph handling policy.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompoundPolicy {
    /// Preserve retained subgraphs and their ancestor chain.
    #[default]
    Preserve,
    /// Remove subgraph containers and keep only retained leaf nodes.
    ///
    /// Reserved for a later compound-layout slice.
    Flatten,
}
