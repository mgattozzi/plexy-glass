use std::mem::take;

use hegel::{TestCase, generators as gs};
use plexy_glass_client::Host;
use plexy_glass_client::picker::{PickerRow, PickerState, RowKind, RowStatus};
use plexy_glass_emulator::display_width;
use plexy_glass_protocol::PtySize;

/// Split the render into box rows delimited by the `\x1b[..H` position escapes
/// (the render emits one per row and NO `\r`/`\n`); drop every other escape.
/// Same rule as the unit-test `box_lines` helper.
fn box_lines(s: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            let mut ended_h = false;
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    ended_h = e == 'H';
                    break;
                }
            }
            if ended_h && !cur.is_empty() {
                lines.push(take(&mut cur));
            }
        } else if c != '\r' && c != '\n' {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// The row numbers addressed by `\x1b[{r};{c}H` position escapes ONLY — SGR
/// (`\x1b[38;2;..m`), erase (`..J`), and mode (`?..`) escapes are skipped, so a
/// truecolor `38`/`48` is never mistaken for a row.
fn positioned_rows(s: &str) -> Vec<u16> {
    let mut rows = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("\x1b[") {
        rest = &rest[i + 2..];
        let Some(e) = rest.find(|c: char| c.is_ascii_alphabetic()) else {
            break;
        };
        let params = &rest[..e];
        let term = &rest[e..=e];
        if term == "H"
            && let Some((r, _)) = params.split_once(';')
            && let Ok(rn) = r.parse::<u16>()
        {
            rows.push(rn);
        }
        rest = &rest[e + 1..];
    }
    rows
}

#[hegel::test(test_cases = 400)]
fn prop_box_fits_terminal(tc: TestCase) {
    let cols = tc.draw(gs::integers::<u16>().min_value(4).max_value(400));
    let rows = tc.draw(gs::integers::<u16>().min_value(3).max_value(120));
    let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(60));
    let host_rows: Vec<PickerRow> = (0..n)
        .map(|i| PickerRow {
            name: format!("s{i}"),
            label: format!("session-{i}"),
            host: Host::Local,
            kind: RowKind::Session,
            status: RowStatus::Live,
        })
        .collect();
    let mut s = PickerState::new(host_rows);
    s.set_size(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    });
    let text = String::from_utf8(s.render()).expect("utf8");
    for line in box_lines(&text) {
        tc.note(&format!("cols={cols} line={line:?}"));
        assert!(
            display_width(&line) as usize <= cols as usize,
            "no line exceeds cols"
        );
    }
    for r in positioned_rows(&text) {
        tc.note(&format!("rows={rows} positioned_row={r}"));
        assert!(r as usize <= rows as usize, "row {r} within {rows}");
    }
}
