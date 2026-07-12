//! Byte stream → `KeyEvent` state machine.
//!
//! Recognizes the standard VT/xterm/SS3 key encodings plus kitty CSI-u
//! sequences (extended with colon sub-fields + protocol scoping). On bail,
//! returns the buffered bytes verbatim so callers can pass them through to the
//! shell.

use std::mem;

use plexy_glass_mux::{Direction, Key, KeyEvent, KeyEventKind, Modifiers};

#[derive(Debug, Clone)]
pub enum KeyParseOutput {
    Pending,
    Event { event: KeyEvent, bytes: Vec<u8> },
    Bytes(Vec<u8>),
}

/// Which keyboard protocol the *outer* terminal negotiated for this client.
/// Threaded in so decode is deterministic instead of "accept all three at once".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardProtocol {
    Legacy,
    ModifyOtherKeys,
    Kitty,
    /// Unknown (pre-handshake / older peer): permissive, so we accept Kitty
    /// CSI-u *and* legacy, exactly as the parser did before this change.
    #[default]
    Permissive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    SawEsc,
    SawCsi,   // ESC [
    AccumCsi, // ESC [ <params> ; ...
    SawSs3,   // ESC O
}

/// One CSI parameter: a primary value plus its colon sub-fields.
/// `\e[105:73;6u` parses as params `[ {sub:[105,73]}, {sub:[6]} ]`.
#[derive(Debug, Clone, Default)]
struct Param {
    /// Each colon-separated field; `sub[0]` is the primary value. `None` = an
    /// empty field (e.g. `2::5`) so callers can default it.
    sub: Vec<Option<u32>>,
}

impl Param {
    fn primary(&self) -> u32 {
        self.sub.first().copied().flatten().unwrap_or(0)
    }
    fn sub_at(&self, i: usize) -> Option<u32> {
        self.sub.get(i).copied().flatten()
    }
}

pub struct KeyParser {
    state: State,
    buf: Vec<u8>,
    params: Vec<Param>,
    /// Accumulator for the colon sub-field currently being read.
    cur_sub: Option<u32>,
    cur_sub_seen: bool,
    cur_param_started: bool,
    last_param_flushed: bool,
    protocol: KeyboardProtocol,
}

