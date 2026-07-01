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
    /// A wheel notch. For a vertical wheel (`horizontal == false`) positive
    /// `delta` = up, negative = down; for a horizontal wheel positive = left,
    /// negative = right. The axis is kept distinct so a horizontal scroll isn't
    /// mistaken for a vertical one when scrolling scrollback or forwarding to a
    /// mouse-reporting child.
    Wheel { delta: i16, horizontal: bool },
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseParseAction {
    Pending,
    Event(MouseEvent),
    /// Byte was not part of a mouse sequence; route elsewhere.
    Other(u8),
    /// A partial sequence was abandoned; the held bytes are returned so
    /// the caller can route them through a different parser (e.g. keys).
    BailedBytes(Vec<u8>),
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
    held: Vec<u8>,
}

impl MouseParser {
    pub fn new() -> Self {
        Self {
            state: ParseState::Idle,
            params: [0; 3],
            held: Vec::with_capacity(16),
        }
    }

    /// Feed one byte. See `MouseParseAction` for return semantics.
    pub fn consume(&mut self, byte: u8) -> MouseParseAction {
        match self.state {
            ParseState::Idle => {
                if byte == 0x1b {
                    self.held.push(byte);
                    self.state = ParseState::SawEsc;
                    MouseParseAction::Pending
                } else {
                    // held is empty here, so there are no buffered bytes to recover.
                    MouseParseAction::Other(byte)
                }
            }
            ParseState::SawEsc => {
                if byte == b'[' {
                    self.held.push(byte);
                    self.state = ParseState::SawBracket;
                    MouseParseAction::Pending
                } else {
                    // Not a CSI sequence at all; return held ESC + current byte.
                    self.bail_with_byte(byte)
                }
            }
            ParseState::SawBracket => {
                if byte == b'<' {
                    self.held.push(byte);
                    self.state = ParseState::SawLt;
                    MouseParseAction::Pending
                } else {
                    // CSI without `<` isn't an SGR mouse sequence. Bail and
                    // return ESC + '[' + current byte so the key parser sees them.
                    self.bail_with_byte(byte)
                }
            }
            ParseState::SawLt => {
                // The '<' is already in held. Push byte and enter AccumParam.
                self.held.push(byte);
                self.state = ParseState::AccumParam(0);
                self.accum_param(byte, 0)
            }
            ParseState::AccumParam(idx) => {
                self.held.push(byte);
                self.accum_param(byte, idx)
            }
        }
    }

    /// Process a byte while in `AccumParam(idx)`. Called only after the byte
    /// has already been pushed to `held`.
    fn accum_param(&mut self, byte: u8, idx: u8) -> MouseParseAction {
        // invariant: FSM only ever sets idx to 0, 1, or 2
        debug_assert!(idx <= 2, "AccumParam idx out of range");
        if byte.is_ascii_digit() {
            // saturating on BOTH ops: a long digit run from a buggy/malicious
            // client would overflow the trailing plain `+` once the mul
            // saturated to u32::MAX (debug panic / release wrap). build_event
            // clamps into u16 anyway, so saturating here is harmless.
            self.params[idx as usize] = self.params[idx as usize]
                .saturating_mul(10)
                .saturating_add(u32::from(byte - b'0'));
            MouseParseAction::Pending
        } else if byte == b';' {
            if idx >= 2 {
                // Too many params; bail and return all buffered bytes including ';'.
                self.bail_already_pushed()
            } else {
                self.state = ParseState::AccumParam(idx + 1);
                MouseParseAction::Pending
            }
        } else if byte == b'M' || byte == b'm' {
            let evt = self.build_event(byte == b'M');
            self.reset_state();
            // Discard held (successfully parsed, so nothing needs re-routing).
            self.held.clear();
            MouseParseAction::Event(evt)
        } else {
            // An unexpected byte mid-sequence means the wire is garbage, so bail.
            self.bail_already_pushed()
        }
    }

    /// Whether the parser is mid-sequence (holding bytes that have not yet been
    /// resolved to a mouse event or bailed). A lone `\x1b` parks here as
    /// `SawEsc`, so the connection loop must treat a mid-sequence mouse parser
    /// as "pending" too (the key parser hasn't seen the ESC yet).
    pub const fn is_mid_sequence(&self) -> bool {
        !matches!(self.state, ParseState::Idle)
    }

    /// Abandon any partial sequence and return the held bytes so the caller can
    /// re-route them (e.g. into the key parser). Used by the Esc idle-flush:
    /// the lone `\x1b` parks in this parser's `held`, never reaching the key
    /// parser, so flushing the key parser alone would never see it. Returns the
    /// held bytes (empty when idle).
    pub fn flush(&mut self) -> Vec<u8> {
        let bytes = std::mem::take(&mut self.held);
        self.reset_state();
        bytes
    }

    /// Push `byte` to held, drain held into `BailedBytes`, and reset state.
    fn bail_with_byte(&mut self, byte: u8) -> MouseParseAction {
        self.held.push(byte);
        self.bail_already_pushed()
    }

