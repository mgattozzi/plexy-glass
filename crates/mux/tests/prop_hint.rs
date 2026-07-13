//! Property tests for the hint-mode label scheme (`hint.rs`): `assign_labels`
//! must produce a prefix-free, unique label set (no label is a prefix of
//! another — the property that would catch a picker-class ambiguity, where
//! two on-screen targets share a reachable label and one becomes
//! unreachable), and typing a target's full label through `handle_hint` must
//! narrow to exactly that target. `resolve_overlaps`'s overlap-freeness and
//! maximality are asserted separately, inside `hint.rs` itself (see the
//! doc comment on that property): the function is module-private, so an
//! integration test here can't reach it, only the public label/typing API.

use hegel::{TestCase, generators as gs};
use plexy_glass_mux::{
    HintAction, HintKind, HintOutcome, HintPick, HintState, HintTarget, Key, KeyEvent,
    assign_labels, handle_hint,
};

/// A pool of 24 distinct ASCII letters, so `alphabet_len(k)` below can slice
/// off exactly `k` distinct chars without drawing a separate random alphabet
/// (only the character COUNT matters to `assign_labels`, per its own doc:
/// "Assumes alphabet has >= 2 chars"; the specific letters are irrelevant to
/// its logic, which indexes `Vec<char>` positionally).
const POOL: &str = "abcdefghijklmnopqrstuvwx";

fn draw_alphabet(tc: &TestCase) -> String {
    let k = tc.draw(gs::integers::<usize>().min_value(2).max_value(POOL.len()));
    POOL[..k].to_string()
}

fn draw_targets(n: usize) -> Vec<HintTarget> {
    (0..n)
        .map(|i| HintTarget {
            start: plexy_glass_mux::Point::new(0, (i % u16::MAX as usize) as u16),
            text: format!("target-{i}"),
            kind: HintKind::Sha,
        })
        .collect()
}

/// `n` is drawn up to 300: comfortably past `alphabet_len=2`'s single-length
/// capacity of 64 (2^6, the loop's old hardcoded bound) so a regression of
/// the duplicate-label bug this property caught (n > k^6 with a small
/// alphabet wrapped the mixed-radix counter and silently repeated labels)
/// would fail again immediately.
#[hegel::test(test_cases = 300)]
fn assign_labels_are_unique_and_prefix_free(tc: TestCase) {
    let alphabet = draw_alphabet(&tc);
    let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(300));
    let labels = assign_labels(n, &alphabet);
    tc.note(&format!("n={n} alphabet={alphabet:?} labels={labels:?}"));

    assert_eq!(
        labels.len(),
        n,
        "assign_labels must return exactly n labels"
    );

    for (i, a) in labels.iter().enumerate() {
        for (j, b) in labels.iter().enumerate() {
            if i == j {
                continue;
            }
            assert_ne!(a, b, "labels[{i}] and labels[{j}] are identical: {a:?}");
            assert!(
                !b.starts_with(a.as_str()),
                "label {a:?} (index {i}) is a prefix of {b:?} (index {j}): a \
                 partial type of {a:?} would be ambiguous"
            );
        }
    }
}

/// Typing a target's full label (all lowercase) through `handle_hint` must
/// narrow to a `Pick` naming exactly that target's `copy_text`, never a
/// different one — the end-to-end consequence of labels being unique and
/// prefix-free. Also exercises the uppercase-final-char `Open` action.
#[hegel::test(test_cases = 300)]
fn full_label_narrows_to_exactly_its_own_target(tc: TestCase) {
    let alphabet = draw_alphabet(&tc);
    let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(80));
    let targets = draw_targets(n);
    let base = HintState::new(targets, &alphabet);
    tc.note(&format!(
        "n={n} alphabet={alphabet:?} labels={:?}",
        base.labeled.iter().map(|(l, _)| l).collect::<Vec<_>>()
    ));

    for (label, target) in &base.labeled {
        // Lowercase run: every char but typed as-is (labels are already
        // lowercase) -> Copy.
        let mut st = base.clone();
        st.typed.clear();
        let mut outcome = HintOutcome::None;
        for c in label.chars() {
            outcome = handle_hint(&KeyEvent::plain(Key::Char(c)), &mut st);
        }
        assert_eq!(
            outcome,
            HintOutcome::Pick(HintPick {
                text: target.copy_text(),
                action: HintAction::Copy,
            }),
            "typing full label {label:?} must narrow to its own target {target:?}"
        );

        // Same label, final char uppercased -> Open, still the same target.
        let mut st = base.clone();
        st.typed.clear();
        let mut chars: Vec<char> = label.chars().collect();
        if let Some(last) = chars.last_mut() {
            *last = last.to_ascii_uppercase();
        }
        let mut outcome = HintOutcome::None;
        for c in chars {
            outcome = handle_hint(&KeyEvent::plain(Key::Char(c)), &mut st);
        }
        assert_eq!(
            outcome,
            HintOutcome::Pick(HintPick {
                text: target.open_text(),
                action: HintAction::Open,
            }),
            "typing full label {label:?} with an uppercase final char must Open its own target"
        );
    }
}
