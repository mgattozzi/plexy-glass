//! Property tests for `RowMark`, the OSC 133 block annotations that ride rows
//! through scrollback, eviction, and reflow. Setters must round-trip and `merge`
//! must union flags (the reflow merge depends on both).

use hegel::{TestCase, generators as gs};
use plexy_glass_emulator::RowMark;

fn draw_flag(tc: &TestCase) -> u8 {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(3)) {
        0 => RowMark::PROMPT_START,
        1 => RowMark::OUTPUT_START,
        2 => RowMark::BLOCK_END,
        _ => RowMark::PROMPT_END,
    }
}

#[hegel::test(test_cases = 500)]
fn prompt_end_col_round_trips(tc: TestCase) {
    let col = tc.draw(gs::integers::<u16>());
    let mut m = RowMark::default();
    assert_eq!(
        m.prompt_end_col(),
        None,
        "default carries no prompt-end col"
    );
    m.set_prompt_end(col);
    assert_eq!(m.prompt_end_col(), Some(col));
    assert!(m.contains(RowMark::PROMPT_END));
}

#[hegel::test(test_cases = 500)]
fn duration_round_trips(tc: TestCase) {
    let dur = if tc.draw(gs::booleans()) {
        Some(tc.draw(gs::integers::<u32>()))
    } else {
        None
    };
    let mut m = RowMark::default();
    m.set_duration(dur);
    assert_eq!(m.duration_ms(), dur);
}

#[hegel::test(test_cases = 500)]
fn set_flag_is_observable_and_idempotent(tc: TestCase) {
    let flag = draw_flag(&tc);
    let mut m = RowMark::default();
    m.set(flag);
    assert!(m.contains(flag));
    m.set(flag);
    assert!(m.contains(flag), "setting a flag twice is idempotent");
}

#[hegel::test(test_cases = 500)]
fn merge_unions_flags(tc: TestCase) {
    let fa = draw_flag(&tc);
    let fb = draw_flag(&tc);
    let mut a = RowMark::default();
    a.set(fa);
    let mut b = RowMark::default();
    b.set(fb);
    a.merge(b);
    assert!(a.contains(fa), "merge keeps self's flag");
    assert!(a.contains(fb), "merge adds other's flag");
}
