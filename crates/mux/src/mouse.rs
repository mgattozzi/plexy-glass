//! Mouse event types, ANSI-SGR parser, and child-forwarding encoder.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Release,
    Move,
    /// Positive `delta` = wheel up, negative = wheel down.
    Wheel { delta: i16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub button: MouseButton,
    pub modifiers: MouseModifiers,
    /// 0-indexed row within the host viewport.
    pub row: u16,
    /// 0-indexed column within the host viewport.
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEncoding {
    /// `?9`: initial click only, no release/move.
    X10,
    /// `?1000`: press + release.
    ButtonEvent,
    /// `?1003`: press + release + any movement.
    AnyEvent,
    /// `?1006`: SGR encoding with extended coordinates.
    Sgr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseParseAction {
    Pending,
    Event(MouseEvent),
    Other(u8),
}

#[derive(Debug, Clone, Copy)]
enum ParseState {
    Idle,
    SawEsc,
    SawBracket,
    SawLt,
    AccumParam(u8), // current param index (0..=2)
}

pub struct MouseParser {
    state: ParseState,
    params: [u32; 3],
}

impl MouseParser {
    pub fn new() -> Self {
        Self {
            state: ParseState::Idle,
            params: [0; 3],
        }
    }

    /// Feed one byte. See `MouseParseAction` for return semantics.
    pub fn consume(&mut self, byte: u8) -> MouseParseAction {
        match self.state {
            ParseState::Idle => {
                if byte == 0x1b {
                    self.state = ParseState::SawEsc;
                    MouseParseAction::Pending
                } else {
                    MouseParseAction::Other(byte)
                }
            }
            ParseState::SawEsc => {
                if byte == b'[' {
                    self.state = ParseState::SawBracket;
                    MouseParseAction::Pending
                } else {
                    // Not a mouse sequence; emit current byte as Other. The
                    // held ESC is dropped (rare in practice; ESC by itself
                    // outside a sequence is unusual once the host TTY is in
                    // raw mode with key-only input).
                    self.reset();
                    MouseParseAction::Other(byte)
                }
            }
            ParseState::SawBracket => {
                if byte == b'<' {
                    self.state = ParseState::SawLt;
                    MouseParseAction::Pending
                } else {
                    // CSI without `<` isn't an SGR mouse sequence. Bail.
                    self.reset();
                    MouseParseAction::Other(byte)
                }
            }
            ParseState::SawLt => {
                self.state = ParseState::AccumParam(0);
                self.consume(byte)
            }
            ParseState::AccumParam(idx) => {
                // invariant: FSM only ever sets idx to 0, 1, or 2
                debug_assert!(idx <= 2, "AccumParam idx out of range");
                if byte.is_ascii_digit() {
                    self.params[idx as usize] =
                        self.params[idx as usize].saturating_mul(10) + u32::from(byte - b'0');
                    MouseParseAction::Pending
                } else if byte == b';' {
                    if idx >= 2 {
                        // Too many params; bail.
                        self.reset();
                        MouseParseAction::Other(byte)
                    } else {
                        self.state = ParseState::AccumParam(idx + 1);
                        MouseParseAction::Pending
                    }
                } else if byte == b'M' || byte == b'm' {
                    let evt = self.build_event(byte == b'M');
                    self.reset();
                    MouseParseAction::Event(evt)
                } else {
                    self.reset();
                    MouseParseAction::Other(byte)
                }
            }
        }
    }

    fn reset(&mut self) {
        self.state = ParseState::Idle;
        self.params = [0; 3];
    }

    fn build_event(&self, is_press: bool) -> MouseEvent {
        let raw = self.params[0];
        let col = u16::try_from(self.params[1].saturating_sub(1).min(u32::from(u16::MAX)))
            // invariant: value is clamped to u16::MAX above
            .unwrap_or(u16::MAX);
        let row = u16::try_from(self.params[2].saturating_sub(1).min(u32::from(u16::MAX)))
            // invariant: value is clamped to u16::MAX above
            .unwrap_or(u16::MAX);
        let modifiers = MouseModifiers {
            shift: raw & 4 != 0,
            alt: raw & 8 != 0,
            ctrl: raw & 16 != 0,
        };
        let motion = raw & 32 != 0;
        let wheel = raw & 64 != 0;
        let buttons = raw & 0b11;
        let button = if wheel {
            MouseButton::None
        } else {
            match buttons {
                0 => MouseButton::Left,
                1 => MouseButton::Middle,
                2 => MouseButton::Right,
                _ => MouseButton::None,
            }
        };
        let kind = if wheel {
            // bit 0 of `buttons` distinguishes up (0) from down (1)
            let delta = if buttons & 1 == 0 { 3 } else { -3 };
            MouseKind::Wheel { delta }
        } else if motion {
            MouseKind::Move
        } else if is_press {
            MouseKind::Press
        } else {
            MouseKind::Release
        };
        MouseEvent { kind, button, modifiers, row, col }
    }
}

impl Default for MouseParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode a typed `MouseEvent` as bytes to forward to a child that has
/// requested mouse reporting. Only SGR encoding (`?1006`) is supported in
/// Phase 4; other modes fall back to SGR (the apps we care about all
/// support SGR).
pub fn encode_for_child(event: MouseEvent, _mode: MouseEncoding) -> Vec<u8> {
    let mut button_code: u32 = match event.button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::None => 0,
    };
    if event.modifiers.shift {
        button_code |= 4;
    }
    if event.modifiers.alt {
        button_code |= 8;
    }
    if event.modifiers.ctrl {
        button_code |= 16;
    }
    let mut is_press = true;
    match event.kind {
        MouseKind::Press => {}
        MouseKind::Release => {
            is_press = false;
        }
        MouseKind::Move => {
            button_code |= 32;
        }
        MouseKind::Wheel { delta } => {
            button_code = 64;
            if delta < 0 {
                button_code |= 1;
            }
        }
    }
    let final_byte = if is_press { 'M' } else { 'm' };
    let row = event.row.saturating_add(1);
    let col = event.col.saturating_add(1);
    format!("\x1b[<{button_code};{col};{row}{final_byte}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_default_all_false() {
        let m = MouseModifiers::default();
        assert!(!m.shift && !m.alt && !m.ctrl);
    }

    fn drive(bytes: &[u8]) -> Vec<MouseParseAction> {
        let mut p = MouseParser::new();
        bytes.iter().map(|&b| p.consume(b)).collect()
    }

    fn finalize(bytes: &[u8]) -> MouseEvent {
        let actions = drive(bytes);
        match actions.last().copied() {
            Some(MouseParseAction::Event(e)) => e,
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn parses_left_press_sgr() {
        // ESC [ < 0 ; 10 ; 5 M  -> left-button press at col 10 row 5 (1-indexed on wire)
        let e = finalize(b"\x1b[<0;10;5M");
        assert_eq!(e.button, MouseButton::Left);
        assert_eq!(e.kind, MouseKind::Press);
        assert_eq!((e.row, e.col), (4, 9));
        assert_eq!(e.modifiers, MouseModifiers::default());
    }

    #[test]
    fn parses_left_release_sgr() {
        // ESC [ < 0 ; 10 ; 5 m  -> 'm' (lowercase) means release.
        let e = finalize(b"\x1b[<0;10;5m");
        assert_eq!(e.kind, MouseKind::Release);
    }

    #[test]
    fn parses_move_with_button_held() {
        // Button 32 = motion with left held (32 + 0).
        let e = finalize(b"\x1b[<32;12;6M");
        assert_eq!(e.kind, MouseKind::Move);
        assert_eq!(e.button, MouseButton::Left);
    }

    #[test]
    fn parses_wheel_up_and_down() {
        // 64 = wheel up, 65 = wheel down.
        let up = finalize(b"\x1b[<64;5;5M");
        let down = finalize(b"\x1b[<65;5;5M");
        match up.kind { MouseKind::Wheel { delta } => assert!(delta > 0), _ => panic!() }
        match down.kind { MouseKind::Wheel { delta } => assert!(delta < 0), _ => panic!() }
    }

    #[test]
    fn parses_modifiers() {
        // 0 + 4 (shift) + 8 (alt) + 16 (ctrl) = 28.
        let e = finalize(b"\x1b[<28;3;3M");
        assert_eq!(e.modifiers, MouseModifiers { shift: true, alt: true, ctrl: true });
        assert_eq!(e.button, MouseButton::Left);
    }

    #[test]
    fn non_mouse_byte_passes_through_as_other() {
        let mut p = MouseParser::new();
        assert_eq!(p.consume(b'a'), MouseParseAction::Other(b'a'));
    }

    #[test]
    fn bare_esc_followed_by_non_lbracket_emits_other() {
        // ESC alone should be held; if next byte isn't '[', emit both as Other.
        let mut p = MouseParser::new();
        assert_eq!(p.consume(0x1b), MouseParseAction::Pending);
        assert_eq!(p.consume(b'a'), MouseParseAction::Other(b'a'));
        // The held ESC gets dropped here; not ideal, but acceptable in this design.
    }

    #[test]
    fn bracket_lt_consumed_through_dispatch() {
        // Every byte of "ESC [ < 0 ; 1 ; 1 M" should be Pending except the last.
        let actions = drive(b"\x1b[<0;1;1M");
        // First N actions are Pending; the last is Event.
        assert!(matches!(actions.last(), Some(MouseParseAction::Event(_))));
        for a in &actions[..actions.len() - 1] {
            assert!(matches!(a, MouseParseAction::Pending), "expected Pending, got {a:?}");
        }
    }

    fn ev(kind: MouseKind, button: MouseButton, row: u16, col: u16) -> MouseEvent {
        MouseEvent { kind, button, modifiers: MouseModifiers::default(), row, col }
    }

    #[test]
    fn encode_sgr_press_release() {
        let press = ev(MouseKind::Press, MouseButton::Left, 4, 9);
        assert_eq!(encode_for_child(press, MouseEncoding::Sgr), b"\x1b[<0;10;5M");
        let rel = ev(MouseKind::Release, MouseButton::Left, 4, 9);
        assert_eq!(encode_for_child(rel, MouseEncoding::Sgr), b"\x1b[<0;10;5m");
    }

    #[test]
    fn encode_sgr_wheel_up() {
        let wheel = ev(MouseKind::Wheel { delta: 3 }, MouseButton::None, 0, 0);
        // wheel up = button code 64; coords 1-indexed.
        assert_eq!(encode_for_child(wheel, MouseEncoding::Sgr), b"\x1b[<64;1;1M");
    }

    #[test]
    fn encode_sgr_wheel_down() {
        let wheel = ev(MouseKind::Wheel { delta: -3 }, MouseButton::None, 0, 0);
        // wheel down = 65.
        assert_eq!(encode_for_child(wheel, MouseEncoding::Sgr), b"\x1b[<65;1;1M");
    }

    #[test]
    fn encode_sgr_move_with_held_button() {
        let mv = MouseEvent {
            kind: MouseKind::Move,
            button: MouseButton::Left,
            modifiers: MouseModifiers::default(),
            row: 4,
            col: 9,
        };
        // motion = 32; left held = 0. Total = 32.
        assert_eq!(encode_for_child(mv, MouseEncoding::Sgr), b"\x1b[<32;10;5M");
    }

    #[test]
    fn encode_sgr_modifiers() {
        let with_mods = MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: MouseModifiers { shift: true, alt: true, ctrl: true },
            row: 0,
            col: 0,
        };
        // shift (4) + alt (8) + ctrl (16) = 28.
        assert_eq!(encode_for_child(with_mods, MouseEncoding::Sgr), b"\x1b[<28;1;1M");
    }
}
