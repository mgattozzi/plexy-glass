//! VTE-backed ANSI parser. Delivers callbacks via the `ScreenOps` trait so the
//! parser can be unit-tested independently of `Screen`.

use std::mem;

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

/// Pre-scan state ahead of `vte`. `vte` 0.15 drops APC, so we intercept
/// `ESC _ … ST` (graphics APCs `G…` are diverted, other APCs dropped); and
/// under the `std` feature `vte` accumulates OSC into an unbounded `Vec`, so we
/// also track `ESC ] … ST` / `0x9d … ST` to truncate a hostile OSC body at a
/// fixed cap before it reaches `vte` (the bytes are still forwarded, just
/// bounded). Everything else is forwarded untouched.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Ground,
    Esc,
    Apc,
    ApcEsc,
    /// Inside an OSC body; forwarding bytes to `vte` but capping the count.
    Osc,
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
    apc_state: ScanState,
    apc_buf: Vec<u8>,
    /// OSC body bytes forwarded to `vte` for the OSC currently being scanned.
    /// Persists across `advance` calls so a sequence split across reads is
    /// capped as a whole, not per read. Reset when a new OSC begins.
    osc_len: usize,
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
            apc_state: ScanState::Ground,
            apc_buf: Vec::new(),
            osc_len: 0,
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
        // Cap the OSC body forwarded to `vte` (same order as DCS_CAP). Past this
        // the body bytes are dropped, so `vte`'s unbounded (std-feature) OSC Vec
        // can't grow without bound on a hostile/unterminated OSC.
        const OSC_CAP: usize = 4 * 1024 * 1024;
        let mut segs: Vec<Seg> = Vec::new();
        let mut run: Vec<u8> = Vec::new();
        let push = |buf: &mut Vec<u8>, b: u8| {
            // Equivalent note: `< APC_CAP` vs `<= APC_CAP` is off-by-one in a 16 MiB
            // buffer; the maximum buffer size differs by exactly 1 byte, which is
            // indistinguishable in any practical test (would require exactly APC_CAP bytes).
            if buf.len() < APC_CAP {
                buf.push(b);
            }
        };
        for &b in bytes {
            match self.apc_state {
                ScanState::Ground => {
                    if b == 0x1b {
                        self.apc_state = ScanState::Esc;
                    } else if b == 0x9d {
                        // C1 OSC introducer: forward it and start capping the body.
                        run.push(b);
                        self.apc_state = ScanState::Osc;
                        self.osc_len = 0;
                    } else {
                        run.push(b);
                    }
                }
                ScanState::Esc => {
                    if b == 0x5f {
                        self.apc_state = ScanState::Apc;
                        self.apc_buf.clear();
                    } else if b == 0x5d {
                        // `ESC ]` OSC introducer: forward it, cap the body.
                        run.push(0x1b);
                        run.push(0x5d);
                        self.apc_state = ScanState::Osc;
                        self.osc_len = 0;
                    } else {
                        run.push(0x1b);
                        if b == 0x1b {
                            self.apc_state = ScanState::Esc;
                        } else {
                            run.push(b);
                            self.apc_state = ScanState::Ground;
                        }
                    }
                }
                ScanState::Osc => {
                    // `vte`'s `advance_osc_string` leaves OSC ONLY on BEL
                    // (0x07), CAN/SUB (0x18/0x1a), or ESC (0x1b, for ST =
                    // `ESC \`) -- see vte 0.15.0 src/lib.rs `advance_osc_string`.
                    // 0x9c (C1 ST) is NOT one of those arms: `vte` falls through
                    // to `action_osc_put` and keeps collecting body bytes, so it
                    // must NOT be treated as a terminator here either, or a
                    // hostile OSC that plants a bare 0x9c mid-body would make
                    // this pre-scan stop counting/capping while `vte` keeps
                    // growing its unbounded (std-feature) Vec underneath us --
                    // exactly the OOM this cap exists to prevent. On ESC we
                    // defer to the Esc arm (which also re-enters Osc on a
                    // chained `ESC ]`, so each OSC is capped independently).
                    // Every real terminator is forwarded so `vte` still
                    // dispatches. Body bytes (including 0x9c) are forwarded
                    // only up to the cap, then dropped.
                    match b {
                        0x1b => self.apc_state = ScanState::Esc,
                        0x07 | 0x18 | 0x1a => {
                            run.push(b);
                            self.apc_state = ScanState::Ground;
                        }
                        _ => {
                            if self.osc_len < OSC_CAP {
                                run.push(b);
                                self.osc_len += 1;
                            }
                        }
                    }
                }
                ScanState::Apc => {
                    if b == 0x9c {
                        flush_run(&mut segs, &mut run);
                        push_graphics(&mut segs, &mut self.apc_buf);
                        self.apc_state = ScanState::Ground;
                    } else if b == 0x1b {
                        self.apc_state = ScanState::ApcEsc;
                    } else {
                        push(&mut self.apc_buf, b);
                    }
                }
                ScanState::ApcEsc => {
                    if b == 0x5c {
                        flush_run(&mut segs, &mut run);
                        push_graphics(&mut segs, &mut self.apc_buf);
                        self.apc_state = ScanState::Ground;
                    } else {
                        push(&mut self.apc_buf, 0x1b);
                        if b == 0x1b {
                            self.apc_state = ScanState::ApcEsc;
                        } else {
                            push(&mut self.apc_buf, b);
                            self.apc_state = ScanState::Apc;
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
        let buf = mem::take(&mut self.pending);
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
        segs.push(Seg::Run(mem::take(run)));
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
        // Find the byte offset where the LAST grapheme begins. Everything before it
        // is complete and can be flushed; the trailing grapheme stays in `pending`
        // since it may still grow via combining marks. Working off offsets avoids
        // cloning the whole buffer plus a `Vec<&str>` on every `print()`.
        // Equivalent note: the guard `i > 0` vs no guard (or `i >= 0`) is equivalent
        // because when i=0 the for loop condition `offset >= last_start=0` triggers
        // immediately and `drain(..0)` is a no-op, the same result as the early
        // return.
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
        let buf = mem::take(self.pending);
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
        self.dcs_params.extend(params.iter().map(<[u16]>::to_vec));
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
    use std::iter;

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
            let p: Vec<Vec<u16>> = params.iter().map(<[u16]>::to_vec).collect();
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
        input.extend(iter::repeat_n(b'a', DCS_CAP + 100));
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

    #[test]
    fn graphics_apc_payload_survives_1mb_size() {
        // APC_CAP = 16 * 1024 * 1024. If either `*` were replaced by `+` the cap
        // would fall to ~17 KiB or ~1 MiB. A 1.2 MiB payload fits within the
        // correct 16 MiB cap but would be truncated by either mutated constant.
        let payload_size: usize = 1_200_000;
        let mut input = Vec::with_capacity(payload_size + 8);
        input.extend_from_slice(b"\x1b_G");
        input.extend(iter::repeat_n(b'A', payload_size));
        input.extend_from_slice(b"\x1b\\");
        let s = drive(&input);
        assert_eq!(s.graphics.len(), 1, "graphics APC must be captured");
        // framing: ESC _ (2) + G (1) + As (payload_size) + ESC \ (2) = payload_size + 5
        assert_eq!(
            s.graphics[0].len(),
            payload_size + 5,
            "full payload must not be truncated by a too-small APC_CAP"
        );
    }

    #[test]
    fn apc_esc_inside_payload_before_terminator() {
        // ESC _ G <data> ESC <non-backslash> <more-data> ESC \
        // The inner ESC is pushed into the APC payload (literal), and only the
        // final ESC \ terminates. Mutating `b == 0x1b` → `b != 0x1b` in the
        // ApcEsc branch would cause the APC to never terminate.
        let input = b"\x1b_Ga\x1bb\x1b\\"; // ESC _ G a ESC b ESC \
        let s = drive(input);
        assert_eq!(s.graphics.len(), 1, "APC with inner ESC must be captured");
        // framed = ESC _ (2) + [G, a, ESC, b] (4) + ESC \ (2) = 8 bytes
        assert_eq!(
            s.graphics[0], b"\x1b_Ga\x1bb\x1b\\",
            "inner ESC must be preserved in the payload"
        );
    }

    #[test]
    fn apc_double_esc_then_terminator() {
        // ESC _ G <data> ESC ESC \ : the first ESC goes into the payload,
        // the second ESC starts the string terminator, and \ completes it.
        // Mutating the `b == 0x1b` arm in ApcEsc causes this to push both
        // ESCs into the payload and never terminate.
        let input = b"\x1b_Gdata\x1b\x1b\\"; // ESC _ G d a t a ESC ESC \
        let s = drive(input);
        assert_eq!(
            s.graphics.len(),
            1,
            "double-ESC terminated APC must be captured"
        );
        // framed = ESC _ (2) + [G,d,a,t,a,ESC] (6) + ESC \ (2) = 10 bytes
        assert_eq!(
            s.graphics[0], b"\x1b_Gdata\x1b\x1b\\",
            "inner ESC must be part of payload; only the second ESC is the terminator prefix"
        );
    }

    #[test]
    fn osc_body_is_capped_in_the_prescan() {
        // vte 0.15 under the std feature accumulates OSC into an unbounded Vec,
        // so a child streaming a giant OSC could OOM the daemon. The pre-scan
        // truncates the body at OSC_CAP (4 MiB) before vte sees it; the OSC still
        // dispatches once (terminator forwarded), just with a bounded body.
        let mut input = Vec::from(&b"\x1b]0;"[..]);
        input.extend(iter::repeat_n(b'A', 5 * 1024 * 1024)); // 5 MiB > OSC_CAP
        input.extend_from_slice(b"\x07");
        let s = drive(&input);
        assert_eq!(s.osc.len(), 1, "one OSC dispatched");
        // params[0] = "0", params[1] = the (capped) body. Without the cap the
        // body would be the full 5 MiB.
        assert!(
            s.osc[0][1].len() <= 4 * 1024 * 1024,
            "OSC body must be capped; got {}",
            s.osc[0][1].len()
        );
        assert!(!s.osc[0][1].is_empty(), "the capped body still reaches vte");
    }

    #[test]
    fn osc_0x9c_mid_body_does_not_disable_the_cap() {
        // 0x9c is the C1 form of ST, but `vte` 0.15's `advance_osc_string` does
        // NOT treat it as a terminator (only 0x07/0x18/0x1a/0x1b are), so `vte`
        // just keeps collecting it as a body byte. If the pre-scan mistakenly
        // treated a bare 0x9c as "OSC over", it would stop counting/capping
        // right there and return to Ground, while `vte` stayed in OscString and
        // kept pushing every following byte into its unbounded Vec -- a DoS
        // bypass of the cap. Plant a 0x9c mid-body, then flood past OSC_CAP:
        // the body `vte` sees must still be bounded.
        let mut input = Vec::from(&b"\x1b]0;\x9c"[..]);
        input.extend(iter::repeat_n(b'A', 5 * 1024 * 1024)); // 5 MiB > OSC_CAP
        input.extend_from_slice(b"\x07");
        let s = drive(&input);
        assert_eq!(s.osc.len(), 1, "one OSC dispatched");
        assert!(
            s.osc[0][1].len() <= 4 * 1024 * 1024,
            "OSC body must stay capped even with a 0x9c byte inside it; got {}",
            s.osc[0][1].len()
        );
        assert!(!s.osc[0][1].is_empty(), "the capped body still reaches vte");
    }

    #[test]
    fn osc_st_terminator_leaves_osc_and_following_csi_dispatches() {
        // `ESC \` (ST) ends the OSC via the Esc arm; a CSI right after must still
        // dispatch, proving the pre-scan left the Osc state cleanly.
        let s = drive(b"\x1b]0;hi\x1b\\\x1b[31m");
        assert_eq!(s.osc.len(), 1);
        assert_eq!(s.osc[0][1], b"hi");
        assert_eq!(s.csi.len(), 1, "the CSI after the OSC must dispatch");
        assert_eq!(s.csi[0].2, 'm');
    }

    #[test]
    fn unterminated_osc_does_not_swallow_the_next_osc() {
        // An unterminated giant OSC must not park the parser forever: the ESC
        // that starts the next sequence ends the flood, and the next OSC lands
        // (a chained `ESC ]` re-enters the Osc state, capping it independently).
        let mut input = Vec::from(&b"\x1b]0;"[..]);
        input.extend(iter::repeat_n(b'A', 5 * 1024 * 1024)); // no terminator
        input.extend_from_slice(b"\x1b]2;after\x07");
        let s = drive(&input);
        // The following OSC must land whether or not vte also dispatched the
        // aborted flood, so assert on the LAST dispatch, which is "after" either way.
        let last = s.osc.last().expect("the following OSC must dispatch");
        assert_eq!(last[0], b"2");
        assert_eq!(last[1], b"after");
    }

    #[test]
    fn combining_mark_cap_exact_boundary() {
        // PENDING_CAP = 4096. A cluster of exactly 4096 bytes (2-byte base +
        // 2047 x 2-byte combining marks) should NOT be force-flushed yet, because
        // the check is `pending.len() > 4096`, which is false at exactly 4096.
        // If the comparison were `>=`, flush_all would fire one print() earlier.
        let mut p = Parser::new();
        let mut s = MockScreen::default();
        let mut input = String::from("\u{00C0}"); // U+00C0 = 2 bytes in UTF-8
        for _ in 0..2047 {
            input.push('\u{0301}'); // COMBINING ACUTE ACCENT = 2 bytes each
        }
        assert_eq!(input.len(), 4096, "sanity: exactly PENDING_CAP bytes");
        p.advance(&mut s, input.as_bytes());
        // With `>`: the cluster occupies exactly 4096 bytes, below the cap, so no
        // force-flush; it stays in pending as a growing cluster.
        assert!(
            s.graphemes.is_empty(),
            "at exactly PENDING_CAP bytes the cluster must NOT be force-flushed yet"
        );
        assert_eq!(
            p.pending.len(),
            4096,
            "the 4096-byte cluster stays in pending until the cap is exceeded"
        );
    }
}
