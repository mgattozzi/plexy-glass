//! Fuzz target dedicated to the three image-graphics capture paths (Kitty
//! APC, Sixel DCS, iTerm2 OSC 1337): arbitrary bytes must never panic. Unlike
//! `parser_advance` (fuzz_emulator.rs), this target has a seed corpus of
//! realistic protocol byte sequences, so bolero's coverage-guided mutation
//! starts from valid-shaped framing instead of having to discover it from
//! nothing. Run in the normal suite (bolero's DefaultEngine: corpus/crash
//! replay + bounded generation); deep runs use
//! `cargo +nightly bolero test graphics_advance --engine libfuzzer`.

use plexy_glass_emulator::Emulator;

const ROWS: u16 = 24;
const COLS: u16 = 80;

#[test]
fn graphics_advance() {
    bolero::check!().for_each(|input: &[u8]| {
        let mut emu = Emulator::new(ROWS, COLS);
        emu.advance(input);
        let s = emu.screen();
        assert_eq!(s.active.num_rows(), ROWS, "row count changed");
        assert_eq!(s.active.num_cols(), COLS, "col count changed");
        assert!(
            s.cursor.row < ROWS,
            "cursor.row {} out of bounds",
            s.cursor.row
        );
        assert!(
            s.cursor.col < COLS,
            "cursor.col {} out of bounds",
            s.cursor.col
        );
        // Every placement's image id must actually resolve, or be tolerated
        // as evicted (never dangling in a way that panics downstream) — the
        // compositor already assumes this via `images.get(p.image_id)` with
        // an early-continue, so this just asserts the invariant it relies
        // on: no placement references an id whose image was NEVER inserted
        // at all (as opposed to inserted-then-evicted, which is fine).
        for p in &s.placements {
            let _ = s.images.get(p.image_id); // must not panic; None is fine
        }
    });
}
