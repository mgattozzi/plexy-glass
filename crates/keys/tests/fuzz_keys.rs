//! Fuzz target: arbitrary bytes fed to the key parser must never panic.

use plexy_glass_keys::KeyParser;

#[test]
fn key_consume() {
    bolero::check!().for_each(|input: &[u8]| {
        let mut p = KeyParser::new();
        for &b in input {
            let _ = p.consume(b);
        }
    });
}
