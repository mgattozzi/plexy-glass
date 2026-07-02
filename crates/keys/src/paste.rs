//! Byte stream → bracketed-paste accumulator.
//!
//! The host TTY (when bracketed paste is enabled by the inner app)
//! wraps user paste content in `\x1b[200~...\x1b[201~`. `PasteParser`
//! recognizes that wrapper, accumulates the content, and emits it as a
//! single `Paste` output so the caller can route it past the keymap.
//!
//! Bytes that aren't part of a paste sequence (or partial open
//! sequences that turn out not to be a paste) are returned via
//! `NotPaste` so the caller can feed them through other parsers
//! (mouse, key).

use std::mem;

#[derive(Debug, Clone)]
pub enum PasteParseOutput {
    /// Byte was consumed as part of an in-progress sequence or paste content.
    Pending,
    /// A complete paste was recognized; here's the inner content.
    Paste(Vec<u8>),
    /// The buffered bytes are not part of any paste sequence; route them
    /// elsewhere (e.g. through `MouseParser`/`KeyParser`).
    NotPaste(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    SawEsc,
    SawBracket,
    SawTwo,
    SawTwoZero,
    SawTwoZeroZero,
    Content,
    InEsc,
    InBracket,
    InTwo,
    InTwoZero,
    InTwoZeroOne,
}

/// Default cap per paste. Matches OSC 52's 4 MiB limit.
const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024;

pub struct PasteParser {
    state: State,
    /// Bytes consumed during a partial open or close sequence; on bail,
    /// these get returned via `NotPaste` (open) or appended to the paste
    /// buffer (close).
    held: Vec<u8>,
    /// Paste content accumulator (only used in Content* states).
    buffer: Vec<u8>,
    /// Truncation cap.
    max_bytes: usize,
    /// Tripped once when truncation happens; resets on each new paste.
    truncated: bool,
}

impl Default for PasteParser {
    fn default() -> Self {
        Self::new()
    }
}

impl PasteParser {
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_MAX_BYTES)
    }

    pub fn with_cap(max_bytes: usize) -> Self {
        Self {
            state: State::Idle,
            held: Vec::with_capacity(8),
            buffer: Vec::with_capacity(64),
            max_bytes,
            truncated: false,
        }
    }

    pub fn consume(&mut self, byte: u8) -> PasteParseOutput {
        match self.state {
            State::Idle => {
                if byte == 0x1b {
                    self.held.push(byte);
                    self.state = State::SawEsc;
                    PasteParseOutput::Pending
                } else {
                    PasteParseOutput::NotPaste(vec![byte])
                }
            }
            State::SawEsc => self.advance_open(byte, b'[', State::SawBracket),
            State::SawBracket => self.advance_open(byte, b'2', State::SawTwo),
            State::SawTwo => self.advance_open(byte, b'0', State::SawTwoZero),
            State::SawTwoZero => self.advance_open(byte, b'0', State::SawTwoZeroZero),
            State::SawTwoZeroZero => {
                if byte == b'~' {
                    // Opener complete: discard the held bytes and start collecting content.
                    self.held.clear();
                    self.buffer.clear();
                    self.truncated = false;
                    self.state = State::Content;
                    PasteParseOutput::Pending
                } else {
                    self.bail_open(byte)
                }
            }
            State::Content => {
                if byte == 0x1b {
                    self.held.push(byte);
                    self.state = State::InEsc;
                } else {
                    self.push_content(byte);
                }
                PasteParseOutput::Pending
            }
            State::InEsc => self.advance_close(byte, b'[', State::InBracket),
            State::InBracket => self.advance_close(byte, b'2', State::InTwo),
            State::InTwo => self.advance_close(byte, b'0', State::InTwoZero),
            State::InTwoZero => self.advance_close(byte, b'1', State::InTwoZeroOne),
            State::InTwoZeroOne => {
                if byte == b'~' {
                    // Closer complete, emit the paste.
                    self.held.clear();
                    let buffer = mem::take(&mut self.buffer);
                    self.state = State::Idle;
                    self.truncated = false;
                    PasteParseOutput::Paste(buffer)
                } else {
                    // Not the closer; the held bytes are paste content.
                    self.flush_held_into_buffer();
                    self.push_content(byte);
                    self.state = State::Content;
                    PasteParseOutput::Pending
                }
            }
        }
    }

    /// Whether the parser is mid-sequence on a *partial open*, i.e. it has
    /// speculatively swallowed `\x1b…` while checking for a `\x1b[200~` opener
    /// but hasn't decided yet. A lone `\x1b` parks here (state `SawEsc`), which
    /// is why a bare Esc never reaches the key parser without a flush. We
    /// deliberately do NOT report mid-*content*/mid-close states as pending:
    /// inside an active paste there is no Esc to deliver, and the connection
    /// loop must not arm the Esc idle-flush there (it would prematurely bail an
    /// in-progress paste).
    pub const fn is_pending_open(&self) -> bool {
        matches!(
            self.state,
            State::SawEsc
                | State::SawBracket
                | State::SawTwo
                | State::SawTwoZero
                | State::SawTwoZeroZero
        )
    }

    /// Abandon a partial *open* sequence and return the held bytes as
    /// `NotPaste` so the caller can re-route them (mouse → key). The lone-`\x1b`
    /// case returns `[0x1b]`, which the key parser then turns into Escape. A
    /// no-op (returns `None`) when not on a partial open, since mid-content and
    /// mid-close are left untouched so an in-progress paste isn't corrupted.
    pub fn flush_open(&mut self) -> Option<PasteParseOutput> {
        if !self.is_pending_open() {
            return None;
        }
        let out = mem::take(&mut self.held);
        self.state = State::Idle;
        Some(PasteParseOutput::NotPaste(out))
    }

    /// Helper for opener-state transitions: advance if matched, bail if not.
    fn advance_open(&mut self, byte: u8, expected: u8, next: State) -> PasteParseOutput {
        if byte == expected {
            self.held.push(byte);
            self.state = next;
            PasteParseOutput::Pending
        } else {
            self.bail_open(byte)
        }
    }

    /// Helper for closer-state transitions inside content: advance if
    /// matched, otherwise flush the held bytes into the buffer and treat
    /// the current byte as content.
    fn advance_close(&mut self, byte: u8, expected: u8, next: State) -> PasteParseOutput {
        if byte == expected {
            self.held.push(byte);
            self.state = next;
        } else {
            self.flush_held_into_buffer();
            self.push_content(byte);
            self.state = State::Content;
        }
        PasteParseOutput::Pending
    }

    /// Bail from a partial open sequence: emit held + current byte as
    /// `NotPaste` and return to `Idle`.
    fn bail_open(&mut self, byte: u8) -> PasteParseOutput {
        let mut out = mem::take(&mut self.held);
        out.push(byte);
        self.state = State::Idle;
        PasteParseOutput::NotPaste(out)
    }

    fn flush_held_into_buffer(&mut self) {
        for b in mem::take(&mut self.held) {
            self.push_content(b);
        }
    }

    fn push_content(&mut self, byte: u8) {
        if self.buffer.len() < self.max_bytes {
            self.buffer.push(byte);
        } else if !self.truncated {
            self.truncated = true;
            tracing::warn!(cap = self.max_bytes, "paste exceeds cap; truncating");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(bytes: &[u8]) -> Vec<PasteParseOutput> {
        let mut p = PasteParser::new();
        bytes.iter().map(|&b| p.consume(b)).collect()
    }

    fn collect_pastes(outputs: &[PasteParseOutput]) -> Vec<Vec<u8>> {
        outputs
            .iter()
            .filter_map(|o| match o {
                PasteParseOutput::Paste(bs) => Some(bs.clone()),
                _ => None,
            })
            .collect()
    }

    fn collect_not_paste(outputs: &[PasteParseOutput]) -> Vec<Vec<u8>> {
        outputs
            .iter()
            .filter_map(|o| match o {
                PasteParseOutput::NotPaste(bs) => Some(bs.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_byte_emits_not_paste() {
        let outs = drive(b"a");
        let np = collect_not_paste(&outs);
        assert_eq!(np, vec![b"a".to_vec()]);
    }

    #[test]
    fn happy_path_paste() {
        let outs = drive(b"\x1b[200~hello\x1b[201~");
        let pastes = collect_pastes(&outs);
        assert_eq!(pastes, vec![b"hello".to_vec()]);
    }

    #[test]
    fn paste_with_nested_esc_treated_as_content() {
        let outs = drive(b"\x1b[200~hi\x1b[mthere\x1b[201~");
        let pastes = collect_pastes(&outs);
        assert_eq!(pastes, vec![b"hi\x1b[mthere".to_vec()]);
    }

    #[test]
    fn partial_open_bails_to_not_paste() {
        let outs = drive(b"\x1b[200Q");
        let np = collect_not_paste(&outs);
        let merged: Vec<u8> = np.into_iter().flatten().collect();
        assert_eq!(merged, b"\x1b[200Q");
    }

    #[test]
    fn empty_paste() {
        let outs = drive(b"\x1b[200~\x1b[201~");
        let pastes = collect_pastes(&outs);
        assert_eq!(pastes, vec![Vec::<u8>::new()]);
    }

    #[test]
    fn back_to_back_pastes_parse_independently() {
        let outs = drive(b"\x1b[200~a\x1b[201~\x1b[200~b\x1b[201~");
        let pastes = collect_pastes(&outs);
        assert_eq!(pastes, vec![b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn flush_back_respects_truncation_cap() {
        // Cap = 4. Fill exactly the cap, then a PARTIAL closer ("\x1b[201Q")
        // whose held bytes flush back as content (they must NOT leak past the
        // cap), then a real closer. The emitted paste is exactly the first 4.
        let mut p = PasteParser::with_cap(4);
        let mut outs = Vec::new();
        for &b in b"\x1b[200~abcd" {
            outs.push(p.consume(b)); // opener + 4 content bytes (== cap)
        }
        for &b in b"\x1b[201Q" {
            outs.push(p.consume(b)); // partial closer → held bytes flush back
        }
        for &b in b"\x1b[201~" {
            outs.push(p.consume(b)); // real closer
        }
        let pastes = collect_pastes(&outs);
        assert_eq!(
            pastes,
            vec![b"abcd".to_vec()],
            "flushed-back held bytes must respect the cap"
        );
    }

    #[test]
    fn closer_like_bytes_inside_content() {
        let outs = drive(b"\x1b[200~a\x1b[201Qb\x1b[201~");
        let pastes = collect_pastes(&outs);
        assert_eq!(pastes, vec![b"a\x1b[201Qb".to_vec()]);
    }

    #[test]
    fn oversized_paste_truncates_to_cap() {
        let mut p = PasteParser::with_cap(4);
        for &b in b"\x1b[200~" {
            assert!(matches!(p.consume(b), PasteParseOutput::Pending));
        }
        for &b in b"ABCDEFGH" {
            assert!(matches!(p.consume(b), PasteParseOutput::Pending));
        }
        let mut closer_output = None;
        for &b in b"\x1b[201~" {
            let o = p.consume(b);
            if matches!(o, PasteParseOutput::Paste(_)) {
                closer_output = Some(o);
            }
        }
        let paste = match closer_output {
            Some(PasteParseOutput::Paste(bs)) => bs,
            other => panic!("expected Paste; got {other:?}"),
        };
        assert_eq!(paste, b"ABCD");
    }

    #[test]
    fn opener_then_immediate_bail_returns_buffered_bytes() {
        let outs = drive(b"\x1b[20Q");
        let np: Vec<u8> = collect_not_paste(&outs).into_iter().flatten().collect();
        assert_eq!(np, b"\x1b[20Q");
    }

    #[test]
    fn lone_esc_followed_by_non_bracket_is_not_paste() {
        let outs = drive(b"\x1ba");
        let np: Vec<u8> = collect_not_paste(&outs).into_iter().flatten().collect();
        assert_eq!(np, b"\x1ba");
    }

    #[test]
    fn lone_esc_parks_pending_open_and_flushes_back_the_esc() {
        let mut p = PasteParser::new();
        assert!(matches!(p.consume(0x1b), PasteParseOutput::Pending));
        assert!(p.is_pending_open(), "a lone ESC parks on a partial open");
        match p.flush_open().expect("flush yields the held ESC") {
            PasteParseOutput::NotPaste(bs) => assert_eq!(bs, vec![0x1b]),
            other => panic!("expected NotPaste([0x1b]), got {other:?}"),
        }
        assert!(!p.is_pending_open(), "back to Idle after flush");
        assert!(p.flush_open().is_none(), "second flush is a no-op");
    }

    #[test]
    fn partial_open_flushes_back_all_held_bytes() {
        let mut p = PasteParser::new();
        for &b in b"\x1b[2" {
            assert!(matches!(p.consume(b), PasteParseOutput::Pending));
        }
        assert!(p.is_pending_open());
        match p.flush_open().expect("flush yields held bytes") {
            PasteParseOutput::NotPaste(bs) => assert_eq!(bs, b"\x1b[2"),
            other => panic!("expected NotPaste, got {other:?}"),
        }
    }

    #[test]
    fn mid_content_is_not_pending_open() {
        // Inside an active paste there is no Esc to deliver; the connection
        // loop must NOT arm the Esc idle-flush, and flush_open must no-op so it
        // doesn't corrupt the in-progress paste.
        let mut p = PasteParser::new();
        for &b in b"\x1b[200~ab" {
            p.consume(b);
        }
        assert!(!p.is_pending_open(), "mid-content is not a partial open");
        assert!(p.flush_open().is_none());
        // The paste still completes correctly afterwards.
        let mut paste = None;
        for &b in b"c\x1b[201~" {
            if let PasteParseOutput::Paste(bs) = p.consume(b) {
                paste = Some(bs);
            }
        }
        assert_eq!(paste, Some(b"abc".to_vec()));
    }
}
