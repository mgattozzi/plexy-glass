//! Per-pane keyboard-protocol negotiation state.
//!
//! Tracks the modifyOtherKeys level and the Kitty keyboard-protocol flag
//! stacks (independent per screen buffer). This module owns ONLY the state a
//! child negotiates with the emulator; it never sees a `KeyEvent` and must not
//! depend on `keys`/`mux` (every crate depends on `emulator`, so such a dep
//! would cycle). Key *encoding* lives in the `keys` crate, which reads this
//! state via the accessors here.

/// Maximum Kitty flag-stack depth before the oldest entry is evicted.
const KITTY_STACK_CAP: usize = 32;

/// One Kitty keyboard-protocol flag stack (per screen buffer).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct KittyStack {
    current: u8,
    stack: Vec<u8>,
}

impl KittyStack {
    /// `\e[=<flags>;<mode>u` sets flags in place.
    /// mode 1 = set-exactly (default), 2 = OR-in, 3 = clear listed bits.
    fn set(&mut self, flags: u8, mode: u8) {
        match mode {
            2 => self.current |= flags,
            3 => self.current &= !flags,
            _ => self.current = flags,
        }
    }

    /// `\e[><flags>u` pushes the given flags, evicting the oldest entry when
    /// the depth cap is hit.
    fn push(&mut self, flags: u8) {
        self.stack.push(self.current);
        if self.stack.len() > KITTY_STACK_CAP {
            self.stack.remove(0);
        }
        self.current = flags;
    }

    /// `\e[<<n>u` pops `n` entries; popping an empty stack resets flags to 0.
    fn pop(&mut self, n: u16) {
        for _ in 0..n.max(1) {
            match self.stack.pop() {
                Some(prev) => self.current = prev,
                None => {
                    self.current = 0;
                    break;
                }
            }
        }
    }

    fn clear(&mut self) {
        self.current = 0;
        self.stack.clear();
    }
}

/// Per-pane keyboard negotiation state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyboardState {
    modify_other_keys: u8,
    kitty_main: KittyStack,
    kitty_alt: KittyStack,
}

impl KeyboardState {
    /// `\e[>4;<Pv>m` sets the modifyOtherKeys level (0/1/2; others ignored).
    pub fn set_modify_other_keys(&mut self, level: u16) {
        if level <= 2 {
            self.modify_other_keys = level as u8;
        }
    }

    /// `\e[>4m` / `\e[>4;m` resets modifyOtherKeys to the initial level (0).
    pub fn reset_modify_other_keys(&mut self) {
        self.modify_other_keys = 0;
    }

    /// Current modifyOtherKeys level, read by the daemon's re-encode stage.
    pub fn modify_other_keys(&self) -> u8 {
        self.modify_other_keys
    }

    /// `\e[=<flags>;<mode>u` on the active screen's stack.
    pub fn kitty_set(&mut self, alt_screen: bool, flags: u8, mode: u8) {
        self.active_mut(alt_screen).set(flags, mode);
    }

    /// `\e[><flags>u` pushes onto the active screen's stack.
    pub fn kitty_push(&mut self, alt_screen: bool, flags: u8) {
        self.active_mut(alt_screen).push(flags);
    }

    /// `\e[<<n>u` pops the active screen's stack.
    pub fn kitty_pop(&mut self, alt_screen: bool, n: u16) {
        self.active_mut(alt_screen).pop(n);
    }

    /// Current Kitty flags for the active screen, read by both the `\e[?u`
    /// reply and the daemon's re-encode stage.
    pub fn kitty_flags(&self, alt_screen: bool) -> u8 {
        self.active(alt_screen).current
    }

    /// Clear all negotiation state (RIS / DECSTR).
    pub fn reset(&mut self) {
        self.modify_other_keys = 0;
        self.kitty_main.clear();
        self.kitty_alt.clear();
    }

    fn active(&self, alt_screen: bool) -> &KittyStack {
        if alt_screen { &self.kitty_alt } else { &self.kitty_main }
    }