impl Default for KeyParser {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyParser {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            buf: Vec::with_capacity(16),
            params: Vec::new(),
            cur_sub: None,
            cur_sub_seen: false,
            cur_param_started: false,
            last_param_flushed: false,
            protocol: KeyboardProtocol::Permissive,
        }
    }

    /// Build a parser whose decode is scoped to a negotiated protocol.
    #[must_use]
    pub const fn with_protocol(mut self, protocol: KeyboardProtocol) -> Self {
        self.protocol = protocol;
        self
    }

    pub fn consume(&mut self, byte: u8) -> KeyParseOutput {
        self.buf.push(byte);
        match self.state {
            State::Idle => self.step_idle(byte),
            State::SawEsc => self.step_saw_esc(byte),
            State::SawCsi | State::AccumCsi => self.step_csi(byte),
            State::SawSs3 => self.step_ss3(byte),
        }
    }

    /// Whether the parser is mid-sequence (has buffered bytes awaiting more
    /// input), i.e. not `Idle`. The connection loop uses this to decide
    /// whether to arm the Esc idle-flush timer (only meaningful while a lone
    /// `\x1b` or a partial CSI/SS3 is pending).
    pub fn is_mid_sequence(&self) -> bool {
        self.state != State::Idle
    }

    pub fn flush(&mut self) -> Option<KeyParseOutput> {
        if self.state == State::SawEsc {
            let bytes = mem::take(&mut self.buf);
            self.reset_state();
            return Some(KeyParseOutput::Event {
                event: KeyEvent::plain(Key::Escape),
                bytes,
            });
        }
        None
    }

    fn step_idle(&mut self, byte: u8) -> KeyParseOutput {
        match byte {
            0x1b => {
                self.state = State::SawEsc;
                KeyParseOutput::Pending
            }
            0x09 => self.emit_simple(Key::Tab, Modifiers::empty()),
            0x0d => self.emit_simple(Key::Enter, Modifiers::empty()),
            0x7f => self.emit_simple(Key::Backspace, Modifiers::empty()),
            0x00 => self.emit_simple(Key::Char(' '), Modifiers::CTRL),
            0x01..=0x1a => {
                let ch = (byte + b'`') as char;
                self.emit_simple(Key::Char(ch), Modifiers::CTRL)
            }
            byte if (0x20..=0x7e).contains(&byte) => {
                self.emit_simple(Key::Char(byte as char), Modifiers::empty())
            }
            _ => {
                let bytes = mem::take(&mut self.buf);
                self.reset_state();
                KeyParseOutput::Bytes(bytes)
            }
        }
    }

    fn step_saw_esc(&mut self, byte: u8) -> KeyParseOutput {
        match byte {
            b'[' => {
                self.state = State::SawCsi;
                KeyParseOutput::Pending
            }
            b'O' => {
                self.state = State::SawSs3;
                KeyParseOutput::Pending
            }
            byte if (0x20..=0x7e).contains(&byte) => {
                let bytes = mem::take(&mut self.buf);
                self.reset_state();
                KeyParseOutput::Event {
                    event: KeyEvent::new(Key::Char(byte as char), Modifiers::ALT),
                    bytes,
                }
            }
            _ => self.bail(),
        }
    }

    fn step_csi(&mut self, byte: u8) -> KeyParseOutput {
        if byte.is_ascii_digit() {
            self.state = State::AccumCsi;
            self.cur_param_started = true;
            self.cur_sub_seen = true;
            let acc = self
                .cur_sub
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(u32::from(byte - b'0'));
            self.cur_sub = Some(acc);
            return KeyParseOutput::Pending;
        }
        if byte == b':' {
            self.state = State::AccumCsi;
            self.cur_param_started = true;
            self.push_sub();
            return KeyParseOutput::Pending;
        }
        if byte == b';' {
            self.state = State::AccumCsi;
            self.flush_param();
            return KeyParseOutput::Pending;
        }
        if self.cur_param_started {
            self.flush_param();
        }
        self.dispatch_csi(byte)
    }

    /// Close the current colon sub-field onto the current param.
    fn push_sub(&mut self) {
        self.cur_param_started = true;
        if self.params.is_empty() || self.last_param_flushed {
            self.params.push(Param::default());
            self.last_param_flushed = false;
        }
        // invariant: just ensured at least one param exists above.
        let p = self.params.last_mut().expect("param pushed above");
        p.sub.push(if self.cur_sub_seen {
            self.cur_sub
        } else {
            None
        });
        self.cur_sub = None;
        self.cur_sub_seen = false;
    }

    /// Close the current param entirely (on `;` or the final byte).
    fn flush_param(&mut self) {
        self.push_sub();
        self.last_param_flushed = true;
    }

    fn step_ss3(&mut self, byte: u8) -> KeyParseOutput {
        let event = match byte {
            b'P' => KeyEvent::plain(Key::Function(1)),
            b'Q' => KeyEvent::plain(Key::Function(2)),
            b'R' => KeyEvent::plain(Key::Function(3)),
            b'S' => KeyEvent::plain(Key::Function(4)),
            b'H' => KeyEvent::plain(Key::Home),
            b'F' => KeyEvent::plain(Key::End),
            b'M' => KeyEvent::plain(Key::KeypadEnter),
            b'A' => KeyEvent::plain(Key::Arrow(Direction::Up)),
            b'B' => KeyEvent::plain(Key::Arrow(Direction::Down)),
            b'C' => KeyEvent::plain(Key::Arrow(Direction::Right)),
            b'D' => KeyEvent::plain(Key::Arrow(Direction::Left)),
            _ => return self.bail(),
        };
        let bytes = mem::take(&mut self.buf);
        self.reset_state();
        KeyParseOutput::Event { event, bytes }
    }

    fn dispatch_csi(&mut self, byte: u8) -> KeyParseOutput {
        let params = mem::take(&mut self.params);
        match self.decode_csi(byte, &params) {
            Some(event) => {
                let bytes = mem::take(&mut self.buf);
                self.reset_state();
                KeyParseOutput::Event { event, bytes }
            }
            None => self.bail(),
        }
    }

    fn decode_csi(&self, byte: u8, params: &[Param]) -> Option<KeyEvent> {
        let p0 = params.first().map(Param::primary);
        let p1 = params.get(1).map(Param::primary);
        // modifyOtherKeys 27-form: `CSI 27 ; mods ; code ~`, the key whose codepoint
        // is the THIRD param, modifiers from the SECOND. Mapped through the same
        // codepoint→KeyEvent path as CSI-u so it is symmetric with the encoder
        // (`encode::modify_other_keys_bytes` emits this form). Decoded before the
        // generic tilde arm so `27` is not mistaken for a (nonexistent)
        // function-key number.
        if byte == b'~' && p0 == Some(27) {
            return Self::decode_modify_other_keys(params);
        }
        match (byte, p0, p1) {
            (b'A', None, None) => Some(KeyEvent::plain(Key::Arrow(Direction::Up))),
            (b'B', None, None) => Some(KeyEvent::plain(Key::Arrow(Direction::Down))),
            (b'C', None, None) => Some(KeyEvent::plain(Key::Arrow(Direction::Right))),
            (b'D', None, None) => Some(KeyEvent::plain(Key::Arrow(Direction::Left))),
            (b'H', None, None) => Some(KeyEvent::plain(Key::Home)),
            (b'F', None, None) => Some(KeyEvent::plain(Key::End)),
            (b'~', Some(n), None) => key_from_tilde(n).map(KeyEvent::plain),
            (b'Z', None, None) => Some(KeyEvent::new(Key::Tab, Modifiers::SHIFT)),
            (b'A', Some(1), Some(m)) => Some(KeyEvent::new(
                Key::Arrow(Direction::Up),
                decode_xterm_mods(m),
            )),
            (b'B', Some(1), Some(m)) => Some(KeyEvent::new(
                Key::Arrow(Direction::Down),
                decode_xterm_mods(m),
            )),
            (b'C', Some(1), Some(m)) => Some(KeyEvent::new(
                Key::Arrow(Direction::Right),
                decode_xterm_mods(m),
            )),
            (b'D', Some(1), Some(m)) => Some(KeyEvent::new(
                Key::Arrow(Direction::Left),
                decode_xterm_mods(m),
            )),
            (b'H', Some(1), Some(m)) => Some(KeyEvent::new(Key::Home, decode_xterm_mods(m))),
            (b'F', Some(1), Some(m)) => Some(KeyEvent::new(Key::End, decode_xterm_mods(m))),
            (b'P', Some(1), Some(m)) => Some(KeyEvent::new(Key::Function(1), decode_xterm_mods(m))),
            (b'Q', Some(1), Some(m)) => Some(KeyEvent::new(Key::Function(2), decode_xterm_mods(m))),
            (b'R', Some(1), Some(m)) => Some(KeyEvent::new(Key::Function(3), decode_xterm_mods(m))),
            (b'S', Some(1), Some(m)) => Some(KeyEvent::new(Key::Function(4), decode_xterm_mods(m))),
            (b'~', Some(n), Some(m)) => {
                key_from_tilde(n).map(|k| KeyEvent::new(k, decode_xterm_mods(m)))
            }
            (b'u', Some(_), _) => self.decode_kitty_u(params),
            _ => None,
        }
    }

    fn decode_kitty_u(&self, params: &[Param]) -> Option<KeyEvent> {
        // CSI-u isn't the wire form for a Legacy or modifyOtherKeys client, so
        // those strict modes ignore it (bail to raw bytes). Kitty and Permissive
        // accept it.
        if matches!(
            self.protocol,
            KeyboardProtocol::ModifyOtherKeys | KeyboardProtocol::Legacy
        ) {
            return None;
        }
        let key_param = params.first()?;
        let code = key_param.primary();
        // Alternates: code:shifted:base
        let shifted = key_param.sub_at(1).and_then(char::from_u32);
        let base_layout = key_param.sub_at(2).and_then(char::from_u32);
        // Modifiers + event type live in param 2: mods[:event]. The wire mods
        // param is 1-based; `.max(1)` only guards a malformed 0 (decode_xterm_mods
        // already maps both 0 and 1 to "no modifiers").
        let mods = params
            .get(1)
            .map_or_else(Modifiers::empty, |p| decode_xterm_mods(p.primary().max(1)));
        let kind = match params.get(1).and_then(|p| p.sub_at(1)).unwrap_or(1) {
            2 => KeyEventKind::Repeat,
            3 => KeyEventKind::Release,
            _ => KeyEventKind::Press,
        };
        // Associated text: param 3, colon-separated codepoints.
        let text = params.get(2).and_then(|p| {
            let s: String = p
                .sub
                .iter()
                .filter_map(|v| v.and_then(char::from_u32))
                .collect();
            (!s.is_empty()).then(|| smol_str::SmolStr::new(s))
        });
        let mut ev = kitty_key(code, mods)?;
        ev.kind = kind;
        ev.text = text;
        ev.shifted = shifted;
        ev.base_layout = base_layout;
        Some(ev)
    }

    /// Decode the modifyOtherKeys 27-form `CSI 27 ; mods ; code ~`. Params are
    /// `[27, mods, code]`; the codepoint is param 3, modifiers param 2. Maps
    /// through `kitty_key` so the resulting `KeyEvent` is identical to what the
    /// CSI-u path produces for the same key, which keeps parse symmetric with
    /// the encoder. Malformed (missing mods/code, non-mappable codepoint) →
    /// `None` (the caller's bail). Accepted in every protocol scope: the 27-form
    /// is the modifyOtherKeys wire form, and a permissive parser must accept it.
    fn decode_modify_other_keys(params: &[Param]) -> Option<KeyEvent> {
        let mods = decode_xterm_mods(params.get(1)?.primary().max(1));
        let code = params.get(2)?.primary();
        kitty_key(code, mods)
    }

    fn emit_simple(&mut self, key: Key, mods: Modifiers) -> KeyParseOutput {
        let bytes = mem::take(&mut self.buf);
        self.reset_state();
        KeyParseOutput::Event {
            event: KeyEvent::new(key, mods),
            bytes,
        }
    }

    fn bail(&mut self) -> KeyParseOutput {
        let bytes = mem::take(&mut self.buf);
        self.reset_state();
        KeyParseOutput::Bytes(bytes)
    }

    fn reset_state(&mut self) {
        self.state = State::Idle;
        self.params.clear();
        self.cur_sub = None;
        self.cur_sub_seen = false;
        self.cur_param_started = false;
        self.last_param_flushed = false;
    }
}

