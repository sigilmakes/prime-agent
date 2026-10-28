//! Nesting graph setup and cleanup for compound graph layout.
//!
//! Creates border top/bottom nodes and weighted nesting edges that constrain
//! compound node children to be ranked between the border nodes. After ranking,
//! cleanup removes the nesting edges and root node.

use std::collections::{HashMap, HashSet, VecDeque};

use super::graph::LayoutGraph;
use super::types::{AcyclicPolicy, NodeId};

/// Compute the depth of each node in the compound parent hierarchy.
/// Top-level nodes get depth 1, each nesting level adds 1.
pub(crate) fn tree_depths(lg: &LayoutGraph) -> HashMap<usize, i32> {
    let mut depths = HashMap::new();
    let n = lg.node_ids.len();

    for i in 0..n {
        if lg.parents[i].is_none() {
            compute_depth(lg, i, 1, &mut depths);
        }
    }
    depths
}

/// Check if `node` is a descendant of `ancestor` in the parent hierarchy.
fn is_descendant(lg: &LayoutGraph, node: usize, ancestor: usize) -> bool {
    let mut current = lg.parents[node];
    while let Some(p) = current {
        if p == ancestor {
            return true;
        }
        current = lg.parents[p];
    }
    false
}

/// Check if `start` can reach any node inside `compound_idx` via forward edges.
fn can_reach_compound(
    start: usize,
    compound_idx: usize,
    lg: &LayoutGraph,
    fwd_adj: &[Vec<usize>],
) -> bool {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    visited.insert(start);
    queue.push_back(start);
    while let Some(node) = queue.pop_front() {
        for &next in &fwd_adj[node] {
            if is_descendant(lg, next, compound_idx) {
                return true;
            }
            if visited.insert(next) {
                queue.push_back(next);
            }
        }
    }
    false
}

fn compute_depth(lg: &LayoutGraph, node: usize, depth: i32, depths: &mut HashMap<usize, i32>) {
    depths.insert(node, depth);
    for (i, parent) in lg.parents.iter().enumerate() {
        if *parent == Some(node) {
            compute_depth(lg, i, depth + 1, depths);
        }
    }
}

/// Add nesting structure to the layout graph for compound nodes.
///
/// For each compound node:
/// - Creates border_top and border_bottom dummy nodes
/// - Adds nesting edges: top -> each child and each child -> bottom
///
/// Also creates a nesting root node connected to all top-level nodes.
/// Nesting edges use high weights to dominate ranking.
#[cfg(test)]
pub fn run(lg: &mut LayoutGraph) {
    run_with_policy(lg, AcyclicPolicy::default());
}

