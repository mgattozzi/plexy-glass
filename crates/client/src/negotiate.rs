//! Outer-terminal keyboard-protocol probe, classification, and the
//! enable/teardown byte sets. Decode of focus/color-scheme replies lives in
//! `pump.rs`; this module owns the negotiation handshake and its precise
//! inverse.

use plexy_glass_protocol::NegotiatedKbd;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::time::{Duration, Instant};

/// The probe we write to the outer terminal: query Kitty flags (`\e[?u`),
/// XTVERSION (`\e[>q`), then DA1 (`\e[c`) **last** as the sentinel. Every
/// terminal answers DA1, and replies arrive in query order, so seeing the DA1
/// reply guarantees the Kitty and XTVERSION replies already arrived, and that
/// lets us consume them all and never leak a late XTVERSION DCS into a pane.
pub const PROBE: &[u8] = b"\x1b[?u\x1b[>q\x1b[c";

/// Kitty flags we PUSH on a Kitty-capable outer terminal. Deliberately
/// conservative: only `disambiguate` (0b1). We do NOT enable `report_event_types`
/// (every key reports a release too), `report_all_keys` (every key incl. plain
/// text + bare modifiers becomes a CSI-u event), or `report_associated_text`,
/// since those flood the input stream with events the per-pane down-convert
/// would have to reproduce perfectly, and any gap garbles typing (e.g. release
/// events double every keystroke). Disambiguation (Ctrl+i ≠ Tab, Ctrl+m ≠
/// Enter) is the high-value, low-risk subset: plain/shifted text still arrives
/// as legacy bytes, only genuinely ambiguous combos become CSI-u. Richer
/// fidelity for panes that negotiate full Kitty is a future opt-in once the
/// down-convert is battle-tested.
const OUTER_KITTY_FLAGS: u8 = 0b1;

/// Classify the outer terminal from the bytes it replied to `PROBE`.
///
/// - A Kitty flags report `\e[?<n>u` ⇒ Kitty (we push `OUTER_KITTY_FLAGS`).
/// - An XTVERSION DCS reply `\eP>|...\e\\` (and no Kitty report) ⇒
///   modifyOtherKeys-capable ⇒ ModifyOtherKeys(1) (level 1 = no release/glyph
///   reconstruction needed; conservative for the same reason as the Kitty flags).
/// - Otherwise (only DA1, or nothing) ⇒ Legacy.
pub fn classify(reply: &[u8]) -> NegotiatedKbd {
    if has_kitty_flags_report(reply) {
        NegotiatedKbd::Kitty(OUTER_KITTY_FLAGS)
    } else if has_xtversion_reply(reply) {
        NegotiatedKbd::ModifyOtherKeys(1)
    } else {
        NegotiatedKbd::Legacy
    }
}

