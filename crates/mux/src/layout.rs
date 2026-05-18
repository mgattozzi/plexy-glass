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
enum LayoutNode {
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
}

impl LayoutTree {
    /// Find the pane nearest to `pane` in `dir`. Uses rect adjacency:
    /// computes the cell just past `pane`'s edge in `dir` and asks which pane
    /// is there.
    pub fn next_in_direction(
        &self,
        pane: PaneId,
        viewport: Rect,
        dir: crate::direction::Direction,
    ) -> Option<PaneId> {
        let rect = self.rect_of(pane, viewport)?;
        let (probe_r, probe_c) = match dir {
            crate::direction::Direction::Up => {
                if rect.row == 0 { return None; }
                (rect.row.saturating_sub(2), rect.col + rect.cols / 2)
            }
            crate::direction::Direction::Down => {
                let r = rect.bottom_edge_row().saturating_add(2);
                (r, rect.col + rect.cols / 2)
            }
            crate::direction::Direction::Left => {
                if rect.col == 0 { return None; }
                (rect.row + rect.rows / 2, rect.col.saturating_sub(2))
            }
            crate::direction::Direction::Right => {
                let c = rect.right_edge_col().saturating_add(2);
                (rect.row + rect.rows / 2, c)
            }
        };
        self.pane_at_coord(viewport, probe_r, probe_c)
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
        let (new_root, _) = resize_in(root, toward, axis, delta_cells, viewport);
        self.root = Some(new_root);
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

/// Returns `(new_node, contains_target)`.
fn resize_in(
    node: LayoutNode,
    target: PaneId,
    axis: SplitDir,
    delta_cells: i32,
    viewport: Rect,
) -> (LayoutNode, bool) {
    match node {
        LayoutNode::Leaf(p) => (LayoutNode::Leaf(p), p == target),
        LayoutNode::Split { dir, ratio, first, second } => {
            let (a_rect, b_rect) = viewport.subdivide(dir, ratio);
            let (new_first, in_first) = resize_in(*first, target, axis, delta_cells, a_rect);
            let (new_second, in_second) =
                resize_in(*second, target, axis, delta_cells, b_rect);

            let mut new_ratio = ratio;
            if dir == axis && (in_first || in_second) {
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
            }

            (
                LayoutNode::Split {
                    dir,
                    ratio: new_ratio,
                    first: Box::new(new_first),
                    second: Box::new(new_second),
                },
                in_first || in_second,
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
}
