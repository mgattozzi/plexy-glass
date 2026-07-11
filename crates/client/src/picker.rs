//! Pure core for the client-rendered session picker. Milestone A shipped a flat
//! same-daemon list; Milestone B turns it into a host-tagged, sectioned roster:
//! the current daemon's sessions plus every OTHER daemon in
//! `{local} ∪ config-remotes ∪ ad-hoc`, grouped into a `local` section,
//! configured-remote host rows, a divider, and ad-hoc host rows. The daemon
//! sends `ServerMsg::OpenSessionPicker` to a v12+ client; `pump` builds a
//! `PickerState`, drives it from stdin, and renders it directly to the client's
//! own terminal — no compositor, no daemon round-trip per keystroke. Modeled on
//! the mux finder cores (`crates/mux/src/{tree,history}.rs`): a pure `*State` +
//! `handle_*`→outcome enum, unit-testable independent of rendering.
//!
//! Section headers (`local`) and the configured/ad-hoc divider are **synthesized
//! at render time** from which sections have ≥1 visible (filter-matching) row —
//! they are NOT stored as rows, so the cursor never lands on them and the
//! selectable set is exactly the real rows (session rows + host rows). Remote
//! hosts get a real, selectable `Host` row (the anchor for `n`/`x`) that doubles
//! as that host's visible header.

use plexy_glass_emulator::truncate_to_width;

/// Whether a row is a session (attach/switch target) or a remote host (the
/// `n`/`x` target + the section header for that host's sessions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Session,
    Host,
}

/// Reachability of a host row (session rows are always `Live`). Filled in
/// incrementally by the streaming per-host query (`query::HostStatus` →
/// `RowStatus`): `Pending` while the query is in flight, then one of the
/// resolved states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowStatus {
    Live,
    Empty,
    Unreachable,
    Pending,
    VersionMismatch(u16),
}

/// One row in the picker list. `name` is the wire identity (`SwitchSession` /
/// reconnect session name for a session row, the ssh target for a host row);
/// `label` is the pre-formatted display text and the filter haystack; `host` is
/// `None` for the local daemon's sessions and `Some(target)` for a remote's
/// (and for host rows); `kind`/`status` drive the sectioning + indicators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRow {
    pub name: String,
    pub label: String,
    pub host: Option<String>,
    pub kind: RowKind,
    pub status: RowStatus,
}

/// What the user did with the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    /// Same-daemon switch (a local/current session row).
    Switch(String),
    /// Nothing chosen; repaint and resume.
    Cancel,
    /// Cross-daemon jump: attach to `host`'s daemon and `name` (the routing —
    /// `PumpExit::ReconnectTo` — is wired in Task 6).
    Reconnect { host: Option<String>, name: String },
    /// New session `name` on `host`'s daemon (create-if-missing at reconnect).
    New { host: Option<String>, name: String },
    /// Forget an ad-hoc host from the client-side roster file.
    Forget { host: String },
}

/// The picker's input sub-mode. In `Navigate`, printables filter and the
/// cursor moves; `n`/`x` are gated actions (see `handle_navigate`). In
/// `Prompting`, every printable (including `n`/`x`) types the new session's
/// name, `Enter` commits `New`, `Esc` returns to `Navigate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerMode {
    Navigate,
    Prompting { host: Option<String>, buf: String },
}

/// Filter + cursor over a fixed row list, plus the input sub-mode and the set of
/// ad-hoc host names (so `x` knows which host rows are forgettable and render
/// knows where the configured/ad-hoc divider goes). `cursor` indexes into
/// `visible()` (the filter-matching rows), not `rows`; it re-clamps whenever the
/// filter narrows the list, matching the mux finder cores.
pub struct PickerState {
    rows: Vec<PickerRow>,
    /// Host names that came from the ad-hoc roster file (not `config.kdl`).
    /// Set by the pump after `new` (Task 5); empty for the local-only picker.
    adhoc: Vec<String>,
    filter: String,
    cursor: usize,
    mode: PickerMode,
}

