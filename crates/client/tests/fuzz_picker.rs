//! Fuzz target: arbitrary bytes replayed through the picker's input state
//! machine must never panic and must keep the cursor/selection consistent. The
//! picker's cursor ranges over `visible().len() + 1` positions — the real rows
//! plus the always-present synthesized `＋ Connect to a host…` slot — so after
//! every byte EXACTLY ONE of `selected()` (a real row) and `is_new_host_selected()`
//! (the `＋` slot) holds: the cursor never dangles off the selectable set. The
//! exposed filter stays ASCII and `render` stays valid UTF-8. Run in the normal
//! suite (bolero DefaultEngine: corpus/crash replay + bounded random generation);
//! deep, coverage-guided runs use
//! `cargo bolero test picker_replay --engine libfuzzer`.

use plexy_glass_client::Host;
use plexy_glass_client::picker::{PickerRow, PickerState, RowKind, RowStatus};

fn row(name: &str, label: &str, host: Option<&str>, kind: RowKind, status: RowStatus) -> PickerRow {
    PickerRow {
        name: name.into(),
        label: label.into(),
        host: host.map(Host::from),
        kind,
        status,
    }
}

/// The roster shape the pump assembles: a local anchor, local + remote session
/// rows, a configured host, and an ad-hoc host. The `＋ Connect to a host…` slot
/// is NOT a row — the picker synthesizes it — so it isn't listed here.
fn roster_rows() -> Vec<PickerRow> {
    vec![
        row("local", "local", None, RowKind::Host, RowStatus::Live),
        row(
            "main",
            "main - 1 win",
            None,
            RowKind::Session,
            RowStatus::Live,
        ),
        row(
            "build",
            "build - 2 win",
            None,
            RowKind::Session,
            RowStatus::Live,
        ),
        row("prod", "prod", Some("prod"), RowKind::Host, RowStatus::Live),
        row(
            "api",
            "api - 1 win",
            Some("prod"),
            RowKind::Session,
            RowStatus::Live,
        ),
        row(
            "scratch",
            "scratch",
            Some("scratch"),
            RowKind::Host,
            RowStatus::Unreachable,
        ),
    ]
}

/// The picker's structural invariants. The cursor sits on exactly one of the
/// `visible().len() + 1` selectable positions, so `selected()` (a real row) and
/// `is_new_host_selected()` (the `＋` slot) are mutually exclusive and total —
/// exactly one holds after every byte, which is precisely "the cursor stays in
/// bounds of the +1 model". Only ASCII ever reaches the exposed filter, and the
/// buffers (which ride inside `render`) never corrupt the frame.
fn check(p: &PickerState) {
    assert_ne!(
        p.selected().is_some(),
        p.is_new_host_selected(),
        "cursor sits on exactly one of a real row or the ＋ slot"
    );
    assert!(
        p.filter().is_ascii(),
        "filter holds only ASCII: {:?}",
        p.filter()
    );
    assert!(
        String::from_utf8(p.render()).is_ok(),
        "render output is valid UTF-8"
    );
}

#[test]
fn picker_replay() {
    bolero::check!().for_each(|input: &[u8]| {
        let mut p = PickerState::new(roster_rows());
        p.set_adhoc_hosts(vec!["scratch".into()]);

        check(&p);
        for &b in input {
            let _ = p.handle_key(b);
            check(&p);
        }
    });
}
