//! Property tests for the command-block helpers: FoldProjection (visible↔unified
//! line mapping), the visible-space scroll geometry, and format_duration.

use hegel::{TestCase, generators as gs};
use plexy_glass_emulator::{Emulator, Screen};
use plexy_glass_mux::blocks::{self, FoldProjection};
use plexy_glass_mux::{ScrollOffset, UnifiedLine, VisibleLine};

#[hegel::test(test_cases = 1000)]
fn format_duration_total_and_well_formed(tc: TestCase) {
    let ms = tc.draw(gs::integers::<u32>().min_value(0).max_value(u32::MAX));
    let s = blocks::format_duration(ms);
    tc.note(&format!("ms={ms} -> {s:?}"));
    assert!(!s.is_empty(), "format_duration must never return empty");
    // Sub-second is "<n>ms"; ≥1s is a seconds form that never ends in "ms".
    if ms < 1000 {
        assert!(s.ends_with("ms"), "sub-second must be ms form");
    } else {
        assert!(
            s.ends_with('s') && !s.ends_with("ms"),
            "≥1s must be a seconds form"
        );
    }
    // No "10.0s" artifact: a tenths form's integer part is a single 1..=9 digit.
    if let Some(dot) = s.find('.') {
        let int = &s[..dot];
        assert!(
            int.len() == 1 && int != "0" && int.bytes().all(|b| b.is_ascii_digit()),
            "tenths form integer part must be a single non-zero digit"
        );
    }
}

#[test]
fn format_duration_boundary_cliffs() {
    // Pin the documented branch boundaries (example anchors alongside the property).
    assert_eq!(blocks::format_duration(340), "340ms");
    assert_eq!(blocks::format_duration(1_000), "1.0s");
    assert_eq!(blocks::format_duration(9_949), "9.9s");
    assert_eq!(blocks::format_duration(9_950), "10s");
    assert_eq!(blocks::format_duration(60_000), "1m00s");
}

/// Build a screen of `k` completed blocks + 1 running block, then fold a random
/// subset of the completed prompts. Each block: prompt (133;A) + a few output rows.
fn build_folded_screen(tc: &TestCase) -> Screen {
    let k = tc.draw(gs::integers::<u8>().min_value(1).max_value(5)) as usize;
    let mut e = Emulator::new(40, 40);
    for i in 0..k {
        let out_rows = tc.draw(gs::integers::<u8>().min_value(1).max_value(3));
        e.advance(format!("\x1b]133;A\x07$ cmd{i}\r\n").as_bytes());
        e.advance(b"\x1b]133;C\x07");
        for r in 0..out_rows {
            e.advance(format!("out{i}_{r}\r\n").as_bytes());
        }
        e.advance(b"\x1b]133;D;0\x07");
    }
    // A final running block so the earlier blocks are completed (foldable).
    e.advance(b"\x1b]133;A\x07$ running\r\n\x1b]133;C\x07");
    e.advance(b"\x1b[m"); // flush the trailing grapheme
    let mut s = e.screen().clone();
    // Fold a random subset of the prompt lines.
    for p in blocks::all_prompt_lines(&s) {
        if tc.draw(gs::booleans()) {
            blocks::set_block_folded(&mut s, p, true);
        }
    }
    s
}

fn screen_total(s: &Screen) -> u32 {
    (s.scrollback.rows().len() + s.active.rows.len()) as u32
}

#[hegel::test(test_cases = 400)]
fn fold_projection_visible_unified_round_trip(tc: TestCase) {
    let s = build_folded_screen(&tc);
    let total = screen_total(&s);
    let proj = FoldProjection::build(&s);
    let vt = proj.visible_total();
    tc.note(&format!(
        "total={total} visible_total={vt} identity={}",
        proj.is_identity()
    ));

    assert!(vt <= total, "visible_total {vt} must be ≤ total {total}");
    // Inverse on the visible domain: every visible line maps to a unified line
    // that maps back to it; the unified line is in range and itself visible.
    for v in 0..vt {
        let u = proj.to_unified(VisibleLine::new(v));
        assert!(
            u.get() < total.max(1),
            "to_unified({v})={} out of range (total {total})",
            u.get()
        );
        assert_eq!(
            proj.from_unified(u),
            Some(VisibleLine::new(v)),
            "from_unified(to_unified({v})) != Some({v})"
        );
    }
    // Strict monotonicity of `to_unified` on [0, vt).
    for v in 0..vt.saturating_sub(1) {
        assert!(
            proj.to_unified(VisibleLine::new(v)) < proj.to_unified(VisibleLine::new(v + 1)),
            "to_unified not strictly increasing at {v}"
        );
    }
    // `from_unified` is a bijection visible-unified ↔ [0, vt): exactly vt unified
    // lines map to `Some`, and the images are exactly {0..vt}.
    let visible_count = (0..total)
        .filter(|&u| proj.from_unified(UnifiedLine::new(u)).is_some())
        .count() as u32;
    assert_eq!(
        visible_count, vt,
        "exactly visible_total unified lines must be visible"
    );
}

