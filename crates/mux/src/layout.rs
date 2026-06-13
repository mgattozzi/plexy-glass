//! Binary split-tree for pane layout.
//!
//! A `LayoutTree` always has at least one Leaf. Splitting a leaf converts it
//! into a `Split` whose children are the original leaf and the new one.
//! Closing a leaf removes it and promotes its sibling into the parent's slot.

use crate::{
    direction::SplitDir,
    pane_id::PaneId,
    rect::Rect,
};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LayoutNode {
    Leaf(PaneId),
    Split {
        dir: SplitDir,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitPosition {
    /// New pane sits in the `first` slot; existing target moves to `second`.
    Before,
    /// New pane sits in the `second` slot; existing target stays in `first`.
    After,
}

/// Which border of a pane was hit (used by drag-resize).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderSide {
    Right,
    Bottom,
}

/// Result of a border hit-test: the pane whose right/bottom edge was clicked,
/// plus the side. Combined, these uniquely identify a Split ancestor whose
/// ratio should be adjusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderHit {
    pub adjacent_pane: PaneId,
    pub side: BorderSide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOutcome {
    /// The pane was removed; tree still has at least one leaf.
    SiblingPromoted,
    /// The pane was the only leaf; the tree is now empty (caller should drop it).
    TreeEmpty,
    /// The pane wasn't in the tree to begin with, so this is an idempotent no-op.
    NotPresent,
}

#[derive(Debug, Error, PartialEq)]
pub enum LayoutError {
    #[error("pane {0:?} not found in layout")]
    PaneNotFound(PaneId),
}

#[derive(Debug, Clone)]
pub struct LayoutTree {
    root: Option<LayoutNode>,
}

impl LayoutTree {
    pub fn single(pane: PaneId) -> Self {
        Self { root: Some(LayoutNode::Leaf(pane)) }
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        if let Some(n) = &self.root {
            collect_panes(n, &mut out);
        }
        out
    }

    /// Split `target` along `dir`. `new_pane` is inserted on the side given by
    /// `position` (Before = first child; After = second child). Ratio defaults to 0.5.
    pub fn split(
        &mut self,
        target: PaneId,
        dir: SplitDir,
        new_pane: PaneId,
        position: SplitPosition,
    ) -> Result<(), LayoutError> {
        let Some(root) = self.root.take() else {
            return Err(LayoutError::PaneNotFound(target));
        };
        let (replaced, found) = split_in(root, target, dir, new_pane, position);
        self.root = Some(replaced);
        if found {
            Ok(())
        } else {
            Err(LayoutError::PaneNotFound(target))
        }
    }

    /// Remove `target`. Returns `TreeEmpty` if the tree becomes empty.
    pub fn close(&mut self, target: PaneId) -> CloseOutcome {
        let Some(root) = self.root.take() else {
            return CloseOutcome::NotPresent;
        };
        match close_in(root, target) {
            CloseResult::SamePane => CloseOutcome::TreeEmpty,
            CloseResult::Replaced(new_root) => {
                self.root = Some(new_root);
                CloseOutcome::SiblingPromoted
            }
            CloseResult::NotPresent(orig) => {
                self.root = Some(orig);
                CloseOutcome::NotPresent
            }
        }
    }

    /// Compute the rectangle for `pane` within `viewport` by walking the tree.
    pub fn rect_of(&self, pane: PaneId, viewport: Rect) -> Option<Rect> {
        let root = self.root.as_ref()?;
        rect_of_in(root, pane, viewport)
    }

    /// Replace the tree with `preset` arranged over `panes` (order matters:
    /// for the main-* presets the FIRST pane takes the main slot). Empty
    /// `panes` is a no-op; a single pane becomes a bare Leaf.
    pub fn apply_preset(&mut self, preset: crate::preset::LayoutPreset, panes: &[PaneId]) {
        if panes.is_empty() {
            return;
        }
        debug_assert!(
            {
                let mut seen = panes.to_vec();
                seen.sort_unstable();
                seen.windows(2).all(|w| w[0] != w[1])
            },
            "apply_preset requires unique PaneIds (duplicates would violate the \
             one-leaf-per-pane tree invariant)"
        );
        self.root = Some(crate::preset::build(preset, panes));
    }

    /// Overwrite split ratios in preorder (root, then the first subtree, then
    /// the second), clamping each to subdivide's [0.1, 0.9]. Used by session
    /// restore to re-apply saved ratios after the shape replay (the saved
    /// preorder ratio list maps 1:1 onto the rebuilt tree). Extra ratios are
    /// ignored; missing ones leave splits at their current value. Returns how
    /// many were applied.
    pub fn set_ratios_preorder(&mut self, ratios: &[f32]) -> usize {
        fn walk(node: &mut LayoutNode, ratios: &[f32], i: &mut usize) {
            if let LayoutNode::Split { ratio, first, second, .. } = node {
                let Some(r) = ratios.get(*i) else { return };
                *ratio = r.clamp(0.1, 0.9);
                *i += 1;
                walk(first, ratios, i);
                walk(second, ratios, i);
            }
        }
        let mut i = 0;
        if let Some(root) = self.root.as_mut() {
            walk(root, ratios, &mut i);
        }
        i
    }
}

impl LayoutTree {
    /// Find the pane nearest to `pane` in `dir`. Enumerates every other pane,
    /// keeps the ones whose rect is adjacent in the requested direction (one
    /// separator cell away with overlapping perpendicular range), and picks
    /// the candidate with the largest perpendicular overlap. Ties broken by
    /// the candidate closest to the source pane's perpendicular center.
    pub fn next_in_direction(
        &self,
        pane: PaneId,
        viewport: Rect,
        dir: crate::direction::Direction,
    ) -> Option<PaneId> {
        use crate::direction::Direction;
        let src = self.rect_of(pane, viewport)?;
        let panes: Vec<(PaneId, Rect)> = self
            .panes()
            .into_iter()
            .filter(|p| *p != pane)
            .filter_map(|p| self.rect_of(p, viewport).map(|r| (p, r)))
            .collect();

        match dir {
            Direction::Right => {
                let target_col = src.right_edge_col().checked_add(2)?;
                pick_neighbor(&panes, |r| r.col == target_col, src, /*horizontal=*/ true)
            }
            Direction::Left => {
                let target_right = src.col.checked_sub(2)?;
                pick_neighbor(
                    &panes,
                    |r| r.right_edge_col() == target_right,
                    src,
                    /*horizontal=*/ true,
                )
            }
            Direction::Down => {
                let target_row = src.bottom_edge_row().checked_add(2)?;
                pick_neighbor(&panes, |r| r.row == target_row, src, /*horizontal=*/ false)
            }
            Direction::Up => {
                let target_bottom = src.row.checked_sub(2)?;
                pick_neighbor(
                    &panes,
                    |r| r.bottom_edge_row() == target_bottom,
                    src,
                    /*horizontal=*/ false,
                )
            }
        }
    }

    /// Find which pane (if any) is under the given viewport coordinate.
    pub fn pane_at_coord(&self, viewport: Rect, row: u16, col: u16) -> Option<PaneId> {
        let root = self.root.as_ref()?;
        pane_at_in(root, viewport, row, col)
    }

    /// Adjust the nearest enclosing split (in `axis`) so that the side
    /// containing `toward` grows by `delta_cells`. Clamps to [0.1, 0.9].
    pub fn resize_split(
        &mut self,
        toward: PaneId,
        axis: SplitDir,
        delta_cells: i32,
        viewport: Rect,
    ) {
        let Some(root) = self.root.take() else { return };
        let (new_root, _, _) = resize_in(root, toward, axis, delta_cells, viewport);
        self.root = Some(new_root);
    }

    /// Swap the slots of two existing leaves: pane `a` ends up where `b` was and
    /// vice-versa. All-or-nothing: if either id is absent (or `a == b`), the tree
    /// is left **unchanged** and `false` is returned (so a partial swap can never
    /// rename a live leaf to a non-existent id). When both are present, a single
    /// atomic walk rewrites leaf `a`→`b` and leaf `b`→`a` in one pass (a naive
    /// replace-then-replace would double-apply).
    pub fn swap_panes(&mut self, a: PaneId, b: PaneId) -> bool {
        let panes = self.panes();
        if a == b || !panes.contains(&a) || !panes.contains(&b) {
            return false;
        }
        let Some(root) = self.root.as_mut() else {
            return false;
        };
        let mut found_a = false;
        let mut found_b = false;
        swap_in(root, a, b, &mut found_a, &mut found_b);
        found_a && found_b
    }

    /// Replace the occupant of leaf `old` with `new`. One-way and
    /// shape-preserving: the tree structure, every other leaf, and `old`'s
    /// rect are untouched; only the leaf's pane id changes. Returns `false`
    /// (tree unchanged) when `old` is not a leaf. NOT the atomic two-way
    /// `swap_panes`: a cross-window swap does one `replace_leaf` per window.
    /// The caller is responsible for `new` not already being a leaf (pane
    /// ids are unique per window).
    pub fn replace_leaf(&mut self, old: PaneId, new: PaneId) -> bool {
        fn walk(node: &mut LayoutNode, old: PaneId, new: PaneId) -> bool {
            match node {
                LayoutNode::Leaf(p) => {
                    if *p == old {
                        *p = new;
                        true
                    } else {
                        false
                    }
                }
                // `||` short-circuits, so the walk stops at the first match.
                LayoutNode::Split { first, second, .. } => {
                    walk(first, old, new) || walk(second, old, new)
                }
            }
        }
        match self.root.as_mut() {
            Some(root) => walk(root, old, new),
            None => false,
        }
    }

    /// DFS-order list of pane IDs. Used by callers building on-disk pane
    /// indices (persistence), and the order is stable for a given tree.
    pub fn dfs_leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        if let Some(root) = self.root.as_ref() {
            dfs_collect(root, &mut out);
        }
        out
    }

    /// Walk the layout into a generic recursive structure. `leaf(pane_id, dfs_idx)`
    /// builds a leaf result; `split(dir, ratio, first, second)` combines two
    /// already-recursed children into a split result. Returns `None` if the
    /// tree is empty.
    pub fn map_layout<L, S, R>(&self, mut leaf: L, mut split: S) -> Option<R>
    where
        L: FnMut(PaneId, u32) -> R,
        S: FnMut(SplitDir, f32, R, R) -> R,
    {
        let root = self.root.as_ref()?;
        let mut idx: u32 = 0;
        Some(map_node(root, &mut idx, &mut leaf, &mut split))
    }

    /// Return a `BorderHit` if `(row, col)` is exactly on the gutter cell
    /// between two panes (column for SplitV, row for SplitH).
    pub fn border_at(&self, viewport: Rect, row: u16, col: u16) -> Option<BorderHit> {
        let root = self.root.as_ref()?;
        border_at_recurse(root, viewport, row, col)
    }

    /// Move the split adjacent to `adjacent_pane` on the given `side` by
    /// `delta` cells along its orientation axis. Returns the actually-applied
    /// delta (clamped so each child retains at least `MIN_PANE_CELLS` cells).
    /// Returns 0 if no matching split exists or the drag bottomed out.
    pub fn adjust_split(
        &mut self,
        adjacent_pane: PaneId,
        side: BorderSide,
        delta: i16,
        viewport: Rect,
    ) -> i16 {
        let Some(root) = self.root.as_mut() else { return 0 };
        adjust_split_recurse(root, viewport, adjacent_pane, side, delta)
    }
}