pub fn run_with_policy(lg: &mut LayoutGraph, acyclic_policy: AcyclicPolicy) {
    if lg.compound_nodes.is_empty() {
        return;
    }

    // Compute tree depths and nodeSep (ref: nesting-graph.js:33-36)
    let depths = tree_depths(lg);
    let max_depth = depths.values().copied().max().unwrap_or(1);
    let height = max_depth - 1;
    let node_sep = if height > 0 { 2 * height + 1 } else { 1 };
    lg.node_rank_factor = Some(node_sep);

    // Capture original edge count before any nesting edges are added.
    let orig_edge_count = lg.edges.len();

    // Multiply ALL existing edge minlens by node_sep (ref: nesting-graph.js:41)
    for minlen in &mut lg.edge_minlens {
        *minlen *= node_sep;
    }

    let n = lg.node_count();
    let nesting_weight = (n * 2) as f64;

    // Create border top/bottom nodes for each compound node (ref: nesting-graph.js:52-55)
    let compound_indices: Vec<usize> = lg.compound_nodes.iter().copied().collect();
    for &compound_idx in &compound_indices {
        let compound_id = lg.node_ids[compound_idx].0.clone();

        let top_id = NodeId(format!("_bt_{}", compound_id));
        let top_idx = lg.add_nesting_node(top_id);
        lg.border_top.insert(compound_idx, top_idx);
        lg.parents[top_idx] = Some(compound_idx);

        let bot_id = NodeId(format!("_bb_{}", compound_id));
        let bot_idx = lg.add_nesting_node(bot_id);
        lg.border_bottom.insert(compound_idx, bot_idx);
        lg.parents[bot_idx] = Some(compound_idx);
    }

    // For each compound node, add nesting edges using child border nodes when available
    // (ref: nesting-graph.js:56-70)
    for &compound_idx in &compound_indices {
        let top_idx = lg.border_top[&compound_idx];
        let bot_idx = lg.border_bottom[&compound_idx];

        // Capture current children excluding the border top/bottom nodes.
        // This mirrors dagre's behavior where children are fetched before
        // adding border nodes, so the borders don't participate in nesting edges.
        let children: Vec<usize> = lg
            .parents
            .iter()
            .enumerate()
            .filter(|(i, p)| **p == Some(compound_idx) && *i != top_idx && *i != bot_idx)
            .map(|(i, _)| i)
            .collect();

        for child in children {
            let (child_top, child_bottom, weight) = if let (Some(&ct), Some(&cb)) =
                (lg.border_top.get(&child), lg.border_bottom.get(&child))
            {
                (ct, cb, nesting_weight)
            } else {
                (child, child, nesting_weight * 2.0)
            };

            let minlen = if child_top != child_bottom {
                1
            } else {
                height - depths[&compound_idx] + 1
            };

            let e1 = lg.add_nesting_edge_with_minlen(top_idx, child_top, weight, minlen);
            lg.nesting_edges.insert(e1);
            let e2 = lg.add_nesting_edge_with_minlen(child_bottom, bot_idx, weight, minlen);
            lg.nesting_edges.insert(e2);
        }
    }

    // Note: dagre.js does not add sibling-compound separation edges here.

    // Add exit constraints for forward (non-reversed) cross-boundary edges.
    // For edges where the source is inside a compound but the target is
    // outside, constrain the target to be ranked after the compound's
    // bottom border.
    //
    // Reversed back-edges are skipped: the original code applied the
    // constraint on the pre-reversal direction, which created a
    // contradictory ranking constraint (forcing the target node below the
    // wrong compound's border).  The reversed edge's own minlen and the
    // nesting structure are sufficient without an explicit exit constraint.
    // See issue #152.
    //
    // Skip exit constraints when the target can reach back into the
    // compound via forward edges.  Adding border_bottom → target while
    // a path target → … → internal_node exists creates a constraint cycle
    // (border_bottom → target → … → internal_node → border_bottom) that
    // the ranker resolves by placing external nodes far above/below the
    // subgraph, producing layouts divergent from dagre.js.  See #173.
    if matches!(acyclic_policy, AcyclicPolicy::SemanticCompoundFeedback) {
        // Build forward adjacency from original non-reversed edges.
        let mut fwd_adj: Vec<Vec<usize>> = vec![Vec::new(); lg.node_ids.len()];
        for ei in 0..orig_edge_count {
            if lg.reversed_edges.contains(&ei) {
                continue;
            }
            let (from, to, _) = lg.edges[ei];
            fwd_adj[from].push(to);
        }

        for ei in 0..orig_edge_count {
            if lg.reversed_edges.contains(&ei) {
                continue;
            }
            let (from, to, _) = lg.edges[ei];
            if let Some(compound_idx) = lg.parents[from]
                && lg.compound_nodes.contains(&compound_idx)
                && !is_descendant(lg, to, compound_idx)
                && to != compound_idx
                && let Some(&bot_idx) = lg.border_bottom.get(&compound_idx)
            {
                if can_reach_compound(to, compound_idx, lg, &fwd_adj) {
                    continue;
                }
                let e = lg.add_nesting_edge_with_minlen(bot_idx, to, 0.0, 1);
                lg.nesting_edges.insert(e);
            }
        }
    }

    // Create root node connecting to all top-level nodes and compound border_tops
    let root_id = NodeId("_nesting_root".to_string());
    let root_idx = lg.add_nesting_node(root_id);
    lg.nesting_root = Some(root_idx);

    // Connect root to top-level leaf nodes: weight=0, minlen=node_sep
    // (ref: nesting-graph.js:74)
    let top_level: Vec<usize> = (0..n)
        .filter(|&i| lg.parents[i].is_none() && !lg.compound_nodes.contains(&i))
        .collect();
    for idx in top_level {
        let e = lg.add_nesting_edge_with_minlen(root_idx, idx, 0.0, node_sep);
        lg.nesting_edges.insert(e);
    }
    // Connect root to top-level compound border_tops: weight=0, minlen=height+depth
    // (ref: nesting-graph.js:81)
    let compound_indices_for_roots: Vec<usize> = lg.compound_nodes.iter().copied().collect();
    for compound_idx in compound_indices_for_roots {
        if lg.parents[compound_idx].is_some() {
            continue; // Only top-level compounds connect to root
        }
        let top_idx = lg.border_top[&compound_idx];
        let minlen = height + depths[&compound_idx];
        let e = lg.add_nesting_edge_with_minlen(root_idx, top_idx, 0.0, minlen);
        lg.nesting_edges.insert(e);
    }
}

