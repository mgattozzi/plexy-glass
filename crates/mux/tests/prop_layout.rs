//! Property tests for layout geometry: `rect_of` and `pane_at_coord` must be
//! consistent inverses. Every cell inside a pane's content rect must hit-test
//! back to that pane, which is the invariant underneath all mouse routing.

use hegel::{TestCase, generators as gs};
use plexy_glass_mux::{LayoutTree, PaneId, Rect, SplitDir, SplitPosition};

/// Build a random split layout by applying up to a handful of splits to random
/// existing panes. Returns the tree.
fn random_layout(tc: &TestCase) -> LayoutTree {
    let mut t = LayoutTree::single(PaneId(0));
    let mut next: u32 = 1;
    let splits = tc.draw(gs::integers::<u8>().min_value(0).max_value(6));
    for _ in 0..splits {
        let panes = t.panes();
        let idx = tc.draw(
            gs::integers::<usize>()
                .min_value(0)
                .max_value(panes.len() - 1),
        );
        let target = panes[idx];
        let dir = if tc.draw(gs::booleans()) {
            SplitDir::Vertical
        } else {
            SplitDir::Horizontal
        };
        if t.split(target, dir, PaneId(next), SplitPosition::After)
            .is_ok()
        {
            next += 1;
        }
    }
    t
}

#[hegel::test(test_cases = 300)]
fn every_corner_of_a_pane_rect_hit_tests_to_it(tc: TestCase) {
    let t = random_layout(&tc);
    // A generous viewport so panes keep real content rects after the gutters.
    let vp = Rect::new(0, 0, 60, 100);
    for pane in t.panes() {
        let Some(rect) = t.rect_of(pane, vp) else {
            continue;
        };
        if rect.rows == 0 || rect.cols == 0 {
            continue;
        }
        let r1 = rect.row + rect.rows - 1;
        let c1 = rect.col + rect.cols - 1;
        let rmid = rect.row + rect.rows / 2;
        let cmid = rect.col + rect.cols / 2;
        // Each corner and the centre of a CONTENT rect (gutters excluded) must
        // map back to this pane.
        for (r, c) in [
            (rect.row, rect.col),
            (rect.row, c1),
            (r1, rect.col),
            (r1, c1),
            (rmid, cmid),
        ] {
            tc.note(&format!("pane={pane:?} rect={rect:?} cell=({r},{c})"));
            assert_eq!(
                t.pane_at_coord(vp, r, c),
                Some(pane),
                "cell ({r},{c}) lies in pane {pane:?}'s rect but pane_at_coord disagrees",
            );
        }
    }
}

/// `replace_leaf(old, new)` is the slot-preserving swap that "drop to shell on
/// command exit" relies on (a dying command pane's leaf is re-occupied by a
/// fresh shell). It must keep the layout's SHAPE: the pane count is unchanged,
/// the new id inherits the old leaf's exact rect, and every OTHER pane's rect
/// is untouched, only the occupant id changes.
#[hegel::test(test_cases = 300)]
fn replace_leaf_preserves_slot_set_and_rects(tc: TestCase) {
    let mut t = random_layout(&tc);
    let vp = Rect::new(0, 0, 60, 100);
    let panes_before = t.panes();
    // Pick a leaf to replace and a fresh id not already in the tree.
    let idx = tc.draw(
        gs::integers::<usize>()
            .min_value(0)
            .max_value(panes_before.len() - 1),
    );
    let old = panes_before[idx];
    let new = PaneId(10_000); // random_layout only ever mints ids < 7
    let old_rect = t.rect_of(old, vp);
    let others_before: Vec<(PaneId, Option<Rect>)> = panes_before
        .iter()
        .filter(|p| **p != old)
        .map(|p| (*p, t.rect_of(*p, vp)))
        .collect();

    let replaced = t.replace_leaf(old, new);
    assert!(
        replaced,
        "replacing an existing leaf {old:?} must report success"
    );

    let panes_after = t.panes();
    tc.note(&format!(
        "before={panes_before:?} old={old:?} after={panes_after:?}"
    ));
    // Arity preserved: same count, old gone, new present.
    assert_eq!(
        panes_after.len(),
        panes_before.len(),
        "pane count must not change"
    );
    assert!(!panes_after.contains(&old), "old id must be gone");
    assert!(panes_after.contains(&new), "new id must occupy the slot");
    // The new occupant inherits the exact slot rect.
    assert_eq!(
        t.rect_of(new, vp),
        old_rect,
        "new pane inherits the old slot's rect"
    );
    // Every other pane's rect is untouched.
    for (p, rect) in others_before {
        assert_eq!(
            t.rect_of(p, vp),
            rect,
            "sibling {p:?} rect must be unchanged"
        );
    }
}

#[hegel::test(test_cases = 300)]
fn pane_rects_never_overlap(tc: TestCase) {
    let t = random_layout(&tc);
    let vp = Rect::new(0, 0, 60, 100);
    let rects: Vec<(PaneId, Rect)> = t
        .panes()
        .into_iter()
        .filter_map(|p| t.rect_of(p, vp).map(|r| (p, r)))
        .collect();
    for (i, (pa, a)) in rects.iter().enumerate() {
        for (pb, b) in rects.iter().skip(i + 1) {
            let disjoint = a.col + a.cols <= b.col
                || b.col + b.cols <= a.col
                || a.row + a.rows <= b.row
                || b.row + b.rows <= a.row;
            assert!(disjoint, "pane {pa:?} {a:?} overlaps pane {pb:?} {b:?}");
        }
    }
}