impl PickerState {
    pub const fn new(rows: Vec<PickerRow>) -> Self {
        Self {
            rows,
            adhoc: Vec::new(),
            filter: String::new(),
            cursor: 0,
            mode: PickerMode::Navigate,
        }
    }

    /// Like [`new`](Self::new) but parks the cursor on the current daemon's
    /// current session row (matched by `host` + `name`) instead of row 0,
    /// restoring Milestone A's cursor-on-current behavior that the flat
    /// `is_current` drop lost. Falls back to the first row (the current-daemon
    /// anchor) when that session isn't present.
    pub fn new_with_current(
        rows: Vec<PickerRow>,
        current_host: &Option<String>,
        current_session: &str,
    ) -> Self {
        let cursor = rows
            .iter()
            .position(|r| {
                r.kind == RowKind::Session && r.host == *current_host && r.name == current_session
            })
            .unwrap_or(0);
        Self {
            rows,
            adhoc: Vec::new(),
            filter: String::new(),
            cursor,
            mode: PickerMode::Navigate,
        }
    }

    /// Fold a resolved daemon's streaming query result into the rows: set that
    /// host's `Host` anchor status and replace its `Session` child rows (both
    /// matched by `host`, so `None` targets the local anchor). `sessions` is
    /// empty for anything but a `Live` result; a `Live` daemon's sessions are
    /// spliced in right after its anchor. Idempotent — a re-resolve drops the
    /// prior session rows first — and re-clamps the cursor.
    pub fn resolve_host(
        &mut self,
        host: &Option<String>,
        status: RowStatus,
        mut sessions: Vec<PickerRow>,
    ) {
        self.rows
            .retain(|r| !(r.kind == RowKind::Session && r.host == *host));
        if let Some(idx) = self
            .rows
            .iter()
            .position(|r| r.kind == RowKind::Host && r.host == *host)
        {
            self.rows[idx].status = status;
            let mut tail = self.rows.split_off(idx + 1);
            self.rows.append(&mut sessions);
            self.rows.append(&mut tail);
        }
        self.clamp();
    }

    /// Tell the picker which hosts are ad-hoc (vs configured). Drives `x`→Forget
    /// gating and the render-time divider. Called by the pump once the roster is
    /// assembled (Task 5).
    pub fn set_adhoc_hosts(&mut self, hosts: Vec<String>) {
        self.adhoc = hosts;
    }

    /// Replace every OTHER-daemon row (anything not the current daemon's own
    /// anchor/session rows, matched by `host`) with a freshly assembled set —
    /// the picker's `x`/Forget rebuild (Task 6): the roster changed underneath
    /// (a host was forgotten), so its host rows — and any already-resolved
    /// session rows spliced under them — are discarded and replaced with fresh
    /// `Pending` anchors from the updated roster; the current daemon's own rows
    /// are untouched. Also refreshes the ad-hoc set (`set_adhoc_hosts`) and
    /// re-clamps the cursor (mirrors `resolve_host`).
    pub fn replace_other_rows(
        &mut self,
        current_host: &Option<String>,
        rows: Vec<PickerRow>,
        adhoc: Vec<String>,
    ) {
        self.rows.retain(|r| &r.host == current_host);
        self.rows.extend(rows);
        self.adhoc = adhoc;
        self.clamp();
    }

    /// Rows matching the live filter (case-insensitive substring on the label),
    /// in original order. Every visible row is selectable — headers/dividers are
    /// synthesized at render time and are not rows.
    pub fn visible(&self) -> Vec<&PickerRow> {
        let f = self.filter.to_lowercase();
        self.rows
            .iter()
            .filter(|r| r.label.to_lowercase().contains(&f))
            .collect()
    }

    /// The row under the cursor, or `None` when the filter matches nothing.
    pub fn selected(&self) -> Option<&PickerRow> {
        self.visible().get(self.cursor).copied()
    }

