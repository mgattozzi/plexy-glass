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
}
