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
}