/// Minimum cells per pane along its constrained axis. Borders can't drag a
/// child below this.
const MIN_PANE_CELLS: u16 = 4;

fn border_at_recurse(node: &LayoutNode, viewport: Rect, row: u16, col: u16) -> Option<BorderHit> {
    match node {
        LayoutNode::Leaf(_) => None,
        LayoutNode::Split { dir, ratio, first, second } => {
            let (a, b) = viewport.subdivide(*dir, *ratio);
            match dir {
                SplitDir::Vertical => {
                    // Gutter column sits between `a` and `b` (subdivide reserves
                    // one separator cell). It's the column after `a`'s last col.
                    let gutter_col = a.col.saturating_add(a.cols);
                    if col == gutter_col && row >= a.row && row < a.row.saturating_add(a.rows) {
                        return Some(BorderHit {
                            adjacent_pane: rightmost_leaf(first),
                            side: BorderSide::Right,
                        });
                    }
                }
                SplitDir::Horizontal => {
                    let gutter_row = a.row.saturating_add(a.rows);
                    if row == gutter_row && col >= a.col && col < a.col.saturating_add(a.cols) {
                        return Some(BorderHit {
                            adjacent_pane: bottommost_leaf(first),
                            side: BorderSide::Bottom,
                        });
                    }
                }
            }
            border_at_recurse(first, a, row, col).or_else(|| border_at_recurse(second, b, row, col))
        }
    }
}

