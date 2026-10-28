//! Corridor-aware label placement primitives for the text grid.
//!
//! Projects a routed polyline into grid space and classifies each
//! occupied cell by role. `choose_corridor_aware_anchor` consumes the
//! footprint to steer an authoritative label anchor off load-bearing
//! corridor glyphs. Called from `derive/mod.rs` for every routed edge
//! with an authoritative `label_geometry`.

use std::collections::BTreeMap;

use super::{GridLayout, SubgraphBounds};
use crate::graph::geometry::EdgeLabelSide;
use crate::graph::grid::routing::Segment;

pub(crate) type GridCell = (usize, usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellRole {
    Corridor,
    Corner,
    Terminal,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PathFootprint {
    pub cells: BTreeMap<GridCell, CellRole>,
}

/// Build a `PathFootprint` directly from a grid-space polyline (e.g.
/// the post-processed `routed_edge_paths` used by the text renderer).
/// Callers draw from this exact polyline, so building the footprint
/// from grid cells avoids the float→grid quantization mismatch where
/// a corner glyph would otherwise land one cell off from a float-space
/// bend point.
///
/// The integration path in `derive/mod.rs` only needs
/// `extend_grid_polyline_into` (merging many edges into one footprint);
/// this single-polyline entry point exists for unit tests.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn project_grid_polyline(path: &[GridCell]) -> PathFootprint {
    let mut footprint = PathFootprint::default();
    extend_grid_polyline_into(path, &mut footprint);
    footprint
}

/// Union the footprint of a grid-space polyline into `dest`. Used by
/// callers that need a single footprint across several edges (labels
/// on one edge must not stomp another edge's corner/terminal glyphs).
pub(crate) fn extend_grid_polyline_into(path: &[GridCell], dest: &mut PathFootprint) {
    if path.len() < 2 {
        return;
    }

    // Corridor cells first, then tag corners, then terminals. Using
    // `entry(...).or_insert` respects the priority: Corner upgrades
    // Corridor; Terminal upgrades both.
    for window in path.windows(2) {
        fill_grid_segment_cells(window[0], window[1], &mut dest.cells);
    }

    for i in 1..path.len() - 1 {
        let prev_axis = grid_segment_axis(path[i - 1], path[i]);
        let next_axis = grid_segment_axis(path[i], path[i + 1]);
        if prev_axis != next_axis && prev_axis.is_some() && next_axis.is_some() {
            dest.cells
                .entry(path[i])
                .and_modify(|role| {
                    if !matches!(role, CellRole::Terminal) {
                        *role = CellRole::Corner;
                    }
                })
                .or_insert(CellRole::Corner);
        }
    }

    dest.cells.insert(path[0], CellRole::Terminal);
    dest.cells.insert(path[path.len() - 1], CellRole::Terminal);
}

/// Build a `PathFootprint` directly from an ordered slice of Pass 3 segments.
///
/// Mirrors `extend_grid_polyline_into` semantics but derives the polyline
/// implicitly from segment endpoints. Used at render time, where
/// `RoutedEdge.segments` is the source-of-truth for what the renderer will
/// actually paint.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn segments_to_footprint(segments: &[Segment]) -> PathFootprint {
    let mut footprint = PathFootprint::default();
    extend_segments_into(segments, &mut footprint);
    footprint
}

/// Union the footprint of Pass 3 segments into `dest`. Used by the render-time
/// placer to build a single global footprint across every routed edge.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn extend_segments_into(segments: &[Segment], dest: &mut PathFootprint) {
    if segments.is_empty() {
        return;
    }

    // (1) Corridor fill: each segment contributes every cell on its active
    //     axis, using `entry().or_insert` so overlapping corridors don't
    //     downgrade a Corner or Terminal placed by an adjacent segment.
    for seg in segments {
        match *seg {
            Segment::Horizontal { y, x_start, x_end } => {
                let (lo, hi) = (x_start.min(x_end), x_start.max(x_end));
                for col in lo..=hi {
                    dest.cells.entry((col, y)).or_insert(CellRole::Corridor);
                }
            }
            Segment::Vertical { x, y_start, y_end } => {
                let (lo, hi) = (y_start.min(y_end), y_start.max(y_end));
                for row in lo..=hi {
                    dest.cells.entry((x, row)).or_insert(CellRole::Corridor);
                }
            }
        }
    }

    // (2) Corner upgrade: consecutive segments meeting at a shared cell with
    //     different axes produce a Corner. Terminal wins over Corner.
    for w in segments.windows(2) {
        let prev_end = segment_end_cell(&w[0]);
        let next_start = segment_start_cell(&w[1]);
        if prev_end == next_start && segment_axis(&w[0]) != segment_axis(&w[1]) {
            dest.cells
                .entry(prev_end)
                .and_modify(|role| {
                    if !matches!(role, CellRole::Terminal) {
                        *role = CellRole::Corner;
                    }
                })
                .or_insert(CellRole::Corner);
        }
    }

    // (3) Terminal upgrade: first cell of first segment + last cell of last
    //     segment. Unconditional insert — Terminal is the top of the role
    //     lattice. These are exactly where the text renderer paints the
    //     launch glyph and the arrowhead.
    let first = segment_start_cell(&segments[0]);
    let last = segment_end_cell(&segments[segments.len() - 1]);
    dest.cells.insert(first, CellRole::Terminal);
    dest.cells.insert(last, CellRole::Terminal);
}

