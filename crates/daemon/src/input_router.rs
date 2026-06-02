//! Classifies raw client input bytes into typed key events, mouse events,
//! paste blocks, or passthrough bytes.

use plexy_glass_keys::{KeyParseOutput, KeyParser, PasteParseOutput, PasteParser};
use plexy_glass_mux::{KeyEvent, MouseEvent, MouseParseAction, MouseParser};

#[derive(Debug, Clone)]
pub enum InputEvent {
    Key(KeyEvent, Vec<u8>),
    Mouse(MouseEvent),
    /// A bracketed-paste block from the host TTY. The contained bytes
    /// are the inner content (no wrapper). The connection layer decides
    /// whether to wrap or strip based on the active pane's mode.
    Paste(Vec<u8>),
    /// Bytes that didn't parse as paste, mouse, or key, so they pass through to the shell.
    Bytes(Vec<u8>),
}

pub struct InputRouter {
    paste: PasteParser,
    mouse: MouseParser,
    keys: KeyParser,
}

impl Default for InputRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl InputRouter {
    pub fn new() -> Self {
        Self {
            paste: PasteParser::new(),
            mouse: MouseParser::new(),
            keys: KeyParser::new(),
        }
    }

    /// Build a router whose key decode is scoped to the client's negotiated
    /// outer-terminal protocol.
    pub fn with_protocol(protocol: plexy_glass_keys::KeyboardProtocol) -> Self {
        Self {
            paste: PasteParser::new(),
            mouse: MouseParser::new(),
            keys: KeyParser::new().with_protocol(protocol),
        }
    }

    pub fn classify(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            match self.paste.consume(b) {
                PasteParseOutput::Pending => {}
                PasteParseOutput::Paste(content) => {
                    out.push(InputEvent::Paste(content));
                }
                PasteParseOutput::NotPaste(bs) => {
                    for byte in bs {
                        self.feed_mouse_then_key(byte, &mut out);
                    }
                }
            }
        }
        out
    }

    pub fn flush_keys(&mut self) -> Option<InputEvent> {
        match self.keys.flush()? {
            KeyParseOutput::Event { event, bytes } => Some(InputEvent::Key(event, bytes)),
            KeyParseOutput::Bytes(bs) => Some(InputEvent::Bytes(bs)),
            KeyParseOutput::Pending => None,
        }
    }

    fn feed_mouse_then_key(&mut self, byte: u8, out: &mut Vec<InputEvent>) {
        match self.mouse.consume(byte) {
            MouseParseAction::Pending => {}
            MouseParseAction::Event(e) => out.push(InputEvent::Mouse(e)),
            MouseParseAction::Other(byte) => self.feed_key(byte, out),
            MouseParseAction::BailedBytes(bs) => {
                for byte in bs {
                    self.feed_key(byte, out);
                }
            }
        }
    }

    fn feed_key(&mut self, byte: u8, out: &mut Vec<InputEvent>) {
        match self.keys.consume(byte) {
            KeyParseOutput::Pending => {}
            KeyParseOutput::Event { event, bytes } => out.push(InputEvent::Key(event, bytes)),
            KeyParseOutput::Bytes(bs) => out.push(InputEvent::Bytes(bs)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_mux::{Direction, Key, Modifiers};

    #[test]
    fn wrapped_paste_emits_paste_event_with_inner_bytes() {
        let mut r = InputRouter::new();
        let events = r.classify(b"\x1b[200~echo HELLO\x1b[201~");
        let pastes: Vec<&[u8]> = events
            .iter()
            .filter_map(|e| match e {
                InputEvent::Paste(bs) => Some(bs.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(pastes, vec![b"echo HELLO".as_slice()]);
        // No Key events from inside the paste.
        for e in &events {
            assert!(!matches!(e, InputEvent::Key(..)), "unexpected Key inside paste: {e:?}");
        }
    }

    #[test]
    fn plain_arrow_still_parses_as_key() {
        let mut r = InputRouter::new();
        let events = r.classify(b"\x1b[A");
        let key = events
            .iter()
            .find_map(|e| match e {
                InputEvent::Key(ke, _) => Some(ke),
                _ => None,
            })
            .expect("key event");
        assert_eq!(key.key, Key::Arrow(Direction::Up));
        assert!(key.mods.is_empty());
    }

    #[test]
    fn plain_char_still_parses_as_key() {
        let mut r = InputRouter::new();
        let events = r.classify(b"a");
        match events.first() {
            Some(InputEvent::Key(ke, _)) => {
                assert_eq!(ke.key, Key::Char('a'));
            }
            other => panic!("expected Key event, got {other:?}"),
        }
    }

    #[test]
    fn mouse_event_still_routes_to_mouse() {
        let mut r = InputRouter::new();
        let events = r.classify(b"\x1b[<0;10;5M");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], InputEvent::Mouse(_)));
    }

    #[test]
    fn paste_then_typing_routes_separately() {
        let mut r = InputRouter::new();
        let events = r.classify(b"\x1b[200~hi\x1b[201~a");
        let mut saw_paste = false;
        let mut saw_a = false;
        for e in events {
            match e {
                InputEvent::Paste(bs) => {
                    assert_eq!(bs, b"hi");
                    saw_paste = true;
                }
                InputEvent::Key(ke, _)
                    if ke.key == Key::Char('a') && ke.mods == Modifiers::empty() =>
                {
                    saw_a = true;
                }
                _ => {}
            }
        }
        assert!(saw_paste && saw_a);
    }
}
