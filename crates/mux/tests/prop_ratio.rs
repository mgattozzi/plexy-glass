//! Property: `Ratio::new` is total. It maps EVERY `f32` — including NaN and
//! ±inf — into `[0.1, 0.9]` and never yields NaN. This is the single clamp site
//! the layout geometry leans on; the invariant used to be a `.clamp(0.1, 0.9)`
//! re-applied by hand at every subdivide / resize / restore site (plus a
//! defensive NaN branch in the declared-ratio math). If `Ratio::new` ever let a
//! poisoned value through, `subdivide` would produce phantom rects.

use hegel::{TestCase, generators as gs};
use plexy_glass_mux::Ratio;

fn assert_in_range(r: Ratio) {
    let v = r.get();
    assert!(!v.is_nan(), "Ratio produced NaN: {v}");
    assert!((0.1..=0.9).contains(&v), "Ratio out of [0.1, 0.9]: {v}");
}

#[hegel::test(test_cases = 1000)]
fn ratio_new_clamps_every_float(tc: TestCase) {
    // An unbounded `floats()` generator may draw NaN and ±inf (per its docs), so
    // this exercises the non-finite inputs too; the explicit cases below pin
    // them deterministically regardless of what the generator happens to draw.
    let f = tc.draw(gs::floats::<f32>());
    tc.note(&format!("input={f}"));
    assert_in_range(Ratio::new(f));
    // `adjust` runs back through `new`, so any delta off any base stays total.
    let d = tc.draw(gs::floats::<f32>());
    assert_in_range(Ratio::new(f).adjust(d));
}

#[test]
fn ratio_new_handles_non_finite_and_extremes() {
    for f in [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        0.0,
        -0.0,
        -1.0e30,
        1.0e30,
        0.05,
        0.95,
        0.5,
    ] {
        assert_in_range(Ratio::new(f));
    }
    let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
    // Non-finite collapses to the valid mid-split, not to a clamp edge.
    assert!(close(Ratio::new(f32::NAN).get(), 0.5));
    assert!(close(Ratio::new(f32::INFINITY).get(), 0.5));
    assert!(close(Ratio::new(f32::NEG_INFINITY).get(), 0.5));
    // In-range values pass through untouched; out-of-range clamp to the edges.
    assert!(close(Ratio::new(0.5).get(), 0.5));
    assert!(close(Ratio::new(0.0).get(), 0.1));
    assert!(close(Ratio::new(1.0).get(), 0.9));
}
