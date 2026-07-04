//! Property tests for the display-width module, the single source of truth for
//! terminal layout. These invariants must hold for ANY string (ASCII, CJK,
//! emoji, ZWJ sequences, combining marks, control chars).
//!
//! Note that there are two distinct measures here: `display_width` is the raw
//! Unicode width (combining marks 0), while `grapheme_advance` (used by
//! `truncate_to_width` and `graphemes_with_width`) clamps each cluster to at
//! least one cell so a lone zero-width grapheme still occupies a grid column.
//! The properties are written against the measure each function actually uses.

use hegel::{TestCase, generators as gs};
use plexy_glass_emulator::width::{display_width, graphemes_with_width, truncate_to_width};

/// Total grid advance (the measure `truncate_to_width` honours).
fn advance_width(s: &str) -> u32 {
    graphemes_with_width(s).map(|(_, w)| u32::from(w)).sum()
}

#[hegel::test(test_cases = 500)]
fn truncate_is_a_prefix_within_budget(tc: TestCase) {
    let s = tc.draw(gs::text());
    let max = tc.draw(gs::integers::<u16>().min_value(0).max_value(64));
    let t = truncate_to_width(&s, max);
    assert!(
        s.starts_with(t),
        "truncation must return a prefix of the input"
    );
    assert!(
        advance_width(t) <= u32::from(max),
        "truncated advance exceeds the budget"
    );
}

#[hegel::test(test_cases = 500)]
fn truncate_is_a_noop_when_the_string_fits(tc: TestCase) {
    let s = tc.draw(gs::text());
    let aw = advance_width(&s);
    if aw > u32::from(u16::MAX) - 64 {
        return; // beyond the u16 budget domain; not a meaningful case
    }
    // Any budget at or above the full ADVANCE width returns the string unchanged.
    let extra = tc.draw(gs::integers::<u16>().min_value(0).max_value(50));
    let max = u16::try_from(aw).unwrap_or(u16::MAX).saturating_add(extra);
    assert_eq!(
        truncate_to_width(&s, max),
        s,
        "must not truncate a string that fits"
    );
}

// NOTE: `display_width(whole) == Σ display_width(grapheme)` is deliberately NOT
// asserted, because `unicode-width` is context-sensitive (a cluster's width
// measured in isolation can differ from its contribution in context, e.g.
// variation- or regional-indicator sequences). That equality is a property of
// the dependency, not of plexy. The properties below test only plexy's own
// logic.

#[hegel::test(test_cases = 500)]
fn grapheme_advance_is_raw_width_clamped_to_one(tc: TestCase) {
    let s = tc.draw(gs::text());
    for (g, adv) in graphemes_with_width(&s) {
        assert_eq!(
            adv,
            display_width(g).max(1),
            "advance must be the raw width clamped to ≥ 1"
        );
        assert!(adv >= 1, "every grapheme advances at least one cell");
    }
}

#[hegel::test(test_cases = 500)]
fn truncate_is_monotonic_in_budget(tc: TestCase) {
    let s = tc.draw(gs::text());
    let a = tc.draw(gs::integers::<u16>().min_value(0).max_value(40));
    let b = tc.draw(gs::integers::<u16>().min_value(0).max_value(40));
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let small = truncate_to_width(&s, lo);
    let large = truncate_to_width(&s, hi);
    assert!(
        large.starts_with(small),
        "a larger budget keeps at least the smaller prefix"
    );
}
