//! Hint mode: scan the visible grid for interesting spans (URLs, paths, SHAs,
//! and so on), label them, and let the user pick one to copy or open. Pure
//! core: depends only on the emulator screen + `regex`, so it builds and tests
//! standalone (like `selection.rs`).

use plexy_glass_emulator::{Screen, display_width};
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintKind {
    Hyperlink,
    Url,
    Email,
    Path,
    Uuid,
    Ip,
    Sha,
    HexColor,
}

impl HintKind {
    /// Lower = higher priority when two spans overlap.
    fn priority(self) -> u8 {
        match self {
            HintKind::Hyperlink => 0,
            HintKind::Url => 1,
            HintKind::Email => 2,
            HintKind::Path => 3,
            HintKind::Uuid => 4,
            HintKind::Ip => 5,
            HintKind::Sha => 6,
            HintKind::HexColor => 7,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintTarget {
    /// Cell where the span starts (row, col) in the active grid.
    pub start: (u16, u16),
    /// The span's text. For `Hyperlink` this is the OSC 8 URL, not the
    /// on-screen label; every other kind is the matched on-screen substring.
    pub text: String,
    pub kind: HintKind,
}

impl HintTarget {
    /// Text handed to the OS opener. For `Path` a trailing `:line[:col]` is
    /// stripped (the opener doesn't understand it); every other kind opens
    /// verbatim.
    pub fn open_text(&self) -> String {
        if self.kind == HintKind::Path
            && let Some(m) = LINE_COL_RE.find(&self.text)
        {
            return self.text[..m.start()].to_string();
        }
        self.text.clone()
    }
}

static LINE_COL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r":\d+(?::\d+)?$").expect("static hint regex"));

struct Pattern {
    kind: HintKind,
    re: Regex,
}

static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    let p = |kind, re: &str| Pattern {
        kind,
        re: Regex::new(re).expect("static hint regex"),
    };
    vec![
        p(HintKind::Url, r"https?://[^\s<>\x22'\\)\]}]+"),
        p(HintKind::Email, r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"),
        p(
            HintKind::Path,
            r"(?:[~.]{0,2}/)?[\w.+-]+(?:/[\w.+-]+)+(?::\d+(?::\d+)?)?",
        ),
        p(
            HintKind::Uuid,
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        ),
        p(HintKind::Ip, r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b"),
        p(HintKind::Sha, r"\b[0-9a-f]{7,40}\b"),
        p(HintKind::HexColor, r"#(?:[0-9a-fA-F]{8}|[0-9a-fA-F]{6}|[0-9a-fA-F]{3})\b"),
    ]
});

struct Span {
    start_col: u16,
    end_col: u16,
    kind: HintKind,
    text: String,
}

/// Scan every visible row of the active grid into ordered, overlap-resolved
/// targets (reading order: top-to-bottom, left-to-right).
pub fn scan_hints(screen: &Screen) -> Vec<HintTarget> {
    let mut out = Vec::new();
    for row in 0..screen.active.num_rows() {
        scan_row(screen, row, &mut out);
    }
    out
}

fn scan_row(screen: &Screen, row: u16, out: &mut Vec<HintTarget>) {
    let grid = &screen.active;
    let cols = grid.num_cols();
    // Build the row text from non-spacer cells, recording each grapheme's byte
    // offset → grid column so a regex match maps back to columns (mirrors
    // copy_mode::find_matches).
    let mut line = String::new();
    let mut starts: Vec<(usize, u16)> = Vec::new();
    for col in 0..cols {
        let Some(cell) = grid.get_cell(row, col) else {
            continue;
        };
        if cell.is_wide_spacer() {
            continue;
        }
        let g = cell.grapheme.as_str();
        let g = if g.is_empty() { " " } else { g };
        starts.push((line.len(), col));
        line.push_str(g);
    }
    starts.push((line.len(), cols)); // end sentinel

    let col_at = |byte: usize| -> u16 {
        // last recorded start whose byte offset is <= byte
        let idx = starts.partition_point(|&(b, _)| b <= byte).saturating_sub(1);
        starts[idx].1
    };

    let mut spans: Vec<Span> = Vec::new();
    for pat in PATTERNS.iter() {
        for m in pat.re.find_iter(&line) {
            let text = trim_trailing(pat.kind, m.as_str());
            if text.is_empty() {
                continue;
            }
            let start_col = col_at(m.start());
            spans.push(Span {
                start_col,
                end_col: start_col + display_width(text),
                kind: pat.kind,
                text: text.to_string(),
            });
        }
    }
    push_hyperlink_spans(screen, row, &mut spans);

    for s in resolve_overlaps(spans) {
        out.push(HintTarget {
            start: (row, s.start_col),
            text: s.text,
            kind: s.kind,
        });
    }
}

fn trim_trailing(kind: HintKind, s: &str) -> &str {
    match kind {
        HintKind::Url => s.trim_end_matches(|c: char| ".,;:!?)]}'\"".contains(c)),
        _ => s,
    }
}

fn push_hyperlink_spans(screen: &Screen, row: u16, spans: &mut Vec<Span>) {
    let grid = &screen.active;
    let cols = grid.num_cols();
    let mut col = 0u16;
    while col < cols {
        let id = grid.get_cell(row, col).and_then(|c| c.hyperlink_id);
        let Some(id) = id else {
            col += 1;
            continue;
        };
        let start = col;
        while col < cols && grid.get_cell(row, col).and_then(|c| c.hyperlink_id) == Some(id) {
            col += 1;
        }
        if let Some(url) = screen.hyperlinks.get(id) {
            spans.push(Span {
                start_col: start,
                end_col: col,
                kind: HintKind::Hyperlink,
                text: url.to_string(),
            });
        }
    }
}

/// Leftmost-longest-highest-priority sweep: at each column keep the span that
/// starts earliest, then is longest, then has the lowest `priority()`, and skip
/// anything it covers. Stops a URL from being shredded into a path + SHA.
fn resolve_overlaps(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_by(|a, b| {
        a.start_col
            .cmp(&b.start_col)
            .then((b.end_col - b.start_col).cmp(&(a.end_col - a.start_col)))
            .then(a.kind.priority().cmp(&b.kind.priority()))
    });
    let mut kept: Vec<Span> = Vec::new();
    let mut covered_until = 0u16;
    for s in spans {
        if s.start_col >= covered_until {
            covered_until = s.end_col;
            kept.push(s);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_emulator::Emulator;

    fn screen_from(rows: u16, cols: u16, lines: &[&str]) -> plexy_glass_emulator::Screen {
        let mut e = Emulator::new(rows, cols);
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                e.advance(b"\r\n");
            }
            e.advance(line.as_bytes());
        }
        // No-op SGR flushes the parser's pending grapheme so the last grapheme
        // lands in the grid before we clone.
        e.advance(b"\x1b[m");
        e.screen().clone()
    }

    fn kinds(ts: &[HintTarget]) -> Vec<HintKind> {
        ts.iter().map(|t| t.kind).collect()
    }

    #[test]
    fn scans_url() {
        let s = screen_from(1, 40, &["see http://example.com/x now"]);
        let ts = scan_hints(&s);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].kind, HintKind::Url);
        assert_eq!(ts[0].text, "http://example.com/x");
        assert_eq!(ts[0].start, (0, 4));
    }

    #[test]
    fn url_trailing_punctuation_trimmed() {
        let s = screen_from(1, 40, &["go to https://a.io/path."]);
        let ts = scan_hints(&s);
        assert_eq!(ts[0].text, "https://a.io/path");
    }

    #[test]
    fn scans_path_with_line_col_and_open_strips_it() {
        let s = screen_from(1, 40, &["err at src/main.rs:42:7 here"]);
        let ts = scan_hints(&s);
        let t = ts.iter().find(|t| t.kind == HintKind::Path).expect("path");
        assert_eq!(t.text, "src/main.rs:42:7");
        assert_eq!(t.open_text(), "src/main.rs");
    }

    #[test]
    fn scans_absolute_and_home_paths() {
        let s = screen_from(1, 40, &["~/notes/todo.md and /etc/hosts"]);
        let ts = scan_hints(&s);
        let texts: Vec<&str> = ts.iter().map(|t| t.text.as_str()).collect();
        assert!(texts.contains(&"~/notes/todo.md"), "{texts:?}");
        assert!(texts.contains(&"/etc/hosts"), "{texts:?}");
    }

    #[test]
    fn scans_sha_uuid_ip_hex_email() {
        let s = screen_from(1, 80, &["deadbeef1234 at 192.168.1.1 #ff8800 a@b.io"]);
        let ks = kinds(&scan_hints(&s));
        assert!(ks.contains(&HintKind::Sha), "{ks:?}");
        assert!(ks.contains(&HintKind::Ip), "{ks:?}");
        assert!(ks.contains(&HintKind::HexColor), "{ks:?}");
        assert!(ks.contains(&HintKind::Email), "{ks:?}");
    }

    #[test]
    fn scans_uuid() {
        let s = screen_from(1, 50, &["id 550e8400-e29b-41d4-a716-446655440000 ok"]);
        let ts = scan_hints(&s);
        let t = ts.iter().find(|t| t.kind == HintKind::Uuid).expect("uuid");
        assert_eq!(t.text, "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn url_not_shredded_into_path_or_sha() {
        // The sha-looking + path-looking substrings inside the URL must not produce
        // extra targets: longest-match-wins, so the url out-prioritizes them.
        let s = screen_from(1, 50, &["x http://h.com/deadbeef1234/p y"]);
        let ts = scan_hints(&s);
        assert_eq!(ts.len(), 1, "{:?}", ts);
        assert_eq!(ts[0].kind, HintKind::Url);
    }

    #[test]
    fn scans_osc8_hyperlink_range() {
        // OSC 8 link: ESC]8;;URL ST  TEXT  ESC]8;; ST
        let line = "\x1b]8;;https://docs.rs\x1b\\docs\x1b]8;;\x1b\\";
        let s = screen_from(1, 20, &[line]);
        let ts = scan_hints(&s);
        let t = ts.iter().find(|t| t.kind == HintKind::Hyperlink).expect("link");
        assert_eq!(t.text, "https://docs.rs");
        assert_eq!(t.start, (0, 0));
    }

    #[test]
    fn empty_grid_no_targets() {
        let s = screen_from(1, 20, &[""]);
        assert!(scan_hints(&s).is_empty());
    }
}
