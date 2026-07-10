//! Pure core for the client-rendered session picker (multi-daemon session
//! picker, Milestone A: same-daemon sessions only). The daemon sends
//! `ServerMsg::OpenSessionPicker` to a v12+ client instead of opening its own
//! overlay; `pump` builds a `PickerState` from the session list, drives it
//! from stdin, and renders it directly to the client's own terminal — no
//! compositor, no daemon round-trip per keystroke. Modeled on the mux
//! history-palette core (`crates/mux/src/history.rs`): printables filter,
//! arrows/Ctrl-N/Ctrl-P/Ctrl-J/Ctrl-K move, Enter selects, Esc cancels.
//!
//! Milestone A only produces `PickerOutcome::{Switch, Cancel}`; Milestone B
//! (the multi-daemon roster) adds `Reconnect`/`New`/`Forget`.

/// One row in the picker list: the daemon's session name (the wire identity
/// used in `SwitchSession`), the pre-formatted display label, and whether
/// this is the session the client is currently attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRow {
    pub name: String,
    pub label: String,
    pub is_current: bool,
}

/// What the user did with the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    Switch(String),
    Cancel,
}

/// Filter + cursor over a fixed row list. `cursor` indexes into `visible()`,
/// not `rows` — it re-clamps (not remaps) whenever the filter narrows the
/// list, matching the mux finder cores (`crates/mux/src/history.rs`).
pub struct PickerState {
    rows: Vec<PickerRow>,
    filter: String,
    cursor: usize,
}

impl PickerState {
    pub fn new(rows: Vec<PickerRow>) -> Self {
        // Start the cursor on the current row if present, so Enter with no
        // input re-selects the session already attached (a no-op the pump
        // treats as cancel).
        let cursor = rows.iter().position(|r| r.is_current).unwrap_or(0);
        Self {
            rows,
            filter: String::new(),
            cursor,
        }
    }

    /// Rows matching the live filter (case-insensitive substring on the
    /// label), in original order.
    pub fn visible(&self) -> Vec<&PickerRow> {
        let f = self.filter.to_lowercase();
        self.rows
            .iter()
            .filter(|r| r.label.to_lowercase().contains(&f))
            .collect()
    }

    /// Apply one input byte. Returns `Some(outcome)` when the key
    /// commits/cancels the picker, `None` while merely navigating/filtering.
    ///
    /// Arrow-key escape sequences (`\e[A`/`\e[B`) are multi-byte, so the pump
    /// decodes them to the Ctrl-P/Ctrl-N equivalents (`0x10`/`0x0e`) before
    /// calling this. Enter is `\r` (raw mode sends `\r`, not `\n`, for
    /// Enter); `\n` is reserved for Ctrl-J (down), matching the Ctrl-K (up) /
    /// Ctrl-J (down) pairing used elsewhere in the codebase.
    pub fn handle_key(&mut self, byte: u8) -> Option<PickerOutcome> {
        match byte {
            0x1b => Some(PickerOutcome::Cancel), // Esc
            b'\r' => {
                let vis = self.visible();
                vis.get(self.cursor)
                    .map(|r| PickerOutcome::Switch(r.name.clone()))
            }
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
            b if b.is_ascii_graphic() || b == b' ' => {
                self.filter.push(b as char);
                self.clamp();
                None
            }
            _ => None,
        }
    }

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

    /// Bytes to draw the picker on the client's own terminal: clear-and-home,
    /// a title, each visible row (current session marked, cursor row
    /// reversed), and a filter line. A flat unstyled list — no compositor, no
    /// config-driven palette; Milestone A only lists the current daemon's
    /// sessions.
    pub fn render(&self) -> Vec<u8> {
        // ponytail: no real terminal-width plumbing into the picker yet (it
        // renders straight to the client's own tty outside the compositor);
        // 100 cols covers any realistic session label. Revisit if rows
        // routinely wrap.
        const MAX_ROW_WIDTH: u16 = 100;
        let mut out = String::new();
        out.push_str("\x1b[2J\x1b[H");
        out.push_str("plexy-glass \u{2014} switch session\r\n\r\n");
        for (i, row) in self.visible().into_iter().enumerate() {
            let marker = if row.is_current { "* " } else { "  " };
            let label = plexy_glass_emulator::truncate_to_width(&row.label, MAX_ROW_WIDTH);
            if i == self.cursor {
                out.push_str("\x1b[7m");
                out.push_str(marker);
                out.push_str(label);
                out.push_str("\x1b[0m\r\n");
            } else {
                out.push_str(marker);
                out.push_str(label);
                out.push_str("\r\n");
            }
        }
        out.push_str("\r\nfilter: ");
        out.push_str(&self.filter);
        out.push_str("\r\n");
        out.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PickerState {
        PickerState::new(vec![
            PickerRow {
                name: "main".into(),
                label: "main — 1 win".into(),
                is_current: true,
            },
            PickerRow {
                name: "build".into(),
                label: "build — 1 win".into(),
                is_current: false,
            },
        ])
    }

    #[test]
    fn enter_selects_the_cursor_row() {
        let mut s = state();
        // cursor starts on the current row ("main"); move down to "build".
        assert_eq!(s.handle_key(0x0e), None); // Ctrl-N / down → move
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Switch("build".into()))
        );
    }

    #[test]
    fn esc_cancels() {
        let mut s = state();
        assert_eq!(s.handle_key(0x1b), Some(PickerOutcome::Cancel));
    }

    #[test]
    fn filter_narrows_rows() {
        let mut s = state();
        s.handle_key(b'b'); // filter "b" → only "build"
        assert_eq!(s.visible().len(), 1);
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Switch("build".into()))
        );
    }

    #[test]
    fn render_contains_the_title_and_every_visible_row() {
        let s = state();
        let text = String::from_utf8(s.render()).expect("render output is valid UTF-8");
        assert!(text.contains("switch session"));
        assert!(text.contains("main — 1 win"));
        assert!(text.contains("build — 1 win"));
    }
}