/// Compute min_rank and max_rank for each compound node from border node ranks.
///
/// Must be called after ranking and nesting cleanup. Border top/bottom nodes
/// retain their assigned ranks, which define the vertical span of each compound node.
pub fn assign_rank_minmax(lg: &mut LayoutGraph) {
    let compound_indices: Vec<usize> = lg.compound_nodes.iter().copied().collect();
    for compound_idx in compound_indices {
        // Use border_top for min_rank; title ranks should not extend border chains.
        if let Some(&top_idx) = lg.border_top.get(&compound_idx) {
            lg.min_rank.insert(compound_idx, lg.ranks[top_idx]);
        } else if let Some(&title_idx) = lg.border_title.get(&compound_idx) {
            lg.min_rank.insert(compound_idx, lg.ranks[title_idx]);
        }
        if let Some(&bot_idx) = lg.border_bottom.get(&compound_idx) {
            lg.max_rank.insert(compound_idx, lg.ranks[bot_idx]);
        }
    }
}

/// Insert title dummy nodes at correct ranks after ranking is complete.
///
/// For each titled compound, creates a title node at `border_top_rank - 1`.
/// Must be called after rank::run() + rank::normalize() + nesting::cleanup()
/// and before assign_rank_minmax().
pub fn insert_title_nodes(lg: &mut LayoutGraph) {
    let compounds: Vec<usize> = lg.compound_titles.iter().copied().collect();
    for compound_idx in compounds {
        let compound_id = lg.node_ids[compound_idx].0.clone();
        let bt_idx = lg.border_top[&compound_idx];
        let title_rank = lg.ranks[bt_idx] - 1;

        let title_id = NodeId(format!("_tt_{}", compound_id));
        let title_idx = lg.add_nesting_node(title_id);
        lg.ranks[title_idx] = title_rank;
        lg.parents[title_idx] = Some(compound_idx);
        lg.border_title.insert(compound_idx, title_idx);
        lg.position_excluded_nodes.insert(title_idx);

        // Add edge title → border_top so the title participates in
        // ordering and positioning (without an edge it would float freely)
        // Don't mark as nesting edge -- it should survive cleanup and be
        // visible to normalization, ordering, and positioning
        lg.add_nesting_edge(title_idx, bt_idx, 0.0);
    }
}