fn rightmost_leaf(node: &LayoutNode) -> PaneId {
    match node {
        LayoutNode::Leaf(id) => *id,
        LayoutNode::Split { dir, first, second, .. } => match dir {
            SplitDir::Vertical => rightmost_leaf(second),
            SplitDir::Horizontal => rightmost_leaf(first),
        },
    }
}

fn bottommost_leaf(node: &LayoutNode) -> PaneId {
    match node {
        LayoutNode::Leaf(id) => *id,
        LayoutNode::Split { dir, first, second, .. } => match dir {
            SplitDir::Horizontal => bottommost_leaf(second),
            SplitDir::Vertical => bottommost_leaf(first),
        },
    }
}

fn adjust_split_recurse(
    node: &mut LayoutNode,
    viewport: Rect,
    adjacent_pane: PaneId,
    side: BorderSide,
    delta: i16,
) -> i16 {
    let LayoutNode::Split { dir, ratio, first, second } = node else {
        return 0;
    };
    let (a, b) = viewport.subdivide(*dir, *ratio);
    let split_matches = matches!(
        (*dir, side),
        (SplitDir::Vertical, BorderSide::Right) | (SplitDir::Horizontal, BorderSide::Bottom),
    ) && match side {
        BorderSide::Right => rightmost_leaf(first) == adjacent_pane,
        BorderSide::Bottom => bottommost_leaf(first) == adjacent_pane,
    };
    if split_matches {
        // subdivide reserves a 1-cell gutter, so total usable = total - 1.
        let total_usable = match *dir {
            SplitDir::Vertical => viewport.cols.saturating_sub(1),
            SplitDir::Horizontal => viewport.rows.saturating_sub(1),
        };
        if total_usable < 2 * MIN_PANE_CELLS {
            return 0;
        }
        let first_cells = match *dir {
            SplitDir::Vertical => a.cols,
            SplitDir::Horizontal => a.rows,
        };
        let new_first = (first_cells as i32 + delta as i32)
            .max(MIN_PANE_CELLS as i32)
            .min((total_usable - MIN_PANE_CELLS) as i32) as u16;
        let applied = new_first as i32 - first_cells as i32;
        *ratio = (new_first as f32 / total_usable as f32).clamp(0.1, 0.9);
        return applied as i16;
    }
    let from_first = adjust_split_recurse(first, a, adjacent_pane, side, delta);
    if from_first != 0 {
        return from_first;
    }
    adjust_split_recurse(second, b, adjacent_pane, side, delta)
}

