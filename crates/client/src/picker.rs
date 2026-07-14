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
//! Section headers (`local`), the configured/ad-hoc divider, and the trailing
//! `＋ Connect to a host…` slot are **synthesized at render time** — none are
//! stored as rows. Headers/divider come from which sections have ≥1 visible
//! (filter-matching) row; the `＋` slot is always present as the last selectable
//! position. Because it's synthesized, "two `＋` sentinels" or a mid-list one is
//! unrepresentable. The cursor ranges over `visible().len() + 1` positions: a
//! position `< visible().len()` is a real row (`selected()`), and the last one
//! (`== visible().len()`) is the `＋` slot (`is_new_host_selected()`). Remote
//! hosts get a real, selectable `Host` row (the anchor for `n`/`x`) that doubles
//! as that host's visible header.

use std::fmt::Write as _;

use plexy_glass_config::PaletteConfig;
use plexy_glass_emulator::{display_width, truncate_to_width};
use plexy_glass_protocol::PtySize;
use plexy_glass_status::Rgb;

use crate::transport::{Host, InstallPolicy, RemoteName};

/// The picker's resolved colors, mapped to the same palette roles the daemon's
/// `chrome_colors` uses so the box matches every other overlay. Resolved once at
/// picker build from the client's `cfg.palette`; a role absent from a custom
/// palette falls back to the fixed kanagawa-dragon default (matching the config
/// built-in default), so the look is stable across any config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickerTheme {
    pub border: Rgb,      // accent
    pub title: Rgb,       // highlight
    pub footer: Rgb,      // muted
    pub interior: Rgb,    // bg_bar
    pub selected_bg: Rgb, // highlight
    pub live: Rgb,        // ok
    pub empty: Rgb,       // muted
    pub unreachable: Rgb, // alert
    pub warn: Rgb,        // warn
    pub pending: Rgb,     // muted
}

impl PickerTheme {
    const ACCENT: Rgb = Rgb {
        r: 0x73,
        g: 0x7c,
        b: 0x73,
    };
    const HIGHLIGHT: Rgb = Rgb {
        r: 0xb6,
        g: 0x92,
        b: 0x7b,
    };
    const MUTED: Rgb = Rgb {
        r: 0xb6,
        g: 0x92,
        b: 0x7b,
    };
    const BG_BAR: Rgb = Rgb {
        r: 0x28,
        g: 0x27,
        b: 0x27,
    };
    const ALERT: Rgb = Rgb {
        r: 0xc4,
        g: 0x74,
        b: 0x6e,
    };
    const WARN: Rgb = Rgb {
        r: 0xc4,
        g: 0xb2,
        b: 0x8a,
    };
    const OK: Rgb = Rgb {
        r: 0x87,
        g: 0xa9,
        b: 0x87,
    };

    fn role(palette: &PaletteConfig, name: &str, default: Rgb) -> Rgb {
        // Palette entries are pre-parsed `Rgb`; a role absent from a custom
        // palette falls back to the fixed kanagawa-dragon default.
        palette.entries.get(name).copied().unwrap_or(default)
    }

    pub fn resolve(palette: &PaletteConfig) -> Self {
        Self {
            border: Self::role(palette, "accent", Self::ACCENT),
            title: Self::role(palette, "highlight", Self::HIGHLIGHT),
            footer: Self::role(palette, "muted", Self::MUTED),
            interior: Self::role(palette, "bg_bar", Self::BG_BAR),
            selected_bg: Self::role(palette, "highlight", Self::HIGHLIGHT),
            live: Self::role(palette, "ok", Self::OK),
            empty: Self::role(palette, "muted", Self::MUTED),
            unreachable: Self::role(palette, "alert", Self::ALERT),
            warn: Self::role(palette, "warn", Self::WARN),
            pending: Self::role(palette, "muted", Self::MUTED),
        }
    }
}

impl Default for PickerTheme {
    fn default() -> Self {
        // An empty palette resolves nothing → all fixed defaults.
        Self::resolve(&PaletteConfig::default())
    }
}

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
/// the daemon the row lives on ([`Host::Local`] or a remote); `kind`/`status`
/// drive the sectioning + indicators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRow {
    pub name: String,
    pub label: String,
    pub host: Host,
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
    /// `PumpExit::ReconnectTo` — is wired in Task 6). `install` carries the
    /// picker's persistent `i` toggle through to the reconnect `Target`.
    Reconnect {
        host: Host,
        name: String,
        install: InstallPolicy,
    },
    /// New session `name` on `host`'s daemon (create-if-missing at reconnect).
    /// `install` carries the picker's persistent `i` toggle through to the
    /// reconnect `Target`.
    New {
        host: Host,
        name: String,
        install: InstallPolicy,
    },
    /// Forget an ad-hoc host from the client-side roster file.
    Forget { host: RemoteName },
    /// Kill session `name` on `host`'s daemon (`k` then `y` confirms on a
    /// session row). The picker never talks to the daemon itself; the pump
    /// resolves `host` to a `Target` and sends a one-off `KillSession` on a
    /// fresh connection (the `client_kill_session` pattern, `lib.rs:407`).
    Kill { host: Host, name: String },
}

/// The picker's input sub-mode. `Navigate` is **action-first**: letters are
/// actions (`i` install toggle, `n` new, `x` forget, `k` kill), arrows move,
/// `Enter` connects, and `/` enters `Filtering` — no key types into the filter
/// here. `Filtering` is the explicit filter mode: printables edit
/// `self.filter`, `Enter`/an arrow return to `Navigate` (keeping the filter),
/// `Esc` clears it. In `Prompting`, every printable (including `n`/`x`) types
/// the new session's name, `Enter` commits `New`, `Esc` returns to `Navigate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerMode {
    Navigate,
    /// Explicit filter mode, entered with `/` from `Navigate`. Printables edit
    /// `self.filter` live; `Enter` returns to `Navigate` keeping the filter; an
    /// arrow returns to `Navigate` and moves the cursor; `Esc` clears the filter
    /// and returns to `Navigate`.
    Filtering,
    Prompting {
        host: Host,
        buf: String,
    },
    /// Typing a brand-new ad-hoc ssh target (from the synthesized `＋` slot).
    /// Enter with a non-empty `buf` commits `Reconnect`; empty is refused; Esc
    /// returns to `Navigate`. Distinct from `Prompting`, which names a new session.
    PromptingHost {
        buf: String,
    },
    /// Confirm-before-destroy for `k` on a session row (mirrors the
    /// choose-tree's `TreeMode::ConfirmKill`): `y` commits `PickerOutcome::Kill`
    /// for the `{host, name}` captured when `k` was pressed; `n`/Esc aborts back
    /// to `Navigate` with no outcome.
    ConfirmKill {
        host: Host,
        name: String,
    },
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
    /// The daemon this client is currently attached to — `None` when attached
    /// which daemon we are attached to, or `None` when we are attached to NONE
    /// (the standalone picker, opened after an attach failure with no session to
    /// live in). Drives `accept`: a row on the current daemon fast-switches
    /// (`Switch`, no reconnect) and the current daemon's own host anchor is a
    /// no-op `Cancel`, while anything on a DIFFERENT daemon reconnects.
    ///
    /// `None` genuinely means "not attached" only because [`Host`] spells
    /// `Local` out: while the local daemon was encoded as `None`, this field
    /// could not tell "attached to local" from "attached to nothing", and a
    /// standalone picker answered Enter-on-local with `Cancel` (quit) because
    /// every local row compared equal to it.
    current_host: Option<Host>,
    filter: String,
    cursor: usize,
    mode: PickerMode,
    /// Persistent connect-with-install toggle, flipped by `i` in `Navigate`
    /// **unconditionally** (it's a global flag applied to the next host connect,
    /// not tied to any row or the filter). Carried into
    /// `PickerOutcome::{Reconnect,New}` at commit and read by the pump into the
    /// reconnect `Target`.
    install: InstallPolicy,
    /// The client's terminal size, seeded at build and updated on SIGWINCH, so
    /// `render` can center + size the box. Defaults to 24x80 for unit tests.
    size: PtySize,
    /// Resolved palette colors for the box (Task 1). Defaults to the fixed
    /// fallback theme; the pump seeds the real one via `set_theme`.
    theme: PickerTheme,
}

impl PickerState {
    pub fn new(rows: Vec<PickerRow>) -> Self {
        Self {
            rows,
            adhoc: Vec::new(),
            current_host: Some(Host::Local),
            filter: String::new(),
            cursor: 0,
            mode: PickerMode::Navigate,
            install: InstallPolicy::UseExisting,
            size: PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            theme: PickerTheme::default(),
        }
    }

