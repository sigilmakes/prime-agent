//! Phase 1: Make the graph acyclic by identifying back-edges.
//!
//! Uses a DFS-based approach to identify back-edges - edges that point
//! to ancestors in the DFS tree. This preserves the natural forward flow
//! of the graph better than minimum feedback arc set algorithms.

use std::collections::BTreeSet;

use super::graph::LayoutGraph;
use super::types::AcyclicPolicy;

#[derive(Clone, Copy)]
struct CompoundEdge {
    edge_idx: usize,
    from: usize,
    to: usize,
    from_rank: usize,
    to_rank: usize,
}

/// Identify back-edges that need to be reversed for acyclicity.
/// Marks edges in the LayoutGraph's reversed_edges set.
///
/// Uses DFS to find back-edges (edges pointing to ancestors in the DFS tree).
/// This preserves the natural forward flow of the graph better than
/// greedy_feedback_arc_set which may reverse arbitrary edges.
#[cfg(test)]
pub fn run(graph: &mut LayoutGraph) {
    run_with_policy(graph, AcyclicPolicy::default());
}

pub(super) fn run_with_policy(graph: &mut LayoutGraph, policy: AcyclicPolicy) {
    let n = graph.node_ids.len();
    if n == 0 {
        return;
    }

    // Build adjacency list: node -> [(edge_idx, target_node)]
    let mut adj: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for (edge_idx, &(from, to, _)) in graph.edges.iter().enumerate() {
        adj[from].push((edge_idx, to));
    }

    // DFS state
    let mut visited = vec![false; n];
    let mut in_stack = vec![false; n]; // Nodes currently in the recursion stack
    let mut back_edges: BTreeSet<usize> = BTreeSet::new();

    // Run DFS from each unvisited node (handles disconnected components)
    for start in 0..n {
        if !visited[start] {
            dfs_find_back_edges(start, &adj, &mut visited, &mut in_stack, &mut back_edges);
        }
    }

    // For compound graphs, detect compound-level feedback edges that the DFS
    // missed. An edge from compound X to compound Y participates in a
    // compound-level cycle if there is already a path from Y back to X through
    // other compounds. See issue #155.
    if matches!(policy, AcyclicPolicy::SemanticCompoundFeedback) && !graph.compound_nodes.is_empty()
    {
        detect_compound_back_edges(graph, &mut back_edges);
    }

    graph.reversed_edges = back_edges;
}

/// Detect cross-compound edges that participate in compound-level cycles.
///
/// Builds a compound-level graph from un-reversed cross-compound edges, then
/// iteratively removes one deterministic feedback edge per remaining cycle.
fn detect_compound_back_edges(graph: &LayoutGraph, back_edges: &mut BTreeSet<usize>) {
    use std::collections::HashMap;

    // Map compound node index to a local id for compact cycle checks.
    let compound_indices: Vec<usize> = graph.compound_nodes.iter().copied().collect();
    if compound_indices.len() < 2 {
        return;
    }
    let mut compound_id: HashMap<usize, usize> = HashMap::new();
    for (local, &idx) in compound_indices.iter().enumerate() {
        compound_id.insert(idx, local);
    }
    let nc = compound_indices.len();
    let compound_rank: Vec<usize> = compound_indices
        .iter()
        .map(|&idx| {
            graph.compound_semantic_order[idx]
                .or(graph.model_order[idx])
                .unwrap_or(idx)
        })
        .collect();

    // Build compound-level edges from un-reversed cross-compound original edges.
    // Each original edge remains a distinct candidate so tie-breaking can use
    // original edge index and preserve deterministic output.
    let mut compound_edges = Vec::new();

    for (edge_idx, &(from, to, _)) in graph.edges.iter().enumerate() {
        if back_edges.contains(&edge_idx) {
            continue;
        }
        let from_compound = graph.parents[from];
        let to_compound = graph.parents[to];
        if let (Some(fc), Some(tc)) = (from_compound, to_compound)
            && fc != tc
            && let (Some(&fc_local), Some(&tc_local)) = (compound_id.get(&fc), compound_id.get(&tc))
        {
            compound_edges.push(CompoundEdge {
                edge_idx,
                from: fc_local,
                to: tc_local,
                from_rank: compound_rank[fc_local],
                to_rank: compound_rank[tc_local],
            });
        }
    }

    while let Some(edge_idx) = select_compound_feedback_edge(nc, &compound_edges) {
        back_edges.insert(edge_idx);
        compound_edges.retain(|edge| edge.edge_idx != edge_idx);
    }
}

#[derive(Clone, Copy)]
struct CompoundFeedbackCandidate {
    edge_idx: usize,
    from_rank: usize,
    to_rank: usize,
}