fn swap_in(node: &mut LayoutNode, a: PaneId, b: PaneId, found_a: &mut bool, found_b: &mut bool) {
    match node {
        LayoutNode::Leaf(p) => {
            // `else if` so a single leaf is never swapped twice; with `a != b` the
            // two arms touch distinct leaves.
            if *p == a {
                *p = b;
                *found_a = true;
            } else if *p == b {
                *p = a;
                *found_b = true;
            }
        }
        LayoutNode::Split { first, second, .. } => {
            swap_in(first, a, b, found_a, found_b);
            swap_in(second, a, b, found_a, found_b);
        }
    }
}

fn dfs_collect(node: &LayoutNode, out: &mut Vec<PaneId>) {
    match node {
        LayoutNode::Leaf(id) => out.push(*id),
        LayoutNode::Split { first, second, .. } => {
            dfs_collect(first, out);
            dfs_collect(second, out);
        }
    }
}

fn map_node<L, S, R>(node: &LayoutNode, idx: &mut u32, leaf: &mut L, split: &mut S) -> R
where
    L: FnMut(PaneId, u32) -> R,
    S: FnMut(SplitDir, f32, R, R) -> R,
{
    match node {
        LayoutNode::Leaf(id) => {
            let r = leaf(*id, *idx);
            *idx += 1;
            r
        }
        LayoutNode::Split { dir, ratio, first, second } => {
            let f = map_node(first, idx, leaf, split);
            let s = map_node(second, idx, leaf, split);
            split(*dir, *ratio, f, s)
        }
    }
}

fn pane_at_in(node: &LayoutNode, viewport: Rect, row: u16, col: u16) -> Option<PaneId> {
    match node {
        LayoutNode::Leaf(p) => {
            if viewport.contains(row, col) {
                Some(*p)
            } else {
                None
            }
        }
        LayoutNode::Split { dir, ratio, first, second } => {
            let (a, b) = viewport.subdivide(*dir, *ratio);
            if a.contains(row, col) {
                pane_at_in(first, a, row, col)
            } else if b.contains(row, col) {
                pane_at_in(second, b, row, col)
            } else {
                None
            }
        }
    }
}

/// Pick the best neighbor from `panes` matching `edge_pred`. `horizontal`
/// determines whether the perpendicular axis is rows (true: horizontal
/// motion → compare vertical extents) or columns. Candidate selection: most
/// overlap with `src`'s perpendicular range; tie-broken by smallest
/// distance between the candidate's perpendicular center and `src`'s.
fn pick_neighbor(
    panes: &[(PaneId, Rect)],
    edge_pred: impl Fn(&Rect) -> bool,
    src: Rect,
    horizontal: bool,
) -> Option<PaneId> {
    let (src_lo, src_hi) = if horizontal {
        (src.row, src.bottom_edge_row())
    } else {
        (src.col, src.right_edge_col())
    };
    let src_center = src_lo as i32 + (src_hi as i32 - src_lo as i32) / 2;

    panes
        .iter()
        .filter(|(_, r)| edge_pred(r))
        .filter_map(|(p, r)| {
            let (lo, hi) = if horizontal {
                (r.row, r.bottom_edge_row())
            } else {
                (r.col, r.right_edge_col())
            };
            let overlap = overlap_len(lo, hi, src_lo, src_hi);
            if overlap == 0 {
                None
            } else {
                let center = lo as i32 + (hi as i32 - lo as i32) / 2;
                let center_dist = (center - src_center).unsigned_abs();
                Some((*p, overlap, center_dist))
            }
        })
        // Largest overlap wins; ties broken by smallest center distance
        // (so we prefer the candidate aligned with the source).
        .max_by_key(|(_, overlap, dist)| (*overlap, u32::MAX - dist))
        .map(|(p, _, _)| p)
}

fn overlap_len(a_lo: u16, a_hi: u16, b_lo: u16, b_hi: u16) -> u16 {
    let lo = a_lo.max(b_lo);
    let hi = a_hi.min(b_hi);
    if hi >= lo { hi - lo + 1 } else { 0 }
}