    /// Like [`new`](Self::new) but parks the cursor on the current daemon's
    /// current session row (matched by `host` + `name`) instead of row 0,
    /// restoring Milestone A's cursor-on-current behavior that the flat
    /// `is_current` drop lost. Falls back to the first row (the current-daemon
    /// anchor) when that session isn't present.
    pub fn new_with_current(
        rows: Vec<PickerRow>,
        current_host: &Option<Host>,
        current_session: &str,
    ) -> Self {
        let cursor = rows
            .iter()
            .position(|r| {
                r.kind == RowKind::Session
                    && Some(&r.host) == current_host.as_ref()
                    && r.name == current_session
            })
            .unwrap_or(0);
        Self {
            rows,
            adhoc: Vec::new(),
            current_host: current_host.clone(),
            filter: String::new(),
            cursor,
            mode: PickerMode::Navigate,
            install: InstallPolicy::UseExisting,
            size: PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            theme: PickerTheme::default(),
        }
    }

    /// Fold a resolved daemon's streaming query result into the rows: set that
    /// host's `Host` anchor status and replace its `Session` child rows (both
    /// matched by `host`, so `None` targets the local anchor). `sessions` is
    /// empty for anything but a `Live` result; a `Live` daemon's sessions are
    /// spliced in right after its anchor. Idempotent — a re-resolve drops the
    /// prior session rows first — and re-clamps the cursor.
    pub fn resolve_host(&mut self, host: &Host, status: RowStatus, mut sessions: Vec<PickerRow>) {
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

    /// Drop the one session row `(host, name)` — the pump's local roster edit
    /// after a successful kill of a NON-current session (mirrors
    /// `resolve_host`'s retain, but targets exactly one row instead of every
    /// session under a host). Re-clamps the cursor same as the other
    /// roster-mutating methods.
    pub fn remove_row(&mut self, host: &Host, name: &str) {
        self.rows
            .retain(|r| !(r.kind == RowKind::Session && r.host == *host && r.name == name));
        self.clamp();
    }

    /// Seed the terminal size (pump, at build) and update it on resize.
    pub const fn set_size(&mut self, size: PtySize) {
        self.size = size;
    }

    /// Seed the resolved palette theme (pump, at build).
    pub const fn set_theme(&mut self, theme: PickerTheme) {
        self.theme = theme;
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
        current_host: &Option<Host>,
        rows: Vec<PickerRow>,
        adhoc: Vec<String>,
    ) {
        self.rows.retain(|r| Some(&r.host) == current_host.as_ref());
        self.rows.extend(rows);
        self.adhoc = adhoc;
        self.clamp();
    }

    /// Rows matching the live filter (case-insensitive substring on the label),
    /// in original order. Every visible row is selectable — headers, the divider,
    /// and the `＋ Connect to a host…` slot are synthesized at render time and are
    /// not rows.
    pub fn visible(&self) -> Vec<&PickerRow> {
        let f = self.filter.to_lowercase();
        self.rows
            .iter()
            .filter(|r| r.label.to_lowercase().contains(&f))
            .collect()
    }

    /// The row under the cursor, or `None` when the cursor sits on the synthesized
    /// `＋` slot (`is_new_host_selected`) — which is also the only position when the
    /// filter matches nothing, since the slot is always present.
    pub fn selected(&self) -> Option<&PickerRow> {
        self.visible().get(self.cursor).copied()
    }

    /// Whether the cursor sits on the synthesized `＋ Connect to a host…` slot,
    /// the always-present last position (`cursor == visible().len()`). Enter here
    /// opens the host prompt and `i` still toggles install; `selected()` is `None`
    /// exactly when this is `true`.
    pub fn is_new_host_selected(&self) -> bool {
        self.cursor == self.visible().len()
    }

    /// The live filter text.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Whether the next host connect provisions the remote binary first.
    pub const fn install_enabled(&self) -> bool {
        self.install.provisions()
    }

    /// Apply one input byte. Returns `Some(outcome)` when the key commits/cancels
    /// the picker, `None` while navigating/filtering/prompting.
    ///
    /// Arrow-key escape sequences (`\e[A`/`\e[B`) are multi-byte, so the pump
    /// decodes them to Ctrl-P/Ctrl-N (`0x10`/`0x0e`) before calling this. Enter
    /// is `\r` (raw mode sends `\r`); `\n` is Ctrl-J (down), pairing with Ctrl-K
    /// (up) as elsewhere in the codebase.
    pub fn handle_key(&mut self, byte: u8) -> Option<PickerOutcome> {
        match self.mode {
            PickerMode::Prompting { .. } => self.handle_prompting(byte),
            PickerMode::PromptingHost { .. } => self.handle_prompting_host(byte),
            PickerMode::ConfirmKill { .. } => self.handle_confirm_kill(byte),
            PickerMode::Filtering => self.handle_filtering(byte),
            PickerMode::Navigate => self.handle_navigate(byte),
        }
    }

    /// Action-first navigation: letters are ACTIONS, never filter input. `/`
    /// switches to `Filtering` (the only way to type into the filter). Any key
    /// that isn't a bound action is a no-op — no implicit filtering.
    fn handle_navigate(&mut self, byte: u8) -> Option<PickerOutcome> {
        match byte {
            0x1b => Some(PickerOutcome::Cancel), // Esc
            b'\r' => {
                // The synthesized `＋` slot opens the host prompt instead of
                // committing; intercept it here so `accept()` only ever sees rows.
                if self.is_new_host_selected() {
                    self.mode = PickerMode::PromptingHost { buf: String::new() };
                    return None;
                }
                self.accept()
            }
            0x0e | 0x0a => {
                self.move_cursor(1);
                None
            } // Ctrl-N / Ctrl-J (down)
            0x10 | 0x0b => {
                self.move_cursor(-1);
                None
            } // Ctrl-P / Ctrl-K (up)
            b'/' => {
                self.mode = PickerMode::Filtering;
                None
            }
            // `i` toggles the persistent connect-with-install flag
            // UNCONDITIONALLY — it's a global flag applied to the next host
            // connect, not tied to the cursor row or the filter. It never
            // produces an outcome; the toggle is read at `accept`/`New`-commit
            // time and shown in the footer.
            b'i' => {
                self.install = self.install.toggled();
                None
            }
            // `n` opens the new-session prompt only on a host row (the LOCAL
            // anchor is a `Host` row with `host: None`, so `n` works on the
            // current daemon too, not just remotes). Anywhere else it's a no-op.
            b'n' => {
                if let Some(row) = self.selected()
                    && row.kind == RowKind::Host
                {
                    self.mode = PickerMode::Prompting {
                        host: row.host.clone(),
                        buf: String::new(),
                    };
                }
                None
            }
            // `x` forgets only an ad-hoc host row; it's a no-op on the local
            // anchor, on configured hosts, on session rows, and on the `＋` slot
            // (`selected()` is `None` there) — nothing to forget on any of those.
            b'x' => match self.selected() {
                Some(row) if row.kind == RowKind::Host => match row.host.remote() {
                    Some(h) if self.is_adhoc(h) => Some(PickerOutcome::Forget { host: h.clone() }),
                    _ => None,
                },
                _ => None,
            },
            // `k` opens the kill confirmation only on a session row; a no-op on
            // a host row (the anchor identifies a daemon, not a session to
            // kill) and on the `＋` slot (`selected()` is `None` there).
            b'k' => {
                if let Some(row) = self.selected()
                    && row.kind == RowKind::Session
                {
                    self.mode = PickerMode::ConfirmKill {
                        host: row.host.clone(),
                        name: row.name.clone(),
                    };
                }
                None
            }
            _ => None, // no implicit filtering: unbound keys do nothing
        }
    }

    /// `y`/`n` (or Esc) for the `k`-opened kill confirmation: `y` commits
    /// `PickerOutcome::Kill` for the `{host, name}` captured at `k`-press time
    /// and returns to `Navigate`; `n`/Esc aborts back to `Navigate` with no
    /// outcome; anything else is a no-op (stays in `ConfirmKill`).
    fn handle_confirm_kill(&mut self, byte: u8) -> Option<PickerOutcome> {
        match byte {
            b'y' => {
                let PickerMode::ConfirmKill { host, name } = &self.mode else {
                    return None; // invariant: only reached while ConfirmKill
                };
                let outcome = PickerOutcome::Kill {
                    host: host.clone(),
                    name: name.clone(),
                };
                self.mode = PickerMode::Navigate;
                Some(outcome)
            }
            b'n' | 0x1b => {
                self.mode = PickerMode::Navigate;
                None
            }
            _ => None,
        }
    }

    /// Explicit filter mode (entered with `/`): printables narrow the list live.
    /// `Enter` returns to `Navigate` keeping the filter; an arrow returns to
    /// `Navigate` and moves; `Esc` clears the filter and returns to `Navigate`.
    fn handle_filtering(&mut self, byte: u8) -> Option<PickerOutcome> {
        match byte {
            b'\r' => {
                // Done editing: keep the filter applied and go navigate the
                // narrowed list.
                self.mode = PickerMode::Navigate;
                None
            }
            0x0e | 0x0a => {
                // An arrow ends filter mode AND applies the move (per the model).
                self.mode = PickerMode::Navigate;
                self.move_cursor(1);
                None
            } // Ctrl-N / Ctrl-J (down)
            0x10 | 0x0b => {
                self.mode = PickerMode::Navigate;
                self.move_cursor(-1);
                None
            } // Ctrl-P / Ctrl-K (up)
            0x1b => {
                // Esc clears the filter and returns to Navigate (the way to
                // undo a narrow).
                self.filter.clear();
                self.cursor = 0;
                self.clamp();
                self.mode = PickerMode::Navigate;
                None
            }
            0x7f => {
                self.filter.pop();
                self.cursor = 0; // fzf model: editing the filter resets to the top
                self.clamp();
                None
            } // Backspace
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
                if buf.is_empty() {
                    // Refuse an empty-named New: stay in Prompting rather than
                    // commit `New{name:""}` (which the daemon's `validate_name`
                    // rejects with `EmptyName`, ejecting the client). The user
                    // types a name or hits Esc to abandon.
                    return None;
                }
                let outcome = PickerOutcome::New {
                    host: host.clone(),
                    name: buf.clone(),
                    install: self.install,
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

    /// Typing a brand-new ad-hoc ssh target after Enter on the `＋` sentinel.
    /// Mirrors `handle_prompting`, but a non-empty buf commits `Reconnect` (the
    /// same outcome the rest of the picker uses to re-attach over SSH), carrying
    /// the persistent `i` install toggle. An empty buf is refused (stays
    /// prompting); Esc returns to Navigate.
    fn handle_prompting_host(&mut self, byte: u8) -> Option<PickerOutcome> {
        match byte {
            0x1b => {
                self.mode = PickerMode::Navigate;
                None
            }
            b'\r' => {
                let PickerMode::PromptingHost { buf } = &self.mode else {
                    return None; // invariant: only reached while PromptingHost
                };
                if buf.is_empty() {
                    return None; // refuse an empty host; stay prompting
                }
                let outcome = PickerOutcome::Reconnect {
                    host: Host::Remote(RemoteName::from(buf.clone())),
                    name: String::new(),
                    install: self.install,
                };
                self.mode = PickerMode::Navigate;
                Some(outcome)
            }
            0x7f => {
                if let PickerMode::PromptingHost { buf } = &mut self.mode {
                    buf.pop();
                }
                None
            } // Backspace
            b if b.is_ascii_graphic() || b == b' ' => {
                if let PickerMode::PromptingHost { buf } = &mut self.mode {
                    buf.push(b as char);
                }
                None
            }
            _ => None,
        }
    }

    /// Enter on the cursor row, routed by whether the row belongs to the daemon
    /// we're already attached to (`row.host == self.current_host`):
    /// - **Session on the current daemon** → `Switch` (a fast `SwitchSession`
    ///   over the live transport, no reconnect). Unambiguous: the current daemon
    ///   is excluded from the roster query, so no other row carries its host.
    /// - **Session on a different daemon** → `Reconnect` (a full re-attach).
    /// - **Current daemon's own host anchor** → `Cancel`: you're already here,
    ///   so Enter on it just closes the picker (no eject, no empty-name attach).
    /// - **A different daemon's host anchor** → `Reconnect` to that daemon's
    ///   DEFAULT session (empty `name`, normalized to the daemon default in the
    ///   pump before it hits the wire).
    ///
    /// An empty filtered view yields `None` (cursor parked).
    fn accept(&self) -> Option<PickerOutcome> {
        let row = self.selected()?;
        // `as_ref() == Some(&row.host)` and not `== self.current_host`: when we
        // are attached to nothing this is false for EVERY row, so every Enter is a
        // Reconnect and neither `Switch` (needs the attached daemon) nor an
        // anchor `Cancel` (indistinguishable from Esc at the call site) can be
        // produced by a standalone picker.
        let same_daemon = self.current_host.as_ref() == Some(&row.host);
        match row.kind {
            RowKind::Session if same_daemon => Some(PickerOutcome::Switch(row.name.clone())),
            RowKind::Session => Some(PickerOutcome::Reconnect {
                host: row.host.clone(),
                name: row.name.clone(),
                install: self.install,
            }),
            RowKind::Host if same_daemon => Some(PickerOutcome::Cancel),
            RowKind::Host => Some(PickerOutcome::Reconnect {
                host: row.host.clone(),
                name: String::new(),
                install: self.install,
            }),
        }
    }

    fn is_adhoc(&self, host: &RemoteName) -> bool {
        self.adhoc.iter().any(|h| h.as_str() == &**host)
    }

    fn push_filter(&mut self, byte: u8) {
        self.filter.push(byte as char);
        // fzf model (matching `mux::finder`): editing the filter resets the
        // cursor to the top of the narrowed view. Without this, a cursor parked
        // on the current session (or anywhere below row 0) survives the narrow
        // and can land on the always-present `＋ Connect to a host…` slot — so
        // filtering to a session and pressing Enter would open the host prompt
        // instead of switching.
        self.cursor = 0;
        self.clamp();
    }

    /// Step the cursor over the `visible().len() + 1` selectable positions — the
    /// real rows plus the trailing `＋` slot. Bounded via a single modular step;
    /// `n` is at least 1 (the `＋` slot always exists) so the view is never empty.
    fn move_cursor(&mut self, d: isize) {
        let n = self.visible().len() + 1;
        self.cursor = ((self.cursor as isize + d).rem_euclid(n as isize)) as usize;
    }

    fn clamp(&mut self) {
        // Positions are `0..=visible().len()` (the last is the `＋` slot).
        let last = self.visible().len();
        if self.cursor > last {
            self.cursor = last;
        }
    }

    /// Bytes to draw the picker as a centered, palette-themed box on the client's
    /// own terminal: a clear-and-home, a bordered box sized to `self.size`, a bold
    /// title, the visible rows grouped into sections (a synthesized `local`
    /// header, remote host rows as their own anchors, a full-width `─` divider row
    /// before the ad-hoc block), a filter/new-session prompt line, and a footer of
    /// key hints. The cursor is hidden every frame (`\x1b[?25l`); the pump restores
    /// it on every picker-exit path. Each box row is positioned with one
    /// `\x1b[{r};{c}H` and carries NO `\r`/`\n`. All width math is saturating so a
    /// tiny terminal squishes rather than panics.
    pub fn render(&self) -> Vec<u8> {
        let cols = self.size.cols.max(1) as usize;
        let rows = self.size.rows.max(1) as usize;
        let t = self.theme;

        // Box geometry: near-full-width with a 2-col side margin, centered,
        // saturating so a tiny terminal squishes rather than panics.
        let box_w = cols.saturating_sub(4).clamp(1, cols);
        let box_left = cols.saturating_sub(box_w) / 2;
        let inner_w = box_w.saturating_sub(2); // minus the two │ borders
        let text_w = inner_w.saturating_sub(2); // minus one pad each side

        // ---- content interior lines ----
        let mut lines: Vec<Line> = Vec::new();
        let mut emitted_local = false;
        let mut emitted_divider = false;
        let mut emitted_any = false;
        let visible: Vec<&PickerRow> = self.visible();
        for (i, row) in visible.iter().enumerate() {
            let is_local = row.host.is_local();
            let is_adhoc = row.host.remote().is_some_and(|h| self.is_adhoc(h));
            if is_local && !emitted_local {
                if row.kind == RowKind::Session {
                    lines.push(Line {
                        glyph: String::new(),
                        glyph_color: None,
                        text: "local".into(),
                        dim: true,
                        selected: false,
                        divider: false,
                    });
                }
                emitted_local = true;
            }
            if is_adhoc && emitted_any && !emitted_divider {
                lines.push(Line {
                    glyph: String::new(),
                    glyph_color: None,
                    text: String::new(),
                    dim: true,
                    selected: false,
                    divider: true,
                });
                emitted_divider = true;
            }
            let (glyph, gcolor, text) = self.row_parts(row);
            lines.push(Line {
                glyph,
                glyph_color: Some(gcolor),
                text,
                dim: false,
                selected: i == self.cursor,
                divider: false,
            });
            emitted_any = true;
        }

        // The synthesized `＋ Connect to a host…` slot: drawn last, always present
        // (never filtered), muted like the footer, and highlighted when the cursor
        // sits on it (`cursor == visible.len()`). Synthesized like the headers and
        // divider, so a second or mid-list `＋` is unrepresentable.
        lines.push(Line {
            glyph: String::new(),
            glyph_color: Some(t.footer),
            text: "\u{ff0b} Connect to a host\u{2026}".into(),
            dim: false,
            selected: self.cursor == visible.len(),
            divider: false,
        });

        // ---- title / footer / prompt ----
        let title = " plexy-glass ";
        let footer = self.footer_hint();
        let prompt = match &self.mode {
            PickerMode::Prompting { host, buf } => {
                format!("new session on {host}: {buf}")
            }
            PickerMode::PromptingHost { buf } => format!("connect to host: {buf}"),
            // The confirmation itself is the footer message (`footer_hint`);
            // the prompt line stays blank here.
            PickerMode::ConfirmKill { .. } => String::new(),
            PickerMode::Filtering => format!("filter: {}", self.filter),
            // In Navigate a non-empty filter stays visible (the list is still
            // narrowed) with a hint that `/` re-enters editing; an empty filter
            // shows nothing.
            PickerMode::Navigate => {
                if self.filter.is_empty() {
                    String::new()
                } else {
                    format!("filter: {}  (/ to edit)", self.filter)
                }
            }
        };

        // Interior rows = content lines + a blank + the prompt line.
        let mut interior: Vec<Line> = lines;
        interior.push(Line {
            glyph: String::new(),
            glyph_color: None,
            text: String::new(),
            dim: false,
            selected: false,
            divider: false,
        });
        interior.push(Line {
            glyph: String::new(),
            glyph_color: None,
            text: prompt,
            dim: true,
            selected: false,
            divider: false,
        });

        // Box height clamps to the terminal; content beyond is clipped top-anchored
        // (no scroll — same unbounded-list limitation as before; realistically few
        // daemons/sessions). ponytail: scroll-to-follow is future work.
        let box_h = (interior.len() + 2).min(rows);
        let content_rows = box_h.saturating_sub(2);
        interior.truncate(content_rows);
        let box_top = rows.saturating_sub(box_h) / 2;

        // ---- emit ----
        let mut out = String::new();
        out.push_str("\x1b[2J\x1b[H\x1b[?25l"); // clear, home, hide cursor
        let border = sgr_fg(t.border);
        for r in 0..box_h {
            // 1-based absolute position of this box row. Writing to a `String`
            // is infallible, so the `fmt::Result` is discarded.
            let _ = write!(out, "\x1b[{};{}H", box_top + r + 1, box_left + 1);
            if r == 0 {
                out.push_str(&border);
                out.push_str(&edge_row(
                    '\u{250c}',
                    '\u{2510}',
                    inner_w,
                    Some((title, t.title)),
                    t.border,
                ));
                out.push_str(RESET);
            } else if r == box_h - 1 {
                out.push_str(&border);
                out.push_str(&edge_row(
                    '\u{2514}',
                    '\u{2518}',
                    inner_w,
                    Some((&footer, t.footer)),
                    t.border,
                ));
                out.push_str(RESET);
            } else {
                let line = &interior[r - 1];
                out.push_str(&self.frame_interior(line, inner_w, text_w));
            }
        }
        out.push_str(RESET);
        out.into_bytes()
    }

    /// The glyph, its status color, and the label text of a content row — the
    /// coloring split out of the old `row_line`. Session rows get a 2-space row
    /// indent baked into the glyph (so `frame_interior`, which renders
    /// `glyph + " " + text`, indents the whole row without a separate field).
    fn row_parts(&self, row: &PickerRow) -> (String, Rgb, String) {
        let color = status_color(&row.status, &self.theme);
        let glyph = status_glyph(&row.status);
        match row.kind {
            RowKind::Host => {
                let text = if row.host.is_local() {
                    row.name.clone()
                } else {
                    let tag = if row.host.remote().is_some_and(|h| self.is_adhoc(h)) {
                        "ad-hoc"
                    } else {
                        "configured"
                    };
                    format!("{}  ({tag})", row.name)
                };
                (glyph, color, text)
            }
            RowKind::Session => (format!("  {glyph}"), color, row.label.clone()),
        }
    }

    /// Frame one interior line inside `│ … │`, padding to `text_w` on the
    /// theme's interior bg; the selected row fills its whole width with the
    /// selection bg. Divider rows draw a full-width `─` run in the border color.
    fn frame_interior(&self, line: &Line, inner_w: usize, text_w: usize) -> String {
        let t = self.theme;
        let border = sgr_fg(t.border);
        if line.divider {
            return format!(
                "{border}\u{2502}{}\u{2502}{RESET}",
                "\u{2500}".repeat(inner_w)
            );
        }
        // PLAIN content (no SGR) first, so the width math is exact.
        let plain = if line.glyph.is_empty() {
            line.text.clone()
        } else {
            format!("{} {}", line.glyph, line.text)
        };
        let shown = truncate_to_width(&plain, text_w as u16).to_string();
        let pad = " ".repeat(text_w.saturating_sub(display_width(&shown) as usize));
        // Style: selection bar, dim (headers/prompt text), or a glyph-colored row
        // on the interior bg.
        let (bg, fg) = if line.selected {
            (sgr_bg(t.selected_bg), sgr_fg(t.interior))
        } else if line.dim {
            (sgr_bg(t.interior), sgr_fg(t.footer))
        } else {
            (
                sgr_bg(t.interior),
                line.glyph_color.map(sgr_fg).unwrap_or_default(),
            )
        };
        // `│` + pad-space + content + pad + `│`, content styled on `bg`.
        format!("{border}\u{2502}{RESET}{bg}{fg} {shown}{pad} {RESET}{border}\u{2502}{RESET}")
    }

    /// The picker's footer key-hint string, mode-aware: `Navigate` lists the
    /// action keys (with the live `i install: on/off` toggle state), `Filtering`
    /// the type-to-narrow hints, and the prompts a confirm/cancel pair.
    fn footer_hint(&self) -> String {
        match &self.mode {
            PickerMode::Filtering => {
                " type to filter \u{00b7} \u{23ce}/\u{2191}\u{2193} done \u{00b7} esc clear ".into()
            }
            PickerMode::Prompting { .. } | PickerMode::PromptingHost { .. } => {
                " \u{23ce} confirm \u{00b7} esc cancel ".into()
            }
            PickerMode::ConfirmKill { name, .. } => format!(" Kill session '{name}'?  y / n "),
            PickerMode::Navigate => {
                let ins = if self.install.provisions() {
                    "on"
                } else {
                    "off"
                };
                format!(
                    " \u{2191}/\u{2193} move \u{00b7} \u{23ce} connect \u{00b7} / filter \u{00b7} n new \u{00b7} i install: {ins} \u{00b7} x forget \u{00b7} k kill \u{00b7} esc "
                )
            }
        }
    }
}

const RESET: &str = "\x1b[0m";

fn sgr_fg(c: Rgb) -> String {
    format!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b)
}

fn sgr_bg(c: Rgb) -> String {
    format!("\x1b[48;2;{};{};{}m", c.r, c.g, c.b)
}

/// One interior content line, pre-resolved to (glyph, glyph color, text).
/// `divider` marks the full-width dashed row; `selected` the cursor bar.
struct Line {
    glyph: String,
    glyph_color: Option<Rgb>,
    text: String,
    dim: bool,
    selected: bool,
    divider: bool,
}

/// A top/bottom border row: `corner_l` + a centered label in `─` + `corner_r`.
/// An over-wide label is **truncated to the interior**, not dropped — a title or
/// footer must never silently vanish on a narrow box (an 80-col terminal is the
/// baseline). When the label exactly fills the interior it touches the corners
/// (no `─` padding); a `None` label is a full-width `─` run.
fn edge_row(
    corner_l: char,
    corner_r: char,
    inner_w: usize,
    label: Option<(&str, Rgb)>,
    border: Rgb,
) -> String {
    let mut mid = String::new();
    match label {
        Some((text, color)) => {
            let text = truncate_to_width(text, inner_w as u16);
            let pad = inner_w.saturating_sub(display_width(text) as usize);
            let left = pad / 2;
            let right = pad - left;
            mid.push_str(&"\u{2500}".repeat(left));
            mid.push_str(&sgr_fg(color));
            mid.push_str("\x1b[1m");
            mid.push_str(text);
            mid.push_str(RESET);
            mid.push_str(&sgr_fg(border));
            mid.push_str(&"\u{2500}".repeat(right));
        }
        None => mid.push_str(&"\u{2500}".repeat(inner_w)),
    }
    format!("{corner_l}{mid}{corner_r}")
}

const fn status_color(status: &RowStatus, t: &PickerTheme) -> Rgb {
    match status {
        RowStatus::Live => t.live,
        RowStatus::Empty => t.empty,
        RowStatus::Unreachable => t.unreachable,
        RowStatus::Pending => t.pending,
        RowStatus::VersionMismatch(_) => t.warn,
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
    use std::collections::HashMap;
    use std::mem::take;

    use plexy_glass_config::PaletteConfig;
    use plexy_glass_status::Rgb;

    use super::*;

    /// Split the themed render into its visible box rows. **The renderer positions
    /// each row with `\x1b[{r};{c}H` and emits NO `\r`/`\n`** (Step 3), so ROWS ARE
    /// DELIMITED BY THE CURSOR-POSITION ESCAPES, not newlines — do not split on
    /// `\r`/`\n`. This consumes each CSI escape up to its terminating letter: a
    /// `…H` (cursor position) starts a new row; every other escape (SGR `…m`, erase
    /// `…J`, hide `…l`) is dropped. The result is the plain visible text per row.
    fn box_lines(bytes: &[u8]) -> Vec<String> {
        let s = String::from_utf8(bytes.to_vec()).expect("render output is valid UTF-8");
        let mut lines = Vec::new();
        let mut cur = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                let mut ended_h = false;
                for e in chars.by_ref() {
                    if e.is_ascii_alphabetic() {
                        ended_h = e == 'H';
                        break;
                    }
                }
                if ended_h && !cur.is_empty() {
                    lines.push(take(&mut cur));
                }
            } else if c != '\r' && c != '\n' {
                cur.push(c);
            }
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        lines
    }

    /// The visible box rows joined for `contains(...)` content checks.
    fn visible_text(bytes: &[u8]) -> String {
        box_lines(bytes).join("\n")
    }

    /// Does any box row, once its `│` borders are trimmed, consist solely of `─`?
    /// That's the configured/ad-hoc divider interior row. The top/bottom border
    /// rows start with `┌`/`└` (not `│`) and contain corners/title text, so they
    /// don't match.
    fn has_divider_row(bytes: &[u8]) -> bool {
        box_lines(bytes).iter().any(|l| {
            let t = l.trim_matches('\u{2502}').trim();
            !t.is_empty() && t.chars().all(|c| c == '\u{2500}')
        })
    }

    #[test]
    fn theme_resolves_roles_and_falls_back() {
        // Empty palette → every role is the fixed default.
        let d = PickerTheme::resolve(&PaletteConfig::default());
        assert_eq!(d, PickerTheme::default());
        assert_eq!(
            d.border,
            Rgb {
                r: 0x73,
                g: 0x7c,
                b: 0x73
            }
        ); // accent default

        // A palette that overrides `accent` moves only the border.
        let mut e = HashMap::new();
        e.insert("accent".to_string(), Rgb { r: 1, g: 2, b: 3 });
        let t = PickerTheme::resolve(&PaletteConfig { entries: e });
        assert_eq!(t.border, Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(t.title, d.title, "unset roles keep their default");
    }

    /// A remote daemon by ssh target.
    fn remote(name: &str) -> Host {
        Host::Remote(RemoteName::from(name))
    }

    fn session(name: &str, host: Option<&str>) -> PickerRow {
        PickerRow {
            name: name.into(),
            label: format!("{name} \u{2014} 1 win"),
            host: host.map_or(Host::Local, remote),
            kind: RowKind::Session,
            status: RowStatus::Live,
        }
    }

    fn host_row(host: &str, status: RowStatus) -> PickerRow {
        PickerRow {
            name: host.into(),
            label: host.into(),
            host: remote(host),
            kind: RowKind::Host,
            status,
        }
    }

    /// The current LOCAL daemon's anchor row (Task 5): a `Host` row on `Local`.
    fn host_row_local() -> PickerRow {
        PickerRow {
            name: "local".into(),
            label: "local".into(),
            host: Host::Local,
            kind: RowKind::Host,
            status: RowStatus::Live,
        }
    }

    /// A label == name session row (no ` — 1 win` noise) for clean filter tests.
    fn plain(name: &str) -> PickerRow {
        PickerRow {
            name: name.into(),
            label: name.into(),
            host: Host::Local,
            kind: RowKind::Session,
            status: RowStatus::Live,
        }
    }

    /// The rows `roster_state` builds on.
    fn roster_rows_for_test() -> Vec<PickerRow> {
        vec![
            session("main", None),
            session("build", None),
            host_row("prod", RowStatus::Live),
            session("api", Some("prod")),
            host_row("scratch", RowStatus::Unreachable),
        ]
    }

    // Two local sessions, a configured host + its session, and an ad-hoc host.
    // The `＋ Connect to a host…` slot is synthesized by the picker, not a row, so
    // it is present on EVERY `PickerState` (navigate to `visible().len()` to reach
    // it) — there is no separate "with sentinel" fixture anymore.
    fn roster_state() -> PickerState {
        let mut s = PickerState::new(roster_rows_for_test());
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
        s.handle_key(b'/'); // enter Filtering — typing is filter input now
        s.handle_key(b'b'); // filter "b" → only "build"
        assert_eq!(s.visible().len(), 1);
        assert_eq!(
            s.handle_key(b'\r'),
            None,
            "Enter ends filter mode, keeps filter"
        );
        assert_eq!(s.mode, PickerMode::Navigate);
        assert_eq!(s.visible().len(), 1, "filter is still applied");
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Switch("build".into()))
        );
    }

    #[test]
    fn render_draws_a_bordered_box_with_title_and_footer() {
        let s = roster_state();
        let out = s.render();
        // Cursor hidden every frame.
        assert!(
            out.windows(6).any(|w| w == b"\x1b[?25l"),
            "hides the cursor"
        );
        let text = visible_text(&out);
        assert!(text.contains("plexy-glass"), "title present");
        assert!(text.contains('\u{2502}'), "box has a vertical border │");
        assert!(text.contains('\u{250c}'), "box has a top-left corner ┌");
        // Footer key hints, including the `i install:` toggle state.
        assert!(text.contains("connect"), "footer hint");
        assert!(text.contains("install: off"), "install toggle in footer");
        // No box row exceeds the terminal width. `display_width` returns u16.
        for line in box_lines(&out) {
            assert!(
                display_width(&line) as usize <= s.size.cols as usize,
                "line {line:?} within cols"
            );
        }
    }

    #[test]
    fn render_contains_the_title_and_every_visible_row() {
        let s = PickerState::new(vec![session("main", None), session("build", None)]);
        let text = visible_text(&s.render());
        assert!(text.contains("plexy-glass"), "title");
        assert!(text.contains("main \u{2014} 1 win"));
        assert!(text.contains("build \u{2014} 1 win"));
    }

    // --- (a) synthesized section headers + divider ---

    #[test]
    fn render_synthesizes_headers_and_divider() {
        let s = roster_state();
        let text = visible_text(&s.render());
        assert!(text.contains("local"), "local section header");
        assert!(text.contains("prod"), "configured host row");
        assert!(text.contains("(configured)"), "configured tag");
        assert!(text.contains("scratch"), "ad-hoc host row");
        assert!(text.contains("(ad-hoc)"), "ad-hoc tag");
        assert!(
            has_divider_row(&s.render()),
            "divider row between configured and ad-hoc"
        );
    }

    #[test]
    fn headers_only_for_sections_with_visible_rows() {
        // Filter to a local session only: the `local` header still renders
        // (synthesized from the visible child, not filtered — "main" is not the
        // word "local"), but nothing configured/ad-hoc and no divider.
        let mut s = roster_state();
        s.handle_key(b'/'); // enter Filtering before typing the query
        for c in "main".chars() {
            s.handle_key(c as u8);
        }
        let text = visible_text(&s.render());
        assert!(
            text.contains("local"),
            "local header survives a child-only filter"
        );
        assert!(!text.contains("(configured)"), "prod filtered out");
        assert!(!text.contains("(ad-hoc)"), "scratch filtered out");
        assert!(
            !has_divider_row(&s.render()),
            "no divider without an ad-hoc block"
        );
    }

    // --- (b) bounded cursor over the selectable set; empty view parks + no-ops ---

    #[test]
    fn cursor_visits_only_selectable_rows() {
        let mut s = roster_state();
        let n = s.visible().len();
        assert_eq!(n, 5, "session + host rows are all selectable");
        // n real rows + the synthesized ＋ slot = n+1 positions. At each, the
        // cursor lands on EXACTLY ONE of a real row (`selected`) or the ＋ slot
        // (`is_new_host_selected`) — never a synthesized header.
        for _ in 0..(n + 2) {
            assert_ne!(
                s.selected().is_some(),
                s.is_new_host_selected(),
                "cursor on exactly one of a real row or the ＋ slot"
            );
            s.handle_key(0x0e); // down; wraps over n+1, stays bounded
        }
    }

    #[test]
    fn filter_matching_nothing_rests_on_the_connect_slot() {
        let mut s = roster_state();
        s.handle_key(b'/'); // enter Filtering
        for c in "zzzz".chars() {
            s.handle_key(c as u8);
        }
        assert!(s.visible().is_empty(), "no real row matches");
        // With no real rows, the always-present ＋ slot is the only position.
        assert_eq!(s.selected(), None, "no real row under the cursor");
        assert!(s.is_new_host_selected(), "cursor rests on the ＋ slot");
        // Enter ends filter mode (empty view kept); back in Navigate, a move is a
        // bounded no-op (only the ＋ slot exists) and Enter opens the host prompt.
        s.handle_key(b'\r');
        assert_eq!(s.mode, PickerMode::Navigate);
        assert_eq!(
            s.handle_key(0x0e),
            None,
            "move is a bounded no-op (only the ＋ slot)"
        );
        assert!(
            s.is_new_host_selected(),
            "still on the ＋ slot after a move"
        );
        assert_eq!(
            s.handle_key(b'\r'),
            None,
            "Enter opens the host prompt (no outcome yet)"
        );
        assert_eq!(
            s.mode,
            PickerMode::PromptingHost { buf: String::new() },
            "Enter on the ＋ slot opens the host prompt"
        );
    }

    // --- (c) inside `Filtering`, `n`/`x` are ordinary filter input ---

    #[test]
    fn filtering_with_n_narrows_to_nginx() {
        let mut s = PickerState::new(vec![plain("nginx"), plain("build"), plain("prod-x")]);
        s.handle_key(b'/'); // enter Filtering; now n is filter input, not an action
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
        s.handle_key(b'/'); // enter Filtering; now x is filter input, not Forget
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
            Some(PickerOutcome::New { host, name, .. }) => {
                assert_eq!(host, remote("prod"));
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
            Some(PickerOutcome::New { host, name, .. }) => {
                assert_eq!(host, Host::Local, "the local anchor's host is Local");
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
        assert_eq!(
            s.selected().map(|r| r.name.clone()),
            Some("scratch".to_string())
        );
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
        assert_eq!(s.handle_key(b'x'), None, "x on the local anchor is a no-op");
        assert_eq!(s.filter(), "", "the no-op does not leak into the filter");
    }

    // --- Task 7: k / ConfirmKill (kill a session from the picker) ---

    /// `n` local session rows, cursor parked on the first (`names[0]`).
    fn picker_with_sessions(names: &[&str]) -> PickerState {
        PickerState::new(names.iter().map(|n| session(n, None)).collect())
    }

    /// Cursor parked on the local anchor (a `Host` row), not a session.
    fn picker_on_host_row() -> PickerState {
        PickerState::new(vec![host_row_local(), session("main", None)])
    }

    #[test]
    fn k_on_a_session_row_enters_confirm_kill() {
        let mut p = picker_with_sessions(&["work", "play"]);
        assert!(p.handle_key(b'k').is_none());
        assert!(matches!(&p.mode, PickerMode::ConfirmKill { name, .. } if name == "work"));
    }

    #[test]
    fn confirm_kill_y_emits_kill_and_n_aborts() {
        let mut p = picker_with_sessions(&["work", "play"]);
        p.handle_key(b'k');
        // n aborts back to Navigate, no outcome.
        assert!(p.handle_key(b'n').is_none());
        assert!(matches!(p.mode, PickerMode::Navigate));
        // k then y emits Kill for the row captured at k-press time.
        p.handle_key(b'k');
        assert!(matches!(
            p.handle_key(b'y'),
            Some(PickerOutcome::Kill { name, .. }) if name == "work"
        ));
        assert!(
            matches!(p.mode, PickerMode::Navigate),
            "y returns to Navigate"
        );
    }

    #[test]
    fn confirm_kill_esc_also_aborts() {
        let mut p = picker_with_sessions(&["work"]);
        p.handle_key(b'k');
        assert_eq!(p.handle_key(0x1b), None, "Esc aborts like n");
        assert!(matches!(p.mode, PickerMode::Navigate));
    }

    #[test]
    fn k_is_a_no_op_on_a_host_row() {
        let mut p = picker_on_host_row();
        assert!(p.handle_key(b'k').is_none());
        assert!(matches!(p.mode, PickerMode::Navigate));
    }

    #[test]
    fn k_is_a_no_op_on_the_connect_slot() {
        // An empty roster: the only position is the synthesized `＋` slot
        // (`selected()` is `None` there) — `k` has no session row to act on.
        let mut p = PickerState::new(vec![]);
        assert!(p.is_new_host_selected());
        assert_eq!(p.handle_key(b'k'), None);
        assert!(matches!(p.mode, PickerMode::Navigate));
    }

    #[test]
    fn k_carries_the_rows_host_for_a_remote_session() {
        let mut p = PickerState::new(vec![session("api", Some("prod"))]);
        p.handle_key(b'k');
        assert_eq!(
            p.mode,
            PickerMode::ConfirmKill {
                host: remote("prod"),
                name: "api".into(),
            }
        );
        assert_eq!(
            p.handle_key(b'y'),
            Some(PickerOutcome::Kill {
                host: remote("prod"),
                name: "api".into(),
            })
        );
    }

    #[test]
    fn confirm_kill_footer_names_the_session() {
        let mut p = picker_with_sessions(&["work"]);
        p.handle_key(b'k');
        let text = visible_text(&p.render());
        assert!(text.contains("Kill session 'work'?"), "got: {text}");
        assert!(text.contains("y / n"), "got: {text}");
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
        let s = PickerState::new_with_current(rows, &Some(Host::Local), "build");
        assert_eq!(s.selected().map(|r| r.name.clone()), Some("build".into()));
    }

    #[test]
    fn new_with_current_falls_back_to_row_zero_when_session_absent() {
        let rows = vec![host_row_local(), session("main", None)];
        let s = PickerState::new_with_current(rows, &Some(Host::Local), "ghost");
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
            &remote("prod"),
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
        assert_eq!(rows[prod + 1].host, remote("prod"));
    }

    #[test]
    fn resolve_host_unreachable_sets_status_only() {
        let mut s = PickerState::new(vec![host_row("prod", RowStatus::Pending)]);
        s.resolve_host(&remote("prod"), RowStatus::Unreachable, vec![]);
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
        s.resolve_host(&remote("prod"), RowStatus::Live, vec![live("a"), live("b")]);
        // A second Live result replaces (does not duplicate) the child rows.
        s.resolve_host(&remote("prod"), RowStatus::Live, vec![live("c")]);
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
            &Some(Host::Local),
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
        assert_eq!(
            s.selected().map(|r| r.name.clone()),
            Some("prod".to_string())
        );
        s.replace_other_rows(&Some(Host::Local), vec![], vec![]);
        assert_eq!(
            s.visible().len(),
            1,
            "prod's row was dropped, none replaced it"
        );
        // The cursor must stay within the `visible().len() + 1` positions, not
        // dangle past the end: after prod's row vanishes it lands on the still
        // in-bounds ＋ slot (index 1 == the new `visible().len()`). Exactly one of
        // a real row or the ＋ slot holds — never neither.
        assert_ne!(
            s.selected().is_some(),
            s.is_new_host_selected(),
            "cursor re-clamped to a valid position, not dangling"
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
                host: remote("prod"),
                name: "api".into(),
                install: InstallPolicy::UseExisting,
            })
        );
    }

    // --- (h) host-aware accept: current-daemon anchor no-ops, other daemons
    // reconnect, same-daemon sessions fast-switch (Findings 1 + 2) ---

    /// Critical #1: Enter on the CURRENT daemon's own host anchor
    /// (`host == current_host`) must be a no-op `Cancel`, not a
    /// `Reconnect{name:""}` that the daemon rejects and ejects the client on.
    #[test]
    fn enter_on_current_daemon_anchor_cancels_not_reconnect() {
        let mut s = PickerState::new_with_current(
            vec![host_row_local(), session("main", None)],
            &Some(Host::Local),
            "main",
        );
        s.handle_key(0x10); // up → the local anchor (row host Local == attached Local)
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Host));
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Cancel),
            "Enter on the current daemon's anchor closes the picker; it does NOT reconnect with an empty name"
        );
    }

    /// Attached to NOTHING (`current_host: None`), every row belongs to some
    /// OTHER daemon — including the local ones. This is the property the
    /// standalone picker rests on, and it is only expressible because `Host`
    /// names `Local`: while the local daemon was encoded as `None`, this state
    /// was indistinguishable from "attached to local", so Enter on the local
    /// anchor answered `Cancel` (which the caller cannot tell from Esc, i.e.
    /// quit to the shell) and local session rows answered `Switch` (which needs
    /// the attached daemon a standalone picker does not have).
    #[test]
    fn detached_picker_reconnects_to_local_rows_instead_of_switching_or_cancelling() {
        let mut s = PickerState::new_with_current(
            vec![host_row_local(), session("main", None)],
            &None, // attached to nothing
            "main",
        );

        // The local session row: Reconnect, NOT Switch.
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Host));
        s.handle_key(0x0e); // down → the local session row
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Reconnect {
                host: Host::Local,
                name: "main".into(),
                install: InstallPolicy::UseExisting,
            }),
            "detached, a local session row must reconnect: there is no attached daemon to Switch within"
        );

        // The local host anchor: Reconnect, NOT Cancel.
        let mut s = PickerState::new_with_current(
            vec![host_row_local(), session("main", None)],
            &None,
            "main",
        );
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Reconnect {
                host: Host::Local,
                name: String::new(),
                install: InstallPolicy::UseExisting,
            }),
            "detached, Enter on the local anchor must connect to it, not quit to the shell"
        );
    }

    /// Enter on a DIFFERENT daemon's host anchor reconnects to that daemon's
    /// default session — `accept` emits an empty `name` (the pump normalizes it
    /// to the daemon default before the wire; see the pump test).
    #[test]
    fn enter_on_other_host_anchor_reconnects_with_empty_name() {
        let mut s = PickerState::new_with_current(
            vec![host_row_local(), host_row("prod", RowStatus::Pending)],
            &Some(Host::Local),
            "main",
        );
        s.handle_key(0x0e); // down → prod anchor (host Some("prod") != current None)
        assert_eq!(
            s.selected().map(|r| r.name.clone()),
            Some("prod".to_string())
        );
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Reconnect {
                host: remote("prod"),
                name: String::new(),
                install: InstallPolicy::UseExisting,
            })
        );
    }

    /// Finding 2: attached to remote `h`, the current daemon's own sessions are
    /// tagged `Some("h")`. Picking one must fast-`Switch` (same transport), and
    /// picking a session on a DIFFERENT daemon must `Reconnect`.
    #[test]
    fn attached_remote_same_daemon_switches_other_reconnects() {
        let host = Some(remote("h"));
        let mut s = PickerState::new_with_current(
            vec![
                host_row("h", RowStatus::Live), // current daemon's anchor (host Some("h"))
                session("a", Some("h")),        // a session ON the current daemon
                host_row("other", RowStatus::Pending),
                session("b", Some("other")), // a session on a DIFFERENT daemon
            ],
            &host,
            "a",
        );
        // Cursor parks on "a" (the current session). Same daemon → Switch, not
        // a full reconnect.
        assert_eq!(s.selected().map(|r| r.name.clone()), Some("a".into()));
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Switch("a".into())),
            "a session on the daemon we're attached to fast-switches, no reconnect"
        );
        s.handle_key(0x0e); // → other anchor
        s.handle_key(0x0e); // → b
        assert_eq!(s.selected().map(|r| r.name.clone()), Some("b".into()));
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Reconnect {
                host: remote("other"),
                name: "b".into(),
                install: InstallPolicy::UseExisting,
            }),
            "a session on a different daemon reconnects"
        );
    }

    #[test]
    fn i_toggles_install_unconditionally() {
        // `i` toggles install regardless of the row under the cursor or the
        // filter — no gate. Prove it flips on and back off.
        let mut s = PickerState::new(vec![host_row("prod", RowStatus::Live)]);
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Host));
        assert!(!s.install_enabled());
        assert_eq!(s.handle_key(b'i'), None);
        assert!(s.install_enabled(), "i toggles install on");
        assert_eq!(s.filter(), "", "i is an action, not filter input");
        assert_eq!(s.handle_key(b'i'), None);
        assert!(!s.install_enabled(), "i toggles back off");
    }

    #[test]
    fn i_on_a_session_row_toggles_and_does_not_filter() {
        // roster_state parks the cursor on `main`, a Session row. `i` is an
        // action in Navigate now, not filter input.
        let mut s = roster_state();
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Session));
        assert_eq!(s.handle_key(b'i'), None);
        assert!(s.install_enabled(), "i toggles even on a session row");
        assert_eq!(s.filter(), "", "i did not begin a filter");
    }

    #[test]
    fn reconnect_carries_the_install_toggle() {
        let mut s = PickerState::new_with_current(
            vec![host_row("prod", RowStatus::Pending)],
            &Some(Host::Local),
            "main",
        );
        assert_eq!(
            s.selected().map(|r| r.name.clone()),
            Some("prod".to_string())
        );
        s.handle_key(b'i'); // install on
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Reconnect {
                host: remote("prod"),
                name: String::new(),
                install: InstallPolicy::Provision,
            })
        );
    }

    /// Critical #1 (source b): `n` then an immediate Enter with an empty buffer
    /// must NOT commit `New{name:""}` — it stays in `Prompting` so no empty name
    /// can reach the wire. Typing a real name afterward still commits.
    #[test]
    fn n_then_empty_enter_refuses_new_and_stays_prompting() {
        let mut s = PickerState::new(vec![host_row_local(), session("main", None)]);
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Host));
        assert_eq!(s.handle_key(b'n'), None, "n opens the prompt");
        assert_eq!(
            s.handle_key(b'\r'),
            None,
            "Enter on an empty name is refused: no New, no eject"
        );
        // Still Prompting — typing a name and committing proves it.
        for c in "fresh".chars() {
            assert_eq!(s.handle_key(c as u8), None);
        }
        match s.handle_key(b'\r') {
            Some(PickerOutcome::New { host, name, .. }) => {
                assert_eq!(host, Host::Local);
                assert_eq!(name, "fresh");
            }
            other => panic!("expected New after typing a name, got {other:?}"),
        }
    }

    // --- (i) the synthesized `＋ Connect to a host…` slot ---

    #[test]
    fn connect_slot_is_selectable_and_survives_filter() {
        let mut s = roster_state();
        // It's the last selectable position, past every real row.
        let n = s.visible().len();
        for _ in 0..n {
            s.handle_key(0x0e);
        }
        assert!(s.is_new_host_selected(), "cursor reaches the ＋ slot last");
        assert_eq!(s.selected(), None, "the ＋ slot is not a row");
        // A filter matching no host still leaves the ＋ slot reachable.
        let mut s2 = roster_state();
        s2.handle_key(b'/'); // enter Filtering before typing the query
        for c in "zzzznomatch".chars() {
            s2.handle_key(c as u8);
        }
        assert!(s2.visible().is_empty(), "no real row survives the filter");
        assert!(s2.is_new_host_selected(), "the ＋ slot is still reachable");
    }

    #[test]
    fn enter_on_connect_slot_prompts_then_connects_with_install() {
        let mut s = roster_state();
        let n = s.visible().len();
        for _ in 0..n {
            s.handle_key(0x0e);
        }
        assert!(s.is_new_host_selected());
        s.handle_key(b'i'); // install on (i toggles unconditionally in Navigate)
        assert!(s.install_enabled());
        assert_eq!(s.handle_key(b'\r'), None, "Enter opens the host prompt");
        for c in "wsl2".chars() {
            assert_eq!(s.handle_key(c as u8), None);
        }
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Reconnect {
                host: remote("wsl2"),
                name: String::new(),
                install: InstallPolicy::Provision,
            })
        );
    }

    #[test]
    fn empty_host_prompt_is_refused() {
        let mut s = roster_state();
        let n = s.visible().len();
        for _ in 0..n {
            s.handle_key(0x0e);
        }
        assert!(s.is_new_host_selected());
        s.handle_key(b'\r'); // into PromptingHost
        assert_eq!(
            s.handle_key(b'\r'),
            None,
            "empty host refused, stays prompting"
        );
        for c in "box".chars() {
            s.handle_key(c as u8);
        }
        assert!(matches!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Reconnect { .. })
        ));
    }

    #[test]
    fn render_draws_the_synthesized_connect_slot_last() {
        // The ＋ slot is synthesized at render time (not a row), always present,
        // and drawn after every real row — even under a filter that matches
        // nothing (the "this host isn't listed, add it" flow).
        let s = roster_state();
        assert!(
            visible_text(&s.render()).contains("Connect to a host"),
            "＋ slot rendered"
        );
        let mut narrowed = roster_state();
        narrowed.handle_key(b'/');
        for c in "zzznomatch".chars() {
            narrowed.handle_key(c as u8);
        }
        assert!(
            visible_text(&narrowed.render()).contains("Connect to a host"),
            "＋ slot survives a non-matching filter"
        );
    }

    // --- (j) explicit-filter input model: Navigate is action-first, `/` enters
    // an explicit Filtering mode ---

    #[test]
    fn slash_enters_filtering_and_typing_narrows() {
        let mut s = PickerState::new(vec![plain("main"), plain("build")]);
        assert_eq!(s.mode, PickerMode::Navigate);
        assert_eq!(s.handle_key(b'/'), None);
        assert_eq!(s.mode, PickerMode::Filtering, "/ enters Filtering");
        s.handle_key(b'b');
        assert_eq!(s.filter(), "b");
        let names: Vec<_> = s.visible().iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec!["build".to_string()], "visible() narrowed");
    }

    #[test]
    fn filtering_enter_returns_to_navigate_keeping_filter() {
        let mut s = PickerState::new(vec![plain("main"), plain("build")]);
        s.handle_key(b'/');
        s.handle_key(b'b');
        assert_eq!(s.handle_key(b'\r'), None);
        assert_eq!(s.mode, PickerMode::Navigate);
        assert_eq!(s.filter(), "b", "filter kept after leaving Filtering");
        assert_eq!(s.visible().len(), 1, "list stays narrowed");
    }

    #[test]
    fn filtering_arrow_exits_to_navigate_and_moves() {
        let mut s = PickerState::new(vec![plain("alpha"), plain("alberta"), plain("beta")]);
        s.handle_key(b'/');
        for c in "al".chars() {
            s.handle_key(c as u8);
        }
        assert_eq!(s.visible().len(), 2, "two rows match 'al'");
        assert_eq!(s.selected().map(|r| r.name.clone()), Some("alpha".into()));
        assert_eq!(s.handle_key(0x0e), None); // arrow down
        assert_eq!(s.mode, PickerMode::Navigate, "an arrow ends filter mode");
        assert_eq!(
            s.selected().map(|r| r.name.clone()),
            Some("alberta".into()),
            "and the cursor moved"
        );
    }

    #[test]
    fn filtering_esc_clears_filter_and_returns_to_navigate() {
        let mut s = PickerState::new(vec![plain("main"), plain("build")]);
        s.handle_key(b'/');
        s.handle_key(b'b');
        assert_eq!(s.visible().len(), 1);
        assert_eq!(s.handle_key(0x1b), None);
        assert_eq!(s.mode, PickerMode::Navigate);
        assert_eq!(s.filter(), "", "Esc cleared the filter");
        assert_eq!(s.visible().len(), 2, "list is full again");
    }

    #[test]
    fn i_toggles_on_host_session_and_connect_slot() {
        // On a host row.
        let mut s = PickerState::new(vec![host_row("prod", RowStatus::Live)]);
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Host));
        s.handle_key(b'i');
        assert!(s.install_enabled(), "i toggles on a host row");
        assert_eq!(s.filter(), "");
        // On a session row (roster_state parks on `main`).
        let mut s = roster_state();
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Session));
        s.handle_key(b'i');
        assert!(s.install_enabled(), "i toggles on a session row");
        assert_eq!(s.filter(), "");
        // On the synthesized `＋` slot.
        let mut s = roster_state();
        let n = s.visible().len();
        for _ in 0..n {
            s.handle_key(0x0e);
        }
        assert!(s.is_new_host_selected());
        s.handle_key(b'i');
        assert!(s.install_enabled(), "i toggles on the ＋ slot");
        assert_eq!(s.filter(), "");
    }

    #[test]
    fn bare_unbound_letter_is_a_no_op() {
        let mut s = roster_state();
        let before = s.selected().map(|r| r.name.clone());
        assert_eq!(
            s.handle_key(b'z'),
            None,
            "an unbound letter produces no outcome"
        );
        assert_eq!(s.filter(), "", "and does not filter");
        assert_eq!(s.mode, PickerMode::Navigate);
        assert_eq!(
            s.selected().map(|r| r.name.clone()),
            before,
            "the cursor did not move"
        );
        assert!(!s.install_enabled());
    }

    #[test]
    fn n_is_action_on_host_and_no_op_elsewhere() {
        // Session row: no-op.
        let mut s = roster_state();
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Session));
        assert_eq!(s.handle_key(b'n'), None);
        assert_eq!(
            s.mode,
            PickerMode::Navigate,
            "n on a session row is a no-op"
        );
        assert_eq!(s.filter(), "");
        // Host row: opens the new-session prompt.
        s.handle_key(0x0e); // build
        s.handle_key(0x0e); // prod (host)
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Host));
        assert_eq!(s.handle_key(b'n'), None);
        assert!(
            matches!(s.mode, PickerMode::Prompting { .. }),
            "n on a host row opens the prompt"
        );
        // The `＋` slot: no-op.
        let mut s = roster_state();
        let n = s.visible().len();
        for _ in 0..n {
            s.handle_key(0x0e);
        }
        assert!(s.is_new_host_selected());
        assert_eq!(s.handle_key(b'n'), None);
        assert_eq!(s.mode, PickerMode::Navigate, "n on the ＋ slot is a no-op");
    }

    #[test]
    fn x_is_a_no_op_on_session_rows_and_the_connect_slot() {
        // Session row.
        let mut s = roster_state();
        assert_eq!(s.selected().map(|r| r.kind), Some(RowKind::Session));
        assert_eq!(s.handle_key(b'x'), None);
        assert_eq!(s.filter(), "");
        // The `＋` slot.
        let mut s = roster_state();
        let n = s.visible().len();
        for _ in 0..n {
            s.handle_key(0x0e);
        }
        assert!(s.is_new_host_selected());
        assert_eq!(s.handle_key(b'x'), None);
        assert_eq!(s.filter(), "");
    }

    // --- (k) cross-feature combinations: the sequences the old per-branch tests
    // never drove ---

    #[test]
    fn filter_then_enter_then_i_toggles_with_filter_intact() {
        let mut s = PickerState::new(vec![plain("alpha"), plain("beta")]);
        s.handle_key(b'/');
        for c in "beta".chars() {
            s.handle_key(c as u8);
        }
        s.handle_key(b'\r'); // end filter mode, filter kept
        assert_eq!(s.mode, PickerMode::Navigate);
        assert_eq!(s.visible().len(), 1);
        s.handle_key(b'i');
        assert!(s.install_enabled(), "i toggles after leaving Filtering");
        assert_eq!(s.filter(), "beta", "the filter is intact");
        assert_eq!(s.visible().len(), 1, "still narrowed");
    }

    #[test]
    fn filter_then_arrow_then_x_forgets() {
        let mut s = PickerState::new(vec![
            host_row("aardvark", RowStatus::Live),
            host_row("aarhus", RowStatus::Live),
        ]);
        s.set_adhoc_hosts(vec!["aardvark".into(), "aarhus".into()]);
        s.handle_key(b'/');
        for c in "aar".chars() {
            s.handle_key(c as u8);
        }
        assert_eq!(s.visible().len(), 2, "both ad-hoc hosts match");
        s.handle_key(0x0e); // arrow: ends Filtering AND moves to the second row
        assert_eq!(s.mode, PickerMode::Navigate);
        assert_eq!(s.selected().map(|r| r.name.clone()), Some("aarhus".into()));
        assert_eq!(
            s.handle_key(b'x'),
            Some(PickerOutcome::Forget {
                host: "aarhus".into()
            }),
            "x is an action in Navigate, forgetting the ad-hoc host under the cursor"
        );
    }

    #[test]
    fn install_on_then_filter_then_connect_carries_install() {
        let mut s = PickerState::new_with_current(
            vec![
                host_row("prod", RowStatus::Live),
                session("api", Some("prod")),
            ],
            &Some(Host::Local),
            "main",
        );
        s.handle_key(b'i'); // install on, in Navigate
        assert!(s.install_enabled());
        s.handle_key(b'/'); // Filtering
        for c in "api".chars() {
            s.handle_key(c as u8);
        }
        s.handle_key(b'\r'); // end filter mode, filter kept
        assert_eq!(s.mode, PickerMode::Navigate);
        assert_eq!(s.selected().map(|r| r.name.clone()), Some("api".into()));
        assert_eq!(
            s.handle_key(b'\r'),
            Some(PickerOutcome::Reconnect {
                host: remote("prod"),
                name: "api".into(),
                install: InstallPolicy::Provision,
            }),
            "connect carries the install toggle set before filtering"
        );
    }

    #[test]
    fn filter_nonmatch_leaves_connect_slot_selectable() {
        let mut s = roster_state();
        s.handle_key(b'/');
        for c in "zzznomatch".chars() {
            s.handle_key(c as u8);
        }
        assert!(s.visible().is_empty(), "no real row survives the filter");
        assert!(s.is_new_host_selected(), "the ＋ slot is still selectable");
        s.handle_key(b'\r'); // end filter mode
        assert_eq!(s.mode, PickerMode::Navigate);
        assert!(s.is_new_host_selected());
        assert_eq!(
            s.handle_key(b'\r'),
            None,
            "Enter on ＋ opens the host prompt"
        );
        assert_eq!(
            s.mode,
            PickerMode::PromptingHost { buf: String::new() },
            "and it is the host prompt"
        );
    }
}
