//! Model events emitted by accepted MMDS model mutations.
//!
//! See the crate-level [Stability](crate#stability) section for the
//! variant-addition and field-addition policy on the public types in this module.
//!
//! Model events describe state transitions accepted by the MMDS model. They are not snapshot diffs.
//! To compare two fully materialized document states, use
//! [`crate::mmds::diff::diff_documents`] and inspect [`crate::mmds::diff::Change`]
//! entries instead.

use super::Subject;

/// Event emitted for an accepted MMDS model state transition.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEvent {
    /// Model event classification.
    pub kind: ModelEventKind,
    /// Entity the event is about.
    pub subject: Subject,
}

/// Kind of state transition accepted by the MMDS model.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelEventKind {
    GeometryLevelChanged,
    DirectionChanged,
    EngineChanged,
    NodeAdded,
    NodeRemoved,
    EdgeAdded,
    EdgeRemoved,
    SubgraphAdded,
    SubgraphRemoved,
    NodeLabelChanged,
    NodeShapeChanged,
    NodeParentChanged,
    NodeStyleChanged,
    EdgeReconnected,
    EdgeEndpointIntentChanged,
    EdgeLabelChanged,
    EdgeStyleChanged,
    SubgraphTitleChanged,
    SubgraphDirectionChanged,
    SubgraphParentChanged,
    SubgraphMembershipChanged,
    SubgraphVisibilityChanged,
    ProfileChanged,
    ExtensionChanged,
}