    /// The live filter text.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Apply one input byte. Returns `Some(outcome)` when the key commits/cancels
    /// the picker, `None` while navigating/filtering/prompting.
    ///
    /// Arrow-key escape sequences (`\e[A`/`\e[B`) are multi-byte, so the pump
    /// decodes them to Ctrl-P/Ctrl-N (`0x10`/`0x0e`) before calling this. Enter
    /// is `\r` (raw mode sends `\r`); `\n` is Ctrl-J (down), pairing with Ctrl-K
    /// (up) as elsewhere in the codebase.
    pub fn handle_key(&mut self, byte: u8) -> Option<PickerOutcome> {
        if matches!(self.mode, PickerMode::Prompting { .. }) {
            self.handle_prompting(byte)
        } else {
            self.handle_navigate(byte)
        }
    }

    fn handle_navigate(&mut self, byte: u8) -> Option<PickerOutcome> {
        match byte {
            0x1b => Some(PickerOutcome::Cancel), // Esc
            b'\r' => self.accept(),
            0x0e | 0x0a => {
                self.move_cursor(1);
                None
            } // Ctrl-N / Ctrl-J (down)
            0x10 | 0x0b => {
                self.move_cursor(-1);
                None
            } // Ctrl-P / Ctrl-K (up)
            0x7f => {
                self.filter.pop();
                self.clamp();
                None
            } // Backspace
            // `n`/`x` are ACTIONS only when the filter is EMPTY and the cursor is
            // on a host row (matched against `self.selected()` directly, not by
            // `host` — the LOCAL anchor is a `Host` row with `host: None` and
            // must be reachable the same as a remote's); otherwise they are
            // ordinary filter input, so `nginx` / `main` / `prod-x` filter
            // correctly. `n` opens the new-session prompt for that host —
            // including the LOCAL anchor, so `n` works on the current daemon too,
            // not just remotes. `x` forgets the host if it's ad-hoc; it's a
            // no-op (swallowed, not filtered) on the local anchor and on
            // configured hosts — there's nothing to forget on either. On a
            // session row they fall through to filter input (a session row is
            // not a host row).
            b'n' if self.filter.is_empty() => {
                match self.selected() {
                    Some(row) if row.kind == RowKind::Host => {
                        self.mode = PickerMode::Prompting {
                            host: row.host.clone(),
                            buf: String::new(),
                        };
                    }
                    _ => self.push_filter(b'n'),
                }
                None
            }
            b'x' if self.filter.is_empty() => match self.selected() {
                Some(row) if row.kind == RowKind::Host => match &row.host {
                    Some(h) if self.is_adhoc(h) => Some(PickerOutcome::Forget { host: h.clone() }),
                    _ => None, // local anchor or configured host: forget is a no-op
                },
                _ => {
                    self.push_filter(b'x');
                    None
                }
            },
            b if b.is_ascii_graphic() || b == b' ' => {
                self.push_filter(b);
                None
            }
            _ => None,
        }
    }

    fn handle_prompting(&mut self, byte: u8) -> Option<PickerOutcome> {
        match byte {
            0x1b => {
                // Esc abandons the name and returns to Navigate.
                self.mode = PickerMode::Navigate;
                None
            }
            b'\r' => {
                let PickerMode::Prompting { host, buf } = &self.mode else {
                    return None; // invariant: only reached while Prompting
                };
                let outcome = PickerOutcome::New {
                    host: host.clone(),
                    name: buf.clone(),
                };
                self.mode = PickerMode::Navigate;
                Some(outcome)
            }
            0x7f => {
                if let PickerMode::Prompting { buf, .. } = &mut self.mode {
                    buf.pop();
                }
                None
            } // Backspace
            b if b.is_ascii_graphic() || b == b' ' => {
                if let PickerMode::Prompting { buf, .. } = &mut self.mode {
                    buf.push(b as char);
                }
                None
            }
            _ => None,
        }
    }

    /// Enter on the cursor row: a local session switches in place, a remote
    /// session reconnects, a host row reconnects to that daemon (default
    /// session). An empty filtered view yields `None` (cursor parked).
    fn accept(&self) -> Option<PickerOutcome> {
        let row = self.selected()?;
        match row.kind {
            RowKind::Session => match &row.host {
                Some(host) => Some(PickerOutcome::Reconnect {
                    host: Some(host.clone()),
                    name: row.name.clone(),
                }),
                None => Some(PickerOutcome::Switch(row.name.clone())),
            },
            RowKind::Host => Some(PickerOutcome::Reconnect {
                host: row.host.clone(),
                name: String::new(),
            }),
        }
    }