fn select_compound_feedback_edge(
    compound_count: usize,
    compound_edges: &[CompoundEdge],
) -> Option<usize> {
    let mut candidates = Vec::new();

    for edge in compound_edges {
        if compound_path_exists(
            compound_count,
            compound_edges,
            edge.edge_idx,
            edge.to,
            edge.from,
        ) {
            candidates.push(CompoundFeedbackCandidate {
                edge_idx: edge.edge_idx,
                from_rank: edge.from_rank,
                to_rank: edge.to_rank,
            });
        }
    }

    candidates
        .iter()
        .filter(|candidate| candidate.from_rank > candidate.to_rank)
        .min_by(|a, b| {
            let a_span = a.from_rank - a.to_rank;
            let b_span = b.from_rank - b.to_rank;
            b_span
                .cmp(&a_span)
                .then_with(|| a.edge_idx.cmp(&b.edge_idx))
        })
        .or_else(|| {
            candidates.iter().min_by_key(|candidate| {
                // If no candidate is semantic-backward, reverse the edge that
                // least violates semantic order, then fall back to edge order.
                (
                    candidate.to_rank.saturating_sub(candidate.from_rank),
                    candidate.edge_idx,
                )
            })
        })
        .map(|candidate| candidate.edge_idx)
}

fn compound_path_exists(
    compound_count: usize,
    compound_edges: &[CompoundEdge],
    skip_edge_idx: usize,
    from: usize,
    to: usize,
) -> bool {
    if from == to {
        return true;
    }

    let mut visited = vec![false; compound_count];
    let mut stack = vec![from];

    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if visited[node] {
            continue;
        }
        visited[node] = true;

        for edge in compound_edges {
            if edge.edge_idx == skip_edge_idx || edge.from != node {
                continue;
            }
            if !visited[edge.to] {
                stack.push(edge.to);
            }
        }
    }

    false
}