/// Returns `(new_node, contains_target, handled)`. `handled` becomes true once
/// some same-axis split on the path to `target` has consumed the resize, so only
/// the NEAREST enclosing same-axis split adjusts its ratio (the documented
/// single-border contract) instead of the change compounding across every
/// same-axis ancestor, which moved unrelated panes.
fn resize_in(
    node: LayoutNode,
    target: PaneId,
    axis: SplitDir,
    delta_cells: i32,
    viewport: Rect,
) -> (LayoutNode, bool, bool) {
    match node {
        LayoutNode::Leaf(p) => (LayoutNode::Leaf(p), p == target, false),
        LayoutNode::Split { dir, ratio, first, second } => {
            let (a_rect, b_rect) = viewport.subdivide(dir, ratio);
            let (new_first, in_first, handled_first) =
                resize_in(*first, target, axis, delta_cells, a_rect);
            let (new_second, in_second, handled_second) =
                resize_in(*second, target, axis, delta_cells, b_rect);

            let descendant_handled = handled_first || handled_second;
            let mut new_ratio = ratio;
            let mut handled = descendant_handled;
            // Adjust only at the nearest enclosing same-axis split: skip if a
            // deeper same-axis split already consumed the resize.
            if dir == axis && (in_first || in_second) && !descendant_handled {
                let size = match axis {
                    SplitDir::Horizontal => viewport.rows.saturating_sub(1).max(1) as i32,
                    SplitDir::Vertical => viewport.cols.saturating_sub(1).max(1) as i32,
                };
                let dr = (delta_cells as f32) / (size as f32);
                if in_first {
                    new_ratio = (ratio + dr).clamp(0.1, 0.9);
                } else {
                    new_ratio = (ratio - dr).clamp(0.1, 0.9);
                }
                handled = true;
            }

            (
                LayoutNode::Split {
                    dir,
                    ratio: new_ratio,
                    first: Box::new(new_first),
                    second: Box::new(new_second),
                },
                in_first || in_second,
                handled,
            )
        }
    }
}

fn collect_panes(node: &LayoutNode, out: &mut Vec<PaneId>) {
    match node {
        LayoutNode::Leaf(p) => out.push(*p),
        LayoutNode::Split { first, second, .. } => {
            collect_panes(first, out);
            collect_panes(second, out);
        }
    }
}

fn split_in(
    node: LayoutNode,
    target: PaneId,
    dir: SplitDir,
    new_pane: PaneId,
    position: SplitPosition,
) -> (LayoutNode, bool) {
    match node {
        LayoutNode::Leaf(p) if p == target => {
            let (first, second) = match position {
                SplitPosition::Before => (LayoutNode::Leaf(new_pane), LayoutNode::Leaf(p)),
                SplitPosition::After => (LayoutNode::Leaf(p), LayoutNode::Leaf(new_pane)),
            };
            (
                LayoutNode::Split {
                    dir,
                    ratio: 0.5,
                    first: Box::new(first),
                    second: Box::new(second),
                },
                true,
            )
        }
        LayoutNode::Leaf(p) => (LayoutNode::Leaf(p), false),
        LayoutNode::Split { dir: sd, ratio, first, second } => {
            let (new_first, found_first) = split_in(*first, target, dir, new_pane, position);
            if found_first {
                return (
                    LayoutNode::Split {
                        dir: sd,
                        ratio,
                        first: Box::new(new_first),
                        second,
                    },
                    true,
                );
            }
            let (new_second, found_second) = split_in(*second, target, dir, new_pane, position);
            (
                LayoutNode::Split {
                    dir: sd,
                    ratio,
                    first: Box::new(new_first),
                    second: Box::new(new_second),
                },
                found_second,
            )
        }
    }
}

enum CloseResult {
    SamePane,
    Replaced(LayoutNode),
    NotPresent(LayoutNode),
}

fn close_in(node: LayoutNode, target: PaneId) -> CloseResult {
    match node {
        LayoutNode::Leaf(p) if p == target => CloseResult::SamePane,
        LayoutNode::Leaf(p) => CloseResult::NotPresent(LayoutNode::Leaf(p)),
        LayoutNode::Split { dir, ratio, first, second } => {
            match close_in(*first, target) {
                CloseResult::SamePane => CloseResult::Replaced(*second),
                CloseResult::Replaced(n) => CloseResult::Replaced(LayoutNode::Split {
                    dir,
                    ratio,
                    first: Box::new(n),
                    second,
                }),
                CloseResult::NotPresent(orig_first) => match close_in(*second, target) {
                    CloseResult::SamePane => CloseResult::Replaced(orig_first),
                    CloseResult::Replaced(n) => CloseResult::Replaced(LayoutNode::Split {
                        dir,
                        ratio,
                        first: Box::new(orig_first),
                        second: Box::new(n),
                    }),
                    CloseResult::NotPresent(orig_second) => CloseResult::NotPresent(
                        LayoutNode::Split {
                            dir,
                            ratio,
                            first: Box::new(orig_first),
                            second: Box::new(orig_second),
                        },
                    ),
                },
            }
        }
    }
}

