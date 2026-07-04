//! Preset pane layouts (tmux-style): pure tree builders over an ordered pane
//! list. Applying a preset rearranges the window's existing panes, and never
//! touches the panes themselves.

use std::fmt;

use crate::direction::SplitDir;
use crate::layout::LayoutNode;
use crate::pane_id::PaneId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutPreset {
    /// Panes side by side in one row.
    EvenHorizontal,
    /// Panes stacked in one column.
    EvenVertical,
    /// Main pane on top (~60%), the rest in an even row below.
    MainHorizontal,
    /// Main pane on the left (~60%), the rest stacked evenly on the right.
    MainVertical,
    /// Near-square grid: round(sqrt(N)) rows, earlier rows take the remainder.
    Tiled,
}

/// The main pane's share in the main-* presets.
const MAIN_RATIO: f32 = 0.6;

impl LayoutPreset {
    /// Cycle order for `next_layout` (declaration order).
    pub const ALL: [Self; 5] = [
        Self::EvenHorizontal,
        Self::EvenVertical,
        Self::MainHorizontal,
        Self::MainVertical,
        Self::Tiled,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::EvenHorizontal => "even-horizontal",
            Self::EvenVertical => "even-vertical",
            Self::MainHorizontal => "main-horizontal",
            Self::MainVertical => "main-vertical",
            Self::Tiled => "tiled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == s)
    }

    /// The next preset in the cycle, wrapping.
    #[must_use]
    pub fn next(self) -> Self {
        // invariant: every variant is in ALL by construction
        let idx = Self::ALL
            .iter()
            .position(|p| *p == self)
            .expect("variant present in ALL");
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }
}

impl fmt::Display for LayoutPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Build the preset's tree over `panes` (non-empty; single pane → Leaf).
pub(crate) fn build(preset: LayoutPreset, panes: &[PaneId]) -> LayoutNode {
    debug_assert!(!panes.is_empty(), "build_preset over zero panes");
    if panes.len() == 1 {
        return LayoutNode::Leaf(panes[0]);
    }
    match preset {
        LayoutPreset::EvenHorizontal => even_leaf_chain(panes, SplitDir::Vertical),
        LayoutPreset::EvenVertical => even_leaf_chain(panes, SplitDir::Horizontal),
        LayoutPreset::MainVertical => LayoutNode::Split {
            dir: SplitDir::Vertical,
            ratio: MAIN_RATIO,
            first: Box::new(LayoutNode::Leaf(panes[0])),
            second: Box::new(even_leaf_chain(&panes[1..], SplitDir::Horizontal)),
        },
        LayoutPreset::MainHorizontal => LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratio: MAIN_RATIO,
            first: Box::new(LayoutNode::Leaf(panes[0])),
            second: Box::new(even_leaf_chain(&panes[1..], SplitDir::Vertical)),
        },
        LayoutPreset::Tiled => {
            let n = panes.len();
            // round(sqrt(N)) rows, clamped >= 1; earlier rows absorb the
            // remainder. N=2/3/4/5 → 1x2, 2+1, 2x2, 3+2 (matches tmux).
            let rows = ((n as f64).sqrt().round() as usize).max(1);
            let base = n / rows;
            let rem = n % rows;
            let mut row_nodes = Vec::with_capacity(rows);
            let mut start = 0usize;
            for i in 0..rows {
                let len = base + usize::from(i < rem);
                row_nodes.push(even_leaf_chain(
                    &panes[start..start + len],
                    SplitDir::Vertical,
                ));
                start += len;
            }
            even_node_chain(row_nodes, SplitDir::Horizontal)
        }
    }
}

/// Balanced even tree of leaves: split the panes in half (first half takes
/// the ceiling) at ratio k1/k. A right-leaning 1/k chain is NOT used because
/// `subdivide` reserves this level's separator only, so at ratio 1/k the first
/// pane absorbs (k-2)/k of the rest's future separators per level, and the
/// accumulated error makes panes uneven by 2 cells at N=5 in a 40x120
/// viewport. Halving keeps the per-level error at <= 1/k cell and halves the
/// depth, so panes stay even within one cell.
fn even_leaf_chain(panes: &[PaneId], dir: SplitDir) -> LayoutNode {
    // invariant: callers pass non-empty slices
    debug_assert!(!panes.is_empty());
    if panes.len() == 1 {
        return LayoutNode::Leaf(panes[0]);
    }
    let k1 = panes.len().div_ceil(2);
    LayoutNode::Split {
        dir,
        ratio: k1 as f32 / panes.len() as f32,
        first: Box::new(even_leaf_chain(&panes[..k1], dir)),
        second: Box::new(even_leaf_chain(&panes[k1..], dir)),
    }
}