/// DFS helper to find back-edges.
fn dfs_find_back_edges(
    node: usize,
    adj: &[Vec<(usize, usize)>],
    visited: &mut [bool],
    in_stack: &mut [bool],
    back_edges: &mut BTreeSet<usize>,
) {
    visited[node] = true;
    in_stack[node] = true;

    for &(edge_idx, target) in &adj[node] {
        if !visited[target] {
            // Tree edge - recurse
            dfs_find_back_edges(target, adj, visited, in_stack, back_edges);
        } else if in_stack[target] {
            // Back edge - target is an ancestor in the current DFS path
            back_edges.insert(edge_idx);
        }
        // Cross edges (visited but not in stack) are fine, don't reverse
    }

    in_stack[node] = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::graph::algorithms::layered::graph::DiGraph;

    #[test]
    fn test_acyclic_no_cycle() {
        let mut graph: DiGraph<()> = DiGraph::new();
        graph.add_node("A", ());
        graph.add_node("B", ());
        graph.add_node("C", ());
        graph.add_edge("A", "B");
        graph.add_edge("B", "C");

        let mut lg = LayoutGraph::from_digraph(&graph, |_, _| (10.0, 10.0));
        run(&mut lg);

        assert!(lg.reversed_edges.is_empty());
    }

    #[test]
    fn test_acyclic_simple_cycle() {
        let mut graph: DiGraph<()> = DiGraph::new();
        graph.add_node("A", ());
        graph.add_node("B", ());
        graph.add_edge("A", "B");
        graph.add_edge("B", "A"); // Cycle

        let mut lg = LayoutGraph::from_digraph(&graph, |_, _| (10.0, 10.0));
        run(&mut lg);

        // One edge should be marked for reversal
        assert_eq!(lg.reversed_edges.len(), 1);
    }

    #[test]
    fn test_acyclic_triangle_cycle() {
        let mut graph: DiGraph<()> = DiGraph::new();
        graph.add_node("A", ());
        graph.add_node("B", ());
        graph.add_node("C", ());
        graph.add_edge("A", "B");
        graph.add_edge("B", "C");
        graph.add_edge("C", "A"); // Creates cycle

        let mut lg = LayoutGraph::from_digraph(&graph, |_, _| (10.0, 10.0));
        run(&mut lg);

        // One edge should be reversed to break the cycle
        assert_eq!(lg.reversed_edges.len(), 1);
    }

    #[test]
    fn test_acyclic_self_loop() {
        let mut graph: DiGraph<()> = DiGraph::new();
        graph.add_node("A", ());
        graph.add_edge("A", "A"); // Self-loop

        let mut lg = LayoutGraph::from_digraph(&graph, |_, _| (10.0, 10.0));
        run(&mut lg);

        // Self-loop should be marked for reversal
        assert_eq!(lg.reversed_edges.len(), 1);
    }

    fn compound_cycle_graph_with_order(compound_insertion_order: &[&str]) -> LayoutGraph {
        // c2 -> a2 doesn't form a directed cycle but creates a compound-level
        // cycle: sg_a -> sg_b -> sg_c -> sg_a via a1 -> b1, b1 -> c1,
        // c2 -> a2. The acyclic phase should detect and reverse c2 -> a2.
        let mut graph: DiGraph<()> = DiGraph::new();
        graph.add_node("a1", ());
        graph.add_node("a2", ());
        graph.add_node("b1", ());
        graph.add_node("c1", ());
        graph.add_node("c2", ());
        for compound in compound_insertion_order {
            graph.add_node(*compound, ());
        }
        graph.set_compound_semantic_order("sg_a", 0);
        graph.set_compound_semantic_order("sg_b", 1);
        graph.set_compound_semantic_order("sg_c", 2);
        graph.add_edge("a1", "b1"); // 0: sg_a -> sg_b
        graph.add_edge("b1", "c1"); // 1: sg_b -> sg_c
        graph.add_edge("c2", "a2"); // 2: sg_c -> sg_a (compound back-edge)
        graph.set_parent("a1", "sg_a");
        graph.set_parent("a2", "sg_a");
        graph.set_parent("b1", "sg_b");
        graph.set_parent("c1", "sg_c");
        graph.set_parent("c2", "sg_c");

        LayoutGraph::from_digraph(&graph, |_, _| (10.0, 10.0))
    }

    #[test]
    fn test_acyclic_compound_level_back_edge() {
        for compound_order in [["sg_a", "sg_b", "sg_c"], ["sg_c", "sg_b", "sg_a"]] {
            let mut lg = compound_cycle_graph_with_order(&compound_order);
            run(&mut lg);

            assert!(
                lg.reversed_edges.contains(&2),
                "Edge c2 -> a2 (index 2) should be reversed as a compound-level back-edge; compound insertion order={compound_order:?}; got {:?}",
                lg.reversed_edges
            );
            assert!(
                !lg.reversed_edges.contains(&1),
                "Edge b1 -> c1 (index 1) should remain forward; compound insertion order={compound_order:?}; got {:?}",
                lg.reversed_edges
            );
        }
    }

    #[test]
    fn test_acyclic_policy_dfs_only_skips_compound_feedback_edge() {
        let mut lg = compound_cycle_graph_with_order(&["sg_c", "sg_b", "sg_a"]);
        run_with_policy(&mut lg, AcyclicPolicy::DfsOnly);

        assert!(
            !lg.reversed_edges.contains(&2),
            "DfsOnly should skip semantic compound feedback detection; got {:?}",
            lg.reversed_edges
        );
    }

    #[test]
    fn test_acyclic_policy_semantic_compound_feedback_reverses_c_to_a() {
        let mut lg = compound_cycle_graph_with_order(&["sg_c", "sg_b", "sg_a"]);
        run_with_policy(&mut lg, AcyclicPolicy::SemanticCompoundFeedback);

        assert!(
            lg.reversed_edges.contains(&2),
            "SemanticCompoundFeedback should preserve compound feedback detection; got {:?}",
            lg.reversed_edges
        );
    }

    #[test]
    fn test_acyclic_compound_level_back_edges_converge_across_disjoint_cycles() {
        let mut graph: DiGraph<()> = DiGraph::new();
        for node in ["a1", "a2", "b1", "c1", "c2", "d1", "d2", "e1", "f1", "f2"] {
            graph.add_node(node, ());
        }
        for (order, compound) in ["sg_a", "sg_b", "sg_c", "sg_d", "sg_e", "sg_f"]
            .iter()
            .enumerate()
        {
            graph.add_node(*compound, ());
            graph.set_compound_semantic_order(*compound, order);
        }

        graph.add_edge("a1", "b1"); // 0: sg_a -> sg_b
        graph.add_edge("b1", "c1"); // 1: sg_b -> sg_c
        graph.add_edge("c2", "a2"); // 2: sg_c -> sg_a
        graph.add_edge("d1", "e1"); // 3: sg_d -> sg_e
        graph.add_edge("e1", "f1"); // 4: sg_e -> sg_f
        graph.add_edge("f2", "d2"); // 5: sg_f -> sg_d

        graph.set_parent("a1", "sg_a");
        graph.set_parent("a2", "sg_a");
        graph.set_parent("b1", "sg_b");
        graph.set_parent("c1", "sg_c");
        graph.set_parent("c2", "sg_c");
        graph.set_parent("d1", "sg_d");
        graph.set_parent("d2", "sg_d");
        graph.set_parent("e1", "sg_e");
        graph.set_parent("f1", "sg_f");
        graph.set_parent("f2", "sg_f");

        let mut lg = LayoutGraph::from_digraph(&graph, |_, _| (10.0, 10.0));
        run(&mut lg);

        assert_eq!(
            lg.reversed_edges,
            BTreeSet::from([2, 5]),
            "each disjoint compound cycle should contribute one semantic-backward feedback edge"
        );
    }

    #[test]
    fn test_acyclic_disconnected() {
        let mut graph: DiGraph<()> = DiGraph::new();
        graph.add_node("A", ());
        graph.add_node("B", ());
        graph.add_node("C", ());
        graph.add_node("D", ());
        graph.add_edge("A", "B");
        graph.add_edge("C", "D");
        // Two disconnected components, no cycles

        let mut lg = LayoutGraph::from_digraph(&graph, |_, _| (10.0, 10.0));
        run(&mut lg);

        assert!(lg.reversed_edges.is_empty());
    }
}