pub const fn decode_xterm_mods(raw: u32) -> Modifiers {
    // Wire modifier param = 1 + bitset:
    //   1=shift 2=alt 4=ctrl 8=super 16=hyper 32=meta 64=caps_lock 128=num_lock
    let bits = (raw.saturating_sub(1) & 0xFF) as u8;
    Modifiers::from_bits_truncate(bits)
}

fn kitty_key(code: u32, mods: Modifiers) -> Option<KeyEvent> {
    // Special control codepoints first.
    match code {
        9 => return Some(KeyEvent::new(Key::Tab, mods)),
        13 => return Some(KeyEvent::new(Key::Enter, mods)),
        27 => return Some(KeyEvent::new(Key::Escape, mods)),
        127 => return Some(KeyEvent::new(Key::Backspace, mods)),
        _ => {}
    }
    // Named functional keys (arrows, Home/End/Insert/Delete, PageUp/Down,
    // F1-F12) are NOT carried in the Kitty private-use range: under the Kitty
    // protocol they keep their legacy CSI/SS3 final-byte forms, already handled
    // by `step_ss3` / `decode_csi` / `key_from_tilde`. The Kitty PUA (U+E000.. =
    // 57344+) is reserved for keys with no legacy form (Caps/Scroll/Num Lock,
    // Print Screen, Pause, Menu, F13-F35, the keypad). `Key` has no variants for
    // those, so they fall through to `Char` (inert in keymaps; the raw bytes
    // still pass through to the child verbatim). An earlier hand-written table
    // both invented unassigned codepoints and, worse, mismapped real Kitty
    // assignments (57358=CapsLock, 57359=ScrollLock, 57361=PrintScreen) onto
    // Right/End/Insert, silently turning those keys into navigation keystrokes;
    // that masquerade is removed.
    char::from_u32(code).map(|c| KeyEvent::new(Key::Char(c), mods))
}

