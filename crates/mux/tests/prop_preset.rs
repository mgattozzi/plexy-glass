//! Property tests for `preset.rs`: each of the five layout presets, applied
//! over a random pane count, must be a BIJECTION on the pane set (dropping or
//! duplicating a pane orphans a live PTY invisibly) and must tile the
//! viewport with no two panes overlapping. `main-*` additionally must put the
//! FIRST pane — the caller's "active pane" convention documented on
//! `LayoutTree::apply_preset` ("order matters: for the main-* presets the
//! FIRST pane takes the main slot") — in the full-height/full-width main
//! slot, with every other pane strictly beside it.

use hegel::{TestCase, generators as gs};
use plexy_glass_mux::{LayoutPreset, LayoutTree, PaneId, Point, Rect, Size};

const VIEWPORT: Rect = Rect::new(Point::new(0, 0), Size::new(48, 132));

fn ids(n: u32) -> Vec<PaneId> {
    (0..n).map(PaneId).collect()
}

fn draw_preset(tc: &TestCase) -> LayoutPreset {
    let idx = tc.draw(
        gs::integers::<usize>()
            .min_value(0)
            .max_value(LayoutPreset::ALL.len() - 1),
    );
    LayoutPreset::ALL[idx]
}

fn draw_n(tc: &TestCase) -> u32 {
    tc.draw(gs::integers::<u32>().min_value(1).max_value(24))
}

#[hegel::test(test_cases = 400)]
fn preset_is_a_bijection_on_the_pane_set(tc: TestCase) {
    let preset = draw_preset(&tc);
    let n = draw_n(&tc);
    let panes = ids(n);
    let mut tree = LayoutTree::single(panes[0]);
    tree.apply_preset(preset, &panes);
    tc.note(&format!("preset={preset} n={n}"));

    let mut got = tree.panes();
    got.sort_unstable();
    let mut want = panes;
    want.sort_unstable();
    assert_eq!(
        got, want,
        "{preset} over {n} panes must keep every pane exactly once (no drop, no duplicate)"
    );
}

#[hegel::test(test_cases = 400)]
fn preset_tiles_the_viewport_without_overlap(tc: TestCase) {
    let preset = draw_preset(&tc);
    let n = draw_n(&tc);
    let panes = ids(n);
    let mut tree = LayoutTree::single(panes[0]);
    tree.apply_preset(preset, &panes);

    let rects: Vec<Rect> = panes
        .iter()
        .map(|p| tree.rect_of(*p, VIEWPORT).expect("every pane is present"))
        .collect();
    tc.note(&format!("preset={preset} n={n} rects={rects:?}"));

    // No two pane rects overlap.
    for (i, a) in rects.iter().enumerate() {
        for b in rects.iter().skip(i + 1) {
            let disjoint = a.col() + a.cols() <= b.col()
                || b.col() + b.cols() <= a.col()
                || a.row() + a.rows() <= b.row()
                || b.row() + b.rows() <= a.row();
            assert!(disjoint, "{preset}: panes overlap: {a:?} vs {b:?}");
        }
    }

    // Coverage: a tree with `n` leaves has exactly `n-1` Split nodes, and
    // `Rect::subdivide` (what every preset builder ultimately calls) reserves
    // exactly one row (SplitH) or column (SplitV) of gutter per split, never
    // more, and never a whole extra pane's worth of dead space. So the total
    // unclaimed area can never exceed `(n-1) * max(viewport rows, viewport
    // cols)` — a bound that follows from the documented gutter contract, not
    // a restatement of the tiled-preset code (which none of this touches).
    let claimed: u64 = rects
        .iter()
        .map(|r| u64::from(r.rows()) * u64::from(r.cols()))
        .sum();
    let total = u64::from(VIEWPORT.rows()) * u64::from(VIEWPORT.cols());
    let unclaimed = total - claimed;
    let max_gutter_per_split = u64::from(VIEWPORT.rows().max(VIEWPORT.cols()));
    let budget = u64::from(n.saturating_sub(1)) * max_gutter_per_split;
    assert!(
        unclaimed <= budget,
        "{preset} over {n} panes leaves {unclaimed} cells unclaimed, past the \
         {budget}-cell gutter budget (n-1 splits, <= 1 row/col of gutter each): {rects:?}"
    );
}

#[hegel::test(test_cases = 200)]
fn main_presets_put_the_first_pane_in_the_full_span_main_slot(tc: TestCase) {
    let n = draw_n(&tc);
    if n < 2 {
        return; // main-* only means something once there's a "rest" to place beside it
    }
    let panes = ids(n);
    for preset in [LayoutPreset::MainHorizontal, LayoutPreset::MainVertical] {
        let mut tree = LayoutTree::single(panes[0]);
        tree.apply_preset(preset, &panes);
        let main = tree.rect_of(panes[0], VIEWPORT).expect("main pane present");
        tc.note(&format!("preset={preset} n={n} main={main:?}"));
        assert_eq!(
            (main.row(), main.col()),
            (0, 0),
            "{preset}: the main slot starts at the origin"
        );
        match preset {
            LayoutPreset::MainVertical => assert_eq!(
                main.rows(),
                VIEWPORT.rows(),
                "{preset}: main pane spans the full height"
            ),
            LayoutPreset::MainHorizontal => assert_eq!(
                main.cols(),
                VIEWPORT.cols(),
                "{preset}: main pane spans the full width"
            ),
            _ => unreachable!("loop only iterates the two main-* presets"),
        }
        // Every other pane sits strictly beside the main pane along the
        // preset's split axis (right of it for MainVertical, below it for
        // MainHorizontal) — confirms `panes[0]` isn't just SOME leaf that
        // happens to match the main rect's geometry, but the actual slot the
        // rest of the panes were placed around.
        for p in &panes[1..] {
            let r = tree.rect_of(*p, VIEWPORT).expect("pane present");
            match preset {
                LayoutPreset::MainVertical => assert!(
                    r.col() > main.col(),
                    "{preset}: pane {p:?} is not right of the main pane: {r:?}"
                ),
                LayoutPreset::MainHorizontal => assert!(
                    r.row() > main.row(),
                    "{preset}: pane {p:?} is not below the main pane: {r:?}"
                ),
                _ => unreachable!("loop only iterates the two main-* presets"),
            }
        }
    }
}
