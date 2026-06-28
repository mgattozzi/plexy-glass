//! Fuzz target: arbitrary bytes fed to the SGR mouse parser must never panic.

use plexy_glass_mux::MouseParser;

#[test]
fn mouse_consume() {
    bolero::check!().for_each(|input: &[u8]| {
        let mut p = MouseParser::new();
        for &b in input {
            let _ = p.consume(b);
        }
    });
}
