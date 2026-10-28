use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::{
    AnchorRef, BoundaryPolicy, CompoundPolicy, LayoutMode, NodePredicate, Selector,
    TraversalDirection, ViewError, ViewSpec, ViewStatement,
};
use crate::mmds::Document;

/// Deterministic keep-set produced by evaluating a `ViewSpec`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ViewEvaluation {
    /// Node IDs retained by the evaluated view.
    pub(super) nodes: BTreeSet<String>,
    /// Subgraph IDs explicitly retained by the evaluated view.
    ///
    /// Ancestor subgraphs required to contain retained nodes are added by
    /// [`project`](crate::views::project) during materialization.
    pub(super) subgraphs: BTreeSet<String>,
}

impl ViewEvaluation {
    fn extend(&mut self, other: ViewEvaluation) {
        self.nodes.extend(other.nodes);
        self.subgraphs.extend(other.subgraphs);
    }

    fn remove(&mut self, other: ViewEvaluation) {
        for node in other.nodes {
            self.nodes.remove(&node);
        }
        for subgraph in other.subgraphs {
            self.subgraphs.remove(&subgraph);
        }
    }
}

/// Evaluate a view spec into node and subgraph keep-sets.
///
/// This function resolves selectors only. It does not clone or prune the MMDS
/// payload, preserve ancestor subgraphs, filter edges, or emit elision events;
/// use [`project`](crate::views::project) for the full materialized view.
pub(super) fn evaluate_view(
    output: &Document,
    spec: &ViewSpec,
) -> Result<ViewEvaluation, ViewError> {
    ensure_supported_spec(spec)?;

    let mut evaluation = ViewEvaluation::default();
    for statement in &spec.statements {
        match statement {
            ViewStatement::Include(selector) => {
                evaluation.extend(evaluate_selector(output, selector)?);
            }
            ViewStatement::Exclude(selector) => {
                evaluation.remove(evaluate_selector(output, selector)?);
            }
        }
    }
    Ok(evaluation)
}

fn ensure_supported_spec(spec: &ViewSpec) -> Result<(), ViewError> {
    if !matches!(spec.layout, LayoutMode::SharedCoordinates) {
        return not_implemented("non-shared-coordinate layout modes");
    }
    if !matches!(spec.boundary, BoundaryPolicy::Omit) {
        return not_implemented("boundary stubs");
    }
    if !matches!(spec.compound, CompoundPolicy::Preserve) {
        return not_implemented("compound flattening");
    }
    Ok(())
}

fn evaluate_selector(output: &Document, selector: &Selector) -> Result<ViewEvaluation, ViewError> {
    match selector {
        Selector::All => Ok(all_elements(output)),
        Selector::Anchor(anchor) => evaluate_anchor(output, anchor),
        Selector::Traversal {
            anchor,
            direction,
            hops,
        } => evaluate_traversal(output, anchor, *direction, *hops),
        Selector::Predicate(predicate) => evaluate_predicate(output, predicate),
        Selector::SubgraphDescendants(id) => evaluate_subgraph_descendants(output, id),
    }
}

fn all_elements(output: &Document) -> ViewEvaluation {
    ViewEvaluation {
        nodes: output.nodes.iter().map(|node| node.id.clone()).collect(),
        subgraphs: output
            .subgraphs
            .iter()
            .map(|subgraph| subgraph.id.clone())
            .collect(),
    }
}

fn evaluate_anchor(output: &Document, anchor: &AnchorRef) -> Result<ViewEvaluation, ViewError> {
    match anchor {
        AnchorRef::Node(id) => {
            if !has_node(output, id) {
                return unknown_anchor(id);
            }
            Ok(ViewEvaluation {
                nodes: BTreeSet::from([id.clone()]),
                subgraphs: BTreeSet::new(),
            })
        }
        AnchorRef::Subgraph(id) => {
            if !has_subgraph(output, id) {
                return unknown_anchor(id);
            }
            Ok(ViewEvaluation {
                nodes: BTreeSet::new(),
                subgraphs: BTreeSet::from([id.clone()]),
            })
        }
        AnchorRef::Edge(_) => not_implemented("edge anchors"),
    }
}

fn evaluate_traversal(
    output: &Document,
    anchor: &AnchorRef,
    direction: TraversalDirection,
    hops: u32,
) -> Result<ViewEvaluation, ViewError> {
    let AnchorRef::Node(anchor_id) = anchor else {
        return match anchor {
            AnchorRef::Subgraph(_) => not_implemented("subgraph traversal"),
            AnchorRef::Edge(_) => not_implemented("edge anchors"),
            AnchorRef::Node(_) => unreachable!(),
        };
    };
    if !has_node(output, anchor_id) {
        return unknown_anchor(anchor_id);
    }

    let (outgoing, incoming) = adjacency(output);
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::from([(anchor_id.clone(), 0)]);
    visited.insert(anchor_id.clone());

    while let Some((node, depth)) = queue.pop_front() {
        if depth == hops {
            continue;
        }
        for next in traversal_neighbors(&node, direction, &outgoing, &incoming) {
            if visited.insert(next.clone()) {
                queue.push_back((next, depth + 1));
            }
        }
    }

    Ok(ViewEvaluation {
        nodes: visited,
        subgraphs: BTreeSet::new(),
    })
}

