//! Property tests for the KDL config decoder. No serializer exists, so the
//! roadmap's parse→serialize→parse fixpoint is replaced by (1) decoder TOTALITY
//! over arbitrary input (it must never panic, only Ok or Err) and (2) a forward
//! KDL-injection round-trip for a clean field (the decoder faithfully reflects
//! authored config).

use hegel::generators as gs;
use hegel::TestCase;
use plexy_glass_config::parse_config;

/// The decoder is total: arbitrary text (incl. malformed KDL, control bytes,
/// huge/empty input) never panics, it returns Ok or Err. This is the
/// untrusted-input surface (config files) with no fuzz target today.
#[hegel::test(test_cases = 1000)]
fn parse_config_never_panics(tc: TestCase) {
    let src = tc.draw(gs::text());
    tc.note(&format!("src = {src:?}"));
    let _ = parse_config(&src); // must not panic regardless of Ok/Err
}

/// Plausible-KDL totality: a document mixing a random node line with a valid
/// `welcome` line never panics the decoder. (`gs::text()` alone is mostly garbage
/// that fails at the KDL syntax gate; the appended `welcome #{b}` line guarantees
/// at least one decodable node, while the random first node adds malformed-input
/// coverage. Either way the decoder must return Ok/Err, never panic.)
#[hegel::test(test_cases = 500)]
fn parse_config_plausible_kdl_never_panics(tc: TestCase) {
    let node = tc.draw(gs::text());
    let val = tc.draw(gs::integers::<i64>().min_value(i64::MIN).max_value(i64::MAX));
    let b = tc.draw(gs::booleans());
    let src = format!("{node} {val} #{b}\nwelcome #{b}\n");
    tc.note(&format!("src = {src:?}"));
    let _ = parse_config(&src);
}

/// Forward injection round-trip: a `welcome #true|#false` node parses to the
/// matching `Config.welcome` (the decoder faithfully reflects authored config for
/// a clean boolean field). The forward direction is all we need without a serializer.
#[hegel::test(test_cases = 200)]
fn welcome_bool_injection_round_trips(tc: TestCase) {
    let want = tc.draw(gs::booleans());
    let src = format!("welcome #{want}\n");
    let cfg = parse_config(&src).expect("a lone welcome node must parse");
    assert_eq!(cfg.welcome, want, "decoded welcome must equal the authored value");
}
