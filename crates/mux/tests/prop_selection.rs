//! Property tests for `Selection`, the click/drag model. The normalized range
//! is always ordered, and the click dead-zone is consistent with its definition.

use hegel::TestCase;
use hegel::generators as gs;
use plexy_glass_mux::{PaneId, Selection};

fn draw_point(tc: &TestCase) -> (u16, u16) {
    (
        tc.draw(gs::integers::<u16>().min_value(0).max_value(200)),
        tc.draw(gs::integers::<u16>().min_value(0).max_value(200)),
    )
}

fn draw_selection(tc: &TestCase) -> Selection {
    let anchor = draw_point(tc);
    let head = draw_point(tc);
    Selection { source_pane: PaneId(0), anchor, head }
}

#[hegel::test(test_cases = 500)]
fn fresh_selection_is_empty_and_a_click(tc: TestCase) {
    let (r, c) = draw_point(&tc);
    let s = Selection::start(PaneId(0), r, c);
    assert!(s.is_empty(), "a fresh selection has anchor == head");
    assert!(s.is_click(), "an empty selection is within the click dead-zone");
}

#[hegel::test(test_cases = 500)]
fn normalized_is_lexicographically_ordered(tc: TestCase) {
    let s = draw_selection(&tc);
    let (a, b) = s.normalized();
    assert!(a <= b, "normalized() must return (lo, hi) with lo <= hi");
    // The normalized endpoints are exactly the anchor/head as an unordered pair.
    let pair = (s.anchor.min(s.head), s.anchor.max(s.head));
    assert_eq!((a, b), pair);
}

#[hegel::test(test_cases = 500)]
fn is_click_implies_same_row_and_one_cell_drift(tc: TestCase) {
    let s = draw_selection(&tc);
    if s.is_click() {
        assert_eq!(s.anchor.0, s.head.0, "a click stays on one row");
        assert!(s.anchor.1.abs_diff(s.head.1) <= 1, "a click drifts at most one column");
    }
    if s.is_empty() {
        assert!(s.is_click(), "every empty selection is a click");
    }
}
