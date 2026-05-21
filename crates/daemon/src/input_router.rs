//! Classifies raw client input bytes into typed keyboard or mouse events.

use plexy_glass_mux::{MouseEvent, MouseParseAction, MouseParser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// Byte that wasn't part of a mouse sequence; route to keymap / pane.
    Key(u8),
    Mouse(MouseEvent),
}

pub struct InputRouter {
    mouse: MouseParser,
}

impl Default for InputRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl InputRouter {
    pub fn new() -> Self {
        Self { mouse: MouseParser::new() }
    }

    /// Classify each byte. Returns events in the order they were produced.
    pub fn classify(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            match self.mouse.consume(b) {
                MouseParseAction::Pending => {}
                MouseParseAction::Event(e) => out.push(InputEvent::Mouse(e)),
                MouseParseAction::Other(byte) => out.push(InputEvent::Key(byte)),
                MouseParseAction::BailedBytes(bs) => {
                    for byte in bs {
                        out.push(InputEvent::Key(byte));
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_mux::{MouseButton, MouseKind};

    #[test]
    fn pure_keys_classify_as_key_events() {
        let mut r = InputRouter::new();
        let events = r.classify(b"ls\n");
        assert_eq!(events.len(), 3);
        for (i, byte) in b"ls\n".iter().enumerate() {
            match events[i] {
                InputEvent::Key(b) => assert_eq!(b, *byte),
                _ => panic!("expected Key"),
            }
        }
    }

    #[test]
    fn sgr_mouse_press_classifies_as_mouse_event() {
        let mut r = InputRouter::new();
        let events = r.classify(b"\x1b[<0;10;5M");
        assert_eq!(events.len(), 1);
        match events[0] {
            InputEvent::Mouse(e) => {
                assert_eq!(e.button, MouseButton::Left);
                assert_eq!(e.kind, MouseKind::Press);
                assert_eq!((e.row, e.col), (4, 9));
            }
            _ => panic!("expected Mouse"),
        }
    }

    #[test]
    fn interleaved_keys_and_mouse() {
        let mut r = InputRouter::new();
        let mut input = Vec::new();
        input.extend_from_slice(b"a");
        input.extend_from_slice(b"\x1b[<0;1;1M");
        input.extend_from_slice(b"b");
        let events = r.classify(&input);
        // 'a', mouse press, 'b'.
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], InputEvent::Key(b'a')));
        assert!(matches!(events[1], InputEvent::Mouse(_)));
        assert!(matches!(events[2], InputEvent::Key(b'b')));
    }

    #[test]
    fn esc_bracket_arrow_emits_all_three_bytes() {
        // Arrow key ESC [ A must not be swallowed; all three bytes arrive as Key events.
        let mut r = InputRouter::new();
        let events = r.classify(b"\x1b[A");
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], InputEvent::Key(0x1b)));
        assert!(matches!(events[1], InputEvent::Key(b'[')));
        assert!(matches!(events[2], InputEvent::Key(b'A')));
    }
}
