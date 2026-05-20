//! `KeyEvent` → legacy VT/xterm byte sequence.

use plexy_glass_mux::KeyEvent;

pub fn legacy_bytes(_event: KeyEvent) -> Vec<u8> {
    // Placeholder; real impl in Task 6.
    Vec::new()
}