#[hegel::test(test_cases = 200)]
fn identity_projection_is_a_no_op(tc: TestCase) {
    let n = tc.draw(gs::integers::<u32>().min_value(0).max_value(50));
    let proj = FoldProjection::identity(n);
    assert!(proj.is_identity());
    assert_eq!(proj.visible_total(), n);
    for v in 0..n {
        assert_eq!(proj.to_unified(VisibleLine::new(v)), UnifiedLine::new(v));
        assert_eq!(
            proj.from_unified(UnifiedLine::new(v)),
            Some(VisibleLine::new(v))
        );
    }
}

/// max_scroll_offset == visible_total - rows; and scroll_offset_for_top is the
/// inverse of scroll_line_at(.., row=0) for any VISIBLE target above the live page.
#[hegel::test(test_cases = 400)]
fn scroll_geometry_inverse(tc: TestCase) {
    let s = build_folded_screen(&tc);
    let rows = tc.draw(gs::integers::<u16>().min_value(1).max_value(20));
    let proj = FoldProjection::build(&s);
    let vt = proj.visible_total();
    let max = blocks::max_scroll_offset(&s, rows);
    tc.note(&format!("vt={vt} rows={rows} max={}", max.get()));
    assert_eq!(
        max,
        ScrollOffset::new(vt.saturating_sub(u32::from(rows))),
        "max_scroll_offset must be visible_total - rows"
    );

    // `scroll_line_at` always returns an in-range, visible line; strictly
    // increasing in `row`.
    for off in [0u32, max.get() / 2, max.get()] {
        let off = ScrollOffset::new(off);
        let mut prev: Option<UnifiedLine> = None;
        for row in 0..rows {
            let line = blocks::scroll_line_at(&s, rows, off, row);
            assert!(
                proj.from_unified(line).is_some(),
                "scroll_line_at landed on a hidden line"
            );
            if let Some(p) = prev {
                assert!(line > p, "scroll_line_at not strictly increasing in row");
            }
            prev = Some(line);
        }
    }
    // For a VISIBLE target whose visible index ≤ max, `offset_for_top` then
    // `line_at(row=0)` recovers it.
    for v in 0..vt {
        if v > max.get() {
            continue;
        } // targets within the bottom `rows` page saturate to offset 0
        let target = proj.to_unified(VisibleLine::new(v));
        let off = blocks::scroll_offset_for_top(&s, rows, target);
        assert!(off <= max, "offset {} exceeds max {}", off.get(), max.get());
        assert_eq!(
            blocks::scroll_line_at(&s, rows, off, 0),
            target,
            "offset_for_top→line_at(0) must recover the target top line"
        );
    }
}

/// The three line-space conversions are true inverses over the newtypes:
/// visible→unified→visible is the identity on the visible domain, and
/// offset→top-unified-line→offset recovers every offset in `[0, max]`. This is
/// the round-trip the newtypes exist to protect (mixing the spaces was the
/// fold/scroll bug class), asserted as a real inverse, not a restatement.
#[hegel::test(test_cases = 400)]
fn three_space_conversions_round_trip(tc: TestCase) {
    let s = build_folded_screen(&tc);
    let proj = FoldProjection::build(&s);
    let vt = proj.visible_total();
    let rows = tc.draw(gs::integers::<u16>().min_value(1).max_value(20));
    let max = blocks::max_scroll_offset(&s, rows);
    tc.note(&format!("vt={vt} rows={rows} max={}", max.get()));

    // visible → unified → visible is the identity on the visible domain.
    for v in 0..vt {
        let vis = VisibleLine::new(v);
        assert_eq!(
            proj.from_unified(proj.to_unified(vis)),
            Some(vis),
            "from_unified(to_unified({v})) must be Some({v})"
        );
    }

    // offset → top unified line → offset recovers every in-range offset. The top
    // line at offset `o` is always visible (never inside a fold), so
    // `scroll_offset_for_top` of it returns `o` exactly for `o` in `[0, max]`.
    for o in 0..=max.get() {
        let off = ScrollOffset::new(o);
        let top = blocks::scroll_line_at(&s, rows, off, 0);
        assert_eq!(
            blocks::scroll_offset_for_top(&s, rows, top),
            off,
            "scroll_offset_for_top(scroll_line_at({o})) must be {o}"
        );
    }
}