fn segment_axis(seg: &Segment) -> Axis {
    match seg {
        Segment::Horizontal { .. } => Axis::Horizontal,
        Segment::Vertical { .. } => Axis::Vertical,
    }
}

/// Seed the footprint with rendered subgraph obstacles: visible border cells
/// plus concurrent-region divider cells and their tee junctions.
///
/// Mirrors `render_subgraph_borders` in `src/render/graph/text/subgraph.rs`:
/// invisible subgraphs contribute no cells; each visible subgraph stamps its
/// frame; every parent with `concurrent_regions.len() >= 2` stamps dashed
/// dividers between adjacent regions plus tee junctions on the parent's top
/// and bottom borders. All cells land as `Terminal` so the placer treats them
/// as load-bearing.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn seed_subgraph_borders_into(dest: &mut PathFootprint, layout: &GridLayout) {
    for bounds in layout.subgraph_bounds.values() {
        if bounds.invisible {
            continue;
        }
        if bounds.width < 2 || bounds.height < 2 {
            continue;
        }
        seed_subgraph_border_box(dest, bounds);
    }
    seed_subgraph_region_dividers(dest, &layout.subgraph_bounds);
}

fn seed_subgraph_border_box(dest: &mut PathFootprint, bounds: &SubgraphBounds) {
    let x0 = bounds.x;
    let y0 = bounds.y;
    let x1 = x0 + bounds.width - 1;
    let y1 = y0 + bounds.height - 1;
    for col in x0..=x1 {
        dest.cells.insert((col, y0), CellRole::Terminal);
        dest.cells.insert((col, y1), CellRole::Terminal);
    }
    for row in y0..=y1 {
        dest.cells.insert((x0, row), CellRole::Terminal);
        dest.cells.insert((x1, row), CellRole::Terminal);
    }
}

fn seed_subgraph_region_dividers(
    dest: &mut PathFootprint,
    subgraph_bounds: &std::collections::HashMap<String, SubgraphBounds>,
) {
    for parent in subgraph_bounds.values() {
        if parent.concurrent_regions.len() < 2 {
            continue;
        }
        let region_bounds: Vec<&SubgraphBounds> = parent
            .concurrent_regions
            .iter()
            .filter_map(|id| subgraph_bounds.get(id))
            .collect();
        for pair in region_bounds.windows(2) {
            let left_region = pair[0];
            let right_region = pair[1];
            let left_right_edge = left_region.x + left_region.width;
            let right_left_edge = right_region.x;
            if right_left_edge <= left_right_edge {
                continue;
            }
            let divider_x = left_right_edge + (right_left_edge - left_right_edge) / 2;
            // Parent top/bottom tees.
            dest.cells.insert((divider_x, parent.y), CellRole::Terminal);
            dest.cells.insert(
                (divider_x, parent.y + parent.height - 1),
                CellRole::Terminal,
            );
            // Dotted vertical between top and bottom (exclusive bottom).
            let top = parent.y + 1;
            let bottom = parent.y + parent.height - 1;
            for row in top..bottom {
                dest.cells
                    .entry((divider_x, row))
                    .and_modify(|r| *r = CellRole::Terminal)
                    .or_insert(CellRole::Terminal);
            }
        }
    }
}

/// Stamp a label rect into `dest` as `Terminal` so subsequent labels steer
/// around it. The rect is specified by its center + dimensions (the same
/// convention `choose_corridor_aware_anchor` uses).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn claim_label_cells_into(
    center: GridCell,
    dims: (usize, usize),
    dest: &mut PathFootprint,
) {
    let (w, h) = (dims.0.max(1), dims.1.max(1));
    let base_x = center.0.saturating_sub(w / 2);
    let base_y = center.1.saturating_sub(h / 2);
    for row in base_y..base_y.saturating_add(h) {
        for col in base_x..base_x.saturating_add(w) {
            dest.cells.insert((col, row), CellRole::Terminal);
        }
    }
}

