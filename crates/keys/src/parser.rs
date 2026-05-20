//! Byte stream → `KeyEvent` state machine.
//!
//! Recognizes the standard VT/xterm/SS3 key encodings plus kitty CSI-u
//! sequences (added in Task 5). On bail, returns the buffered bytes
//! verbatim so callers can pass them through to the shell.

use plexy_glass_mux::{Direction, Key, KeyEvent, Modifiers};

#[derive(Debug, Clone)]
pub enum KeyParseOutput {
    Pending,
    Event { event: KeyEvent, bytes: Vec<u8> },
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    SawEsc,
    SawCsi,   // ESC [
    AccumCsi, // ESC [ <params> ; ...
    SawSs3,   // ESC O
}

pub struct KeyParser {
    state: State,
    buf: Vec<u8>,
    params: Vec<u32>,
    current_param: Option<u32>,
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
            current_param: None,
        }
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

    pub fn flush(&mut self) -> Option<KeyParseOutput> {
        if self.state == State::SawEsc {
            let bytes = std::mem::take(&mut self.buf);
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
                let bytes = std::mem::take(&mut self.buf);
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
                let bytes = std::mem::take(&mut self.buf);
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
            let acc = self
                .current_param
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(u32::from(byte - b'0'));
            self.current_param = Some(acc);
            return KeyParseOutput::Pending;
        }
        if byte == b';' {
            self.state = State::AccumCsi;
            self.params.push(self.current_param.unwrap_or(0));
            self.current_param = None;
            return KeyParseOutput::Pending;
        }
        if let Some(p) = self.current_param.take() {
            self.params.push(p);
        }
        self.dispatch_csi(byte)
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
        let bytes = std::mem::take(&mut self.buf);
        self.reset_state();
        KeyParseOutput::Event { event, bytes }
    }

    fn dispatch_csi(&mut self, byte: u8) -> KeyParseOutput {
        let event_opt: Option<KeyEvent> = match (byte, self.params.as_slice()) {
            (b'A', []) => Some(KeyEvent::plain(Key::Arrow(Direction::Up))),
            (b'B', []) => Some(KeyEvent::plain(Key::Arrow(Direction::Down))),
            (b'C', []) => Some(KeyEvent::plain(Key::Arrow(Direction::Right))),
            (b'D', []) => Some(KeyEvent::plain(Key::Arrow(Direction::Left))),
            (b'H', []) => Some(KeyEvent::plain(Key::Home)),
            (b'F', []) => Some(KeyEvent::plain(Key::End)),
            (b'~', [n]) => key_from_tilde(*n).map(KeyEvent::plain),
            (b'Z', []) => Some(KeyEvent::new(Key::Tab, Modifiers::SHIFT)),
            _ => None,
        };
        match event_opt {
            Some(event) => {
                let bytes = std::mem::take(&mut self.buf);
                self.reset_state();
                KeyParseOutput::Event { event, bytes }
            }
            None => self.bail(),
        }
    }

    fn emit_simple(&mut self, key: Key, mods: Modifiers) -> KeyParseOutput {
        let bytes = std::mem::take(&mut self.buf);
        self.reset_state();
        KeyParseOutput::Event {
            event: KeyEvent::new(key, mods),
            bytes,
        }
    }

    fn bail(&mut self) -> KeyParseOutput {
        let bytes = std::mem::take(&mut self.buf);
        self.reset_state();
        KeyParseOutput::Bytes(bytes)
    }

    fn reset_state(&mut self) {
        self.state = State::Idle;
        self.params.clear();
        self.current_param = None;
    }
}

fn key_from_tilde(n: u32) -> Option<Key> {
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
}