/// Remove nesting edges and root node after ranking.
///
/// Nesting edges are marked for removal (set to zero weight and flagged),
/// and the nesting root is cleared. Border top/bottom nodes remain for
/// rank extraction in assign_rank_minmax.
pub fn cleanup(lg: &mut LayoutGraph) {
    // Mark nesting edges as excluded from downstream processing.
    // They remain in the edges vec (for index stability) but are skipped by
    // normalization, ordering, and BK alignment.
    for &edge_idx in &lg.nesting_edges {
        if edge_idx < lg.edge_weights.len() {
            lg.edge_weights[edge_idx] = 0.0;
        }
        lg.excluded_edges.insert(edge_idx);
    }
    lg.nesting_edges.clear();
    if let Some(root_idx) = lg.nesting_root {
        lg.position_excluded_nodes.insert(root_idx);
    }
    lg.nesting_root = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::graph::algorithms::layered::LayoutConfig;
    use crate::engines::graph::algorithms::layered::graph::{DiGraph, LayoutGraph};

    fn build_test_compound_layout_graph() -> LayoutGraph {
        let mut g: DiGraph<(f64, f64)> = DiGraph::new();
        g.add_node("A", (10.0, 10.0));
        g.add_node("B", (10.0, 10.0));
        g.add_node("sg1", (0.0, 0.0));
        g.add_edge("A", "B");
        g.set_parent("A", "sg1");
        g.set_parent("B", "sg1");
        LayoutGraph::from_digraph(&g, |_, dims| *dims)
    }

    fn build_test_simple_layout_graph() -> LayoutGraph {
        let mut g: DiGraph<(f64, f64)> = DiGraph::new();
        g.add_node("A", (10.0, 10.0));
        g.add_node("B", (10.0, 10.0));
        g.add_edge("A", "B");
        LayoutGraph::from_digraph(&g, |_, dims| *dims)
    }

    #[test]
    fn test_tree_depths_single_level() {
        // sg1 contains A and B
        let lg = build_test_compound_layout_graph();
        let depths = tree_depths(&lg);

        let sg1_idx = lg.node_index[&"sg1".into()];
        assert_eq!(depths[&sg1_idx], 1);

        let a_idx = lg.node_index[&"A".into()];
        let b_idx = lg.node_index[&"B".into()];
        assert_eq!(depths[&a_idx], 2);
        assert_eq!(depths[&b_idx], 2);
    }

    #[test]
    fn test_tree_depths_nested() {
        // outer -> inner -> A, B
        let mut g: DiGraph<(f64, f64)> = DiGraph::new();
        g.add_node("A", (10.0, 10.0));
        g.add_node("B", (10.0, 10.0));
        g.add_node("inner", (0.0, 0.0));
        g.add_node("outer", (0.0, 0.0));
        g.add_edge("A", "B");
        g.set_parent("A", "inner");
        g.set_parent("B", "inner");
        g.set_parent("inner", "outer");
        let lg = LayoutGraph::from_digraph(&g, |_, dims| *dims);

        let depths = tree_depths(&lg);
        let outer_idx = lg.node_index[&"outer".into()];
        let inner_idx = lg.node_index[&"inner".into()];
        let a_idx = lg.node_index[&"A".into()];
        assert_eq!(depths[&outer_idx], 1);
        assert_eq!(depths[&inner_idx], 2);
        assert_eq!(depths[&a_idx], 3);
    }

    #[test]
    fn test_tree_depths_flat_graph() {
        // No compounds — A -> B
        let lg = build_test_simple_layout_graph();
        let depths = tree_depths(&lg);

        let a_idx = lg.node_index[&"A".into()];
        assert_eq!(depths[&a_idx], 1);
    }

    #[test]
    fn test_nesting_run_sets_node_rank_factor_single_level() {
        // sg1 contains A, B — height=1, nodeSep=2*1+1=3
        let mut lg = build_test_compound_layout_graph();
        run(&mut lg);
        assert_eq!(lg.node_rank_factor, Some(3));
    }

    #[test]
    fn test_nesting_run_sets_node_rank_factor_nested() {
        // outer -> inner -> A, B — height=2, nodeSep=2*2+1=5
        let mut g: DiGraph<(f64, f64)> = DiGraph::new();
        g.add_node("A", (10.0, 10.0));
        g.add_node("B", (10.0, 10.0));
        g.add_node("inner", (0.0, 0.0));
        g.add_node("outer", (0.0, 0.0));
        g.add_edge("A", "B");
        g.set_parent("A", "inner");
        g.set_parent("B", "inner");
        g.set_parent("inner", "outer");
        let mut lg = LayoutGraph::from_digraph(&g, |_, dims| *dims);

        run(&mut lg);
        assert_eq!(lg.node_rank_factor, Some(5));
    }

    #[test]
    fn test_nesting_run_no_node_rank_factor_flat() {
        let mut lg = build_test_simple_layout_graph();
        run(&mut lg);
        assert_eq!(lg.node_rank_factor, None);
    }

    #[test]
    fn test_nesting_run_multiplies_edge_minlens() {
        let mut lg = build_test_compound_layout_graph();
        assert_eq!(lg.edge_minlens[0], 1); // A->B starts at minlen=1

        run(&mut lg);

        // The original A->B edge should have minlen multiplied by nodeSep=3
        assert_eq!(lg.edge_minlens[0], 3);
    }

    #[test]
    fn test_nesting_run_multiplies_doubled_minlens() {
        let mut lg = build_test_compound_layout_graph();
        lg.edge_minlens[0] = 2; // Simulates make_space_for_edge_labels

        run(&mut lg);

        // 2 * nodeSep(3) = 6
        assert_eq!(lg.edge_minlens[0], 6);
    }

    #[test]
    fn test_nesting_run_does_not_multiply_nesting_edges() {
        let mut lg = build_test_compound_layout_graph();
        let orig_edge_count = lg.edges.len();

        run(&mut lg);

        // Nesting edges (created by run) should NOT be multiplied
        for i in orig_edge_count..lg.edge_minlens.len() {
            if lg.nesting_edges.contains(&i) {
                assert!(
                    lg.edge_minlens[i] <= 5,
                    "Nesting edge {} has unexpectedly large minlen {}",
                    i,
                    lg.edge_minlens[i]
                );
            }
        }
    }

    // Task 3.1: Depth-dependent border-to-child minlens
    #[test]
    fn test_nesting_border_to_leaf_minlen_single_level() {
        // sg1 contains A, B. height=1, parent_depth=1
        // Leaf minlen = height - parent_depth + 1 = 1 - 1 + 1 = 1
        let mut lg = build_test_compound_layout_graph();
        run(&mut lg);

        let sg1_idx = lg.node_index[&"sg1".into()];
        let a_idx = lg.node_index[&"A".into()];
        let top_idx = lg.border_top[&sg1_idx];

        let edge_pos = lg
            .edges
            .iter()
            .position(|&(from, to, _)| from == top_idx && to == a_idx);
        assert!(edge_pos.is_some(), "Should have border_top -> A edge");
        assert_eq!(lg.edge_minlens[edge_pos.unwrap()], 1);
    }

    // Task 3.2: Root edge minlens
    #[test]
    fn test_nesting_root_to_compound_border_minlen() {
        // sg1(depth=1) contains A,B. height=1.
        // Root -> sg1.border_top: minlen = height + depths[sg1] = 1 + 1 = 2
        let mut lg = build_test_compound_layout_graph();
        run(&mut lg);

        let root = lg.nesting_root.unwrap();
        let sg1_idx = lg.node_index[&"sg1".into()];
        let top_idx = lg.border_top[&sg1_idx];

        let edge = lg
            .edges
            .iter()
            .position(|&(f, t, _)| f == root && t == top_idx);
        assert!(edge.is_some(), "Root -> sg1.border_top edge should exist");
        assert_eq!(
            lg.edge_minlens[edge.unwrap()],
            2,
            "Root to top-border minlen should be height + depth"
        );
    }

    #[test]
    fn test_nesting_root_to_leaf_minlen() {
        // E is top-level (no parent), sg1 contains A,B
        // height=1, node_sep=3
        // Root -> E: minlen = node_sep = 3
        let mut g: DiGraph<(f64, f64)> = DiGraph::new();
        g.add_node("A", (10.0, 10.0));
        g.add_node("B", (10.0, 10.0));
        g.add_node("E", (10.0, 10.0));
        g.add_node("sg1", (0.0, 0.0));
        g.add_edge("A", "B");
        g.add_edge("E", "A");
        g.set_parent("A", "sg1");
        g.set_parent("B", "sg1");
        let mut lg = LayoutGraph::from_digraph(&g, |_, dims| *dims);
        run(&mut lg);

        let root = lg.nesting_root.unwrap();
        let e_idx = lg.node_index[&"E".into()];

        let edge = lg
            .edges
            .iter()
            .position(|&(f, t, _)| f == root && t == e_idx);
        assert!(edge.is_some(), "Root -> E edge should exist");
        assert_eq!(
            lg.edge_minlens[edge.unwrap()],
            3,
            "Root to top-level leaf minlen should be node_sep"
        );
    }

    #[test]
    fn test_nesting_root_edges_have_zero_weight() {
        let mut lg = build_test_compound_layout_graph();
        run(&mut lg);

        let root = lg.nesting_root.unwrap();
        for (i, &(from, _, _)) in lg.edges.iter().enumerate() {
            if from == root {
                assert_eq!(lg.edge_weights[i], 0.0, "Root edges should have weight 0");
            }
        }
    }

    #[test]
    fn test_nesting_run_adds_border_nodes() {
        let mut lg = build_test_compound_layout_graph();
        let sg1_idx = lg.node_index[&"sg1".into()];
        let initial_count = lg.node_count();

        run(&mut lg);

        assert!(lg.border_top.contains_key(&sg1_idx));
        assert!(lg.border_bottom.contains_key(&sg1_idx));
        assert!(lg.node_count() > initial_count);
    }

    #[test]
    fn test_nesting_run_adds_nesting_edges() {
        let mut lg = build_test_compound_layout_graph();

        run(&mut lg);

        assert!(!lg.nesting_edges.is_empty());
    }

    #[test]
    fn test_nesting_run_creates_root() {
        let mut lg = build_test_compound_layout_graph();

        run(&mut lg);

        assert!(lg.nesting_root.is_some());
    }

    #[test]
    fn test_nesting_cleanup_removes_edges() {
        let mut lg = build_test_compound_layout_graph();
        run(&mut lg);
        assert!(!lg.nesting_edges.is_empty());

        cleanup(&mut lg);

        assert!(lg.nesting_root.is_none());
        assert!(lg.nesting_edges.is_empty());
    }

    #[test]
    fn test_nesting_run_noop_simple_graph() {
        let mut lg = build_test_simple_layout_graph();
        let initial = lg.node_count();

        run(&mut lg);

        assert_eq!(lg.node_count(), initial);
    }

    fn build_test_titled_compound_layout_graph() -> LayoutGraph {
        let mut g: DiGraph<(f64, f64)> = DiGraph::new();
        g.add_node("A", (10.0, 10.0));
        g.add_node("B", (10.0, 10.0));
        g.add_node("sg1", (0.0, 0.0));
        g.add_edge("A", "B");
        g.set_parent("A", "sg1");
        g.set_parent("B", "sg1");
        g.set_has_title("sg1");
        LayoutGraph::from_digraph(&g, |_, dims| *dims)
    }

    #[test]
    fn test_nesting_run_does_not_create_title_node() {
        let mut lg = build_test_titled_compound_layout_graph();
        let sg1_idx = lg.node_index[&"sg1".into()];

        run(&mut lg);

        // After run(), border_title should NOT be populated
        // (title nodes are created post-rank, not during nesting)
        assert!(
            !lg.border_title.contains_key(&sg1_idx),
            "run() should not create title nodes"
        );
        // But border_top and border_bottom should still exist
        assert!(lg.border_top.contains_key(&sg1_idx));
        assert!(lg.border_bottom.contains_key(&sg1_idx));
    }

    #[test]
    fn test_titled_compound_gets_title_node_after_insert() {
        use crate::engines::graph::algorithms::layered::rank;

        let mut lg = build_test_titled_compound_layout_graph();
        let sg1_idx = lg.node_index[&"sg1".into()];

        run(&mut lg);
        rank::run(&mut lg, &LayoutConfig::default());
        rank::normalize(&mut lg);
        cleanup(&mut lg);
        insert_title_nodes(&mut lg);

        assert!(lg.border_title.contains_key(&sg1_idx));
        let title_idx = lg.border_title[&sg1_idx];
        assert_eq!(lg.node_ids[title_idx], NodeId::from("_tt_sg1"));
    }

    #[test]
    fn test_nesting_run_no_title_node_for_untitled_compound() {
        let mut lg = build_test_compound_layout_graph();
        let sg1_idx = lg.node_index[&"sg1".into()];

        run(&mut lg);

        assert!(!lg.border_title.contains_key(&sg1_idx));
    }

    #[test]
    fn test_assign_rank_minmax() {
        use crate::engines::graph::algorithms::layered::rank;

        let mut lg = build_test_compound_layout_graph();
        let sg1_idx = lg.node_index[&"sg1".into()];
        run(&mut lg);
        rank::run(&mut lg, &LayoutConfig::default());
        rank::normalize(&mut lg);
        cleanup(&mut lg);

        assign_rank_minmax(&mut lg);

        assert!(lg.min_rank.contains_key(&sg1_idx));
        assert!(lg.max_rank.contains_key(&sg1_idx));
        assert!(lg.min_rank[&sg1_idx] <= lg.max_rank[&sg1_idx]);
    }

    // Deleted: test_sibling_subgraphs_do_not_share_rank_ranges
    // This test asserted disjoint rank ranges for siblings, but that property
    // was removed when sibling separation edges were deleted. Dagre.js does not
    // enforce disjoint rank ranges for sibling subgraphs either.

    #[test]
    fn test_assign_rank_minmax_uses_border_top_for_min() {
        use crate::engines::graph::algorithms::layered::rank;

        let mut lg = build_test_titled_compound_layout_graph();
        let sg1_idx = lg.node_index[&"sg1".into()];

        run(&mut lg);
        rank::run(&mut lg, &LayoutConfig::default());
        rank::normalize(&mut lg);
        cleanup(&mut lg);
        insert_title_nodes(&mut lg);
        assign_rank_minmax(&mut lg);

        let top_idx = lg.border_top[&sg1_idx];
        let title_idx = lg.border_title[&sg1_idx];

        // min_rank should be the border_top rank, not the title's rank
        assert_eq!(lg.min_rank[&sg1_idx], lg.ranks[top_idx]);
        // title rank should be strictly less than border_top rank
        assert!(lg.ranks[title_idx] < lg.ranks[top_idx]);
    }

    #[test]
    fn test_insert_title_nodes_sets_correct_rank() {
        use crate::engines::graph::algorithms::layered::rank;

        let mut lg = build_test_titled_compound_layout_graph();
        let sg1_idx = lg.node_index[&"sg1".into()];

        run(&mut lg);
        rank::run(&mut lg, &LayoutConfig::default());
        rank::normalize(&mut lg);
        cleanup(&mut lg);

        let bt_rank_before = lg.ranks[lg.border_top[&sg1_idx]];

        insert_title_nodes(&mut lg);

        // Title node should exist
        assert!(lg.border_title.contains_key(&sg1_idx));
        let title_idx = lg.border_title[&sg1_idx];

        // Title rank should be border_top_rank - 1
        assert_eq!(lg.ranks[title_idx], bt_rank_before - 1);

        // Title should be a child of the compound
        assert_eq!(lg.parents[title_idx], Some(sg1_idx));
    }

    #[test]
    fn test_insert_title_nodes_multi_subgraph_no_collision() {
        use crate::engines::graph::algorithms::layered::rank;

        let mut g: DiGraph<(f64, f64)> = DiGraph::new();
        g.add_node("A", (10.0, 10.0));
        g.add_node("B", (10.0, 10.0));
        g.add_node("C", (10.0, 10.0));
        g.add_node("D", (10.0, 10.0));
        g.add_node("sg1", (0.0, 0.0));
        g.add_node("sg2", (0.0, 0.0));
        g.add_edge("A", "B");
        g.add_edge("C", "D");
        g.add_edge("A", "C"); // cross-subgraph edge
        g.set_parent("A", "sg1");
        g.set_parent("B", "sg1");
        g.set_parent("C", "sg2");
        g.set_parent("D", "sg2");
        g.set_has_title("sg1");
        g.set_has_title("sg2");

        let mut lg = LayoutGraph::from_digraph(&g, |_, dims| *dims);
        run(&mut lg);
        rank::run(&mut lg, &LayoutConfig::default());
        rank::normalize(&mut lg);
        cleanup(&mut lg);
        insert_title_nodes(&mut lg);

        let sg1_idx = lg.node_index[&"sg1".into()];
        let sg2_idx = lg.node_index[&"sg2".into()];

        let tt1 = lg.border_title[&sg1_idx];
        let tt2 = lg.border_title[&sg2_idx];
        let bt1 = lg.border_top[&sg1_idx];
        let bt2 = lg.border_top[&sg2_idx];

        // Each title is one rank above its own border_top
        assert_eq!(lg.ranks[tt1], lg.ranks[bt1] - 1);
        assert_eq!(lg.ranks[tt2], lg.ranks[bt2] - 1);

        assert!(lg.ranks[tt1] >= 0);
        assert!(lg.ranks[tt2] >= 0);
    }

    #[test]
    fn test_insert_title_nodes_skips_untitled() {
        use crate::engines::graph::algorithms::layered::rank;

        let mut lg = build_test_compound_layout_graph();
        let sg1_idx = lg.node_index[&"sg1".into()];

        run(&mut lg);
        rank::run(&mut lg, &LayoutConfig::default());
        rank::normalize(&mut lg);
        cleanup(&mut lg);
        insert_title_nodes(&mut lg);

        assert!(!lg.border_title.contains_key(&sg1_idx));
    }

    #[test]
    fn test_assign_rank_minmax_noop_simple() {
        use crate::engines::graph::algorithms::layered::rank;

        let mut lg = build_test_simple_layout_graph();
        rank::run(&mut lg, &LayoutConfig::default());
        rank::normalize(&mut lg);

        assign_rank_minmax(&mut lg);

        assert!(lg.min_rank.is_empty());
        assert!(lg.max_rank.is_empty());
    }

    // --- Cross-boundary constraint tests (issue #152) ---

    /// Helper: build two sibling compounds with a cross-boundary edge.
    /// sg_a{a}, sg_b{b}, edge b->a (original direction).
    fn build_sibling_compounds_with_cross_edge() -> LayoutGraph {
        let mut g: DiGraph<(f64, f64)> = DiGraph::new();
        g.add_node("a", (10.0, 10.0));
        g.add_node("b", (10.0, 10.0));
        g.add_node("sg_a", (0.0, 0.0));
        g.add_node("sg_b", (0.0, 0.0));
        g.add_edge("b", "a"); // edge index 0: b -> a
        g.set_parent("a", "sg_a");
        g.set_parent("b", "sg_b");
        LayoutGraph::from_digraph(&g, |_, dims| *dims)
    }

    #[test]
    fn test_exit_constraint_skips_reversed_edge() {
        // Edge b->a (index 0) is reversed. Reversed edges should be
        // skipped entirely — no exit constraint in either direction.
        let mut lg = build_sibling_compounds_with_cross_edge();
        lg.reversed_edges.insert(0); // Mark b->a as reversed

        run(&mut lg);

        let sg_a_idx = lg.node_index[&"sg_a".into()];
        let sg_b_idx = lg.node_index[&"sg_b".into()];
        let a_idx = lg.node_index[&"a".into()];
        let b_idx = lg.node_index[&"b".into()];
        let bot_a = lg.border_bottom[&sg_a_idx];
        let bot_b = lg.border_bottom[&sg_b_idx];

        // Should NOT have border_bottom(sg_b) -> a (pre-reversal exit)
        let has_wrong_exit = lg.edges.iter().enumerate().any(|(i, &(from, to, _))| {
            from == bot_b && to == a_idx && lg.nesting_edges.contains(&i)
        });
        assert!(
            !has_wrong_exit,
            "Should NOT have border_bottom(sg_b) -> a for a reversed edge"
        );

        // Should NOT have border_bottom(sg_a) -> b either (reversed edges skipped)
        let has_flipped_exit = lg.edges.iter().enumerate().any(|(i, &(from, to, _))| {
            from == bot_a && to == b_idx && lg.nesting_edges.contains(&i)
        });
        assert!(
            !has_flipped_exit,
            "Should NOT have any exit constraint for a reversed edge"
        );
    }

    #[test]
    fn test_forward_exit_constraint_preserved() {
        // Edge b->a (index 0) is NOT reversed (forward).
        // Exit from sg_b: border_bottom(sg_b) -> a  ✓
        let mut lg = build_sibling_compounds_with_cross_edge();
        // No reversed edges — forward direction

        run(&mut lg);

        let sg_b_idx = lg.node_index[&"sg_b".into()];
        let a_idx = lg.node_index[&"a".into()];
        let bot_b = lg.border_bottom[&sg_b_idx];

        let has_exit = lg.edges.iter().enumerate().any(|(i, &(from, to, _))| {
            from == bot_b && to == a_idx && lg.nesting_edges.contains(&i)
        });
        assert!(
            has_exit,
            "Forward exit constraint border_bottom(sg_b) -> a must be preserved"
        );
    }

    #[test]
    fn test_reversed_edge_no_false_exit_constraint() {
        // Three sibling compounds: sg_a{a1,a2}, sg_b{b1}, sg_c{c1,c2}
        // Forward: a1->b1, b1->c1. Back-edge: c2->a2 (reversed).
        // Must NOT have border_bottom(sg_c) -> a2.
        let mut g: DiGraph<(f64, f64)> = DiGraph::new();
        g.add_node("a1", (10.0, 10.0));
        g.add_node("a2", (10.0, 10.0));
        g.add_node("b1", (10.0, 10.0));
        g.add_node("c1", (10.0, 10.0));
        g.add_node("c2", (10.0, 10.0));
        g.add_node("sg_a", (0.0, 0.0));
        g.add_node("sg_b", (0.0, 0.0));
        g.add_node("sg_c", (0.0, 0.0));
        g.add_edge("a1", "b1"); // 0
        g.add_edge("b1", "c1"); // 1
        g.add_edge("c2", "a2"); // 2 — will be reversed
        g.set_parent("a1", "sg_a");
        g.set_parent("a2", "sg_a");
        g.set_parent("b1", "sg_b");
        g.set_parent("c1", "sg_c");
        g.set_parent("c2", "sg_c");

        let mut lg = LayoutGraph::from_digraph(&g, |_, dims| *dims);
        lg.reversed_edges.insert(2); // Mark c2->a2 as reversed

        run(&mut lg);

        let sg_c_idx = lg.node_index[&"sg_c".into()];
        let a2_idx = lg.node_index[&"a2".into()];
        let bot_c = lg.border_bottom[&sg_c_idx];

        let has_wrong = lg.edges.iter().enumerate().any(|(i, &(from, to, _))| {
            from == bot_c && to == a2_idx && lg.nesting_edges.contains(&i)
        });
        assert!(
            !has_wrong,
            "Must NOT have border_bottom(sg_c) -> a2 for reversed back-edge"
        );
    }

    // Note: a full-pipeline feasibility test (acyclic + make_space +
    // nesting + rank) for three sibling compounds with a backward edge
    // exposes a pre-existing network simplex bug where nesting minlen
    // constraints are violated.  This is a separate issue from #152 and
    // needs its own investigation.
}
