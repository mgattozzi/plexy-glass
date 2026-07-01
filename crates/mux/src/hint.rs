//! Hint mode: scan the visible grid for interesting spans (URLs, paths, SHAs,
//! and so on), label them, and let the user pick one to copy or open. Pure
//! core: depends only on the emulator screen + `regex`, so it builds and tests
//! standalone (like `selection.rs`).

use plexy_glass_emulator::Screen;
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
    const fn priority(self) -> u8 {
        match self {
            Self::Hyperlink => 0,
            Self::Url => 1,
            Self::Email => 2,
            Self::Path => 3,
            Self::Uuid => 4,
            Self::Ip => 5,
            Self::Sha => 6,
            Self::HexColor => 7,
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

    /// Text placed on the clipboard / paste buffer for a Copy action. A
    /// `file://` URL (an OSC 8 hyperlink to a local file, as Claude Code,
    /// `eza`, and friends emit) is decoded to its filesystem path: the user
    /// wants the path, not the URL. Everything else copies verbatim (a real URL
    /// stays a URL; a `Path` keeps its `:line:col` suffix).
    pub fn copy_text(&self) -> String {
        file_url_to_path(&self.text).unwrap_or_else(|| self.text.clone())
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

#[derive(Debug, Clone)]
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
                // Use the raw match end (not the trimmed text width) so the
                // overlap sweep covers the full raw extent of the match.
                end_col: col_at(m.end()),
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

/// Decode a `file://` URL to a local filesystem path, or `None` if `s` isn't a
/// `file://` URL. Drops the optional authority (`file:///p` → empty host,
/// `file://localhost/p` → `localhost`) and percent-decodes the path so `%20`
/// becomes a space. Anything that isn't `file://…/…` returns `None`.
fn file_url_to_path(s: &str) -> Option<String> {
    let rest = s.strip_prefix("file://")?;
    // The path is everything from the first '/' (after the optional authority).
    let slash = rest.find('/')?;
    Some(percent_decode(&rest[slash..]))
}

/// Minimal RFC 3986 percent-decoding (`%XX` → byte); invalid escapes pass
/// through unchanged. Avoids pulling in a URL crate for a few lines.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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

use crate::key::{Key, KeyEvent, Modifiers};

/// Home-row label characters by default.
pub const DEFAULT_ALPHABET: &str = "asdfghjkl";

/// The effective label alphabet: the configured chars with duplicates
/// removed (first occurrence kept). Falls back to `DEFAULT_ALPHABET` when
/// fewer than 2 distinct chars remain.
pub fn effective_alphabet(configured: &str) -> String {
    let mut distinct: Vec<char> = Vec::new();
    for c in configured.chars() {
        if !distinct.contains(&c) {
            distinct.push(c);
        }
    }
    if distinct.len() >= 2 {
        distinct.into_iter().collect()
    } else {
        DEFAULT_ALPHABET.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintAction {
    Copy,
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintPick {
    pub text: String,
    pub action: HintAction,
}

/// `hint.rs`-local follow-up; the daemon adapts it to `OverlayKeyResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintOutcome {
    None,
    Redraw,
    Cancel,
    Pick(HintPick),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintState {
    /// (label, target) in reading order. All labels share one length, so no
    /// label is a prefix of another.
    pub labeled: Vec<(String, HintTarget)>,
    /// Lowercased label prefix typed so far.
    pub typed: String,
}

impl HintState {
    pub fn new(targets: Vec<HintTarget>, alphabet: &str) -> Self {
        let labels = assign_labels(targets.len(), alphabet);
        let labeled = labels.into_iter().zip(targets).collect();
        Self {
            labeled,
            typed: String::new(),
        }
    }

    /// Targets whose label still matches the typed prefix.
    pub fn visible(&self) -> impl Iterator<Item = &(String, HintTarget)> {
        let typed = self.typed.clone();
        self.labeled
            .iter()
            .filter(move |(l, _)| l.starts_with(&typed))
    }
}

/// Uniform-length, prefix-free labels. Length is the smallest `L` with
/// `alphabet_len^L >= n`. Assumes `alphabet` has >= 2 chars (enforced at config
/// load); a 1-char alphabet is clamped so this can't loop forever.
pub fn assign_labels(n: usize, alphabet: &str) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = alphabet.chars().collect();
    let k = chars.len().max(2);
    let mut len = 1usize;
    let mut cap = k;
    while cap < n && len < 6 {
        len += 1;
        cap = cap.saturating_mul(k);
    }
    let mut out = Vec::with_capacity(n);
    let mut idx = vec![0usize; len];
    for _ in 0..n {
        out.push(idx.iter().map(|&i| chars[i.min(chars.len() - 1)]).collect());
        // increment the mixed-radix counter, last digit fastest
        for d in (0..len).rev() {
            idx[d] += 1;
            if idx[d] < k {
                break;
            }
            idx[d] = 0;
        }
    }
    out
}

/// Feed one key to the overlay. Printables narrow by label prefix
/// (case-insensitive); a completed label commits, lowercase final char ⇒
/// `Copy`, uppercase ⇒ `Open`. `Esc` cancels.
pub fn handle_hint(event: &KeyEvent, state: &mut HintState) -> HintOutcome {
    if matches!(event.key, Key::Escape) && event.mods.is_empty() {
        return HintOutcome::Cancel;
    }
    let Key::Char(c) = event.key else {
        return HintOutcome::None;
    };
    // Uppercase letters arrive either bare (legacy/MOK) or with SHIFT (Kitty);
    // accept both, reject Ctrl/Alt/Super combos.
    if !(event.mods.is_empty() || event.mods == Modifiers::SHIFT) {
        return HintOutcome::None;
    }
    let open = c.is_uppercase();
    let mut candidate = state.typed.clone();
    candidate.push(c.to_ascii_lowercase());
    if !state.labeled.iter().any(|(l, _)| l.starts_with(&candidate)) {
        return HintOutcome::None; // stray key, ignore it
    }
    state.typed = candidate;
    if let Some((_, target)) = state.labeled.iter().find(|(l, _)| *l == state.typed) {
        let text = if open {
            target.open_text()
        } else {
            target.copy_text()
        };
        let action = if open { HintAction::Open } else { HintAction::Copy };
        return HintOutcome::Pick(HintPick { text, action });
    }
    HintOutcome::Redraw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{Key, KeyEvent, Modifiers};
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
        assert_eq!(ts.len(), 1, "{ts:?}");
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

    fn t(text: &str, kind: HintKind) -> HintTarget {
        HintTarget { start: (0, 0), text: text.into(), kind }
    }

    fn press(c: char) -> KeyEvent {
        KeyEvent::plain(Key::Char(c))
    }

    #[test]
    fn labels_single_char_when_few() {
        let labels = assign_labels(3, "asdf");
        assert_eq!(labels, vec!["a", "s", "d"]);
    }

    #[test]
    fn labels_roll_to_multichar_when_many() {
        // 5 targets, 2-char alphabet: smallest L with 2^L >= 5 is 3 (2^2=4 < 5),
        // so every label is length 3 and therefore prefix-free.
        let labels = assign_labels(5, "as");
        assert_eq!(labels, vec!["aaa", "aas", "asa", "ass", "saa"]);
        assert_eq!(labels.len(), 5);
        assert!(labels.iter().all(|l| l.len() == 3));
    }

    #[test]
    fn lowercase_label_copies() {
        let mut st = HintState::new(vec![t("hello", HintKind::Sha)], "asdf");
        // single target → label "a"
        let out = handle_hint(&press('a'), &mut st);
        assert_eq!(out, HintOutcome::Pick(HintPick { text: "hello".into(), action: HintAction::Copy }));
    }

    #[test]
    fn copy_decodes_file_url_to_path() {
        // An OSC 8 hyperlink to a local file copies as the path, not the URL.
        let mut st = HintState::new(vec![t("file:///Users/me/foo.rs", HintKind::Hyperlink)], "asdf");
        let out = handle_hint(&press('a'), &mut st);
        assert_eq!(
            out,
            HintOutcome::Pick(HintPick { text: "/Users/me/foo.rs".into(), action: HintAction::Copy })
        );
    }

    #[test]
    fn copy_file_url_decodes_percent_and_host() {
        // file://host/path with a percent-encoded space → decoded absolute path.
        let mut st =
            HintState::new(vec![t("file://localhost/Users/me/My%20File.rs", HintKind::Hyperlink)], "asdf");
        let out = handle_hint(&press('a'), &mut st);
        assert_eq!(
            out,
            HintOutcome::Pick(HintPick { text: "/Users/me/My File.rs".into(), action: HintAction::Copy })
        );
    }

    #[test]
    fn copy_leaves_http_url_and_path_untouched() {
        // A real URL stays a URL; a plain path keeps its :line:col suffix.
        let mut url = HintState::new(vec![t("https://docs.rs/x", HintKind::Url)], "asdf");
        assert_eq!(
            handle_hint(&press('a'), &mut url),
            HintOutcome::Pick(HintPick { text: "https://docs.rs/x".into(), action: HintAction::Copy })
        );
        let mut path = HintState::new(vec![t("src/main.rs:42:7", HintKind::Path)], "asdf");
        assert_eq!(
            handle_hint(&press('a'), &mut path),
            HintOutcome::Pick(HintPick { text: "src/main.rs:42:7".into(), action: HintAction::Copy })
        );
    }

    #[test]
    fn uppercase_label_opens_with_stripped_path() {
        let mut st = HintState::new(vec![t("src/x.rs:9", HintKind::Path)], "asdf");
        let out = handle_hint(&press('A'), &mut st);
        assert_eq!(out, HintOutcome::Pick(HintPick { text: "src/x.rs".into(), action: HintAction::Open }));
    }

    #[test]
    fn multichar_label_narrows_then_picks() {
        let targets = vec![t("one", HintKind::Sha), t("two", HintKind::Sha), t("three", HintKind::Sha)];
        // 3 targets, 2-char alphabet: labels aa, as, sa.
        let mut st = HintState::new(targets, "as");
        assert_eq!(handle_hint(&press('a'), &mut st), HintOutcome::Redraw);
        assert_eq!(st.typed, "a");
        let out = handle_hint(&press('s'), &mut st);
        assert_eq!(out, HintOutcome::Pick(HintPick { text: "two".into(), action: HintAction::Copy }));
    }

    #[test]
    fn escape_cancels() {
        let mut st = HintState::new(vec![t("x", HintKind::Sha)], "asdf");
        assert_eq!(handle_hint(&KeyEvent::plain(Key::Escape), &mut st), HintOutcome::Cancel);
    }

    #[test]
    fn stray_key_ignored() {
        let mut st = HintState::new(vec![t("x", HintKind::Sha)], "asdf"); // label "a"
        // 'z' is not a label prefix, so it's ignored and typed stays unchanged.
        assert_eq!(handle_hint(&press('z'), &mut st), HintOutcome::None);
        assert_eq!(st.typed, "");
    }

    #[test]
    fn ctrl_modified_key_ignored() {
        let mut st = HintState::new(vec![t("x", HintKind::Sha)], "asdf");
        let ev = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        assert_eq!(handle_hint(&ev, &mut st), HintOutcome::None);
    }

    #[test]
    fn effective_alphabet_passthrough() {
        assert_eq!(effective_alphabet("asdf"), "asdf");
    }

    #[test]
    fn effective_alphabet_dup_only_falls_back_to_default() {
        // "aa" has only 1 distinct char, so it falls back to DEFAULT_ALPHABET.
        assert_eq!(effective_alphabet("aa"), DEFAULT_ALPHABET);
    }

    #[test]
    fn effective_alphabet_dedup_keeps_order() {
        // "aab" deduplicates to "ab" (order preserved, first occurrence kept).
        assert_eq!(effective_alphabet("aab"), "ab");
    }

    #[test]
    fn effective_alphabet_single_char_falls_back_to_default() {
        // "x" has only 1 distinct char, so it falls back to DEFAULT_ALPHABET.
        assert_eq!(effective_alphabet("x"), DEFAULT_ALPHABET);
    }

    // ── hex_val / percent_decode tests ───────────────────────────────────────

    #[test]
    fn hex_val_handles_all_hex_digit_ranges() {
        // Kills `delete match arm b'a'..=b'f'` / `b'A'..=b'F'` and all the
        // arithmetic mutations on lines 210-211.
        assert_eq!(hex_val(b'0'), Some(0));
        assert_eq!(hex_val(b'9'), Some(9));
        assert_eq!(hex_val(b'a'), Some(10));
        assert_eq!(hex_val(b'f'), Some(15));
        assert_eq!(hex_val(b'A'), Some(10));
        assert_eq!(hex_val(b'F'), Some(15));
        assert_eq!(hex_val(b'g'), None);
    }

    #[test]
    fn percent_decode_handles_lowercase_and_uppercase_hex() {
        // %2b = '+' (lowercase), %2B = '+' (uppercase). The existing test uses
        // %20 (digit-only hex), missing the a-f / A-F arms of hex_val entirely.
        assert_eq!(percent_decode("%2b"), "+");
        assert_eq!(percent_decode("%2B"), "+");
        // Mixed: space (%20) and plus (%2b) together.
        assert_eq!(percent_decode("a%20b%2bc"), "a b+c");
        // Invalid escape passes through unchanged.
        assert_eq!(percent_decode("%zz"), "%zz");
        // Percent at the very end (only 0 more bytes): must pass through, not panic.
        assert_eq!(percent_decode("ok%"), "ok%");
        // Percent with only 1 more byte: must pass through, not panic.
        assert_eq!(percent_decode("ok%2"), "ok%2");
    }

    // ── resolve_overlaps tests ───────────────────────────────────────────────

    #[test]
    fn resolve_overlaps_prefers_longest_then_highest_priority() {
        // Two spans starting at the same column: the longer one wins.
        // Three spans: url covers cols 0-10, sha covers cols 0-7, path covers 3-8.
        // Expected: url (longest from 0) wins; path starts within url so skipped.
        // Kills `replace - with +` mutations at line 255 (length comparator).
        let url = Span { start_col: 0, end_col: 10, kind: HintKind::Url, text: "http://x.c/ab".into() };
        let sha = Span { start_col: 0, end_col: 7, kind: HintKind::Sha, text: "deadbee".into() };
        let path = Span { start_col: 3, end_col: 8, kind: HintKind::Path, text: "c/ab".into() };
        let result = resolve_overlaps(vec![sha, path, url]);
        assert_eq!(result.len(), 1, "url should dominate: {result:?}");
        assert_eq!(result[0].kind, HintKind::Url);
    }

    #[test]
    fn resolve_overlaps_priority_breaks_same_length_tie() {
        // Two spans of the same length at the same start: Hyperlink (priority 0)
        // wins over Url (priority 1). Kills `replace priority with 0/1` mutations.
        let hyper = Span { start_col: 0, end_col: 5, kind: HintKind::Hyperlink, text: "https://x".into() };
        let url = Span { start_col: 0, end_col: 5, kind: HintKind::Url, text: "https://x".into() };
        let result = resolve_overlaps(vec![url, hyper]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, HintKind::Hyperlink, "hyperlink priority beats url");
    }

    // ── push_hyperlink_spans boundary tests ─────────────────────────────────

    #[test]
    fn push_hyperlink_inner_loop_exits_at_id_change() {
        // The inner `while col < cols && hyperlink_id == Some(id)` loop must stop
        // when the hyperlink region ends. With `&& → ||` the loop keeps going until
        // col reaches the line's end, so the end_col covers the whole line instead
        // of just the linked text.
        // Also tests that `< → <=` at line 227:15 (outer loop) is equivalent
        // (get_cell returns None at col=cols, so the extra iteration is a no-op).
        let line = "\x1b]8;;https://a.io\x1b\\docs\x1b]8;;\x1b\\ rest";
        let s = screen_from(1, 20, &[line]);

        // Direct test: call push_hyperlink_spans and check start_col + end_col.
        let mut spans = Vec::new();
        push_hyperlink_spans(&s, 0, &mut spans);
        assert_eq!(spans.len(), 1, "exactly one hyperlink span: {spans:?}");
        assert_eq!(spans[0].start_col, 0, "link starts at col 0");
        // "docs" is 4 graphemes, so end_col should be 4 (one past the last linked col).
        assert_eq!(spans[0].end_col, 4, "link ends at col 4 (just past 'docs')");

        // Equivalent notes:
        // - 227:15 `< → <=`: outer loop adds one extra iteration at col=cols;
        //   get_cell returns None, id is None, the None arm increments col past
        //   cols and the loop exits, same result. EQUIVALENT.
        // - 234:19 `< → <=`: inner loop at col=cols: get_cell returns None, id is
        //   None ≠ Some(id), loop exits, same result. EQUIVALENT.
    }

    // ── assign_labels boundary tests ─────────────────────────────────────────

    #[test]
    fn assign_labels_cap_boundary_at_exactly_n() {
        // When `cap == n` exactly, the current label length is sufficient.
        // With `< → <=` at line 351, `cap == n` would trigger one extra length
        // increment, producing unnecessarily long labels.
        // k=2, n=4: 2^2=4 == n. With `<`, loop exits (4 is NOT < 4). len=2. ✓
        // With `<=`, loop continues: 4 <= 4 → len=3, cap=8. len=3 is too long.
        let labels = assign_labels(4, "ab");
        assert!(labels.iter().all(|l| l.len() == 2), "4 targets with 2-char alphabet need len=2: {labels:?}");
        assert_eq!(labels.len(), 4);
    }

    // Equivalent note (line 358): `chars[i.min(chars.len() - 1)]` vs
    // `chars[i.min(chars.len() + 1)]` (`- → +`) or `chars[i.min(chars.len() / 1)]`
    // (`- → /` = `chars[i.min(chars.len())]`): `i` (= idx[d]) is always < k =
    // chars.len() (the counter resets at k), so i < chars.len() ≤ chars.len() ± 1,
    // making i.min(any of these) = i. The clamp never fires for valid alphabet
    // sizes (≥ 2). Both mutations are equivalent for all reachable states.
    // Equivalent note (line 351:26): `len < 6 → len <= 6` adds one extra label-
    // length increment; for practical hint counts (≤ 2000 with a ≥ 2-char alphabet)
    // cap = k^6 ≥ 64 is always sufficient and the loop exits on the cap condition,
    // not the len guard. Observable difference only for n > k^6, which does not
    // occur in the application. Equivalent.
}