fn evaluate_predicate(
    output: &Document,
    predicate: &NodePredicate,
) -> Result<ViewEvaluation, ViewError> {
    let nodes = match predicate {
        NodePredicate::Shape(shape) => output
            .nodes
            .iter()
            .filter(|node| node.shape == *shape)
            .map(|node| node.id.clone())
            .collect(),
        NodePredicate::Parent(parent) => output
            .nodes
            .iter()
            .filter(|node| node.parent.as_deref() == Some(parent.as_str()))
            .map(|node| node.id.clone())
            .collect(),
        NodePredicate::Tag(_) => return not_implemented("tag predicates"),
    };

    Ok(ViewEvaluation {
        nodes,
        subgraphs: BTreeSet::new(),
    })
}

fn evaluate_subgraph_descendants(output: &Document, id: &str) -> Result<ViewEvaluation, ViewError> {
    if !has_subgraph(output, id) {
        return unknown_anchor(id);
    }

    let node_ids: BTreeSet<&str> = output.nodes.iter().map(|node| node.id.as_str()).collect();
    let subgraph_ids: BTreeSet<&str> = output
        .subgraphs
        .iter()
        .map(|subgraph| subgraph.id.as_str())
        .collect();
    let children_by_parent = subgraphs_by_parent(output);

    let mut evaluation = ViewEvaluation::default();
    let mut queue = VecDeque::from([id.to_string()]);
    while let Some(subgraph_id) = queue.pop_front() {
        if !evaluation.subgraphs.insert(subgraph_id.clone()) {
            continue;
        }

        if let Some(subgraph) = output
            .subgraphs
            .iter()
            .find(|subgraph| subgraph.id == subgraph_id)
        {
            for child in &subgraph.children {
                if node_ids.contains(child.as_str()) {
                    evaluation.nodes.insert(child.clone());
                }
                if subgraph_ids.contains(child.as_str()) {
                    queue.push_back(child.clone());
                }
            }
        }

        if let Some(child_subgraphs) = children_by_parent.get(subgraph_id.as_str()) {
            for child_subgraph in child_subgraphs {
                queue.push_back((*child_subgraph).to_string());
            }
        }
    }

    Ok(evaluation)
}

fn adjacency(output: &Document) -> (BTreeMap<String, Vec<String>>, BTreeMap<String, Vec<String>>) {
    let mut outgoing: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut incoming: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for edge in &output.edges {
        outgoing
            .entry(edge.source.clone())
            .or_default()
            .push(edge.target.clone());
        incoming
            .entry(edge.target.clone())
            .or_default()
            .push(edge.source.clone());
    }

    (outgoing, incoming)
}

fn traversal_neighbors(
    node: &str,
    direction: TraversalDirection,
    outgoing: &BTreeMap<String, Vec<String>>,
    incoming: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    match direction {
        TraversalDirection::Downstream => outgoing.get(node).cloned().unwrap_or_default(),
        TraversalDirection::Upstream => incoming.get(node).cloned().unwrap_or_default(),
        TraversalDirection::Neighbors => {
            let mut neighbors = outgoing.get(node).cloned().unwrap_or_default();
            neighbors.extend(incoming.get(node).cloned().unwrap_or_default());
            neighbors
        }
    }
}

fn subgraphs_by_parent(output: &Document) -> BTreeMap<&str, Vec<&str>> {
    let mut by_parent: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for subgraph in &output.subgraphs {
        if let Some(parent) = subgraph.parent.as_deref() {
            by_parent
                .entry(parent)
                .or_default()
                .push(subgraph.id.as_str());
        }
    }
    by_parent
}

fn has_node(output: &Document, id: &str) -> bool {
    output.nodes.iter().any(|node| node.id == id)
}

fn has_subgraph(output: &Document, id: &str) -> bool {
    output.subgraphs.iter().any(|subgraph| subgraph.id == id)
}

fn unknown_anchor<T>(id: &str) -> Result<T, ViewError> {
    Err(ViewError::UnknownAnchor { id: id.to_string() })
}

fn not_implemented<T>(feature: &str) -> Result<T, ViewError> {
    Err(ViewError::NotImplementedYet {
        feature: feature.to_string(),
    })
}
