//! Static terminfo/termcap capability table, the single source of truth for
//! what plexy-glass can render, answered authoritatively to XTGETTCAP (`DCS
//! +q`). We never proxy the host terminal's caps (tmux's lesson): a pane only
//! ever learns of a cap plexy-glass actually honors.

/// One capability's value, as XTGETTCAP would report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    /// Value-less present boolean (e.g. `Su`, `Tc`), so we reply `1+r<name>`.
    Boolean,
    /// Numeric cap (e.g. `Co`=256, `RGB`=8). The value is the decimal digits.
    Num(u32),
    /// String cap, the literal terminfo string (parameterized caps keep `\E`
    /// literal; the hex encoder hex-encodes the bytes as given).
    Str(String),
    /// Not advertised, so we reply `0+r<name>`.
    Unsupported,
}

/// The parameterized colored-underline setter (G9, honest because Phase 0
/// renders it). Kitty/foot's exact `Setulc` string.
///
/// Underline honesty: both the underline COLOR (`Setulc` / SGR 58) and the
/// underline STYLE (`Smulx` / SGR `4:N`) are rendered end-to-end. `handle_sgr`
/// records the `4:0`..`4:5` sub-parameter as a distinct `UnderlineStyle` on the
/// cell pen (with `Attrs::UNDERLINE` remaining the any-underline boolean), and
/// the diff renderer re-emits `4:N` to the outer terminal, so `4:3` (curly)
/// renders as a curl, not a straight underline, with the color independent.
const SETULC: &str = "\\E[58:2:%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m";
/// Styled-underline (undercurl) selector, unlocks `4:3` undercurl in vim/nvim.
/// The STYLE is preserved end-to-end: `handle_sgr` maps `4:0`..`4:5` to a
/// `UnderlineStyle` on the cell and the diff renderer re-emits `4:N`, so curly /
/// double / dotted / dashed survive to the outer terminal (see `SETULC` above).
const SMULX: &str = "\\E[4:%p1%dm";
const SETRGBF: &str = "\\E[38:2:%p1%d:%p2%d:%p3%dm";
const SETRGBB: &str = "\\E[48:2:%p1%d:%p2%d:%p3%dm";

/// Resolve a cap name (termcap *or* terminfo spelling) for the given `$TERM`.
/// `TN`/`name` synthesize from `term`; everything else is a static value.
pub fn lookup(name: &str, term: &str) -> Capability {
    match name {
        // Terminal name, both spellings.
        "TN" | "name" => Capability::Str(term.to_string()),
        // Colors (numeric).
        "Co" | "colors" => Capability::Num(256),
        // Truecolor: RGB is an INTEGER (bits/channel), not a bare boolean.
        "RGB" => Capability::Num(8),
        // Value-less present booleans.
        "Tc" | "Su" => Capability::Boolean,
        // Parameterized strings.
        "Smulx" => Capability::Str(SMULX.to_string()),
        "Setulc" => Capability::Str(SETULC.to_string()),
        "setrgbf" => Capability::Str(SETRGBF.to_string()),
        "setrgbb" => Capability::Str(SETRGBB.to_string()),
        // Alt screen / italics / bracketed paste (unparameterized, so `\E` expands).
        "smcup" => Capability::Str("\x1b[?1049h".to_string()),
        "rmcup" => Capability::Str("\x1b[?1049l".to_string()),
        "sitm" => Capability::Str("\x1b[3m".to_string()),
        "ritm" => Capability::Str("\x1b[23m".to_string()),
        "BE" => Capability::Str("\x1b[?2004h".to_string()),
        "BD" => Capability::Str("\x1b[?2004l".to_string()),
        "PS" => Capability::Str("\x1b[200~".to_string()),
        "PE" => Capability::Str("\x1b[201~".to_string()),
        // OSC 52 clipboard (parameterized, keep \E literal).
        "Ms" => Capability::Str("\\E]52;%p1%s;%p2%s\\E\\\\".to_string()),
        // Key caps we actually emit.
        "kbs" => Capability::Str("\\177".to_string()),
        "kcuu1" => Capability::Str("\\EOA".to_string()),
        "kcud1" => Capability::Str("\\EOB".to_string()),
        "kcuf1" => Capability::Str("\\EOC".to_string()),
        "kcub1" => Capability::Str("\\EOD".to_string()),
        "kHOM" => Capability::Str("\\E[1;2H".to_string()),
        "kEND" => Capability::Str("\\E[1;2F".to_string()),
        "kDC" => Capability::Str("\\E[3;2~".to_string()),
        "kPRV" => Capability::Str("\\E[5;2~".to_string()),
        "kNXT" => Capability::Str("\\E[6;2~".to_string()),
        // Everything else (e.g. `Setulc1` / `ol` / `oc` / `bce`: no indexed `Setulc`
        // variant, palette-reset, or hyperlink terminfo cap) is unsupported.
        _ => Capability::Unsupported,
    }
}

/// Lowercase-hex-encode bytes (2 digits/byte) for XTGETTCAP names/values.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        // invariant: writing to a `String` never fails.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Decode a hex-ASCII cap name. Returns `None` on odd length or non-hex digit.
pub fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    for pair in bytes.chunks_exact(2) {
        out.push((nib(pair[0])? << 4) | nib(pair[1])?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_numeric_cap() {
        assert_eq!(lookup("colors", "xterm-256color"), Capability::Num(256));
        assert_eq!(lookup("Co", "xterm-256color"), Capability::Num(256));
    }

    #[test]
    fn lookup_boolean_cap() {
        assert_eq!(lookup("Su", "xterm-256color"), Capability::Boolean);
        assert_eq!(lookup("Tc", "xterm-256color"), Capability::Boolean);
    }

    #[test]
    fn lookup_rgb_is_integer_not_boolean() {
        assert_eq!(lookup("RGB", "xterm-256color"), Capability::Num(8));
    }

    #[test]
    fn lookup_string_cap() {
        assert!(matches!(
            lookup("Setulc", "xterm-256color"),
            Capability::Str(_)
        ));
    }

    #[test]
    fn lookup_tn_uses_term() {
        match lookup("TN", "screen-256color") {
            Capability::Str(s) => assert_eq!(s.as_str(), "screen-256color"),
            other => panic!("TN should be a string: {other:?}"),
        }
    }

    #[test]
    fn lookup_unsupported_caps() {
        assert_eq!(lookup("Setulc1", "xterm-256color"), Capability::Unsupported);
        assert_eq!(lookup("Xx", "xterm-256color"), Capability::Unsupported);
    }

    #[test]
    fn hex_encode_roundtrip() {
        assert_eq!(hex_encode(b"colors"), "636f6c6f7273");
        assert_eq!(hex_encode(b"256"), "323536");
    }

    #[test]
    fn hex_decode_name() {
        assert_eq!(hex_decode("636f6c6f7273"), Some(b"colors".to_vec()));
        assert_eq!(hex_decode("zz"), None, "non-hex rejected");
        assert_eq!(hex_decode("abc"), None, "odd length rejected");
    }
}
