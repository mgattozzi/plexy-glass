//! Classifies raw client input bytes into typed key events, mouse events,
//! or passthrough bytes.

use plexy_glass_keys::{KeyParseOutput, KeyParser};
use plexy_glass_mux::{KeyEvent, MouseEvent, MouseParseAction, MouseParser};

#[derive(Debug, Clone)]
pub enum InputEvent {
    Key(KeyEvent, Vec<u8>),
    Mouse(MouseEvent),
    /// Bytes that didn't parse as either mouse or key, so they pass through to the shell.
    Bytes(Vec<u8>),
}

pub struct InputRouter {
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
            mouse: MouseParser::new(),
            keys: KeyParser::new(),
        }
    }

    pub fn classify(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            match self.mouse.consume(b) {
                MouseParseAction::Pending => {}
                MouseParseAction::Event(e) => out.push(InputEvent::Mouse(e)),
                MouseParseAction::Other(byte) => self.feed_key(byte, &mut out),
                MouseParseAction::BailedBytes(bs) => {
                    for byte in bs {
                        self.feed_key(byte, &mut out);
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
    fn arrow_up_parses_as_key_event() {
        let mut r = InputRouter::new();
        let events = r.classify(b"\x1b[A");
        let mut found = false;
        for e in events {
            if let InputEvent::Key(ke, bytes) = e {
                assert_eq!(ke.key, Key::Arrow(Direction::Up));
                assert!(ke.mods.is_empty());
                assert_eq!(bytes, b"\x1b[A");
                found = true;
            }
        }
        assert!(found, "expected an arrow-up Key event");
    }

    #[test]
    fn ctrl_left_parses_with_modifier() {
        let mut r = InputRouter::new();
        let events = r.classify(b"\x1b[1;5D");
        let key = events
            .iter()
            .find_map(|e| match e {
                InputEvent::Key(ke, _) => Some(ke),
                _ => None,
            })
            .expect("key event");
        assert_eq!(key.key, Key::Arrow(Direction::Left));
        assert_eq!(key.mods, Modifiers::CTRL);
    }

    #[test]
    fn mouse_event_routes_to_mouse() {
        let mut r = InputRouter::new();
        let events = r.classify(b"\x1b[<0;10;5M");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], InputEvent::Mouse(_)));
    }

    #[test]
    fn plain_char_parses_as_key_event() {
        let mut r = InputRouter::new();
        let events = r.classify(b"a");
        match events.first() {
            Some(InputEvent::Key(ke, bytes)) => {
                assert_eq!(ke.key, Key::Char('a'));
                assert_eq!(bytes.as_slice(), b"a");
            }
            other => panic!("expected Key event, got {other:?}"),
        }
    }
}
