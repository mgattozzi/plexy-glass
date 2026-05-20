//! Byte stream → `KeyEvent` state machine.

use plexy_glass_mux::KeyEvent;

#[derive(Debug, Clone)]
pub enum KeyParseOutput {
    /// Byte was consumed as part of an in-progress sequence.
    Pending,
    /// A complete sequence was recognized.
    Event {
        event: KeyEvent,
        bytes: Vec<u8>,
    },
    /// Bytes were buffered but did not form a recognized sequence, so they
    /// pass through to the shell as-is.
    Bytes(Vec<u8>),
}

pub struct KeyParser;

impl Default for KeyParser {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyParser {
    pub fn new() -> Self {
        Self
    }

    pub fn consume(&mut self, byte: u8) -> KeyParseOutput {
        // Placeholder: real implementation lands in Tasks 3-5.
        KeyParseOutput::Bytes(vec![byte])
    }

    /// Flush any held bytes (e.g. lone ESC on idle timeout).
    pub fn flush(&mut self) -> Option<KeyParseOutput> {
        None
    }
}