    /// Drain held into `BailedBytes` and reset state. Use when the current
    /// byte is already in `held`.
    fn bail_already_pushed(&mut self) -> MouseParseAction {
        let bytes = std::mem::take(&mut self.held);
        self.reset_state();
        MouseParseAction::BailedBytes(bytes)
    }

    const fn reset_state(&mut self) {
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
            // Wheel codes 64=up, 65=down, 66=left, 67=right. Bit 1 of `buttons`
            // selects the axis (horizontal), bit 0 the direction (up/left = 0).
            let horizontal = buttons & 2 != 0;
            let delta = if buttons & 1 == 0 { 3 } else { -3 };
            MouseKind::Wheel { delta, horizontal }
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

/// Encode a typed `MouseEvent` for forwarding to a child, in the encoding the
/// pane negotiated. SGR (`?1006`) uses the extended `\e[<…` form; X10 / normal /
/// any-event panes use the legacy `\e[M` form (button, col, row each +32).
pub fn encode_for_child(event: MouseEvent, mode: MouseEncoding) -> Vec<u8> {
    let mut button_code: u32 = match event.button {
        MouseButton::Left | MouseButton::None => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
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
        MouseKind::Wheel { delta, horizontal } => {
            // OR (not assign) so the modifier bits set above survive; base 64 =
            // vertical wheel, 66 = horizontal; bit 0 flips up→down / left→right.
            button_code |= if horizontal { 66 } else { 64 };
            // `delta < 0` and `delta <= 0` are observationally identical here
            // because the parser always assigns delta = ±3, never 0. Both
            // `prop_mouse` and the parser confirm delta ∈ {-3, +3}.
            if delta < 0 {
                button_code |= 1;
            }
        }
    }
    let row = event.row.saturating_add(1);
    let col = event.col.saturating_add(1);

    match mode {
        MouseEncoding::Sgr => {
            let final_byte = if is_press { 'M' } else { 'm' };
            format!("\x1b[<{button_code};{col};{row}{final_byte}").into_bytes()
        }
        MouseEncoding::X10 | MouseEncoding::ButtonEvent | MouseEncoding::AnyEvent => {
            // Legacy X10/normal: ESC [ M Cb Cx Cy, each byte offset by 32. A
            // release has no button identity, it is reported as code 3.
            let cb = if is_press { button_code } else { 3 };
            let enc = |v: u32| -> u8 {
                let n = v.saturating_add(32).min(255);
                // invariant: clamped to <= 255 above.
                u8::try_from(n).unwrap_or(255)
            };
            vec![0x1b, b'[', b'M', enc(button_code_low(cb)), enc(u32::from(col)), enc(u32::from(row))]
        }
    }
}

/// Clamp the legacy button code into the single-byte (0..=223) range before the
/// +32 offset is applied.
fn button_code_low(cb: u32) -> u32 {
    cb.min(223)
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
        let mut actions = drive(bytes);
        match actions.pop() {
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
        match up.kind { MouseKind::Wheel { delta, .. } => assert!(delta > 0), _ => panic!() }
        match down.kind { MouseKind::Wheel { delta, .. } => assert!(delta < 0), _ => panic!() }
    }

    #[test]
    fn parses_modifiers() {
        // 0 + 4 (shift) + 8 (alt) + 16 (ctrl) = 28.
        let e = finalize(b"\x1b[<28;3;3M");
        assert_eq!(e.modifiers, MouseModifiers { shift: true, alt: true, ctrl: true });
        assert_eq!(e.button, MouseButton::Left);
    }

    #[test]
    fn oversized_param_clamps_without_panic() {
        // A buggy/malicious client sends an 11-digit coordinate. The old plain
        // `+` overflowed once `saturating_mul` hit `u32::MAX` (debug panic /
        // release wrap), so saturating arithmetic has to clamp into the `u16`
        // range instead of panicking.
        let e = finalize(b"\x1b[<0;99999999999;1M");
        assert_eq!(e.col, u16::MAX);
        assert_eq!(e.row, 0); // wire "1" -> 0 (1-indexed)
    }

    #[test]
    fn non_mouse_byte_passes_through_as_other() {
        let mut p = MouseParser::new();
        assert_eq!(p.consume(b'a'), MouseParseAction::Other(b'a'));
    }

    #[test]
    fn bare_esc_followed_by_non_lbracket_returns_bailed_bytes() {
        // ESC alone should be held; if next byte isn't '[', return ESC + byte.
        let mut p = MouseParser::new();
        assert_eq!(p.consume(0x1b), MouseParseAction::Pending);
        match p.consume(b'a') {
            MouseParseAction::BailedBytes(bytes) => assert_eq!(bytes, vec![0x1b, b'a']),
            other => panic!("expected BailedBytes, got {other:?}"),
        }
    }

    #[test]
    fn flush_drains_held_partial_and_resets() {
        let mut p = MouseParser::new();
        assert!(!p.is_mid_sequence());
        assert_eq!(p.consume(0x1b), MouseParseAction::Pending);
        assert_eq!(p.consume(b'['), MouseParseAction::Pending);
        assert!(p.is_mid_sequence(), "ESC [ is a partial mouse sequence");
        assert_eq!(p.flush(), vec![0x1b, b'[']);
        assert!(!p.is_mid_sequence(), "flush reset the parser");
        assert_eq!(p.flush(), Vec::<u8>::new(), "second flush is empty");
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

    #[test]
    fn esc_bracket_then_non_lt_returns_bailed_bytes() {
        // Arrow keys: ESC [ A, so the parser sees ESC [ and then 'A' (not '<').
        let mut p = MouseParser::new();
        assert_eq!(p.consume(0x1b), MouseParseAction::Pending);
        assert_eq!(p.consume(b'['), MouseParseAction::Pending);
        match p.consume(b'A') {
            MouseParseAction::BailedBytes(bytes) => assert_eq!(bytes, vec![0x1b, b'[', b'A']),
            other => panic!("expected BailedBytes, got {other:?}"),
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
        let wheel = ev(MouseKind::Wheel { delta: 3, horizontal: false }, MouseButton::None, 0, 0);
        // wheel up = button code 64; coords 1-indexed.
        assert_eq!(encode_for_child(wheel, MouseEncoding::Sgr), b"\x1b[<64;1;1M");
    }

    #[test]
    fn encode_sgr_wheel_down() {
        let wheel = ev(MouseKind::Wheel { delta: -3, horizontal: false }, MouseButton::None, 0, 0);
        // wheel down = 65.
        assert_eq!(encode_for_child(wheel, MouseEncoding::Sgr), b"\x1b[<65;1;1M");
    }

    #[test]
    fn wheel_encoding_preserves_modifiers() {
        // ctrl+wheel-up: 64 (wheel) | 16 (ctrl) = 80. Regression: the Wheel arm
        // used to ASSIGN 64, wiping the modifier bits.
        let ev = MouseEvent {
            kind: MouseKind::Wheel { delta: 3, horizontal: false },
            button: MouseButton::None,
            modifiers: MouseModifiers { ctrl: true, ..Default::default() },
            row: 0,
            col: 0,
        };
        assert_eq!(encode_for_child(ev, MouseEncoding::Sgr), b"\x1b[<80;1;1M");
    }

    #[test]
    fn horizontal_wheel_round_trips_distinctly() {
        // Decode 66/67 as horizontal (not vertical), and re-encode back.
        let left = drive(b"\x1b[<66;10;5M");
        match left.last() {
            Some(MouseParseAction::Event(e)) => {
                assert_eq!(e.kind, MouseKind::Wheel { delta: 3, horizontal: true });
            }
            other => panic!("expected horizontal wheel, got {other:?}"),
        }
        let l = ev(MouseKind::Wheel { delta: 3, horizontal: true }, MouseButton::None, 4, 9);
        assert_eq!(encode_for_child(l, MouseEncoding::Sgr), b"\x1b[<66;10;5M");
        let r = ev(MouseKind::Wheel { delta: -3, horizontal: true }, MouseButton::None, 4, 9);
        assert_eq!(encode_for_child(r, MouseEncoding::Sgr), b"\x1b[<67;10;5M");
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

    #[test]
    fn encode_legacy_normal_press_uses_csi_m_form() {
        // Under ?1000 (no ?1006) a left press emits the legacy X10/normal form:
        // ESC [ M <b+32> <col+32> <row+32>.
        let press = ev(MouseKind::Press, MouseButton::Left, 4, 9);
        // button 0 -> 32 (space); col 9 -> 1-indexed 10 -> +32 = 42 ('*');
        // row 4 -> 5 -> +32 = 37 ('%').
        assert_eq!(encode_for_child(press, MouseEncoding::ButtonEvent), b"\x1b[M \x2a\x25");
    }

    #[test]
    fn encode_legacy_release_is_button_3() {
        // Legacy release has no button identity: code 3 -> +32 = '#'.
        let rel = ev(MouseKind::Release, MouseButton::Left, 4, 9);
        assert_eq!(encode_for_child(rel, MouseEncoding::ButtonEvent), b"\x1b[M#\x2a\x25");
    }

    #[test]
    fn encode_legacy_wheel_up() {
        // Wheel up legacy: code 64 -> +32 = 96 ('`'); coords 1,1 -> 33 ('!').
        let wheel = ev(MouseKind::Wheel { delta: 3, horizontal: false }, MouseButton::None, 0, 0);
        assert_eq!(encode_for_child(wheel, MouseEncoding::ButtonEvent), b"\x1b[M`\x21\x21");
    }

    #[test]
    fn encode_sgr_still_emits_lt_form() {
        // Under ?1006 the SGR form is unchanged.
        let press = ev(MouseKind::Press, MouseButton::Left, 4, 9);
        assert_eq!(encode_for_child(press, MouseEncoding::Sgr), b"\x1b[<0;10;5M");
    }
}
