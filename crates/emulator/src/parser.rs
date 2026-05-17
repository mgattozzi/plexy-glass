//! VTE-backed ANSI parser. Delivers callbacks via the `ScreenOps` trait so the
//! parser can be unit-tested independently of `Screen`.

use unicode_segmentation::UnicodeSegmentation;

/// Operations the parser invokes on a screen-like sink. `Screen` will impl this.
pub trait ScreenOps {
    fn put_grapheme(&mut self, cluster: &str);
    fn execute_c0(&mut self, byte: u8);
    fn handle_csi(&mut self, params: &vte::Params, intermediates: &[u8], action: char);
    fn handle_osc(&mut self, params: &[&[u8]]);
    fn handle_esc(&mut self, intermediates: &[u8], byte: u8);
}

pub struct Parser {
    vte: vte::Parser,
    pending: String,
}

impl Parser {
    pub fn new() -> Self {
        Self {
            vte: vte::Parser::new(),
            pending: String::new(),
        }
    }

    pub fn advance<S: ScreenOps>(&mut self, screen: &mut S, bytes: &[u8]) {
        let mut perf = Performer {
            screen,
            pending: &mut self.pending,
        };
        self.vte.advance(&mut perf, bytes);
        // Flush any trailing complete grapheme. The final byte may have left a
        // partial cluster (say, a base char still waiting on combining marks), so
        // partials stay in the buffer until the next call.
        perf.flush_complete_clusters();
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

struct Performer<'a, S: ScreenOps> {
    screen: &'a mut S,
    pending: &'a mut String,
}

impl<S: ScreenOps> Performer<'_, S> {
    /// Flush all *complete* graphemes from `pending`, retaining the last one
    /// (which may still be growing via combining marks).
    fn flush_complete_clusters(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let snapshot = self.pending.clone();
        let clusters: Vec<&str> = snapshot.graphemes(true).collect();
        if clusters.len() <= 1 {
            // Could be one growing cluster, so don't flush yet.
            return;
        }
        self.pending.clear();
        for cluster in &clusters[..clusters.len() - 1] {
            self.screen.put_grapheme(cluster);
        }
        self.pending.push_str(clusters[clusters.len() - 1]);
    }

    /// Force-flush the entire pending buffer (called on non-print events).
    fn flush_all(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let buf = std::mem::take(self.pending);
        for cluster in buf.graphemes(true) {
            self.screen.put_grapheme(cluster);
        }
    }
}

impl<S: ScreenOps> vte::Perform for Performer<'_, S> {
    fn print(&mut self, c: char) {
        self.pending.push(c);
        self.flush_complete_clusters();
    }

    fn execute(&mut self, byte: u8) {
        self.flush_all();
        self.screen.execute_c0(byte);
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        self.flush_all();
        self.screen.handle_csi(params, intermediates, action);
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.flush_all();
        self.screen.handle_osc(params);
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.flush_all();
        self.screen.handle_esc(intermediates, byte);
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockScreen {
        graphemes: Vec<String>,
        c0: Vec<u8>,
        csi: Vec<(Vec<Vec<u16>>, Vec<u8>, char)>,
        osc: Vec<Vec<Vec<u8>>>,
        esc: Vec<(Vec<u8>, u8)>,
    }

    impl ScreenOps for MockScreen {
        fn put_grapheme(&mut self, cluster: &str) {
            self.graphemes.push(cluster.to_string());
        }
        fn execute_c0(&mut self, byte: u8) {
            self.c0.push(byte);
        }
        fn handle_csi(&mut self, params: &vte::Params, intermediates: &[u8], action: char) {
            let p: Vec<Vec<u16>> = params.iter().map(|s| s.to_vec()).collect();
            self.csi.push((p, intermediates.to_vec(), action));
        }
        fn handle_osc(&mut self, params: &[&[u8]]) {
            self.osc.push(params.iter().map(|p| p.to_vec()).collect());
        }
        fn handle_esc(&mut self, intermediates: &[u8], byte: u8) {
            self.esc.push((intermediates.to_vec(), byte));
        }
    }

    fn drive(input: &[u8]) -> MockScreen {
        let mut p = Parser::new();
        let mut s = MockScreen::default();
        p.advance(&mut s, input);
        s
    }

    #[test]
    fn ascii_text_dispatches_per_grapheme() {
        let s = drive(b"abc");
        assert_eq!(s.graphemes, vec!["a", "b"]);
        // "c" is the last cluster and may still be growing, so it stays in pending.
    }

    #[test]
    fn control_byte_flushes_pending() {
        let s = drive(b"ab\nx");
        // After "ab", "a" is flushed and "b" stays pending. The \n flushes "b".
        // Then "x" is the new last cluster, pending.
        assert_eq!(s.graphemes, vec!["a", "b"]);
        assert_eq!(s.c0, vec![b'\n']);
    }

    #[test]
    fn csi_dispatches() {
        let s = drive(b"\x1b[1;31m");
        assert_eq!(s.csi.len(), 1);
        let (params, _ints, action) = &s.csi[0];
        assert_eq!(action, &'m');
        assert_eq!(params, &vec![vec![1u16], vec![31u16]]);
    }

    #[test]
    fn osc_dispatches() {
        let s = drive(b"\x1b]0;hello\x07");
        assert_eq!(s.osc.len(), 1);
        // First param is the OSC command number "0"; second is "hello".
        assert_eq!(s.osc[0][0], b"0");
        assert_eq!(s.osc[0][1], b"hello");
    }

    #[test]
    fn combining_mark_attaches_to_base() {
        // "a" + COMBINING ACUTE ACCENT (U+0301) = one grapheme cluster "á".
        let s = drive("a\u{0301}b".as_bytes());
        // "á" should be flushed when "b" arrives.
        assert_eq!(s.graphemes, vec!["a\u{0301}"]);
    }
}
