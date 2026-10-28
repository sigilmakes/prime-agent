use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::{ElisionReason, ViewError, ViewEvent, ViewSpec};
use crate::mmds::{Document, TEXT_EXTENSION_NAMESPACE};
use crate::views::evaluate::evaluate_view;

/// Namespaced marker for payloads materialized by the view subsystem.
///
/// V1 view payloads store an extension at this namespace with
/// `layout_mode: "shared_coordinates"` and `boundary_policy: "omit"`.
/// Runtime MMDS replay uses the marker to opt into view-specific behavior
/// without changing the high-level render facade.
pub const VIEW_EXTENSION_NAMESPACE: &str = "org.mmdflux.view.v1";

const PROJECTION_KEY: &str = "projection";
const NODE_RANKS_KEY: &str = "node_ranks";
const EDGE_WAYPOINTS_KEY: &str = "edge_waypoints";
const LABEL_POSITIONS_KEY: &str = "label_positions";

/// Project a materialized view specification over a canonical MMDS payload.
///
/// The returned [`Document`] is a cloned and pruned payload:
///
/// - retained nodes and subgraphs are copied from the canonical payload
/// - edges survive only when both endpoint nodes are retained
/// - surviving edge IDs are preserved verbatim, so IDs may be sparse
/// - the view marker extension is added under [`VIEW_EXTENSION_NAMESPACE`]
/// - text projection `node_ranks` are filtered to retained nodes when present
///
/// The returned [`ViewEvent`] list describes canonical nodes, subgraphs, and
/// edges that did not survive materialization. Unsupported v1 features return
/// [`ViewError::NotImplementedYet`] before payload cloning.
pub fn project(
    canonical: &Document,
    spec: &ViewSpec,
) -> Result<(Document, Vec<ViewEvent>), ViewError> {
    let evaluation = evaluate_view(canonical, spec)?;
    let retained_subgraphs =
        retained_subgraphs(canonical, &evaluation.subgraphs, &evaluation.nodes);

    let mut output = canonical.clone();
    output.nodes = canonical
        .nodes
        .iter()
        .filter(|node| evaluation.nodes.contains(&node.id))
        .cloned()
        .collect();
    output.edges = canonical
        .edges
        .iter()
        .filter(|edge| {
            evaluation.nodes.contains(&edge.source) && evaluation.nodes.contains(&edge.target)
        })
        .cloned()
        .collect();
    output.subgraphs = canonical
        .subgraphs
        .iter()
        .filter(|subgraph| retained_subgraphs.contains(&subgraph.id))
        .cloned()
        .map(|mut subgraph| {
            subgraph.children.retain(|child| {
                evaluation.nodes.contains(child) || retained_subgraphs.contains(child)
            });
            subgraph
                .concurrent_regions
                .retain(|child| retained_subgraphs.contains(child));
            subgraph
        })
        .collect();
    project_extensions(&mut output, &evaluation.nodes);

    let events = view_events(canonical, &evaluation.nodes, &retained_subgraphs);
    Ok((output, events))
}

fn project_extensions(output: &mut Document, retained_nodes: &BTreeSet<String>) {
    output.extensions.insert(
        VIEW_EXTENSION_NAMESPACE.to_string(),
        shared_coordinates_view_extension(),
    );

    if let Some(text_extension) = output.extensions.get_mut(TEXT_EXTENSION_NAMESPACE) {
        prune_text_projection(text_extension, retained_nodes);
    }
}

fn shared_coordinates_view_extension() -> Map<String, Value> {
    let mut extension = Map::new();
    extension.insert(
        "layout_mode".to_string(),
        Value::String("shared_coordinates".to_string()),
    );
    extension.insert(
        "boundary_policy".to_string(),
        Value::String("omit".to_string()),
    );
    extension
}

fn prune_text_projection(extension: &mut Map<String, Value>, retained_nodes: &BTreeSet<String>) {
    let Some(Value::Object(projection)) = extension.get_mut(PROJECTION_KEY) else {
        return;
    };

    if let Some(Value::Object(node_ranks)) = projection.get_mut(NODE_RANKS_KEY) {
        node_ranks.retain(|node_id, _| retained_nodes.contains(node_id));
    }

    // V1 intentionally drops edge-array-indexed projection maps instead of
    // remapping them through sparse view-local edges.
    projection.remove(EDGE_WAYPOINTS_KEY);
    projection.remove(LABEL_POSITIONS_KEY);
}

fn retained_subgraphs(
    output: &Document,
    explicit_subgraphs: &BTreeSet<String>,
    retained_nodes: &BTreeSet<String>,
) -> BTreeSet<String> {
    let parent_by_subgraph: BTreeMap<String, Option<String>> = output
        .subgraphs
        .iter()
        .map(|subgraph| (subgraph.id.clone(), subgraph.parent.clone()))
        .collect();

    let mut retained = BTreeSet::new();
    for subgraph in explicit_subgraphs {
        add_subgraph_with_ancestors(subgraph, &parent_by_subgraph, &mut retained);
    }
    for node in &output.nodes {
        if retained_nodes.contains(&node.id)
            && let Some(parent) = &node.parent
        {
            add_subgraph_with_ancestors(parent, &parent_by_subgraph, &mut retained);
        }
    }
    retained
}

fn add_subgraph_with_ancestors(
    subgraph: &str,
    parent_by_subgraph: &BTreeMap<String, Option<String>>,
    retained: &mut BTreeSet<String>,
) {
    let mut current = Some(subgraph.to_string());
    while let Some(id) = current {
        if !retained.insert(id.clone()) {
            break;
        }
        current = parent_by_subgraph.get(&id).and_then(Clone::clone);
    }
}

fn view_events(
    canonical: &Document,
    retained_nodes: &BTreeSet<String>,
    retained_subgraphs: &BTreeSet<String>,
) -> Vec<ViewEvent> {
    let mut events = Vec::new();

    for node in &canonical.nodes {
        if !retained_nodes.contains(&node.id) {
            events.push(ViewEvent::NodeLeftView {
                id: node.id.clone(),
                reason: ElisionReason::Excluded,
            });
        }
    }

    for subgraph in &canonical.subgraphs {
        if !retained_subgraphs.contains(&subgraph.id) {
            events.push(ViewEvent::SubgraphLeftView {
                id: subgraph.id.clone(),
                reason: ElisionReason::Excluded,
            });
        }
    }

    let mut edge_ordinals: BTreeMap<(String, String), usize> = BTreeMap::new();
    for edge in &canonical.edges {
        let key = (edge.source.clone(), edge.target.clone());
        let ordinal = edge_ordinals.entry(key).or_default();
        let current_ordinal = *ordinal;
        *ordinal += 1;

        if !(retained_nodes.contains(&edge.source) && retained_nodes.contains(&edge.target)) {
            events.push(ViewEvent::EdgeElided {
                source: edge.source.clone(),
                target: edge.target.clone(),
                ordinal: current_ordinal,
                label: edge.label.clone(),
                reason: ElisionReason::EndpointOutsideView,
            });
        }
    }

    events
}