/// Check whether a label rect (specified by center + dimensions) overlaps any
/// node's bounding box.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn label_rect_overlaps_nodes(
    center: GridCell,
    dims: (usize, usize),
    node_bounds: &std::collections::HashMap<String, super::NodeBounds>,
) -> bool {
    let (w, h) = (dims.0.max(1), dims.1.max(1));
    let base_x = center.0.saturating_sub(w / 2);
    let base_y = center.1.saturating_sub(h / 2);
    for bounds in node_bounds.values() {
        let overlaps_x =
            base_x < bounds.x.saturating_add(bounds.width) && bounds.x < base_x.saturating_add(w);
        let overlaps_y =
            base_y < bounds.y.saturating_add(bounds.height) && bounds.y < base_y.saturating_add(h);
        if overlaps_x && overlaps_y {
            return true;
        }
    }
    false
}

/// Seed the footprint with every cell inside every node's bounding box. Marks
/// each cell as `Terminal` so the placer never lands a label on a node glyph.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn seed_node_cells_into(dest: &mut PathFootprint, layout: &GridLayout) {
    for bounds in layout.node_bounds.values() {
        for row in bounds.y..bounds.y.saturating_add(bounds.height) {
            for col in bounds.x..bounds.x.saturating_add(bounds.width) {
                dest.cells.insert((col, row), CellRole::Terminal);
            }
        }
    }
}

fn segment_start_cell(seg: &Segment) -> GridCell {
    match *seg {
        Segment::Horizontal { y, x_start, .. } => (x_start, y),
        Segment::Vertical { x, y_start, .. } => (x, y_start),
    }
}

fn segment_end_cell(seg: &Segment) -> GridCell {
    match *seg {
        Segment::Horizontal { y, x_end, .. } => (x_end, y),
        Segment::Vertical { x, y_end, .. } => (x, y_end),
    }
}

fn fill_grid_segment_cells(a: GridCell, b: GridCell, cells: &mut BTreeMap<GridCell, CellRole>) {
    match grid_segment_axis(a, b) {
        Some(Axis::Horizontal) => {
            let (c_min, c_max) = (a.0.min(b.0), a.0.max(b.0));
            for col in c_min..=c_max {
                cells.entry((col, a.1)).or_insert(CellRole::Corridor);
            }
        }
        Some(Axis::Vertical) => {
            let (r_min, r_max) = (a.1.min(b.1), a.1.max(b.1));
            for row in r_min..=r_max {
                cells.entry((a.0, row)).or_insert(CellRole::Corridor);
            }
        }
        None => {
            fill_bresenham(a, b, cells);
        }
    }
}