    fn is_adhoc(&self, host: &str) -> bool {
        self.adhoc.iter().any(|h| h == host)
    }

    fn push_filter(&mut self, byte: u8) {
        self.filter.push(byte as char);
        self.clamp();
    }

    /// Step the cursor over the selectable (visible) rows. Bounded: a single
    /// modular step, and an empty view parks the cursor at 0.
    fn move_cursor(&mut self, d: isize) {
        let n = self.visible().len();
        if n == 0 {
            self.cursor = 0;
            return;
        }
        self.cursor = ((self.cursor as isize + d).rem_euclid(n as isize)) as usize;
    }

    fn clamp(&mut self) {
        let n = self.visible().len();
        if self.cursor >= n {
            self.cursor = n.saturating_sub(1);
        }
    }

    /// Bytes to draw the picker on the client's own terminal: clear-and-home, a
    /// title, the visible rows grouped into sections (a synthesized `local`
    /// header, remote host rows as their own section anchors, a divider before
    /// the ad-hoc block), and a filter or new-session prompt line. Headers and
    /// the divider are synthesized here from the visible rows, never stored.
    pub fn render(&self) -> Vec<u8> {
        // ponytail: no real terminal-width plumbing into the picker yet (it
        // renders straight to the client's own tty outside the compositor);
        // 100 cols covers any realistic session/host label. Revisit if rows
        // routinely wrap.
        const MAX_ROW_WIDTH: u16 = 100;
        let mut out = String::new();
        out.push_str("\x1b[2J\x1b[H");
        out.push_str("plexy-glass \u{2014} switch session\r\n\r\n");

        let mut emitted_local = false;
        let mut emitted_divider = false;
        let mut emitted_any = false;
        for (i, row) in self.visible().into_iter().enumerate() {
            let is_local = row.host.is_none();
            let is_adhoc = row.host.as_deref().is_some_and(|h| self.is_adhoc(h));
            if is_local && !emitted_local {
                // A local Host ANCHOR (the current daemon, Task 5) is its own
                // selectable header, so it needs no synthesized text line; only
                // a bare local Session block with no anchor (the Milestone A
                // local-only path) gets the synthesized "local".
                if row.kind == RowKind::Session {
                    out.push_str("local\r\n");
                }
                emitted_local = true;
            }
            if is_adhoc && emitted_any && !emitted_divider {
                out.push_str(&"\u{2500}".repeat(28));
                out.push_str("\r\n");
                emitted_divider = true;
            }

            let full = self.row_line(row);
            let line = truncate_to_width(&full, MAX_ROW_WIDTH);
            if i == self.cursor {
                out.push_str("\x1b[7m");
                out.push_str(line);
                out.push_str("\x1b[0m\r\n");
            } else {
                out.push_str(line);
                out.push_str("\r\n");
            }
            emitted_any = true;
        }

        match &self.mode {
            PickerMode::Prompting { host, buf } => {
                out.push_str("\r\nnew session on ");
                out.push_str(host.as_deref().unwrap_or("local"));
                out.push_str(": ");
                out.push_str(buf);
                out.push_str("\r\n");
            }
            PickerMode::Navigate => {
                out.push_str("\r\nfilter: ");
                out.push_str(&self.filter);
                out.push_str("\r\n");
            }
        }
        out.into_bytes()
    }

    fn row_line(&self, row: &PickerRow) -> String {
        let glyph = status_glyph(&row.status);
        match row.kind {
            RowKind::Host => {
                let name = &row.name;
                if row.host.is_none() {
                    // The current/local daemon anchor: not a configured or
                    // ad-hoc remote, so no `(tag)`.
                    format!("{glyph} {name}")
                } else {
                    let tag = if row.host.as_deref().is_some_and(|h| self.is_adhoc(h)) {
                        "ad-hoc"
                    } else {
                        "configured"
                    };
                    format!("{glyph} {name}  ({tag})")
                }
            }
            RowKind::Session => {
                let label = &row.label;
                format!("  {glyph} {label}")
            }
        }
    }
}

