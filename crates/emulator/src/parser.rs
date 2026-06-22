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
    /// A complete DCS string (intermediates/action/params + payload), delivered
    /// at `unhook`. Used for XTGETTCAP (`DCS +q …`).
    fn handle_dcs(&mut self, intermediates: &[u8], action: u8, params: &[Vec<u16>], payload: &[u8]);
    /// A complete Kitty-graphics APC sequence (`ESC _ G … ST`), re-framed with
    /// its `ESC _` prefix and `ESC \` terminator. Captured ahead of `vte`
    /// (which discards APC). One call per chunk (no merging).
    fn handle_graphics(&mut self, framed: &[u8]);
}

/// APC pre-scan state. `vte` 0.15 drops APC, so we intercept `ESC _ … ST` ahead
/// of it; graphics APCs (`G…`) are diverted, everything else is forwarded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ApcState {
    Ground,
    Esc,
    Apc,
    ApcEsc,
}

/// One ordered pre-scan segment: bytes for `vte`, or a re-framed graphics APC.
enum Seg {
    Run(Vec<u8>),
    Graphics(Vec<u8>),
}

pub struct Parser {
    vte: vte::Parser,
    pending: String,
    dcs_intermediates: Vec<u8>,
    dcs_action: u8,
    dcs_params: Vec<Vec<u16>>,
    dcs_payload: Vec<u8>,
    in_dcs: bool,
    apc_state: ApcState,
    apc_buf: Vec<u8>,
}

impl Parser {
    pub fn new() -> Self {
        Self {
            vte: vte::Parser::new(),
            pending: String::new(),
            dcs_intermediates: Vec::new(),
            dcs_action: 0,
            dcs_params: Vec::new(),
            dcs_payload: Vec::new(),
            in_dcs: false,
            apc_state: ApcState::Ground,
            apc_buf: Vec::new(),
        }
    }

    pub fn advance<S: ScreenOps>(&mut self, screen: &mut S, bytes: &[u8]) {
        // Split into vte-bound runs and diverted graphics APCs, in order, so the
        // cursor is current when a placement is recorded.
        let segs = self.prescan(bytes);
        let mut perf = Performer {
            screen,
            pending: &mut self.pending,
            dcs_intermediates: &mut self.dcs_intermediates,
            dcs_action: &mut self.dcs_action,
            dcs_params: &mut self.dcs_params,
            dcs_payload: &mut self.dcs_payload,
            in_dcs: &mut self.in_dcs,
        };
        for seg in &segs {
            match seg {
                Seg::Run(run) => self.vte.advance(&mut perf, run),
                Seg::Graphics(framed) => perf.screen.handle_graphics(framed),
            }
        }
        // Flush any trailing complete grapheme. The final byte may have left a
        // partial cluster (say, a base char still waiting on combining marks), so
        // partials stay in the buffer until the next call.
        perf.flush_complete_clusters();
    }

    /// Pull `ESC _ … ST` graphics APCs out of `bytes`, returning ordered runs to
    /// feed `vte` interleaved with re-framed graphics blobs (one per chunk). The
    /// state persists across calls so a sequence split across reads is captured.
    fn prescan(&mut self, bytes: &[u8]) -> Vec<Seg> {
        const APC_CAP: usize = 16 * 1024 * 1024;
        let mut segs: Vec<Seg> = Vec::new();
        let mut run: Vec<u8> = Vec::new();
        let push = |buf: &mut Vec<u8>, b: u8| {
            if buf.len() < APC_CAP {
                buf.push(b);
            }
        };
        for &b in bytes {
            match self.apc_state {
                ApcState::Ground => {
                    if b == 0x1b {
                        self.apc_state = ApcState::Esc;
                    } else {
                        run.push(b);
                    }
                }
                ApcState::Esc => {
                    if b == 0x5f {
                        self.apc_state = ApcState::Apc;
                        self.apc_buf.clear();
                    } else {
                        run.push(0x1b);
                        if b == 0x1b {
                            self.apc_state = ApcState::Esc;
                        } else {
                            run.push(b);
                            self.apc_state = ApcState::Ground;
                        }
                    }
                }
                ApcState::Apc => {
                    if b == 0x9c {
                        flush_run(&mut segs, &mut run);
                        push_graphics(&mut segs, &mut self.apc_buf);
                        self.apc_state = ApcState::Ground;
                    } else if b == 0x1b {
                        self.apc_state = ApcState::ApcEsc;
                    } else {
                        push(&mut self.apc_buf, b);
                    }
                }
                ApcState::ApcEsc => {
                    if b == 0x5c {
                        flush_run(&mut segs, &mut run);
                        push_graphics(&mut segs, &mut self.apc_buf);
                        self.apc_state = ApcState::Ground;
                    } else {
                        push(&mut self.apc_buf, 0x1b);
                        if b == 0x1b {
                            self.apc_state = ApcState::ApcEsc;
                        } else {
                            push(&mut self.apc_buf, b);
                            self.apc_state = ApcState::Apc;
                        }
                    }
                }
            }
        }
        flush_run(&mut segs, &mut run);
        segs
    }