fn grid_segment_axis(a: GridCell, b: GridCell) -> Option<Axis> {
    let same_col = a.0 == b.0;
    let same_row = a.1 == b.1;
    match (same_col, same_row) {
        (true, true) => None,
        (true, false) => Some(Axis::Vertical),
        (false, true) => Some(Axis::Horizontal),
        (false, false) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

/// Pick a grid cell for an authoritative label anchor that respects
/// the edge's path footprint.
///
/// The candidate is treated as the *center* of a `label_width`-by-
/// `label_height` block. A placement is "safe" when every cell that
/// block would occupy is either off the path entirely or on a
/// `Corridor` straight-segment cell — `Terminal` and `Corner` cells
/// carry load-bearing glyphs (arrowheads, bend characters) and must
/// never be overwritten. The function walks a prioritized step-set
/// in the direction declared by `side` and returns the first safe
/// neighbor, widening the search ring until a safe slot is found or
/// the ring exceeds `label_height + 2`. If no neighbor is safe, the
/// original candidate is returned (best-effort fallback).
pub(crate) fn choose_corridor_aware_anchor(
    candidate: GridCell,
    side: EdgeLabelSide,
    footprint: &PathFootprint,
    grid_width: usize,
    grid_height: usize,
    label_width: usize,
    label_height: usize,
) -> GridCell {
    if is_safe_block(candidate, label_width, label_height, footprint) {
        return candidate;
    }
    // Widen the shift ring progressively. The label_height + 2 cap
    // keeps the search bounded even for tall labels on dense paths.
    let max_ring = label_height.max(label_width).saturating_add(2);
    for ring in 1..=max_ring {
        for (dx, dy) in shift_steps(side) {
            let (rdx, rdy) = (dx * ring as isize, dy * ring as isize);
            if let Some(shifted) = apply_step(candidate, rdx, rdy, grid_width, grid_height)
                && is_safe_block(shifted, label_width, label_height, footprint)
            {
                return shifted;
            }
        }
    }
    candidate
}

fn is_safe_block(
    center: GridCell,
    label_width: usize,
    label_height: usize,
    footprint: &PathFootprint,
) -> bool {
    let base_x = center.0.saturating_sub(label_width / 2);
    let base_y = center.1.saturating_sub(label_height / 2);
    for row in base_y..base_y.saturating_add(label_height.max(1)) {
        for col in base_x..base_x.saturating_add(label_width.max(1)) {
            if matches!(
                footprint.cells.get(&(col, row)),
                Some(CellRole::Corner | CellRole::Terminal)
            ) {
                return false;
            }
        }
    }
    true
}

fn shift_steps(side: EdgeLabelSide) -> [(isize, isize); 4] {
    // Primary steps follow the declared side first; fallbacks cover
    // the remaining cardinals so the placer always gets a chance to
    // escape a load-bearing cell.
    match side {
        EdgeLabelSide::Below => [(0, 1), (1, 0), (-1, 0), (0, -1)],
        EdgeLabelSide::Above => [(0, -1), (-1, 0), (1, 0), (0, 1)],
        EdgeLabelSide::Center => [(0, 1), (0, -1), (1, 0), (-1, 0)],
    }
}

fn apply_step(
    cell: GridCell,
    dx: isize,
    dy: isize,
    grid_width: usize,
    grid_height: usize,
) -> Option<GridCell> {
    let new_x = (cell.0 as isize).checked_add(dx).filter(|v| *v >= 0)? as usize;
    let new_y = (cell.1 as isize).checked_add(dy).filter(|v| *v >= 0)? as usize;
    if new_x >= grid_width || new_y >= grid_height {
        return None;
    }
    Some((new_x, new_y))
}

fn fill_bresenham(start: GridCell, end: GridCell, cells: &mut BTreeMap<GridCell, CellRole>) {
    let (mut x0, mut y0) = (start.0 as isize, start.1 as isize);
    let (x1, y1) = (end.0 as isize, end.1 as isize);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if x0 >= 0 && y0 >= 0 {
            cells
                .entry((x0 as usize, y0 as usize))
                .or_insert(CellRole::Corridor);
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRID_W: usize = 16;
    const GRID_H: usize = 16;

    #[test]
    fn project_straight_horizontal_segment_fills_corridor_cells() {
        // Grid cells (0,2) and (6,2) are terminals; interior is Corridor.
        let path = vec![(0usize, 2usize), (6, 2)];
        let footprint = project_grid_polyline(&path);
        let corridor_cells: Vec<_> = footprint
            .cells
            .iter()
            .filter(|(_, role)| matches!(role, CellRole::Corridor))
            .map(|(cell, _)| *cell)
            .collect();
        assert_eq!(corridor_cells, (1..=5).map(|c| (c, 2)).collect::<Vec<_>>());

        let terminals: Vec<_> = footprint
            .cells
            .iter()
            .filter(|(_, role)| matches!(role, CellRole::Terminal))
            .map(|(cell, _)| *cell)
            .collect();
        assert_eq!(terminals, vec![(0, 2), (6, 2)]);
    }

    #[test]
    fn project_straight_vertical_segment_fills_corridor_cells() {
        let path = vec![(3usize, 0usize), (3, 5)];
        let footprint = project_grid_polyline(&path);
        let corridor_cells: Vec<_> = footprint
            .cells
            .iter()
            .filter(|(_, role)| matches!(role, CellRole::Corridor))
            .map(|(cell, _)| *cell)
            .collect();
        assert_eq!(corridor_cells, (1..=4).map(|r| (3, r)).collect::<Vec<_>>());

        let terminals: Vec<_> = footprint
            .cells
            .iter()
            .filter(|(_, role)| matches!(role, CellRole::Terminal))
            .map(|(cell, _)| *cell)
            .collect();
        assert_eq!(terminals, vec![(3, 0), (3, 5)]);
    }

    #[test]
    fn project_l_bend_marks_corner_cell() {
        let path = vec![(0usize, 0usize), (3, 0), (3, 3)];
        let footprint = project_grid_polyline(&path);
        let corner_cells: Vec<_> = footprint
            .cells
            .iter()
            .filter(|(_, role)| matches!(role, CellRole::Corner))
            .map(|(cell, _)| *cell)
            .collect();
        assert_eq!(corner_cells, vec![(3, 0)]);

        let terminals: Vec<_> = footprint
            .cells
            .iter()
            .filter(|(_, role)| matches!(role, CellRole::Terminal))
            .map(|(cell, _)| *cell)
            .collect();
        assert_eq!(terminals, vec![(0, 0), (3, 3)]);
    }

    #[test]
    fn project_u_channel_marks_terminals_and_two_corners() {
        let path = vec![(6usize, 1usize), (6, 3), (1, 3), (1, 1)];
        let footprint = project_grid_polyline(&path);

        let terminals: Vec<_> = footprint
            .cells
            .iter()
            .filter(|(_, role)| matches!(role, CellRole::Terminal))
            .map(|(cell, _)| *cell)
            .collect();
        assert_eq!(terminals, vec![(1, 1), (6, 1)]);

        let corners: Vec<_> = footprint
            .cells
            .iter()
            .filter(|(_, role)| matches!(role, CellRole::Corner))
            .map(|(cell, _)| *cell)
            .collect();
        assert_eq!(corners, vec![(1, 3), (6, 3)]);

        let long_leg_corridors: usize = footprint
            .cells
            .iter()
            .filter(|((col, row), role)| {
                matches!(role, CellRole::Corridor) && *row == 3 && *col > 1 && *col < 6
            })
            .count();
        assert_eq!(long_leg_corridors, 4, "4 interior cells on the long leg");
    }

    #[test]
    fn project_degenerate_single_point_returns_empty_footprint() {
        let path = vec![(0usize, 0usize)];
        let footprint = project_grid_polyline(&path);
        assert!(footprint.cells.is_empty());
    }

    // Task 3.3 — choose_corridor_aware_anchor
    use crate::graph::geometry::EdgeLabelSide;

    fn u_channel_footprint() -> PathFootprint {
        project_grid_polyline(&[(6usize, 1usize), (6, 3), (1, 3), (1, 1)])
    }

    #[test]
    fn anchor_on_terminal_cell_shifts_off() {
        let footprint = u_channel_footprint();
        let candidate = (6, 1); // terminal
        let anchor = choose_corridor_aware_anchor(
            candidate,
            EdgeLabelSide::Below,
            &footprint,
            GRID_W,
            GRID_H,
            1,
            1,
        );
        assert_ne!(anchor, candidate);
        assert!(
            !matches!(
                footprint.cells.get(&anchor),
                Some(CellRole::Terminal | CellRole::Corner)
            ),
            "anchor landed on a load-bearing cell: {:?}",
            footprint.cells.get(&anchor)
        );
    }

    #[test]
    fn anchor_on_corner_cell_shifts_off() {
        let footprint = u_channel_footprint();
        let candidate = (1, 3); // corner
        let anchor = choose_corridor_aware_anchor(
            candidate,
            EdgeLabelSide::Below,
            &footprint,
            GRID_W,
            GRID_H,
            1,
            1,
        );
        assert_ne!(anchor, candidate);
        assert!(
            !matches!(
                footprint.cells.get(&anchor),
                Some(CellRole::Terminal | CellRole::Corner)
            ),
            "anchor landed on a load-bearing cell: {:?}",
            footprint.cells.get(&anchor)
        );
    }

    #[test]
    fn anchor_on_corridor_cell_remains_unchanged() {
        let footprint = u_channel_footprint();
        let candidate = (6, 2); // corridor on the right vertical leg
        let anchor = choose_corridor_aware_anchor(
            candidate,
            EdgeLabelSide::Below,
            &footprint,
            GRID_W,
            GRID_H,
            1,
            1,
        );
        assert_eq!(anchor, candidate);
    }

    #[test]
    fn anchor_off_path_remains_unchanged() {
        let footprint = u_channel_footprint();
        // (4, 2) is not on any footprint cell — off-path is safe by
        // definition. The placer preserves anchors that fall off the
        // edge entirely; the caller owns any "snap toward corridor"
        // policy above this layer.
        let candidate = (4, 2);
        let anchor = choose_corridor_aware_anchor(
            candidate,
            EdgeLabelSide::Below,
            &footprint,
            GRID_W,
            GRID_H,
            1,
            1,
        );
        assert_eq!(anchor, candidate);
    }

    #[test]
    fn below_side_prefers_shift_down_first() {
        // A horizontal bar (terminal + corner on top row, corridor
        // below). Candidate lands on the corner; Below should shift
        // down to (col, row+1), not up.
        let footprint = project_grid_polyline(&[(0usize, 0usize), (3, 0), (3, 2)]);
        let anchor = choose_corridor_aware_anchor(
            (3, 0),
            EdgeLabelSide::Below,
            &footprint,
            GRID_W,
            GRID_H,
            1,
            1,
        );
        assert_eq!(anchor, (3, 1));

        let anchor_above = choose_corridor_aware_anchor(
            (3, 0),
            EdgeLabelSide::Above,
            &footprint,
            GRID_W,
            GRID_H,
            1,
            1,
        );
        // Above can't go up from row 0; the fallback picks (2,0) or (4,0).
        assert_ne!(anchor_above, (3, 0));
        assert!(
            !matches!(
                footprint.cells.get(&anchor_above),
                Some(CellRole::Terminal | CellRole::Corner)
            ),
            "Above fallback landed on a load-bearing cell: {:?}",
            anchor_above
        );
    }
}

#[cfg(test)]
mod seed_footprint_tests {
    use super::*;
    use crate::graph::grid::{GridLayout, NodeBounds, SubgraphBounds};

    fn node(x: usize, y: usize, w: usize, h: usize) -> NodeBounds {
        NodeBounds {
            x,
            y,
            width: w,
            height: h,
            layout_center_x: None,
            layout_center_y: None,
        }
    }

    fn subgraph(
        id: &str,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        invisible: bool,
        concurrent_regions: Vec<String>,
    ) -> (String, SubgraphBounds) {
        (
            id.to_string(),
            SubgraphBounds {
                x,
                y,
                width: w,
                height: h,
                title: String::new(),
                depth: 0,
                invisible,
                concurrent_regions,
            },
        )
    }

    #[test]
    fn seed_subgraph_borders_skips_invisible_subgraphs() {
        let mut layout = GridLayout::default();
        let (id, bounds) = subgraph("s", 0, 0, 6, 4, true, Vec::new());
        layout.subgraph_bounds.insert(id, bounds);
        let mut fp = PathFootprint::default();
        seed_subgraph_borders_into(&mut fp, &layout);
        assert!(
            fp.cells.is_empty(),
            "invisible subgraph must contribute no cells"
        );
    }

    #[test]
    fn seed_subgraph_borders_marks_divider_cells_for_concurrent_regions() {
        // Parent occupies 0..20 x 0..8; two 9-wide regions at x=1..10 and x=11..20.
        let mut layout = GridLayout::default();
        let (pid, parent) = subgraph(
            "parent",
            0,
            0,
            20,
            8,
            false,
            vec!["a".to_string(), "b".to_string()],
        );
        let (aid, a) = subgraph("a", 1, 1, 9, 6, false, Vec::new());
        let (bid, b) = subgraph("b", 11, 1, 9, 6, false, Vec::new());
        layout.subgraph_bounds.insert(pid, parent);
        layout.subgraph_bounds.insert(aid, a);
        layout.subgraph_bounds.insert(bid, b);

        let mut fp = PathFootprint::default();
        seed_subgraph_borders_into(&mut fp, &layout);
        // Divider x midpoint = 10 + (11 - 10) / 2 = 10. Parent inner y = 1..7 exclusive.
        // Divider cells for rows 1..7 at x=10.
        for y in 1..7 {
            assert!(
                fp.cells.contains_key(&(10, y)),
                "missing divider cell at (10, {y})"
            );
        }
        // Tee junctions at top (y=0) and bottom (y=7).
        assert!(fp.cells.contains_key(&(10, 0)), "missing top tee junction");
        assert!(
            fp.cells.contains_key(&(10, 7)),
            "missing bottom tee junction"
        );
    }

    #[test]
    fn seed_node_cells_marks_node_interior_as_terminal() {
        let mut layout = GridLayout::default();
        layout.node_bounds.insert("n".to_string(), node(2, 3, 4, 2));
        let mut fp = PathFootprint::default();
        seed_node_cells_into(&mut fp, &layout);
        for y in 3..5 {
            for x in 2..6 {
                assert_eq!(
                    fp.cells.get(&(x, y)),
                    Some(&CellRole::Terminal),
                    "({x}, {y}) should be Terminal"
                );
            }
        }
    }
}

#[cfg(test)]
mod segments_to_footprint_tests {
    use super::*;
    use crate::graph::grid::routing::Segment;

    // ---- Degenerate cases per Q1 §What lines 100-104 ----

    #[test]
    fn empty_segments_returns_empty_footprint() {
        let footprint = segments_to_footprint(&[]);
        assert!(footprint.cells.is_empty());
    }

    #[test]
    fn single_horizontal_segment_marks_terminals_and_interior() {
        let segs = vec![Segment::Horizontal {
            y: 3,
            x_start: 1,
            x_end: 5,
        }];
        let footprint = segments_to_footprint(&segs);
        assert_eq!(footprint.cells.get(&(1, 3)), Some(&CellRole::Terminal));
        assert_eq!(footprint.cells.get(&(5, 3)), Some(&CellRole::Terminal));
        for x in 2..=4 {
            assert_eq!(footprint.cells.get(&(x, 3)), Some(&CellRole::Corridor));
        }
    }

    #[test]
    fn single_vertical_segment_marks_terminals_and_interior() {
        let segs = vec![Segment::Vertical {
            x: 2,
            y_start: 0,
            y_end: 4,
        }];
        let footprint = segments_to_footprint(&segs);
        assert_eq!(footprint.cells.get(&(2, 0)), Some(&CellRole::Terminal));
        assert_eq!(footprint.cells.get(&(2, 4)), Some(&CellRole::Terminal));
        for y in 1..=3 {
            assert_eq!(footprint.cells.get(&(2, y)), Some(&CellRole::Corridor));
        }
    }

    #[test]
    fn degenerate_one_cell_segment_idempotent() {
        // Q1 §What line 103: `H y x_start=x_end` — one cell marked Terminal twice.
        let segs = vec![Segment::Horizontal {
            y: 2,
            x_start: 3,
            x_end: 3,
        }];
        let footprint = segments_to_footprint(&segs);
        assert_eq!(footprint.cells.len(), 1);
        assert_eq!(footprint.cells.get(&(3, 2)), Some(&CellRole::Terminal));
    }

    // ---- Two-segment cases: shared endpoint, different axes -> Corner ----

    #[test]
    fn two_segments_different_axes_shared_endpoint_marks_corner() {
        // L-bend: H y=0 x=0..3, V x=3 y=0..3. Shared cell (3, 0).
        let segs = vec![
            Segment::Horizontal {
                y: 0,
                x_start: 0,
                x_end: 3,
            },
            Segment::Vertical {
                x: 3,
                y_start: 0,
                y_end: 3,
            },
        ];
        let footprint = segments_to_footprint(&segs);
        assert_eq!(footprint.cells.get(&(0, 0)), Some(&CellRole::Terminal));
        assert_eq!(footprint.cells.get(&(3, 3)), Some(&CellRole::Terminal));
        assert_eq!(footprint.cells.get(&(3, 0)), Some(&CellRole::Corner));
        assert_eq!(footprint.cells.get(&(1, 0)), Some(&CellRole::Corridor));
        assert_eq!(footprint.cells.get(&(2, 0)), Some(&CellRole::Corridor));
        assert_eq!(footprint.cells.get(&(3, 1)), Some(&CellRole::Corridor));
        assert_eq!(footprint.cells.get(&(3, 2)), Some(&CellRole::Corridor));
    }

    // ---- Two-segment shared endpoint, SAME axis -> no corner (degenerate join) ----

    #[test]
    fn two_segments_same_axis_shared_endpoint_no_corner() {
        // Two horizontals joined at x=3, y=2.
        // Not a corner (axes match); just a long Corridor with Terminals at the far ends.
        let segs = vec![
            Segment::Horizontal {
                y: 2,
                x_start: 0,
                x_end: 3,
            },
            Segment::Horizontal {
                y: 2,
                x_start: 3,
                x_end: 6,
            },
        ];
        let footprint = segments_to_footprint(&segs);
        assert_eq!(footprint.cells.get(&(0, 2)), Some(&CellRole::Terminal));
        assert_eq!(footprint.cells.get(&(6, 2)), Some(&CellRole::Terminal));
        assert_eq!(footprint.cells.get(&(3, 2)), Some(&CellRole::Corridor));
    }

    // ---- U-channel: 3 segments -> 2 corners, 2 terminals ----

    #[test]
    fn u_channel_three_segments_marks_two_corners_two_terminals() {
        // V down-H left-V up.
        let segs = vec![
            Segment::Vertical {
                x: 6,
                y_start: 1,
                y_end: 3,
            },
            Segment::Horizontal {
                y: 3,
                x_start: 6,
                x_end: 1,
            },
            Segment::Vertical {
                x: 1,
                y_start: 3,
                y_end: 1,
            },
        ];
        let footprint = segments_to_footprint(&segs);
        assert_eq!(footprint.cells.get(&(6, 1)), Some(&CellRole::Terminal));
        assert_eq!(footprint.cells.get(&(1, 1)), Some(&CellRole::Terminal));
        assert_eq!(footprint.cells.get(&(6, 3)), Some(&CellRole::Corner));
        assert_eq!(footprint.cells.get(&(1, 3)), Some(&CellRole::Corner));
        for x in 2..=5 {
            assert_eq!(footprint.cells.get(&(x, 3)), Some(&CellRole::Corridor));
        }
    }

    // ---- Self-edge shape (C5 / C8 precondition per Q7) ----

    #[test]
    fn self_edge_three_segments_marks_corners_at_both_bends() {
        // Per Q1 §Empirical self_loop_labeled edge idx=1 B→B "retry":
        // [H y=7 x=11→14, V x=14 y=7→9, H y=9 x=11→14]
        let segs = vec![
            Segment::Horizontal {
                y: 7,
                x_start: 11,
                x_end: 14,
            },
            Segment::Vertical {
                x: 14,
                y_start: 7,
                y_end: 9,
            },
            Segment::Horizontal {
                y: 9,
                x_start: 14,
                x_end: 11,
            },
        ];
        let footprint = segments_to_footprint(&segs);
        assert_eq!(footprint.cells.get(&(11, 7)), Some(&CellRole::Terminal));
        assert_eq!(footprint.cells.get(&(11, 9)), Some(&CellRole::Terminal));
        assert_eq!(footprint.cells.get(&(14, 7)), Some(&CellRole::Corner));
        assert_eq!(footprint.cells.get(&(14, 9)), Some(&CellRole::Corner));
    }

    // ---- Render-time placement canaries C1 and C3. ----

    /// C1: forward vertical (TD) 2-segment L-path. The projected label center
    /// lands on the Corner cell where the vertical meets the horizontal; the
    /// corridor-aware placer must steer it off the Corner onto a Corridor or
    /// unclaimed cell.
    #[test]
    fn c1_forward_vertical_corner_avoid() {
        // H at y=0 from x=0..3, V at x=3 from y=0..5. Corner at (3, 0).
        let segs = vec![
            Segment::Horizontal {
                y: 0,
                x_start: 0,
                x_end: 3,
            },
            Segment::Vertical {
                x: 3,
                y_start: 0,
                y_end: 5,
            },
        ];
        let footprint = segments_to_footprint(&segs);
        // Candidate sits on the Corner; side=Below should steer it onto a
        // Corridor cell off the load-bearing corner.
        let anchor =
            choose_corridor_aware_anchor((3, 0), EdgeLabelSide::Below, &footprint, 10, 10, 1, 1);
        assert_ne!(anchor, (3, 0), "placer must shift off the corner");
        assert!(
            !matches!(
                footprint.cells.get(&anchor),
                Some(CellRole::Corner | CellRole::Terminal)
            ),
            "anchor landed on a load-bearing cell: {:?}",
            footprint.cells.get(&anchor)
        );
    }

    /// C3: forward horizontal (LR) 1-segment path, side=Above. The placer
    /// must honor the declared Above side when shifting off a load-bearing
    /// Terminal at the target endpoint.
    #[test]
    fn c3_forward_horizontal_side_above() {
        // H at y=5 from x=0..6. Terminals at (0, 5) and (6, 5).
        let segs = vec![Segment::Horizontal {
            y: 5,
            x_start: 0,
            x_end: 6,
        }];
        let footprint = segments_to_footprint(&segs);
        // Candidate on a Terminal (right endpoint). side=Above -> shift up.
        let anchor =
            choose_corridor_aware_anchor((6, 5), EdgeLabelSide::Above, &footprint, 10, 10, 1, 1);
        assert_ne!(anchor, (6, 5), "placer must shift off terminal");
        assert!(
            anchor.1 < 5 || !matches!(footprint.cells.get(&anchor), Some(CellRole::Terminal)),
            "Above must not land on a Terminal below the edge"
        );
    }

    // ---- Parity with extend_grid_polyline_into on L-bend ----

    #[test]
    fn segments_to_footprint_matches_extend_grid_polyline_on_l_bend() {
        // This test mirrors the original footprint parity harness:
        // segments_footprint_matches_polyline_on_l_bend.
        let segs = vec![
            Segment::Horizontal {
                y: 0,
                x_start: 0,
                x_end: 3,
            },
            Segment::Vertical {
                x: 3,
                y_start: 0,
                y_end: 3,
            },
        ];
        let fp_segments = segments_to_footprint(&segs);
        let fp_polyline = project_grid_polyline(&[(0, 0), (3, 0), (3, 3)]);
        assert_eq!(fp_segments.cells, fp_polyline.cells);
    }
}