const fn key_from_tilde(n: u32) -> Option<Key> {
    match n {
        2 => Some(Key::Insert),
        3 => Some(Key::Delete),
        5 => Some(Key::PageUp),
        6 => Some(Key::PageDown),
        7 => Some(Key::Home),
        8 => Some(Key::End),
        11 => Some(Key::Function(1)),
        12 => Some(Key::Function(2)),
        13 => Some(Key::Function(3)),
        14 => Some(Key::Function(4)),
        15 => Some(Key::Function(5)),
        17 => Some(Key::Function(6)),
        18 => Some(Key::Function(7)),
        19 => Some(Key::Function(8)),
        20 => Some(Key::Function(9)),
        21 => Some(Key::Function(10)),
        23 => Some(Key::Function(11)),
        24 => Some(Key::Function(12)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(bytes: &[u8]) -> Vec<KeyParseOutput> {
        let mut p = KeyParser::new();
        bytes.iter().map(|&b| p.consume(b)).collect()
    }

    fn last_event(bytes: &[u8]) -> KeyEvent {
        let mut p = KeyParser::new();
        let mut last = None;
        for &b in bytes {
            if let KeyParseOutput::Event { event, .. } = p.consume(b) {
                last = Some(event);
            }
        }
        last.unwrap_or_else(|| panic!("no event for bytes {bytes:?}"))
    }

    #[test]
    fn raw_printable_emits_char() {
        let e = last_event(b"a");
        assert_eq!(e.key, Key::Char('a'));
        assert!(e.mods.is_empty());
    }

    #[test]
    fn raw_tab_emits_tab() {
        let e = last_event(b"\x09");
        assert_eq!(e.key, Key::Tab);
        assert!(e.mods.is_empty());
    }

    #[test]
    fn raw_enter_emits_enter() {
        let e = last_event(b"\x0d");
        assert_eq!(e.key, Key::Enter);
    }

    #[test]
    fn raw_backspace_emits_backspace() {
        let e = last_event(b"\x7f");
        assert_eq!(e.key, Key::Backspace);
    }

    #[test]
    fn ctrl_letter_decodes_to_char_plus_ctrl() {
        let e = last_event(&[0x01]);
        assert_eq!(e.key, Key::Char('a'));
        assert_eq!(e.mods, Modifiers::CTRL);
    }

    #[test]
    fn csi_arrow_up() {
        let e = last_event(b"\x1b[A");
        assert_eq!(e.key, Key::Arrow(Direction::Up));
        assert!(e.mods.is_empty());
    }

    #[test]
    fn csi_arrow_down_left_right() {
        assert_eq!(last_event(b"\x1b[B").key, Key::Arrow(Direction::Down));
        assert_eq!(last_event(b"\x1b[C").key, Key::Arrow(Direction::Right));
        assert_eq!(last_event(b"\x1b[D").key, Key::Arrow(Direction::Left));
    }

    #[test]
    fn ss3_f1_through_f4() {
        assert_eq!(last_event(b"\x1bOP").key, Key::Function(1));
        assert_eq!(last_event(b"\x1bOQ").key, Key::Function(2));
        assert_eq!(last_event(b"\x1bOR").key, Key::Function(3));
        assert_eq!(last_event(b"\x1bOS").key, Key::Function(4));
    }

    #[test]
    fn csi_tilde_f5_f12() {
        assert_eq!(last_event(b"\x1b[15~").key, Key::Function(5));
        assert_eq!(last_event(b"\x1b[17~").key, Key::Function(6));
        assert_eq!(last_event(b"\x1b[24~").key, Key::Function(12));
    }

    #[test]
    fn csi_tilde_navigation_keys() {
        assert_eq!(last_event(b"\x1b[2~").key, Key::Insert);
        assert_eq!(last_event(b"\x1b[3~").key, Key::Delete);
        assert_eq!(last_event(b"\x1b[5~").key, Key::PageUp);
        assert_eq!(last_event(b"\x1b[6~").key, Key::PageDown);
    }

    #[test]
    fn csi_home_end_short_form() {
        assert_eq!(last_event(b"\x1b[H").key, Key::Home);
        assert_eq!(last_event(b"\x1b[F").key, Key::End);
    }

    #[test]
    fn ss3_home_end_arrow_keys() {
        assert_eq!(last_event(b"\x1bOH").key, Key::Home);
        assert_eq!(last_event(b"\x1bOF").key, Key::End);
        assert_eq!(last_event(b"\x1bOA").key, Key::Arrow(Direction::Up));
    }

    #[test]
    fn esc_alone_flushes_to_escape() {
        let mut p = KeyParser::new();
        assert!(matches!(p.consume(0x1b), KeyParseOutput::Pending));
        let flushed = p.flush().expect("flush should emit Escape");
        match flushed {
            KeyParseOutput::Event { event, .. } => assert_eq!(event.key, Key::Escape),
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn esc_plus_printable_decodes_to_alt_char() {
        let e = last_event(b"\x1ba");
        assert_eq!(e.key, Key::Char('a'));
        assert_eq!(e.mods, Modifiers::ALT);
    }

    #[test]
    fn csi_shift_tab() {
        let e = last_event(b"\x1b[Z");
        assert_eq!(e.key, Key::Tab);
        assert_eq!(e.mods, Modifiers::SHIFT);
    }

    #[test]
    fn unrecognized_csi_bails_with_original_bytes() {
        let outputs = drive(b"\x1b[Q");
        let last = outputs.last().unwrap();
        match last {
            KeyParseOutput::Bytes(bs) => assert_eq!(bs.as_slice(), b"\x1b[Q"),
            _ => panic!("expected Bytes, got {last:?}"),
        }
    }

    #[test]
    fn csi_ctrl_left() {
        let e = last_event(b"\x1b[1;5D");
        assert_eq!(e.key, Key::Arrow(Direction::Left));
        assert_eq!(e.mods, Modifiers::CTRL);
    }

    #[test]
    fn csi_shift_alt_up() {
        let e = last_event(b"\x1b[1;4A");
        assert_eq!(e.key, Key::Arrow(Direction::Up));
        assert_eq!(e.mods, Modifiers::SHIFT | Modifiers::ALT);
    }

    #[test]
    fn csi_ctrl_alt_shift_right() {
        let e = last_event(b"\x1b[1;8C");
        assert_eq!(e.key, Key::Arrow(Direction::Right));
        assert_eq!(e.mods, Modifiers::CTRL | Modifiers::ALT | Modifiers::SHIFT);
    }

    #[test]
    fn csi_modified_f1() {
        let e = last_event(b"\x1b[1;5P");
        assert_eq!(e.key, Key::Function(1));
        assert_eq!(e.mods, Modifiers::CTRL);
    }

    #[test]
    fn csi_modified_pageup() {
        let e = last_event(b"\x1b[5;5~");
        assert_eq!(e.key, Key::PageUp);
        assert_eq!(e.mods, Modifiers::CTRL);
    }

    #[test]
    fn csi_modified_delete() {
        let e = last_event(b"\x1b[3;3~");
        assert_eq!(e.key, Key::Delete);
        assert_eq!(e.mods, Modifiers::ALT);
    }

    #[test]
    fn kitty_ctrl_a() {
        let e = last_event(b"\x1b[97;5u");
        assert_eq!(e.key, Key::Char('a'));
        assert_eq!(e.mods, Modifiers::CTRL);
    }

    #[test]
    fn kitty_ctrl_i_distinct_from_tab() {
        let e = last_event(b"\x1b[105;5u");
        assert_eq!(e.key, Key::Char('i'));
        assert_eq!(e.mods, Modifiers::CTRL);
    }

    #[test]
    fn kitty_bare_tab() {
        let e = last_event(b"\x1b[9;1u");
        assert_eq!(e.key, Key::Tab);
        assert!(e.mods.is_empty());
    }

    #[test]
    fn kitty_bare_enter() {
        let e = last_event(b"\x1b[13;1u");
        assert_eq!(e.key, Key::Enter);
    }

    #[test]
    fn kitty_escape() {
        let e = last_event(b"\x1b[27;1u");
        assert_eq!(e.key, Key::Escape);
    }

    #[test]
    fn kitty_function_keys_arrive_via_legacy_forms() {
        // Under the Kitty protocol F-keys keep their legacy SS3/CSI forms; the
        // private-use range is not used for them. F1 = SS3 P, F5 = CSI 15~.
        assert_eq!(last_event(b"\x1bOP").key, Key::Function(1));
        assert_eq!(last_event(b"\x1b[15~").key, Key::Function(5));
    }

    #[test]
    fn kitty_lock_and_system_keys_are_not_mismapped_to_nav() {
        // 57358=CapsLock, 57359=ScrollLock, 57361=PrintScreen in the Kitty PUA.
        // `Key` has no variants for them; they must NOT decode to Right/End/
        // Insert (the old hand-written table's bug). They fall through to Char.
        for code in [57358u32, 57359, 57361] {
            let e = last_event(format!("\x1b[{code};1u").as_bytes());
            assert!(
                !matches!(
                    e.key,
                    Key::Arrow(_)
                        | Key::End
                        | Key::Insert
                        | Key::Home
                        | Key::PageUp
                        | Key::PageDown
                ),
                "kitty code {code} mis-decoded as a navigation key: {:?}",
                e.key
            );
            assert!(matches!(e.key, Key::Char(_)));
        }
    }

    #[test]
    fn kitty_no_modifier_param_implies_none() {
        let e = last_event(b"\x1b[97u");
        assert_eq!(e.key, Key::Char('a'));
        assert!(e.mods.is_empty());
    }

    #[test]
    fn kitty_event_type_release_decoded() {
        // \e[97;5:3u -> Char('a'), Ctrl, Release (':3' = release subparam of mods)
        let e = last_event(b"\x1b[97;5:3u");
        assert_eq!(e.key, Key::Char('a'));
        assert_eq!(e.mods, Modifiers::CTRL);
        assert_eq!(e.kind, plexy_glass_mux::KeyEventKind::Release);
    }

    #[test]
    fn kitty_event_type_repeat_decoded() {
        assert_eq!(
            last_event(b"\x1b[97;5:2u").kind,
            plexy_glass_mux::KeyEventKind::Repeat
        );
    }

    #[test]
    fn kitty_event_type_press_default() {
        assert_eq!(
            last_event(b"\x1b[97;5u").kind,
            plexy_glass_mux::KeyEventKind::Press
        );
        assert_eq!(
            last_event(b"\x1b[97;5:1u").kind,
            plexy_glass_mux::KeyEventKind::Press
        );
    }

    #[test]
    fn kitty_associated_text_decoded() {
        // \e[97;2;65u -> Char('a'), Shift, associated text "A" (param 3 = U+0041)
        let e = last_event(b"\x1b[97;2;65u");
        assert_eq!(e.key, Key::Char('a'));
        assert_eq!(e.mods, Modifiers::SHIFT);
        assert_eq!(e.text.as_deref(), Some("A"));
    }

    #[test]
    fn kitty_associated_text_multi_codepoint() {
        // Param 3 may carry several colon-separated codepoints.
        let e = last_event(b"\x1b[97;2;65:66u");
        assert_eq!(e.text.as_deref(), Some("AB"));
    }

    #[test]
    fn kitty_alternate_keys_shifted() {
        // \e[105:73;6u -> base i=105, shifted I=73, ctrl+shift
        let e = last_event(b"\x1b[105:73;6u");
        assert_eq!(e.key, Key::Char('i'));
        assert_eq!(e.shifted, Some('I'));
        assert_eq!(e.mods, Modifiers::CTRL | Modifiers::SHIFT);
    }

    #[test]
    fn kitty_alternate_keys_base_layout() {
        // code:shifted:base, the third colon field is the base-layout codepoint.
        let e = last_event(b"\x1b[105:73:105;1u");
        assert_eq!(e.key, Key::Char('i'));
        assert_eq!(e.shifted, Some('I'));
        assert_eq!(e.base_layout, Some('i'));
    }

    #[test]
    fn kitty_meta_and_lock_modifiers_decoded() {
        // mods 1 + (meta32 + caps64) = 97 -> META | CAPS_LOCK
        let e = last_event(b"\x1b[97;97u");
        assert!(e.mods.contains(Modifiers::META));
        assert!(e.mods.contains(Modifiers::CAPS_LOCK));
    }

    #[test]
    fn legacy_path_unaffected() {
        let e = last_event(b"\x1b[1;5D");
        assert_eq!(e.key, Key::Arrow(Direction::Left));
        assert_eq!(e.mods, Modifiers::CTRL);
        assert_eq!(e.kind, plexy_glass_mux::KeyEventKind::Press);
    }

    #[test]
    fn legacy_protocol_rejects_csi_u() {
        // Strict Legacy mode must NOT decode a Kitty CSI-u; it bails to the raw
        // bytes for passthrough (the security-relevant gate).
        let mut p = KeyParser::new().with_protocol(KeyboardProtocol::Legacy);
        let mut last = KeyParseOutput::Pending;
        for &b in b"\x1b[97;5u" {
            last = p.consume(b);
        }
        assert!(matches!(last, KeyParseOutput::Bytes(_)));
    }

    #[test]
    fn modify_other_keys_protocol_rejects_csi_u() {
        let mut p = KeyParser::new().with_protocol(KeyboardProtocol::ModifyOtherKeys);
        let mut last = KeyParseOutput::Pending;
        for &b in b"\x1b[97;5u" {
            last = p.consume(b);
        }
        assert!(matches!(last, KeyParseOutput::Bytes(_)));
    }

    // --- modifyOtherKeys 27-form decode (K1) ---

    #[test]
    fn mok_27_form_ctrl_a() {
        // \e[27;5;97~ -> Ctrl+a (mods param 5 = ctrl, code 97 = 'a').
        let e = last_event(b"\x1b[27;5;97~");
        assert_eq!(e.key, Key::Char('a'));
        assert_eq!(e.mods, Modifiers::CTRL);
    }

    #[test]
    fn mok_27_form_shift_enter() {
        // \e[27;2;13~ -> Shift+Enter (mods 2 = shift, code 13 = Enter).
        let e = last_event(b"\x1b[27;2;13~");
        assert_eq!(e.key, Key::Enter);
        assert_eq!(e.mods, Modifiers::SHIFT);
    }

    #[test]
    fn mok_27_form_ctrl_shift_i() {
        // \e[27;6;105~ -> Ctrl+Shift+i (mods 6 = ctrl|shift, code 105 = 'i').
        let e = last_event(b"\x1b[27;6;105~");
        assert_eq!(e.key, Key::Char('i'));
        assert_eq!(e.mods, Modifiers::CTRL | Modifiers::SHIFT);
    }

    #[test]
    fn mok_27_form_escape() {
        // \e[27;2;27~ -> Shift+Escape (code 27 maps to the Escape named key).
        let e = last_event(b"\x1b[27;2;27~");
        assert_eq!(e.key, Key::Escape);
        assert_eq!(e.mods, Modifiers::SHIFT);
    }

    #[test]
    fn mok_27_form_accepted_in_every_protocol() {
        // The 27-form is the modifyOtherKeys wire form, so we accept it under the
        // strict ModifyOtherKeys scope (CSI-u is the one rejected there).
        for proto in [
            KeyboardProtocol::ModifyOtherKeys,
            KeyboardProtocol::Legacy,
            KeyboardProtocol::Kitty,
            KeyboardProtocol::Permissive,
        ] {
            let mut p = KeyParser::new().with_protocol(proto);
            let mut last = KeyParseOutput::Pending;
            for &b in b"\x1b[27;5;97~" {
                last = p.consume(b);
            }
            match last {
                KeyParseOutput::Event { event, .. } => {
                    assert_eq!(event.key, Key::Char('a'), "{proto:?}");
                    assert_eq!(event.mods, Modifiers::CTRL, "{proto:?}");
                }
                other => panic!("expected Event for {proto:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn mok_27_form_missing_code_bails() {
        // \e[27;5~ has no third (code) param: malformed, bail to raw bytes.
        let outputs = drive(b"\x1b[27;5~");
        match outputs.last().unwrap() {
            KeyParseOutput::Bytes(bs) => assert_eq!(bs.as_slice(), b"\x1b[27;5~"),
            other => panic!("expected Bytes, got {other:?}"),
        }
    }

    #[test]
    fn mok_27_form_round_trips_with_encoder() {
        // Symmetric with `encode::modify_other_keys_bytes`: encode a key to its
        // 27-form, parse it back, get the same KeyEvent shape.
        use crate::encode::{KeyboardTarget, ModifyOtherKeysLevel, encode};
        for (key, mods) in [
            (Key::Char('a'), Modifiers::CTRL),
            (Key::Enter, Modifiers::SHIFT),
            (Key::Char('i'), Modifiers::CTRL | Modifiers::SHIFT),
        ] {
            let original = KeyEvent::new(key, mods);
            let bytes = encode(
                &original,
                KeyboardTarget::ModifyOtherKeys(ModifyOtherKeysLevel::Level2),
                false,
            );
            assert!(
                bytes.starts_with(b"\x1b[27;"),
                "expected 27-form for {key:?}/{mods:?}, got {bytes:?}"
            );
            let mut p = KeyParser::new().with_protocol(KeyboardProtocol::ModifyOtherKeys);
            let mut got = None;
            for &b in &bytes {
                if let KeyParseOutput::Event { event, .. } = p.consume(b) {
                    got = Some(event);
                }
            }
            let got = got.expect("decoded event");
            assert_eq!(got.key, key, "{key:?}/{mods:?}");
            assert_eq!(got.mods, mods, "{key:?}/{mods:?}");
        }
    }

    // --- flush guard (K1) ---

    #[test]
    fn flush_after_lone_esc_yields_escape() {
        let mut p = KeyParser::new();
        assert!(matches!(p.consume(0x1b), KeyParseOutput::Pending));
        assert!(p.is_mid_sequence());
        match p.flush().expect("flush should emit Escape") {
            KeyParseOutput::Event { event, .. } => assert_eq!(event.key, Key::Escape),
            other => panic!("expected Event, got {other:?}"),
        }
        // After the flush the parser is back to Idle.
        assert!(!p.is_mid_sequence());
    }

    #[test]
    fn flush_mid_incomplete_csi_does_not_synthesize_escape() {
        // A partial `\x1b[` (state == SawCsi, not SawEsc) must NOT flush into a
        // bogus Escape, so the flush only fires when the state is exactly SawEsc.
        let mut p = KeyParser::new();
        assert!(matches!(p.consume(0x1b), KeyParseOutput::Pending));
        assert!(matches!(p.consume(b'['), KeyParseOutput::Pending));
        assert!(p.is_mid_sequence(), "still mid-sequence after \\x1b[");
        assert!(
            p.flush().is_none(),
            "flush must not synthesize Escape mid-CSI"
        );
        // The real CSI can still complete after the flush no-op.
        let e = {
            let mut last = None;
            for &b in b"A" {
                if let KeyParseOutput::Event { event, .. } = p.consume(b) {
                    last = Some(event);
                }
            }
            last.expect("arrow event")
        };
        assert_eq!(e.key, Key::Arrow(Direction::Up));
    }

    #[test]
    fn flush_when_idle_is_noop() {
        let mut p = KeyParser::new();
        assert!(!p.is_mid_sequence());
        assert!(p.flush().is_none());
    }

    #[test]
    fn is_mid_sequence_tracks_partial_ss3() {
        let mut p = KeyParser::new();
        assert!(!p.is_mid_sequence());
        p.consume(0x1b);
        p.consume(b'O');
        assert!(p.is_mid_sequence());
        p.consume(b'P'); // completes \e O P = F1
        assert!(!p.is_mid_sequence());
    }
}
