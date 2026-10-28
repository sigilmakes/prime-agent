use crate::mmds::diff::ChangeKind;
use crate::mmds::events::ModelEventKind;

// Test-only parity helper for checking that model events and snapshot diffs
// stay aligned where their vocabularies intentionally overlap. Production
// callers should not generally convert events into diff changes: events record
// accepted model mutations, while snapshot diffs report observable differences
// between two materialized documents.
pub(crate) fn model_event_kind_to_change_kind(kind: ModelEventKind) -> ChangeKind {
    match kind {
        ModelEventKind::GeometryLevelChanged => ChangeKind::GeometryLevelChanged,
        ModelEventKind::DirectionChanged => ChangeKind::DirectionChanged,
        ModelEventKind::EngineChanged => ChangeKind::EngineChanged,
        ModelEventKind::NodeAdded => ChangeKind::NodeAdded,
        ModelEventKind::NodeRemoved => ChangeKind::NodeRemoved,
        ModelEventKind::EdgeAdded => ChangeKind::EdgeAdded,
        ModelEventKind::EdgeRemoved => ChangeKind::EdgeRemoved,
        ModelEventKind::SubgraphAdded => ChangeKind::SubgraphAdded,
        ModelEventKind::SubgraphRemoved => ChangeKind::SubgraphRemoved,
        ModelEventKind::NodeLabelChanged => ChangeKind::NodeLabelChanged,
        ModelEventKind::NodeShapeChanged => ChangeKind::NodeShapeChanged,
        ModelEventKind::NodeParentChanged => ChangeKind::NodeParentChanged,
        ModelEventKind::NodeStyleChanged => ChangeKind::NodeStyleChanged,
        ModelEventKind::EdgeReconnected => ChangeKind::EdgeReconnected,
        ModelEventKind::EdgeEndpointIntentChanged => ChangeKind::EdgeEndpointIntentChanged,
        ModelEventKind::EdgeLabelChanged => ChangeKind::EdgeLabelChanged,
        ModelEventKind::EdgeStyleChanged => ChangeKind::EdgeStyleChanged,
        ModelEventKind::SubgraphTitleChanged => ChangeKind::SubgraphTitleChanged,
        ModelEventKind::SubgraphDirectionChanged => ChangeKind::SubgraphDirectionChanged,
        ModelEventKind::SubgraphParentChanged => ChangeKind::SubgraphParentChanged,
        ModelEventKind::SubgraphMembershipChanged => ChangeKind::SubgraphMembershipChanged,
        ModelEventKind::SubgraphVisibilityChanged => ChangeKind::SubgraphVisibilityChanged,
        ModelEventKind::ProfileChanged => ChangeKind::ProfileChanged,
        ModelEventKind::ExtensionChanged => ChangeKind::ExtensionChanged,
    }
}
