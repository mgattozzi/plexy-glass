//! Fuzz target: arbitrary bytes replayed through the picker's input state
//! machine must never panic and must keep the cursor/selection consistent. This
//! is the safety net for the cursor-parking-on-the-sentinel class of bug: after
//! every byte the picker exposes a `selected()` iff its filtered view is
//! non-empty (so the cursor never parks off the visible set), the exposed filter
//! stays ASCII, and `render` stays valid UTF-8. Run in the normal suite (bolero
//! DefaultEngine: corpus/crash replay + bounded random generation); deep,
//! coverage-guided runs use `cargo bolero test picker_replay --engine libfuzzer`.

use plexy_glass_client::picker::{PickerRow, PickerState, RowKind, RowStatus};

fn row(name: &str, label: &str, host: Option<&str>, kind: RowKind, status: RowStatus) -> PickerRow {
    PickerRow {
        name: name.into(),
        label: label.into(),
        host: host.map(str::to_string),
        kind,
        status,
    }
}

/// The roster shape the pump assembles: a local anchor, local + remote session
/// rows, a configured host, and an ad-hoc host.
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

fn newhost() -> PickerRow {
    row("", "", None, RowKind::NewHost, RowStatus::Live)
}

/// The picker's structural invariants. `selected()` is `Some` iff the filtered
/// view is non-empty (the cursor stays in bounds of `visible()`), only ASCII
/// ever reaches the exposed filter, and the buffers (which ride inside `render`)
/// never corrupt the frame.
fn check(p: &PickerState) {
    assert_eq!(
        p.selected().is_some(),
        !p.visible().is_empty(),
        "selected iff a row is visible (cursor stays in bounds)"
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
        // Two rosters: one carrying the always-visible `+ Connect to a host...`
        // sentinel (the historical bug site), one without it so a filter can
        // empty the visible set and exercise the cursor-parks-at-0 path.
        let mut with_sentinel = {
            let mut rows = roster_rows();
            rows.push(newhost());
            PickerState::new(rows)
        };
        with_sentinel.set_adhoc_hosts(vec!["scratch".into()]);
        let mut no_sentinel = PickerState::new(roster_rows());
        no_sentinel.set_adhoc_hosts(vec!["scratch".into()]);

        check(&with_sentinel);
        check(&no_sentinel);
        for &b in input {
            let _ = with_sentinel.handle_key(b);
            let _ = no_sentinel.handle_key(b);
            check(&with_sentinel);
            check(&no_sentinel);
        }
    });
}