/// True once `out` contains a DA1 reply, any CSI sequence ending in `c`
/// (`\e[?...c`). DA1 is sent last in `PROBE`, so this is the sentinel that all
/// earlier replies have arrived.
fn da1_reply_seen(out: &[u8]) -> bool {
    let mut i = 0;
    while i + 1 < out.len() {
        if out[i] == 0x1b && out[i + 1] == b'[' {
            let mut j = i + 2;
            // CSI parameter + intermediate bytes (0x20..=0x3f).
            while j < out.len() && (0x20..=0x3f).contains(&out[j]) {
                j += 1;
            }
            if j < out.len() && out[j] == b'c' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// True if `reply` contains a Kitty flags report `\e[?<digits>u`.
fn has_kitty_flags_report(reply: &[u8]) -> bool {
    let mut i = 0;
    while i + 2 < reply.len() {
        if reply[i] == 0x1b && reply[i + 1] == b'[' && reply[i + 2] == b'?' {
            let mut j = i + 3;
            let start = j;
            while j < reply.len() && reply[j].is_ascii_digit() {
                j += 1;
            }
            if j > start && j < reply.len() && reply[j] == b'u' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// True if `reply` contains an XTVERSION DCS reply `\eP>|...`.
fn has_xtversion_reply(reply: &[u8]) -> bool {
    reply.windows(3).any(|w| w == [0x1b, b'P', b'>'])
}

/// What the client enabled outward, so teardown is the precise inverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnabledCaps {
    pub kbd: NegotiatedKbd,
    pub focus_events: bool,
    pub color_scheme: bool,
}

impl EnabledCaps {
    /// The bytes to send to the outer terminal on attach, given the classified
    /// protocol. Always enables focus (`?1004h`) and color-scheme (`?2031h`).
    pub fn enable_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self.kbd {
            NegotiatedKbd::Kitty(flags) => {
                // Push flags on the outer terminal's stack.
                out.extend_from_slice(format!("\x1b[>{flags}u").as_bytes());
            }
            NegotiatedKbd::ModifyOtherKeys(level) => {
                out.extend_from_slice(format!("\x1b[>4;{level}m").as_bytes());
            }
            NegotiatedKbd::Legacy => {}
        }
        if self.focus_events {
            out.extend_from_slice(b"\x1b[?1004h");
        }
        if self.color_scheme {
            out.extend_from_slice(b"\x1b[?2031h");
        }
        out
    }

    /// The precise inverse of `enable_bytes`: disable exactly what was enabled,
    /// in reverse order.
    pub fn teardown_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.color_scheme {
            out.extend_from_slice(b"\x1b[?2031l");
        }
        if self.focus_events {
            out.extend_from_slice(b"\x1b[?1004l");
        }
        match self.kbd {
            NegotiatedKbd::Kitty(_) => out.extend_from_slice(b"\x1b[<u"),
            NegotiatedKbd::ModifyOtherKeys(_) => out.extend_from_slice(b"\x1b[>4;0m"),
            NegotiatedKbd::Legacy => {}
        }
        out
    }
}

/// Read the outer terminal's reply to `PROBE` for up to `budget`, returning the
/// accumulated bytes. Uses a short `poll(2)` per chunk so a terminal that does
/// not answer cannot hang the client. Best-effort: any error ends the read.
/// A signal interrupt (`EINTR`) during `poll` is treated as timeout and ends the
/// read early; the only consequence is a fallback to `Legacy` classification.
///
/// The fd is borrowed for the duration of the call and is never closed here.
pub fn read_probe_reply(fd: BorrowedFd<'_>, budget: Duration) -> Vec<u8> {
    use nix::libc;
    let raw = fd.as_raw_fd();
    let deadline = Instant::now() + budget;
    let mut out = Vec::new();
    let mut chunk = [0u8; 256];
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let ms = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
        let mut pfd = libc::pollfd {
            fd: raw,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd, count 1, finite non-negative timeout.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc <= 0 {
            break; // timeout or error, stop probing
        }
        // SAFETY: `raw` is a valid readable fd borrowed from the caller for the
        // whole call; `chunk` is a valid writable buffer of `chunk.len()` bytes.
        // We never close `raw` here, so the borrow outlives the read.
        let n = unsafe { libc::read(raw, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        let n = n as usize;
        out.extend_from_slice(&chunk[..n]);
        // Stop as soon as the DA1 reply (the sentinel, sent last) arrives, since
        // that guarantees the Kitty + XTVERSION replies already arrived, and
        // reading further would only capture user type-ahead. This is what
        // prevents a slow XTVERSION DCS from leaking past the probe into a pane.
        if da1_reply_seen(&out) {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_picks_kitty_from_flags_report() {
        // `\e[?31u` is a Kitty flags report; DA1 also present. We classify Kitty
        // but enable only the conservative disambiguate flag (not 31).
        let reply = b"\x1b[?1;2c\x1b[?31u";
        assert_eq!(classify(reply), NegotiatedKbd::Kitty(OUTER_KITTY_FLAGS));
        assert_eq!(OUTER_KITTY_FLAGS, 1, "only disambiguate — no release/all-keys flood");
    }

    #[test]
    fn classify_picks_modkeys_from_xtversion_only() {
        // No Kitty report, but an XTVERSION DCS reply, so conservative level 1.
        let reply = b"\x1b[?1;2c\x1bP>|ghostty 1.0\x1b\\";
        assert_eq!(classify(reply), NegotiatedKbd::ModifyOtherKeys(1));
    }

    #[test]
    fn da1_sentinel_only_fires_after_the_trailing_da1() {
        // Real Ghostty reply order: Kitty report, XTVERSION DCS, then DA1. The
        // sentinel must NOT fire until the trailing DA1 `c` is present, so the
        // XTVERSION DCS is fully captured (never leaked into a pane).
        assert!(!da1_reply_seen(b"\x1b[?31u\x1bP>|ghostty 1.3.1\x1b\\"));
        assert!(da1_reply_seen(b"\x1b[?31u\x1bP>|ghostty 1.3.1\x1b\\\x1b[?1;2c"));
        // A bare `c` in a version string must NOT be mistaken for DA1.
        assert!(!da1_reply_seen(b"\x1bP>|contour 1.0\x1b\\"));
    }

    #[test]
    fn classify_falls_back_to_legacy_on_da1_only() {
        let reply = b"\x1b[?1;2c";
        assert_eq!(classify(reply), NegotiatedKbd::Legacy);
        assert_eq!(classify(b""), NegotiatedKbd::Legacy);
    }

    #[test]
    fn kitty_flags_report_not_confused_with_decrpm() {
        // `\e[?1004;1$y` is a DECRPM reply (ends `$y`, not `u`), not Kitty.
        let reply = b"\x1b[?1004;1$y";
        assert!(!has_kitty_flags_report(reply));
        assert_eq!(classify(reply), NegotiatedKbd::Legacy);
    }

    #[test]
    fn teardown_is_exact_inverse_for_kitty() {
        let caps = EnabledCaps {
            kbd: NegotiatedKbd::Kitty(31),
            focus_events: true,
            color_scheme: true,
        };
        assert_eq!(caps.enable_bytes(), b"\x1b[>31u\x1b[?1004h\x1b[?2031h");
        assert_eq!(caps.teardown_bytes(), b"\x1b[?2031l\x1b[?1004l\x1b[<u");
    }

    #[test]
    fn teardown_is_exact_inverse_for_modkeys() {
        let caps = EnabledCaps {
            kbd: NegotiatedKbd::ModifyOtherKeys(2),
            focus_events: true,
            color_scheme: true,
        };
        assert_eq!(caps.enable_bytes(), b"\x1b[>4;2m\x1b[?1004h\x1b[?2031h");
        assert_eq!(caps.teardown_bytes(), b"\x1b[?2031l\x1b[?1004l\x1b[>4;0m");
    }

    #[test]
    fn teardown_for_legacy_only_touches_focus_and_theme() {
        let caps = EnabledCaps {
            kbd: NegotiatedKbd::Legacy,
            focus_events: true,
            color_scheme: true,
        };
        assert_eq!(caps.enable_bytes(), b"\x1b[?1004h\x1b[?2031h");
        assert_eq!(caps.teardown_bytes(), b"\x1b[?2031l\x1b[?1004l");
    }
}