fn rect_of_in(node: &LayoutNode, target: PaneId, viewport: Rect) -> Option<Rect> {
    match node {
        LayoutNode::Leaf(p) if *p == target => Some(viewport),
        LayoutNode::Leaf(_) => None,
        LayoutNode::Split { dir, ratio, first, second } => {
            let (a, b) = viewport.subdivide(*dir, *ratio);
            rect_of_in(first, target, a).or_else(|| rect_of_in(second, target, b))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_pane_has_full_viewport() {
        let t = LayoutTree::single(PaneId(0));
        let vp = Rect::new(0, 0, 24, 80);
        assert_eq!(t.rect_of(PaneId(0), vp), Some(vp));
    }

    #[test]
    fn split_makes_two_leaves() {
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After).unwrap();
        let panes = t.panes();
        assert!(panes.contains(&PaneId(0)));
        assert!(panes.contains(&PaneId(1)));
        assert_eq!(panes.len(), 2);
    }

    #[test]
    fn split_on_unknown_returns_error() {
        let mut t = LayoutTree::single(PaneId(0));
        let err = t.split(PaneId(99), SplitDir::Vertical, PaneId(1), SplitPosition::After).unwrap_err();
        assert_eq!(err, LayoutError::PaneNotFound(PaneId(99)));
    }

    #[test]
    fn split_rect_distributes_columns() {
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After).unwrap();
        let vp = Rect::new(0, 0, 24, 21);
        let r0 = t.rect_of(PaneId(0), vp).unwrap();
        let r1 = t.rect_of(PaneId(1), vp).unwrap();
        assert_eq!(r0.cols + r1.cols, 20); // 21 minus 1 separator
        assert_eq!(r0.col, 0);
        assert!(r1.col > r0.col);
    }

    #[test]
    fn close_pane_collapses_split() {
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After).unwrap();
        assert_eq!(t.close(PaneId(1)), CloseOutcome::SiblingPromoted);
        assert_eq!(t.panes(), vec![PaneId(0)]);
        let vp = Rect::new(0, 0, 24, 21);
        assert_eq!(t.rect_of(PaneId(0), vp), Some(vp));
    }

    #[test]
    fn close_only_pane_empties_tree() {
        let mut t = LayoutTree::single(PaneId(0));
        assert_eq!(t.close(PaneId(0)), CloseOutcome::TreeEmpty);
        assert!(t.is_empty());
    }

    #[test]
    fn close_unknown_is_idempotent() {
        let mut t = LayoutTree::single(PaneId(0));
        assert_eq!(t.close(PaneId(99)), CloseOutcome::NotPresent);
        assert_eq!(t.panes(), vec![PaneId(0)]);
    }

    use crate::direction::Direction;

    fn build_two_pane_vertical() -> LayoutTree {
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After).unwrap();
        t
    }

    #[test]
    fn pane_at_coord_finds_left_and_right() {
        let t = build_two_pane_vertical();
        let vp = Rect::new(0, 0, 24, 21);
        assert_eq!(t.pane_at_coord(vp, 5, 2), Some(PaneId(0)));
        assert_eq!(t.pane_at_coord(vp, 5, 18), Some(PaneId(1)));
    }

    #[test]
    fn next_in_direction_finds_right_neighbor() {
        let t = build_two_pane_vertical();
        let vp = Rect::new(0, 0, 24, 21);
        assert_eq!(t.next_in_direction(PaneId(0), vp, Direction::Right), Some(PaneId(1)));
        assert_eq!(t.next_in_direction(PaneId(1), vp, Direction::Left), Some(PaneId(0)));
    }

    #[test]
    fn next_in_direction_returns_none_off_edge() {
        let t = build_two_pane_vertical();
        let vp = Rect::new(0, 0, 24, 21);
        assert_eq!(t.next_in_direction(PaneId(0), vp, Direction::Up), None);
        assert_eq!(t.next_in_direction(PaneId(1), vp, Direction::Right), None);
    }

    #[test]
    fn resize_split_changes_ratio() {
        let mut t = build_two_pane_vertical();
        let vp = Rect::new(0, 0, 24, 21);
        let before = t.rect_of(PaneId(0), vp).unwrap().cols;
        t.resize_split(PaneId(0), SplitDir::Vertical, 3, vp);
        let after = t.rect_of(PaneId(0), vp).unwrap().cols;
        assert!(after > before, "pane 0 should have grown");
    }

    #[test]
    fn resize_split_only_moves_nearest_same_axis_border() {
        // (A | (B | C)) all vertical. Resizing toward C must move only the B|C
        // boundary, pane A must be untouched. (Was: the ratio change compounded
        // across every same-axis ancestor and also shrank the unrelated pane A.)
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After)
            .unwrap();
        t.split(PaneId(1), SplitDir::Vertical, PaneId(2), SplitPosition::After)
            .unwrap();
        let vp = Rect::new(0, 0, 24, 80);

        let a_before = t.rect_of(PaneId(0), vp).unwrap();
        let b_before = t.rect_of(PaneId(1), vp).unwrap();
        let c_before = t.rect_of(PaneId(2), vp).unwrap();

        t.resize_split(PaneId(2), SplitDir::Vertical, 4, vp);

        let a_after = t.rect_of(PaneId(0), vp).unwrap();
        let b_after = t.rect_of(PaneId(1), vp).unwrap();
        let c_after = t.rect_of(PaneId(2), vp).unwrap();

        // A is unrelated to the B|C boundary: its position and width are fixed.
        assert_eq!(a_before.col, a_after.col, "A must not move");
        assert_eq!(a_before.cols, a_after.cols, "A width must not change");
        // Only the nearest (B|C) border moved: C grew, B shrank.
        assert!(c_after.cols > c_before.cols, "C should have grown");
        assert!(b_after.cols < b_before.cols, "B should have shrunk");
    }

    /// L | TR / BR: vertical split, then horizontal split on the right side.
    /// From L moving Right must reach a pane on the right (TR by default,
    /// picked by tie-break on center proximity).
    #[test]
    fn next_in_direction_handles_nested_split() {
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After)
            .unwrap();
        t.split(PaneId(1), SplitDir::Horizontal, PaneId(2), SplitPosition::After)
            .unwrap();
        let vp = Rect::new(0, 0, 24, 80);

        // From L, Right should reach one of the two right-side panes.
        let neighbor = t.next_in_direction(PaneId(0), vp, Direction::Right);
        assert!(
            neighbor == Some(PaneId(1)) || neighbor == Some(PaneId(2)),
            "expected TR or BR from L going Right, got {neighbor:?}"
        );

        // From TR, Left should reach L.
        assert_eq!(
            t.next_in_direction(PaneId(1), vp, Direction::Left),
            Some(PaneId(0))
        );
        // From BR, Left should reach L.
        assert_eq!(
            t.next_in_direction(PaneId(2), vp, Direction::Left),
            Some(PaneId(0))
        );
        // From TR, Down should reach BR.
        assert_eq!(
            t.next_in_direction(PaneId(1), vp, Direction::Down),
            Some(PaneId(2))
        );
        // From BR, Up should reach TR.
        assert_eq!(
            t.next_in_direction(PaneId(2), vp, Direction::Up),
            Some(PaneId(1))
        );
    }

    #[test]
    fn border_at_returns_node_on_vertical_gutter() {
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After)
            .unwrap();
        let vp = Rect::new(0, 0, 10, 21);
        // 21 cols → usable 20 → first.cols ≈ 10 → gutter at col 10.
        let hit = t.border_at(vp, 5, 10).expect("on gutter");
        assert_eq!(hit.adjacent_pane, PaneId(0));
        assert_eq!(hit.side, BorderSide::Right);
    }

    #[test]
    fn border_at_returns_none_in_pane_interior() {
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After)
            .unwrap();
        let vp = Rect::new(0, 0, 10, 21);
        assert!(t.border_at(vp, 5, 5).is_none());
        assert!(t.border_at(vp, 5, 15).is_none());
    }

    #[test]
    fn border_at_on_horizontal_gutter() {
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Horizontal, PaneId(1), SplitPosition::After)
            .unwrap();
        let vp = Rect::new(0, 0, 11, 20);
        // 11 rows → usable 10 → first.rows ≈ 5 → gutter at row 5.
        let hit = t.border_at(vp, 5, 10).expect("on horizontal gutter");
        assert_eq!(hit.side, BorderSide::Bottom);
        assert_eq!(hit.adjacent_pane, PaneId(0));
    }

    #[test]
    fn adjust_split_changes_ratio_for_matching_border() {
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After)
            .unwrap();
        let vp = Rect::new(0, 0, 10, 21);
        let before = t.rect_of(PaneId(0), vp).unwrap();
        let applied = t.adjust_split(PaneId(0), BorderSide::Right, 3, vp);
        assert!(applied > 0, "delta of 3 should apply at least partially");
        let after = t.rect_of(PaneId(0), vp).unwrap();
        assert!(after.cols > before.cols, "first pane should grow");
    }

    #[test]
    fn adjust_split_clamps_to_min_size() {
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After)
            .unwrap();
        let vp = Rect::new(0, 0, 10, 21);
        // Try to shrink the first pane below `MIN_PANE_CELLS` (4) with a huge
        // negative delta.
        let applied = t.adjust_split(PaneId(0), BorderSide::Right, -100, vp);
        // First pane started at ~10 cols and can shrink to 4, so delta no smaller than -6.
        assert!(applied >= -6, "should clamp at MIN_PANE_CELLS; got {applied}");
        assert!(applied < 0, "should still apply something");
    }

    #[test]
    fn swap_panes_exchanges_two_leaf_rects() {
        let mut t = build_two_pane_vertical(); // PaneId(0) left, PaneId(1) right
        let vp = Rect::new(0, 0, 24, 21);
        let r0 = t.rect_of(PaneId(0), vp).unwrap();
        let r1 = t.rect_of(PaneId(1), vp).unwrap();
        assert!(t.swap_panes(PaneId(0), PaneId(1)));
        // After the swap, pane 0 occupies the old right rect and pane 1 the left.
        assert_eq!(t.rect_of(PaneId(0), vp).unwrap(), r1);
        assert_eq!(t.rect_of(PaneId(1), vp).unwrap(), r0);
        // Same set of panes.
        let mut panes = t.panes();
        panes.sort();
        assert_eq!(panes, vec![PaneId(0), PaneId(1)]);
    }

    #[test]
    fn swap_panes_in_nested_tree_leaves_third_put() {
        // L | (TR / BR): vertical split, then horizontal split on the right.
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After).unwrap();
        t.split(PaneId(1), SplitDir::Horizontal, PaneId(2), SplitPosition::After).unwrap();
        let vp = Rect::new(0, 0, 24, 80);
        let r0 = t.rect_of(PaneId(0), vp).unwrap();
        let r1 = t.rect_of(PaneId(1), vp).unwrap();
        let r2 = t.rect_of(PaneId(2), vp).unwrap();
        assert!(t.swap_panes(PaneId(1), PaneId(2)));
        assert_eq!(t.rect_of(PaneId(1), vp).unwrap(), r2);
        assert_eq!(t.rect_of(PaneId(2), vp).unwrap(), r1);
        assert_eq!(t.rect_of(PaneId(0), vp).unwrap(), r0, "untouched leaf keeps its rect");
    }

    #[test]
    fn swap_panes_missing_id_returns_false() {
        let mut t = build_two_pane_vertical();
        let before = t.panes();
        assert!(!t.swap_panes(PaneId(0), PaneId(99)));
        assert_eq!(t.panes(), before, "tree unchanged on a missing id");
    }

    #[test]
    fn replace_leaf_swaps_occupant_at_same_rect() {
        let mut t = build_two_pane_vertical(); // PaneId(0) left, PaneId(1) right
        let vp = Rect::new(0, 0, 24, 21);
        let r0 = t.rect_of(PaneId(0), vp).unwrap();
        let r1 = t.rect_of(PaneId(1), vp).unwrap();
        assert!(t.replace_leaf(PaneId(0), PaneId(5)));
        assert_eq!(t.rect_of(PaneId(5), vp), Some(r0), "new id occupies the old slot");
        assert_eq!(t.rect_of(PaneId(0), vp), None, "old id is gone");
        assert_eq!(t.rect_of(PaneId(1), vp), Some(r1), "other leaf untouched");
        let mut panes = t.panes();
        panes.sort();
        assert_eq!(panes, vec![PaneId(1), PaneId(5)]);
    }

    #[test]
    fn replace_leaf_absent_returns_false_and_leaves_tree_unchanged() {
        let mut t = build_two_pane_vertical();
        let vp = Rect::new(0, 0, 24, 21);
        let r0 = t.rect_of(PaneId(0), vp).unwrap();
        let r1 = t.rect_of(PaneId(1), vp).unwrap();
        assert!(!t.replace_leaf(PaneId(99), PaneId(5)));
        assert_eq!(t.panes(), vec![PaneId(0), PaneId(1)]);
        assert_eq!(t.rect_of(PaneId(0), vp), Some(r0));
        assert_eq!(t.rect_of(PaneId(1), vp), Some(r1));
    }

    #[test]
    fn replace_leaf_in_nested_tree_leaves_others_put() {
        // L | (TR / BR): vertical split, then horizontal split on the right.
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After).unwrap();
        t.split(PaneId(1), SplitDir::Horizontal, PaneId(2), SplitPosition::After).unwrap();
        let vp = Rect::new(0, 0, 24, 80);
        let r0 = t.rect_of(PaneId(0), vp).unwrap();
        let r1 = t.rect_of(PaneId(1), vp).unwrap();
        let r2 = t.rect_of(PaneId(2), vp).unwrap();
        assert!(t.replace_leaf(PaneId(1), PaneId(7)));
        assert_eq!(t.rect_of(PaneId(7), vp), Some(r1), "replacement keeps the slot's rect");
        assert_eq!(t.rect_of(PaneId(1), vp), None);
        assert_eq!(t.rect_of(PaneId(0), vp), Some(r0));
        assert_eq!(t.rect_of(PaneId(2), vp), Some(r2));
    }

    #[test]
    fn set_ratios_preorder_applies_in_order_and_clamps() {
        let mut t = LayoutTree::single(PaneId(0));
        t.split(PaneId(0), SplitDir::Vertical, PaneId(1), SplitPosition::After).unwrap();
        t.split(PaneId(0), SplitDir::Horizontal, PaneId(2), SplitPosition::After).unwrap();
        // Preorder: root split, then the nested split under `first`.
        let applied = t.set_ratios_preorder(&[0.3, 0.95]);
        assert_eq!(applied, 2);
        let vp = Rect::new(0, 0, 40, 100);
        let r0 = t.rect_of(PaneId(0), vp).unwrap();
        // Root ratio 0.3 over usable 99 cols → first child ~30 wide.
        assert!((29..=31).contains(&r0.cols), "{r0:?}");
        let r2 = t.rect_of(PaneId(2), vp).unwrap();
        // Nested 0.95 clamps to 0.9: pane 0 gets ~90% of the first column's
        // usable rows, pane 2 the rest (small but >= 1).
        assert!(r2.rows >= 1 && r2.rows <= 8, "{r2:?}");
        // Extra ratios are ignored; missing ones leave defaults.
        assert_eq!(t.set_ratios_preorder(&[0.5]), 1);
    }

    #[test]
    fn adjust_split_returns_zero_for_nonexistent_border() {
        let mut t = LayoutTree::single(PaneId(0));
        let vp = Rect::new(0, 0, 10, 21);
        let applied = t.adjust_split(PaneId(99), BorderSide::Right, 3, vp);
        assert_eq!(applied, 0);
    }
}