    /// Force-flush any pending grapheme cluster to the screen. Use this at
    /// end-of-stream or end-of-test; not needed during streaming because each
    /// non-print event flushes implicitly.
    pub fn flush<S: ScreenOps>(&mut self, screen: &mut S) {
        if self.pending.is_empty() {
            return;
        }
        let buf = std::mem::take(&mut self.pending);
        for cluster in buf.graphemes(true) {
            screen.put_grapheme(cluster);
        }
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

/// Move accumulated vte-bound bytes into a `Run` segment.
fn flush_run(segs: &mut Vec<Seg>, run: &mut Vec<u8>) {
    if !run.is_empty() {
        segs.push(Seg::Run(std::mem::take(run)));
    }
}

/// Finalize a completed APC: graphics APCs (`G…`) are re-framed and diverted as
/// one segment per chunk; other APCs are dropped.
fn push_graphics(segs: &mut Vec<Seg>, buf: &mut Vec<u8>) {
    if buf.first() == Some(&b'G') {
        let mut framed = Vec::with_capacity(buf.len() + 3);
        framed.extend_from_slice(b"\x1b_");
        framed.extend_from_slice(buf);
        framed.extend_from_slice(b"\x1b\\");
        segs.push(Seg::Graphics(framed));
    }
    buf.clear();
}

struct Performer<'a, S: ScreenOps> {
    screen: &'a mut S,
    pending: &'a mut String,
    dcs_intermediates: &'a mut Vec<u8>,
    dcs_action: &'a mut u8,
    dcs_params: &'a mut Vec<Vec<u16>>,
    dcs_payload: &'a mut Vec<u8>,
    in_dcs: &'a mut bool,
}

impl<S: ScreenOps> Performer<'_, S> {
    /// Flush all *complete* graphemes from `pending`, retaining the last one
    /// (which may still be growing via combining marks).
    fn flush_complete_clusters(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        // Bound a single pathologically-large grapheme cluster: a base char
        // followed by an unbounded combining-mark run ("Zalgo" flood) never
        // completes, so `pending` would grow without bound and each print()
        // would re-clone + re-segment the whole buffer (O(n^2) time, O(n)
        // memory). `pending` only ever retains the last (incomplete) cluster,
        // so exceeding this cap means a single cluster has grown pathologically,
        // and we force it onto the screen. Mirrors the DCS_CAP guard; adversarial
        // input only, far above any legitimate cluster.
        const PENDING_CAP: usize = 4096;
        if self.pending.len() > PENDING_CAP {
            self.flush_all();
            return;
        }
        // Find the byte offset where the LAST grapheme begins. Everything before
        // it is complete and can be flushed; the trailing grapheme stays in
        // `pending` (it may still grow via combining marks). Working off offsets
        // avoids cloning the whole buffer + a Vec<&str> on every print().
        let last_start = match self.pending.grapheme_indices(true).next_back() {
            Some((i, _)) if i > 0 => i,
            // Empty handled above; offset 0 = a single (still-growing) cluster.
            _ => return,
        };
        for (offset, cluster) in self.pending.grapheme_indices(true) {
            if offset >= last_start {
                break;
            }
            self.screen.put_grapheme(cluster);
        }
        self.pending.drain(..last_start);
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

    fn hook(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.flush_all();
        *self.in_dcs = true;
        *self.dcs_action = action as u8;
        self.dcs_intermediates.clear();
        self.dcs_intermediates.extend_from_slice(intermediates);
        self.dcs_params.clear();
        self.dcs_params.extend(params.iter().map(|g| g.to_vec()));
        self.dcs_payload.clear();
    }

    fn put(&mut self, byte: u8) {
        if *self.in_dcs {
            // Bound the payload so a child can't OOM us via a giant DCS. Raised
            // from 64 KiB for Sixel, since sixel images routinely exceed that.
            const DCS_CAP: usize = 4 * 1024 * 1024;
            if self.dcs_payload.len() < DCS_CAP {
                self.dcs_payload.push(byte);
            }
        }
    }

    fn unhook(&mut self) {
        if *self.in_dcs {
            *self.in_dcs = false;
            self.screen.handle_dcs(
                self.dcs_intermediates,
                *self.dcs_action,
                self.dcs_params,
                self.dcs_payload,
            );
            self.dcs_payload.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured DCS dispatch: (intermediates, action, params, payload).
    type DcsRecord = (Vec<u8>, u8, Vec<Vec<u16>>, Vec<u8>);

    #[derive(Default)]
    struct MockScreen {
        graphemes: Vec<String>,
        c0: Vec<u8>,
        csi: Vec<(Vec<Vec<u16>>, Vec<u8>, char)>,
        osc: Vec<Vec<Vec<u8>>>,
        esc: Vec<(Vec<u8>, u8)>,
        dcs: Vec<DcsRecord>,
        graphics: Vec<Vec<u8>>,
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
        fn handle_dcs(
            &mut self,
            intermediates: &[u8],
            action: u8,
            params: &[Vec<u16>],
            payload: &[u8],
        ) {
            self.dcs.push((
                intermediates.to_vec(),
                action,
                params.to_vec(),
                payload.to_vec(),
            ));
        }
        fn handle_graphics(&mut self, framed: &[u8]) {
            self.graphics.push(framed.to_vec());
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
    fn dcs_xtgettcap_accumulates_payload() {
        // \eP+q636f6c6f7273\e\\ has the '+' intermediate, 'q' action, hex payload.
        let s = drive(b"\x1bP+q636f6c6f7273\x1b\\");
        assert_eq!(s.dcs.len(), 1);
        let (ints, action, _params, payload) = &s.dcs[0];
        assert_eq!(ints, b"+");
        assert_eq!(*action, b'q');
        assert_eq!(payload, b"636f6c6f7273");
    }

    #[test]
    fn combining_mark_attaches_to_base() {
        // "a" + COMBINING ACUTE ACCENT (U+0301) = one grapheme cluster "á".
        let s = drive("a\u{0301}b".as_bytes());
        // "á" should be flushed when "b" arrives.
        assert_eq!(s.graphemes, vec!["a\u{0301}"]);
    }

    #[test]
    fn dcs_payload_is_capped_at_dcs_cap() {
        // A DCS payload larger than DCS_CAP (4 MiB; raised for Sixel) must be
        // truncated, not grown unbounded, and still dispatch exactly once.
        const DCS_CAP: usize = 4 * 1024 * 1024;
        let mut input = Vec::from(&b"\x1bP+q"[..]);
        input.extend(std::iter::repeat_n(b'a', DCS_CAP + 100));
        input.extend_from_slice(b"\x1b\\");
        let s = drive(&input);
        assert_eq!(s.dcs.len(), 1, "one DCS dispatched");
        let (_ints, _action, _params, payload) = &s.dcs[0];
        assert_eq!(payload.len(), DCS_CAP, "payload truncated to the cap");
    }

    #[test]
    fn graphics_apc_diverted_per_chunk_text_passes_through() {
        // Two back-to-back graphics chunks are captured as TWO segments (no
        // merge), and surrounding text still reaches vte.
        let s = drive(b"hi\x1b_Ga=T,m=1;AA\x1b\\\x1b_Gm=0;BB\x1b\\");
        assert_eq!(s.graphics.len(), 2, "one capture per chunk");
        assert_eq!(s.graphics[0], b"\x1b_Ga=T,m=1;AA\x1b\\");
        assert_eq!(s.graphics[1], b"\x1b_Gm=0;BB\x1b\\");
        assert!(s.graphemes.contains(&"h".to_string()));
    }

    #[test]
    fn graphics_apc_split_across_reads_is_captured_whole() {
        let mut p = Parser::new();
        let mut s = MockScreen::default();
        p.advance(&mut s, b"\x1b_Ga=T,f=100;AA");
        p.advance(&mut s, b"BB\x1b\\");
        assert_eq!(s.graphics, vec![b"\x1b_Ga=T,f=100;AABB\x1b\\".to_vec()]);
    }

    #[test]
    fn csi_after_graphics_still_dispatches() {
        let s = drive(b"\x1b_Ga=T;AA\x1b\\\x1b[31m");
        assert_eq!(s.graphics.len(), 1);
        assert_eq!(s.csi.len(), 1);
        assert_eq!(s.csi[0].2, 'm');
    }

    #[test]
    fn combining_mark_flood_is_bounded() {
        // A base char + a long combining-mark run ("Zalgo") never completes a
        // cluster, so `pending` would grow without bound and each print() would
        // re-clone + re-segment the whole buffer. The cap force-flushes it,
        // keeping `pending` bounded and making forward progress.
        let mut p = Parser::new();
        let mut s = MockScreen::default();
        let mut input = String::from("a");
        for _ in 0..5000 {
            input.push('\u{0301}'); // COMBINING ACUTE ACCENT (2 bytes each)
        }
        p.advance(&mut s, input.as_bytes());
        assert!(
            !s.graphemes.is_empty(),
            "the cap must force-flush the mega-cluster"
        );
        assert!(
            p.pending.len() <= 8192,
            "pending must stay bounded (cap 4096 + slack), got {}",
            p.pending.len()
        );
    }
}
