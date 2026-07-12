//! Fuzz `compositor::compose` over emulator-generated screens at arbitrary
//! geometry. The compositor runs inside the render coordinator every frame,
//! so this proves the geometry/scroll math is panic-free. Runs in the normal
//! suite (DefaultEngine: replays the committed corpus/crashes + bounded
//! generation); deep runs use `cargo +nightly bolero test`.

use plexy_glass_emulator::Emulator;
use plexy_glass_mux::compositor::compose;
use plexy_glass_mux::{PaneDragRole, PaneId, PaneView, Rect, ScrollOffset, StatusPlacement};

#[test]
fn fuzz_compose_does_not_panic() {
    bolero::check!().for_each(|bytes: &[u8]| {
        // A 4-byte header picks geometry; the stream (from byte 4) drives the
        // emulator. bytes[0..=2] are rows/cols/scroll; bytes[3] is reserved for
        // a future fuzzed dimension, so it's intentionally skipped for now.
        if bytes.len() < 4 {
            return;
        }
        let rows = u16::from((bytes[0] % 60).max(1));
        let cols = u16::from((bytes[1] % 200).max(1));
        let scroll = u32::from(bytes[2]);
        let stream = &bytes[4..];

        let mut emu = Emulator::new(rows, cols);
        emu.advance(stream);
        let screen = emu.screen();

        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, rows, cols),
            screen,
            is_active: true,
            scroll_offset: ScrollOffset::new(scroll),
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };

        // Must not panic for any input.
        let _ = compose(
            &[view],
            (rows, cols),
            None,
            StatusPlacement::Bottom,
            None,
            None,
            None,
            None,
            None,
            plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61),
            plexy_glass_mux::ChromeColors::ansi_default(),
        );
    });
}
