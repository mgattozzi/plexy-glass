//! Property test for `DiffRenderer`: `apply(diff(a, b), a) == b`.
//!
//! There is no structured `apply` in production — the renderer's "diff" IS
//! the ANSI byte stream a real terminal renders, there's no intermediate op
//! list to replay by hand. So the harness here plays the renderer's own
//! output back through `plexy_glass_emulator::Emulator`, the same VT parser
//! plexy-glass uses to interpret ITS children's output, and compares the
//! resulting grid to the target `VirtualScreen` cell-for-cell. This is a
//! faithful "apply": `DiffRenderer` independently ENCODES `Cell`s into SGR +
//! cursor-addressing bytes, and `Emulator` independently DECODES those same
//! byte forms back into `Cell`s (it's the parser real child processes'
//! output goes through) — two separately written directions over the same
//! wire format, so a mismatch is a real encode/decode bug, not a tautology.

use hegel::{TestCase, generators as gs};
use plexy_glass_emulator::coords::{Col, Row};
use plexy_glass_emulator::{Attrs, Cell, Color, Emulator, UnderlineStyle};
use plexy_glass_mux::{DiffRenderer, VirtualScreen};
use smol_str::SmolStr;

const CHARS: &[char] = &['A', 'B', 'C', ' ', '.', '#'];
const COLORS: [Color; 5] = [
    Color::Default,
    Color::Indexed(1),
    Color::Indexed(9),
    Color::Indexed(200),
    Color::Rgb(200, 30, 90),
];
/// Every attribute the wire is actually meant to carry both ways.
/// `Attrs::HIGHLIGHT` is deliberately excluded: it's a compositor-synthesized
/// copy-mode-search paint cue with no real SGR code of its own (the renderer
/// encodes it as a background color, `\x1b[103m`, a one-way rendering choice,
/// not a value any real terminal or child process round-trips) — asserting
/// it survives `apply(diff(a,b),a)` would be a mis-specified property, not a
/// real bug.
const ATTR_FLAGS: [Attrs; 8] = [
    Attrs::BOLD,
    Attrs::DIM,
    Attrs::ITALIC,
    Attrs::UNDERLINE,
    Attrs::BLINK,
    Attrs::REVERSE,
    Attrs::HIDDEN,
    Attrs::STRIKETHROUGH,
];
const UNDERLINE_STYLES: [UnderlineStyle; 5] = [
    UnderlineStyle::Single,
    UnderlineStyle::Double,
    UnderlineStyle::Curly,
    UnderlineStyle::Dotted,
    UnderlineStyle::Dashed,
];

fn draw_color(tc: &TestCase) -> Color {
    let i = tc.draw(
        gs::integers::<usize>()
            .min_value(0)
            .max_value(COLORS.len() - 1),
    );
    COLORS[i]
}

fn draw_attrs(tc: &TestCase) -> Attrs {
    let mut attrs = Attrs::empty();
    for flag in ATTR_FLAGS {
        if tc.draw(gs::booleans()) {
            attrs.insert(flag);
        }
    }
    attrs
}

fn draw_cell(tc: &TestCase) -> Cell {
    let ci = tc.draw(
        gs::integers::<usize>()
            .min_value(0)
            .max_value(CHARS.len() - 1),
    );
    let attrs = draw_attrs(tc);
    let underline_style = if attrs.contains(Attrs::UNDERLINE) {
        let i = tc.draw(
            gs::integers::<usize>()
                .min_value(0)
                .max_value(UNDERLINE_STYLES.len() - 1),
        );
        UNDERLINE_STYLES[i]
    } else {
        UnderlineStyle::None
    };
    Cell {
        grapheme: SmolStr::new(CHARS[ci].to_string()),
        fg: draw_color(tc),
        bg: draw_color(tc),
        underline_color: draw_color(tc),
        underline_style,
        attrs,
        ..Cell::default()
    }
}

fn draw_screen(tc: &TestCase, rows: u16, cols: u16) -> VirtualScreen {
    let mut v = VirtualScreen::blank(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            v.put(r, c, draw_cell(tc));
        }
    }
    v
}

/// Replay `chunks` (in order) through a fresh VT parser and return its final
/// grid, row-major, one `Cell` per position — the "apply" side of the
/// invariant, since the renderer's output IS the escape-sequence stream a
/// terminal consumes.
fn apply_via_vt(chunks: &[Vec<u8>], rows: u16, cols: u16) -> Vec<Cell> {
    let mut e = Emulator::new(rows, cols);
    for bytes in chunks {
        e.advance(bytes);
    }
    // Flush the parser's pending-grapheme buffer (the emulator holds the last
    // grapheme until the next byte arrives, for cluster/combining handling —
    // see the project's testing note) with a no-op SGR so the final cell of
    // the final row lands before we read the grid back out.
    e.advance(b"\x1b[m");
    let grid = &e.screen().active;
    let mut out = Vec::with_capacity(rows as usize * cols as usize);
    for r in 0..rows {
        for c in 0..cols {
            out.push(
                grid.get_cell(Row::new(r), Col::new(c))
                    .cloned()
                    .unwrap_or_default(),
            );
        }
    }
    out
}

fn screen_cells(v: &VirtualScreen) -> Vec<Cell> {
    let mut out = Vec::with_capacity(v.rows as usize * v.cols as usize);
    for r in 0..v.rows {
        for c in 0..v.cols {
            out.push(v.cell(r, c).cloned().unwrap_or_default());
        }
    }
    out
}

#[hegel::test(test_cases = 150)]
fn diff_replayed_through_a_real_terminal_reproduces_the_target_frame(tc: TestCase) {
    let rows = tc.draw(gs::integers::<u16>().min_value(1).max_value(6));
    let cols = tc.draw(gs::integers::<u16>().min_value(1).max_value(12));
    let a = draw_screen(&tc, rows, cols);
    let b = draw_screen(&tc, rows, cols);

    let mut d = DiffRenderer::new();
    let bytes_a = d.render(&a); // full repaint, establishes "previous == a"
    let bytes_b = d.render(&b); // the incremental diff from a to b

    let got = apply_via_vt(&[bytes_a, bytes_b], rows, cols);
    let want = screen_cells(&b);
    assert_eq!(
        got, want,
        "apply(diff(a, b), a) must equal b (rows={rows}, cols={cols})"
    );
}

/// Companion case: a bare full repaint alone (no prior frame) must already
/// reproduce its target — `apply(diff(_, a), _) == a` for a fresh renderer,
/// the base case the incremental property above builds on.
#[hegel::test(test_cases = 100)]
fn first_render_alone_reproduces_its_target_frame(tc: TestCase) {
    let rows = tc.draw(gs::integers::<u16>().min_value(1).max_value(6));
    let cols = tc.draw(gs::integers::<u16>().min_value(1).max_value(12));
    let a = draw_screen(&tc, rows, cols);

    let mut d = DiffRenderer::new();
    let bytes_a = d.render(&a);

    let got = apply_via_vt(&[bytes_a], rows, cols);
    let want = screen_cells(&a);
    assert_eq!(
        got, want,
        "a lone full repaint must reproduce its target (rows={rows}, cols={cols})"
    );
}
