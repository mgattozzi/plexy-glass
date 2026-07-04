//! Classifies raw client input bytes into typed key events, mouse events,
//! paste blocks, or passthrough bytes.

use plexy_glass_keys::{
    KeyParseOutput, KeyParser, KeyboardProtocol, PasteParseOutput, PasteParser,
};
use plexy_glass_mux::{KeyEvent, MouseEvent, MouseParseAction, MouseParser};
use plexy_glass_protocol::NegotiatedKbd;

/// Map the client's negotiated outer-terminal protocol to the decode scope.
///
/// A free fn rather than a `From` impl: the orphan rule forbids implementing a
/// foreign trait for two foreign types from the daemon crate.
pub const fn decode_protocol(kbd: NegotiatedKbd) -> KeyboardProtocol {
    match kbd {
        NegotiatedKbd::Legacy => KeyboardProtocol::Legacy,
        // The parser's mode covers both modifyOtherKeys levels and any Kitty flag
        // set, so the negotiated level/flags are informational only here.
        NegotiatedKbd::ModifyOtherKeys(_) => KeyboardProtocol::ModifyOtherKeys,
        NegotiatedKbd::Kitty(_) => KeyboardProtocol::Kitty,
    }
}

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

    /// Flush a buffered, idle escape sequence into an event.
    ///
    /// A lone `\x1b` parks in the FIRST parser of the chain that speculatively
    /// buffers ESC: the paste parser (waiting for a `\x1b[200~` opener), then the
    /// mouse parser (waiting for `\x1b[<…`), then the key parser. None of them
    /// forward the ESC downstream until they bail, so a bare Esc never reaches the
    /// key parser on its own. The flush drains each speculative buffer in chain
    /// order and re-feeds the held bytes through the remaining parsers, exactly as
    /// a real bail would, then flushes the key parser itself (turning a lone ESC
    /// into `Key(Escape)`). A partial CSI/SS3 the key parser still considers
    /// incomplete yields nothing: the key parser's `flush` only fires on an exact
    /// `SawEsc`, so we never synthesize a bogus Escape from a half-finished
    /// sequence. Returns the first resulting event (the idle-flush case is a
    /// single lone ESC).
    ///
    /// # Invariants
    ///
    /// The three steps are **load-bearing in order**, all must run unconditionally
    /// (no early return between them):
    ///
    /// 1. `paste.flush_open` re-feeds the held `\x1b` (and any partial `[200~`
    ///    prefix) INTO the mouse parser, parking it at `SawEsc`.
    /// 2. `mouse.is_mid_sequence` (which reads parser state mutated by step 1)
    ///    then drains that `\x1b` INTO the key parser.
    /// 3. `keys.flush` turns the lone `SawEsc` into `Key(Escape)`.
    ///
    /// Reordering or short-circuiting on the first non-empty step would silently
    /// swallow a lone ESC: the byte would stall in whichever speculative buffer
    /// was non-empty first, never reaching the key parser.
    pub fn flush_keys(&mut self) -> Option<InputEvent> {
        let mut events = Vec::new();
        // 1. Drain a partial paste-open back through mouse → key.
        if let Some(PasteParseOutput::NotPaste(bs)) = self.paste.flush_open() {
            for byte in bs {
                self.feed_mouse_then_key(byte, &mut events);
            }
        }
        // 2. Drain the mouse parser's speculatively-held bytes through key.
        //    This must run after step 1, since step 1 may have just parked an ESC
        //    in the mouse parser, making `is_mid_sequence()` newly true.
        if self.mouse.is_mid_sequence() {
            for byte in self.mouse.flush() {
                self.feed_key(byte, &mut events);
            }
        }
        // 3. Flush the key parser itself (a lone ESC → Escape).
        if let Some(out) = self.keys.flush() {
            match out {
                KeyParseOutput::Event { event, bytes } => {
                    events.push(InputEvent::Key(event, bytes));
                }
                KeyParseOutput::Bytes(bs) => events.push(InputEvent::Bytes(bs)),
                KeyParseOutput::Pending => {}
            }
        }
        events.into_iter().next()
    }

    /// Whether any parser has a buffered `\x1b` (or partial CSI/SS3) parked
    /// awaiting more bytes.
    ///
    /// The connection loop reads this after each `classify` to decide whether to
    /// arm the Esc idle-flush timer. Note that the paste parser is reported
    /// pending ONLY on a partial *open* (`is_pending_open`), never
    /// mid-content/close, where there is no Esc to deliver and arming the timer
    /// would corrupt an in-progress paste.
    pub fn has_pending(&self) -> bool {
        self.paste.is_pending_open() || self.mouse.is_mid_sequence() || self.keys.is_mid_sequence()
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
    use plexy_glass_keys::KeyboardProtocol;
    use plexy_glass_mux::{Direction, Key, Modifiers};

    use super::*;

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
            assert!(
                !matches!(e, InputEvent::Key(..)),
                "unexpected Key inside paste: {e:?}"
            );
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
    fn decode_protocol_maps_negotiated_kbd() {
        use plexy_glass_keys::KeyboardProtocol;
        use plexy_glass_protocol::NegotiatedKbd;
        assert_eq!(
            decode_protocol(NegotiatedKbd::Legacy),
            KeyboardProtocol::Legacy
        );
        assert_eq!(
            decode_protocol(NegotiatedKbd::ModifyOtherKeys(2)),
            KeyboardProtocol::ModifyOtherKeys
        );
        assert_eq!(
            decode_protocol(NegotiatedKbd::Kitty(31)),
            KeyboardProtocol::Kitty
        );
    }

    #[test]
    fn has_pending_true_after_lone_esc_then_flush_yields_escape() {
        let mut r = InputRouter::new();
        assert!(!r.has_pending(), "idle parser is not pending");
        let events = r.classify(b"\x1b");
        assert!(
            events.is_empty(),
            "lone ESC parks pending, emits nothing yet"
        );
        assert!(r.has_pending(), "mid-escape after a lone \\x1b");
        match r.flush_keys() {
            Some(InputEvent::Key(ke, _)) => assert_eq!(ke.key, Key::Escape),
            other => panic!("expected Key(Escape), got {other:?}"),
        }
        assert!(!r.has_pending(), "flush returned the parser to idle");
    }

    #[test]
    fn has_pending_false_after_complete_sequence() {
        let mut r = InputRouter::new();
        r.classify(b"\x1b[A");
        assert!(!r.has_pending(), "a complete CSI leaves nothing pending");
        assert!(r.flush_keys().is_none(), "nothing to flush when idle");
    }

    #[test]
    fn mok_27_form_classifies_as_key() {
        // The 27-form reaches `InputEvent::Key` (not `Bytes`) through the router.
        let mut r = InputRouter::with_protocol(KeyboardProtocol::ModifyOtherKeys);
        let events = r.classify(b"\x1b[27;5;97~");
        match events.first() {
            Some(InputEvent::Key(ke, _)) => {
                assert_eq!(ke.key, Key::Char('a'));
                assert_eq!(ke.mods, Modifiers::CTRL);
            }
            other => panic!("expected Key, got {other:?}"),
        }
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