/// Same balanced tree over pre-built subtrees (the tiled rows).
fn even_node_chain(mut nodes: Vec<LayoutNode>, dir: SplitDir) -> LayoutNode {
    // invariant: callers pass non-empty vecs
    debug_assert!(!nodes.is_empty());
    if nodes.len() == 1 {
        return nodes.remove(0);
    }
    let k = nodes.len();
    let k1 = k.div_ceil(2);
    let rest = nodes.split_off(k1);
    LayoutNode::Split {
        dir,
        ratio: k1 as f32 / k as f32,
        first: Box::new(even_node_chain(nodes, dir)),
        second: Box::new(even_node_chain(rest, dir)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{LayoutTree, PaneId, Rect};

    fn ids(n: u32) -> Vec<PaneId> {
        (0..n).map(PaneId).collect()
    }

    /// Apply `preset` over `n` panes and return each pane's rect in a 40x120
    /// viewport (workable sizes for evenness asserts).
    fn rects(preset: LayoutPreset, n: u32) -> Vec<Rect> {
        let panes = ids(n);
        let mut tree = LayoutTree::single(panes[0]);
        tree.apply_preset(preset, &panes);
        let vp = Rect::new(0, 0, 40, 120);
        panes
            .iter()
            .map(|p| tree.rect_of(*p, vp).expect("pane present after preset"))
            .collect()
    }

    #[test]
    fn names_parse_and_display_round_trip() {
        for p in LayoutPreset::ALL {
            assert_eq!(LayoutPreset::parse(p.name()), Some(p));
            assert_eq!(format!("{p}"), p.name());
        }
        assert_eq!(LayoutPreset::parse("bogus"), None);
    }

    #[test]
    fn cycle_order_is_declaration_order_and_wraps() {
        let mut p = LayoutPreset::EvenHorizontal;
        let seen: Vec<LayoutPreset> = (0..5)
            .map(|_| {
                let cur = p;
                p = p.next();
                cur
            })
            .collect();
        assert_eq!(seen, LayoutPreset::ALL.to_vec());
        assert_eq!(p, LayoutPreset::EvenHorizontal, "wraps to the start");
    }

    #[test]
    fn even_horizontal_is_side_by_side_and_even() {
        for n in 2..=5u32 {
            let rs = rects(LayoutPreset::EvenHorizontal, n);
            // All panes span the full height, sit at row 0.
            assert!(rs.iter().all(|r| r.row == 0 && r.rows == 40), "{n}: {rs:?}");
            // Widths even within 1 cell.
            let min = rs.iter().map(|r| r.cols).min().unwrap();
            let max = rs.iter().map(|r| r.cols).max().unwrap();
            assert!(max - min <= 1, "{n} panes: widths {rs:?}");
        }
    }

    #[test]
    fn even_vertical_is_stacked_and_even() {
        for n in 2..=5u32 {
            let rs = rects(LayoutPreset::EvenVertical, n);
            assert!(
                rs.iter().all(|r| r.col == 0 && r.cols == 120),
                "{n}: {rs:?}"
            );
            let min = rs.iter().map(|r| r.rows).min().unwrap();
            let max = rs.iter().map(|r| r.rows).max().unwrap();
            assert!(max - min <= 1, "{n} panes: heights {rs:?}");
        }
    }

    #[test]
    fn main_vertical_gives_first_pane_the_major_left_share() {
        for n in 2..=5u32 {
            let rs = rects(LayoutPreset::MainVertical, n);
            let main = rs[0];
            assert_eq!((main.row, main.col), (0, 0));
            assert_eq!(main.rows, 40, "main pane spans full height");
            // ~60% of 120 usable-minus-separator; allow rounding slack.
            assert!(main.cols >= 65 && main.cols <= 75, "main width {main:?}");
            // The rest stack evenly to the right of the main pane.
            for r in &rs[1..] {
                assert!(r.col > main.cols, "stacked pane left of main: {r:?}");
            }
            let min = rs[1..].iter().map(|r| r.rows).min().unwrap();
            let max = rs[1..].iter().map(|r| r.rows).max().unwrap();
            assert!(max - min <= 1, "{n}: stack heights {rs:?}");
        }
    }

    #[test]
    fn main_horizontal_gives_first_pane_the_major_top_share() {
        for n in 2..=5u32 {
            let rs = rects(LayoutPreset::MainHorizontal, n);
            let main = rs[0];
            assert_eq!((main.row, main.col), (0, 0));
            assert_eq!(main.cols, 120, "main pane spans full width");
            assert!(
                main.rows >= 21 && main.rows <= 26,
                "{n}: main height {main:?}"
            );
            // The rest tile below the main pane, evenly.
            for r in &rs[1..] {
                assert!(r.row > main.rows, "{n}: row pane above main: {r:?}");
            }
            let min = rs[1..].iter().map(|r| r.rows).min().unwrap();
            let max = rs[1..].iter().map(|r| r.rows).max().unwrap();
            assert!(max - min <= 1, "{n}: row heights {rs:?}");
            let wmin = rs[1..].iter().map(|r| r.cols).min().unwrap();
            let wmax = rs[1..].iter().map(|r| r.cols).max().unwrap();
            assert!(wmax - wmin <= 1, "{n}: row widths {rs:?}");
        }
    }

    #[test]
    fn tiled_grid_dimensions_match_tmux() {
        // N=2 → 1x2; N=3 → 2 rows (2+1); N=4 → 2x2; N=5 → 2 rows (3+2).
        let distinct_rows = |rs: &[Rect]| {
            let mut rows: Vec<u16> = rs.iter().map(|r| r.row).collect();
            rows.sort_unstable();
            rows.dedup();
            rows.len()
        };
        assert_eq!(distinct_rows(&rects(LayoutPreset::Tiled, 2)), 1);
        assert_eq!(distinct_rows(&rects(LayoutPreset::Tiled, 3)), 2);
        assert_eq!(distinct_rows(&rects(LayoutPreset::Tiled, 4)), 2);
        assert_eq!(distinct_rows(&rects(LayoutPreset::Tiled, 5)), 2);
        // 3 panes: first row has 2 panes, second has 1.
        let rs = rects(LayoutPreset::Tiled, 3);
        let top_row = rs.iter().filter(|r| r.row == 0).count();
        assert_eq!(top_row, 2, "{rs:?}");
    }

    #[test]
    fn single_pane_is_a_bare_leaf() {
        let rs = rects(LayoutPreset::Tiled, 1);
        assert_eq!(rs[0], Rect::new(0, 0, 40, 120));
    }

    #[test]
    fn tiled_rows_are_evenly_split_in_height() {
        // `even_node_chain` combines the per-row sub-trees with a balanced ratio.
        // The ratio must be k1/k (not k1%k or k1*k). Verify that all distinct
        // row-heights in the tiled preset are within 1 cell of each other,
        // exercising the two-row case (n=4, k=2, k1=1, ratio should be 0.5).
        let row_heights = |rs: &[Rect]| -> Vec<u16> {
            let mut map = HashMap::new();
            for r in rs {
                map.insert(r.row, r.rows);
            }
            let mut v: Vec<u16> = map.values().copied().collect();
            v.sort_unstable();
            v
        };

        // n=4: two rows of 2 panes, each row should get ~half the height (≈19 rows).
        let rs4 = rects(LayoutPreset::Tiled, 4);
        let hs4 = row_heights(&rs4);
        assert_eq!(hs4.len(), 2, "n=4 tiled must have 2 distinct row heights");
        assert!(
            hs4[1] - hs4[0] <= 1,
            "n=4 tiled row heights uneven: {hs4:?}"
        );

        // n=9: three rows of 3 panes, heights should be within 1 of each other.
        let rs9 = rects(LayoutPreset::Tiled, 9);
        let hs9 = row_heights(&rs9);
        assert_eq!(hs9.len(), 3, "n=9 tiled must have 3 distinct row heights");
        assert!(
            hs9[2] - hs9[0] <= 1,
            "n=9 tiled row heights uneven: {hs9:?}"
        );
    }

    #[test]
    fn every_preset_preserves_all_panes_exactly_once() {
        // Exercises the remainder paths past the geometry tests' n<=5 (e.g.
        // tiled n=7 -> rows 3,2,2). A builder that dropped or duplicated a
        // pane would orphan a PTY invisibly.
        for preset in LayoutPreset::ALL {
            for n in 1..=16u32 {
                let panes = ids(n);
                let mut tree = LayoutTree::single(panes[0]);
                tree.apply_preset(preset, &panes);
                let mut got = tree.panes();
                got.sort_unstable();
                assert_eq!(got, panes, "{preset} over {n} panes");
            }
        }
    }
}