fn status_glyph(status: &RowStatus) -> String {
    match status {
        RowStatus::Live => "\u{25cf}".to_string(),        // ●
        RowStatus::Empty => "\u{25cb}".to_string(),       // ○
        RowStatus::Unreachable => "\u{26a0}".to_string(), // ⚠
        RowStatus::Pending => "\u{2026}".to_string(),     // …
        RowStatus::VersionMismatch(v) => format!("\u{26a0} v{v}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str, host: Option<&str>) -> PickerRow {
        PickerRow {
            name: name.into(),
            label: format!("{name} \u{2014} 1 win"),
            host: host.map(str::to_string),
            kind: RowKind::Session,
            status: RowStatus::Live,
        }
    }

    fn host_row(host: &str, status: RowStatus) -> PickerRow {
        PickerRow {
            name: host.into(),
            label: host.into(),
            host: Some(host.into()),
            kind: RowKind::Host,
            status,
        }
    }

    /// The current LOCAL daemon's anchor row (Task 5): a `Host` row, `host` None.
    fn host_row_local() -> PickerRow {
        PickerRow {
            name: "local".into(),
            label: "local".into(),
            host: None,
            kind: RowKind::Host,
            status: RowStatus::Live,
        }
    }

    /// A label == name session row (no ` — 1 win` noise) for clean filter tests.
    fn plain(name: &str) -> PickerRow {
        PickerRow {
            name: name.into(),
            label: name.into(),
            host: None,
            kind: RowKind::Session,
            status: RowStatus::Live,
        }
    }

    // Two local sessions, a configured host + its session, and an ad-hoc host.
    fn roster_state() -> PickerState {
        let mut s = PickerState::new(vec![
            session("main", None),
            session("build", None),
            host_row("prod", RowStatus::Live),
            session("api", Some("prod")),
            host_row("scratch", RowStatus::Unreachable),
        ]);
        s.set_adhoc_hosts(vec!["scratch".into()]);
        s
    }

    // --- Milestone A behavior, updated to the new row shape ---

    #[test]
    fn enter_selects_the_cursor_row() {
        let mut s = PickerState::new(vec![session("main", None), session("build", None)]);
        assert_eq!(s.handle_key(0x0e), None); // Ctrl-N / down → "build"
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Switch("build".into()))
        );
    }

    #[test]
    fn esc_cancels() {
        let mut s = roster_state();
        assert_eq!(s.handle_key(0x1b), Some(PickerOutcome::Cancel));
    }

    #[test]
    fn filter_narrows_rows() {
        let mut s = PickerState::new(vec![session("main", None), session("build", None)]);
        s.handle_key(b'b'); // filter "b" → only "build"
        assert_eq!(s.visible().len(), 1);
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Switch("build".into()))
        );
    }

    #[test]
    fn render_contains_the_title_and_every_visible_row() {
        let s = PickerState::new(vec![session("main", None), session("build", None)]);
        let text = String::from_utf8(s.render()).expect("render output is valid UTF-8");
        assert!(text.contains("switch session"));
        assert!(text.contains("main \u{2014} 1 win"));
        assert!(text.contains("build \u{2014} 1 win"));
    }

    // --- (a) synthesized section headers + divider ---

    #[test]
    fn render_synthesizes_headers_and_divider() {
        let s = roster_state();
        let text = String::from_utf8(s.render()).expect("render output is valid UTF-8");
        assert!(text.contains("local"), "local section header");
        assert!(text.contains("prod"), "configured host row");
        assert!(text.contains("(configured)"), "configured tag");
        assert!(text.contains("scratch"), "ad-hoc host row");
        assert!(text.contains("(ad-hoc)"), "ad-hoc tag");
        assert!(
            text.contains("\u{2500}\u{2500}\u{2500}\u{2500}"),
            "divider between configured and ad-hoc"
        );
    }

    #[test]
    fn headers_only_for_sections_with_visible_rows() {
        // Filter to a local session only: the `local` header still renders
        // (synthesized from the visible child, not filtered — "main" is not the
        // word "local"), but nothing configured/ad-hoc and no divider.
        let mut s = roster_state();
        for c in "main".chars() {
            s.handle_key(c as u8);
        }
        let text = String::from_utf8(s.render()).expect("render output is valid UTF-8");
        assert!(
            text.contains("local"),
            "local header survives a child-only filter"
        );
        assert!(!text.contains("(configured)"), "prod filtered out");
        assert!(!text.contains("(ad-hoc)"), "scratch filtered out");
        assert!(
            !text.contains("\u{2500}\u{2500}"),
            "no divider without an ad-hoc block"
        );
    }

    // --- (b) bounded cursor over the selectable set; empty view parks + no-ops ---

    #[test]
    fn cursor_visits_only_selectable_rows() {
        let mut s = roster_state();
        let n = s.visible().len();
        assert_eq!(n, 5, "session + host rows are all selectable");
        for _ in 0..(n + 2) {
            // Every cursor position lands on a real row (headers are synthesized,
            // never selectable), so `selected` is always `Some`.
            assert!(s.selected().is_some(), "cursor never parks on a header");
            s.handle_key(0x0e); // down; wraps, stays bounded
        }
    }

    #[test]
    fn filter_matching_nothing_parks_cursor_and_enter_is_none() {
        let mut s = roster_state();
        for c in "zzzz".chars() {
            s.handle_key(c as u8);
        }
        assert!(s.visible().is_empty());
        assert_eq!(s.selected(), None);
        assert_eq!(
            s.handle_key(0x0e),
            None,
            "move is a bounded no-op when empty"
        );
        assert_eq!(s.handle_key(b'\r'), None, "Enter parks: no outcome");
    }

    // --- (c) `n`/`x` are NOT stolen while typing a filter ---

    #[test]
    fn filtering_with_n_narrows_to_nginx() {
        let mut s = PickerState::new(vec![plain("nginx"), plain("build"), plain("prod-x")]);
        // First `n`: filter empty but the cursor is on a SESSION row, so `n` is
        // ordinary filter input, not a New action.
        for c in "nginx".chars() {
            assert_eq!(s.handle_key(c as u8), None);
        }
        assert_eq!(s.filter(), "nginx");
        let names: Vec<_> = s.visible().iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec!["nginx".to_string()]);
    }

    #[test]
    fn filtering_with_trailing_x_lands_on_prod_x() {
        let mut s = PickerState::new(vec![plain("prod-x"), plain("build")]);
        for c in "prod-x".chars() {
            assert_eq!(s.handle_key(c as u8), None);
        }
        assert_eq!(s.filter(), "prod-x");
        let names: Vec<_> = s.visible().iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec!["prod-x".to_string()]);
    }

    // --- (d) `n`/`x` as actions on host rows ---

    #[test]
    fn n_on_host_row_prompts_and_enter_commits_new_with_n_and_x_in_name() {
        let mut s = roster_state();
        s.handle_key(0x0e); // → build
        s.handle_key(0x0e); // → prod (host row, index 2)
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Host));
        assert_eq!(
            s.handle_key(b'n'),
            None,
            "n opens the prompt (no outcome yet)"
        );
        // Every printable — including n and x — types into the name buffer.
        for c in "newx1".chars() {
            assert_eq!(s.handle_key(c as u8), None);
        }
        match s.handle_key(b'\r') {
            Some(PickerOutcome::New { host, name }) => {
                assert_eq!(host, Some("prod".into()));
                assert_eq!(name, "newx1");
            }
            other => panic!("expected New, got {other:?}"),
        }
    }

    /// Regression: `n` must fire on the LOCAL anchor (`RowKind::Host`,
    /// `host: None`) too, not just remote host rows — before the fix,
    /// `cursor_host` only matched `Some(h)` and `n` fell through to filter
    /// input, so you could create a new session on a remote daemon but not on
    /// the current one.
    #[test]
    fn n_on_local_anchor_prompts_and_enter_commits_new_with_host_none() {
        let mut s = PickerState::new(vec![host_row_local(), session("main", None)]);
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Host));
        assert_eq!(
            s.handle_key(b'n'),
            None,
            "n opens the prompt on the local anchor too (no outcome yet)"
        );
        for c in "fresh".chars() {
            assert_eq!(s.handle_key(c as u8), None);
        }
        match s.handle_key(b'\r') {
            Some(PickerOutcome::New { host, name }) => {
                assert_eq!(host, None, "the local anchor's host is None");
                assert_eq!(name, "fresh");
            }
            other => panic!("expected New, got {other:?}"),
        }
    }

    #[test]
    fn x_forgets_adhoc_host_but_not_configured() {
        let mut s = roster_state();
        s.handle_key(0x0e); // build
        s.handle_key(0x0e); // prod (configured host, index 2)
        assert_eq!(
            s.handle_key(b'x'),
            None,
            "x on a configured host is a no-op"
        );
        assert_eq!(s.filter(), "", "the no-op does not leak into the filter");
        s.handle_key(0x0e); // api
        s.handle_key(0x0e); // scratch (ad-hoc host, index 4)
        assert_eq!(s.selected().map(|r| r.name.clone()), Some("scratch".into()));
        assert_eq!(
            s.handle_key(b'x'),
            Some(PickerOutcome::Forget {
                host: "scratch".into()
            })
        );
    }

    /// Regression: `x` must be a true no-op on the LOCAL anchor (`RowKind::Host`,
    /// `host: None`) too, not just configured remotes — before the fix, `x`
    /// relied on `cursor_host`, which returned `None` for a `host: None` Host
    /// row indistinguishably from "not a host row", so it fell through to
    /// `push_filter` and blanked the visible list instead of no-opping.
    #[test]
    fn x_on_local_anchor_is_a_true_no_op() {
        let mut s = PickerState::new(vec![host_row_local(), session("main", None)]);
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Host));
        assert_eq!(
            s.handle_key(b'x'),
            None,
            "x on the local anchor is a no-op"
        );
        assert_eq!(s.filter(), "", "the no-op does not leak into the filter");
    }

    #[test]
    fn prompting_esc_returns_to_navigate() {
        let mut s = roster_state();
        s.handle_key(0x0e);
        s.handle_key(0x0e); // prod host row
        s.handle_key(b'n'); // → Prompting
        s.handle_key(b'a'); // buf "a"
        assert_eq!(
            s.handle_key(0x1b),
            None,
            "Esc leaves Prompting without an outcome"
        );
        // Back in Navigate, Esc cancels the picker.
        assert_eq!(s.handle_key(0x1b), Some(PickerOutcome::Cancel));
    }

    // --- (e) Enter: remote → Reconnect, local → Switch ---

    #[test]
    fn enter_switches_local_session() {
        let mut s = roster_state();
        assert_eq!(s.selected().map(|r| r.name.clone()), Some("main".into()));
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Switch("main".into()))
        );
    }

    // --- (f) Task 5: cursor-on-current + the streaming resolve_host drain ---

    #[test]
    fn new_with_current_parks_cursor_on_current_session() {
        // Rows: local anchor (0), then the two local sessions. The cursor must
        // land on the CURRENT session ("build"), not row 0.
        let rows = vec![
            host_row_local(),
            session("main", None),
            session("build", None),
        ];
        let s = PickerState::new_with_current(rows, &None, "build");
        assert_eq!(s.selected().map(|r| r.name.clone()), Some("build".into()));
    }

    #[test]
    fn new_with_current_falls_back_to_row_zero_when_session_absent() {
        let rows = vec![host_row_local(), session("main", None)];
        let s = PickerState::new_with_current(rows, &None, "ghost");
        // No "ghost" row → cursor parks at 0 (the anchor).
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Host));
    }

    #[test]
    fn resolve_host_live_inserts_sessions_and_sets_status() {
        let mut s = PickerState::new(vec![
            host_row_local(),
            session("main", None),
            host_row("prod", RowStatus::Pending),
        ]);
        s.resolve_host(
            &Some("prod".into()),
            RowStatus::Live,
            vec![session("api", Some("prod"))],
        );
        let rows: Vec<_> = s.visible().into_iter().cloned().collect();
        let prod = rows
            .iter()
            .position(|r| r.name == "prod")
            .expect("prod row");
        assert_eq!(rows[prod].status, RowStatus::Live, "anchor now Live");
        assert_eq!(
            rows[prod + 1].name,
            "api",
            "session spliced after its anchor"
        );
        assert_eq!(rows[prod + 1].kind, RowKind::Session);
        assert_eq!(rows[prod + 1].host.as_deref(), Some("prod"));
    }

    #[test]
    fn resolve_host_unreachable_sets_status_only() {
        let mut s = PickerState::new(vec![host_row("prod", RowStatus::Pending)]);
        s.resolve_host(&Some("prod".into()), RowStatus::Unreachable, vec![]);
        assert_eq!(
            s.selected().map(|r| r.status.clone()),
            Some(RowStatus::Unreachable)
        );
        assert_eq!(s.visible().len(), 1, "no session rows added");
    }

    #[test]
    fn resolve_host_is_idempotent_on_reresolve() {
        let mut s = PickerState::new(vec![host_row("prod", RowStatus::Pending)]);
        let live = |name: &str| session(name, Some("prod"));
        s.resolve_host(
            &Some("prod".into()),
            RowStatus::Live,
            vec![live("a"), live("b")],
        );
        // A second Live result replaces (does not duplicate) the child rows.
        s.resolve_host(&Some("prod".into()), RowStatus::Live, vec![live("c")]);
        let names: Vec<_> = s.visible().iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec!["prod".to_string(), "c".to_string()]);
    }

    // --- (g) Task 6: `x`/Forget rebuild ---

    #[test]
    fn replace_other_rows_drops_other_daemon_rows_and_keeps_current() {
        // A resolved "prod" (with a spliced session) plus a still-Pending
        // "scratch" sit alongside the current-local anchor + its own session.
        // Rebuilding with a fresh (smaller) roster set must drop BOTH old
        // other-daemon rows — including "prod"'s already-resolved child — and
        // keep the current daemon's own rows untouched.
        let mut s = PickerState::new(vec![
            host_row_local(),
            session("main", None),
            host_row("prod", RowStatus::Live),
            session("api", Some("prod")),
            host_row("scratch", RowStatus::Pending),
        ]);
        s.replace_other_rows(
            &None,
            vec![host_row("scratch", RowStatus::Pending)],
            vec!["scratch".into()],
        );
        let names: Vec<_> = s.visible().iter().map(|r| r.name.clone()).collect();
        assert_eq!(
            names,
            vec![
                "local".to_string(),
                "main".to_string(),
                "scratch".to_string()
            ]
        );
        assert!(
            !names.contains(&"prod".to_string()),
            "forgotten host's row is gone"
        );
    }

    #[test]
    fn replace_other_rows_reclamps_a_cursor_that_lost_its_row() {
        let mut s = PickerState::new(vec![host_row_local(), host_row("prod", RowStatus::Live)]);
        s.handle_key(0x0e); // cursor -> "prod" (index 1)
        assert_eq!(s.selected().map(|r| r.name.clone()), Some("prod".into()));
        s.replace_other_rows(&None, vec![], vec![]);
        assert_eq!(
            s.visible().len(),
            1,
            "prod's row was dropped, none replaced it"
        );
        assert!(
            s.selected().is_some(),
            "cursor re-clamped instead of dangling"
        );
    }

    #[test]
    fn render_local_anchor_has_no_configured_tag() {
        let s = PickerState::new(vec![host_row_local(), session("main", None)]);
        let text = String::from_utf8(s.render()).expect("render output is valid UTF-8");
        assert!(text.contains("local"), "the local anchor renders");
        assert!(
            !text.contains("(configured)"),
            "the local anchor is not a configured remote"
        );
    }

    #[test]
    fn enter_reconnects_remote_session() {
        let mut s = roster_state();
        for _ in 0..3 {
            s.handle_key(0x0e); // → api (remote session under prod, index 3)
        }
        assert_eq!(
            s.selected().map(|r| (r.name.clone(), r.kind)),
            Some(("api".into(), RowKind::Session))
        );
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Reconnect {
                host: Some("prod".into()),
                name: "api".into()
            })
        );
    }
}
