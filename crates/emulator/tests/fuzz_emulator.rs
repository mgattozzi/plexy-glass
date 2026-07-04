//! Fuzz target: arbitrary bytes through the VT parser must never panic and must
//! leave the screen structurally consistent. Run in the normal suite (bolero's
//! DefaultEngine: corpus/crash replay + bounded random generation); deep,
//! coverage-guided runs use `cargo bolero test parser_advance --engine libfuzzer`.

use plexy_glass_emulator::Emulator;

const ROWS: u16 = 24;
const COLS: u16 = 80;

#[test]
fn parser_advance() {
    bolero::check!().for_each(|input: &[u8]| {
        let mut emu = Emulator::new(ROWS, COLS);
        emu.advance(input);
        let s = emu.screen();
        // No escape sequence resizes the grid (DECCOLM ?3 is unhandled), so the
        // dimensions are constant and the grid stays rectangular.
        assert_eq!(s.active.num_rows(), ROWS, "row count changed");
        assert_eq!(s.active.num_cols(), COLS, "col count changed");
        for (r, row) in s.active.rows.iter().enumerate() {
            assert_eq!(
                row.cells.len(),
                COLS as usize,
                "row {r} has the wrong width"
            );
        }
        // The cursor is always clamped inside the grid.
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
    });
}