    fn active_mut(&mut self, alt_screen: bool) -> &mut KittyStack {
        if alt_screen { &mut self.kitty_alt } else { &mut self.kitty_main }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modify_other_keys_default_zero() {
        let k = KeyboardState::default();
        assert_eq!(k.modify_other_keys(), 0);
    }

    #[test]
    fn modify_other_keys_set_and_reset() {
        let mut k = KeyboardState::default();
        k.set_modify_other_keys(2);
        assert_eq!(k.modify_other_keys(), 2);
        k.reset_modify_other_keys();
        assert_eq!(k.modify_other_keys(), 0);
    }

    #[test]
    fn modify_other_keys_clamps_out_of_range() {
        let mut k = KeyboardState::default();
        k.set_modify_other_keys(9);
        assert_eq!(k.modify_other_keys(), 0, "out-of-range level is ignored");
    }

    // ---- KittyStack via KeyboardState ----

    #[test]
    fn kitty_set_mode1_sets_exactly() {
        let mut k = KeyboardState::default();
        k.kitty_set(false, 0b0011, 1);
        assert_eq!(k.kitty_flags(false), 0b0011);
        k.kitty_set(false, 0b1100, 1);
        assert_eq!(k.kitty_flags(false), 0b1100, "mode=1 replaces, not ORs");
    }

    #[test]
    fn kitty_set_mode2_ors_in_flags() {
        let mut k = KeyboardState::default();
        k.kitty_set(false, 0b0001, 1); // start with bit 0
        k.kitty_set(false, 0b0110, 2); // OR in bits 1+2
        assert_eq!(k.kitty_flags(false), 0b0111, "mode=2 should OR flags in");
    }

    #[test]
    fn kitty_set_mode3_clears_bits() {
        let mut k = KeyboardState::default();
        k.kitty_set(false, 0b0111, 1); // set bits 0,1,2
        k.kitty_set(false, 0b0010, 3); // clear bit 1
        assert_eq!(k.kitty_flags(false), 0b0101, "mode=3 should clear listed bits");
    }

    #[test]
    fn kitty_push_pop_round_trip() {
        let mut k = KeyboardState::default();
        k.kitty_set(false, 0b0001, 1);
        k.kitty_push(false, 0b0010);
        assert_eq!(k.kitty_flags(false), 0b0010);
        k.kitty_pop(false, 1);
        assert_eq!(k.kitty_flags(false), 0b0001, "pop restores previous value");
    }

    #[test]
    fn kitty_push_evicts_oldest_at_cap() {
        // Push `KITTY_STACK_CAP` + 1 entries (the first push saves `current` to the
        // stack). Once the cap is exceeded the oldest entry is evicted.
        let mut k = KeyboardState::default();
        // Set initial current to sentinel 0x01.
        k.kitty_set(false, 0x01, 1);
        // Push `KITTY_STACK_CAP` entries, so the stack holds exactly `KITTY_STACK_CAP`
        // items with 0x01 as the oldest.
        for i in 2..=(KITTY_STACK_CAP as u8 + 1) {
            k.kitty_push(false, i);
        }
        // One more push tips the stack past the cap, so 0x01 (the oldest) is evicted.
        k.kitty_push(false, 0xFF);
        // Pop everything. If eviction worked, the very last pop resets to 0 (empty
        // stack), because 0x01 was evicted and is no longer present.
        for _ in 0..KITTY_STACK_CAP {
            k.kitty_pop(false, 1);
        }
        // Stack is empty now; one more pop should set current to 0.
        k.kitty_pop(false, 1);
        assert_eq!(
            k.kitty_flags(false),
            0,
            "stack was fully drained; 0x01 (oldest) should have been evicted"
        );
    }

    #[test]
    fn kitty_main_and_alt_are_independent() {
        let mut k = KeyboardState::default();
        k.kitty_set(false, 0xAA, 1);
        k.kitty_set(true, 0x55, 1);
        assert_eq!(k.kitty_flags(false), 0xAA);
        assert_eq!(k.kitty_flags(true), 0x55);
    }

    #[test]
    fn kitty_push_no_eviction_at_exactly_cap() {
        // Fill the stack to exactly `KITTY_STACK_CAP`; the sentinel must survive.
        // With `> CAP` only len=CAP+1 evicts, so at exactly CAP no eviction happens.
        // Mutations `== CAP` or `>= CAP` evict one step early and lose the sentinel.
        let mut k = KeyboardState::default();
        k.kitty_set(false, 0xAA, 1); // sentinel is saved as first stack entry
        for i in 0..KITTY_STACK_CAP {
            k.kitty_push(false, i as u8);
        }
        // Pop all `KITTY_STACK_CAP` entries; the sentinel 0xAA must come back last.
        for _ in 0..KITTY_STACK_CAP {
            k.kitty_pop(false, 1);
        }
        assert_eq!(k.kitty_flags(false), 0xAA, "sentinel must survive: no eviction at exactly cap");
    }

    #[test]
    fn kitty_reset_clears_all() {
        let mut k = KeyboardState::default();
        k.kitty_set(false, 0xFF, 1);
        k.kitty_push(false, 0x0F);
        k.reset();
        assert_eq!(k.kitty_flags(false), 0);
        assert_eq!(k.modify_other_keys(), 0);
    }
}
