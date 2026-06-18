//! On-disk session persistence. Each Session serializes to a per-session
//! JSON file at `$XDG_STATE_HOME/plexy-glass/sessions/<name>.json`.
//! Writes are atomic (tempfile + fsync + rename). Restore is on-demand
//! from `SessionRegistry::attach_or_create`.

use plexy_glass_emulator::{
    Attrs, Cell, Color, Row, RowMark, UnderlineStyle, WrapOrigin,
};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// Schema version. Bump on any non-additive on-disk format change.
///
/// v1 -> v2: added `PaneStateV1.name` (optional, `#[serde(default)]`). v1 files
/// still load (the missing field defaults to `None`) so loads accept either.
///
/// Additive optional fields added since v2 do not bump the version (older files
/// default them to `None` via `#[serde(default)]`): `WindowStateV1.home_cwd`,
/// `PaneStateV1.scrollback`.
pub const SCHEMA_VERSION: u32 = 2;

/// Oldest on-disk schema this build can still load (older files are rejected).
const MIN_SUPPORTED_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionStateV1 {
    pub schema: u32,
    pub name: String,
    pub created: chrono::DateTime<chrono::Utc>,
    pub active_window: usize,
    pub windows: Vec<WindowStateV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WindowStateV1 {
    pub name: String,
    pub sync_input: bool,
    /// Whether `name` is a derived placeholder (auto-rename). Absent in older
    /// files, where `#[serde(default)]` fills it as `false`, i.e. pinned,
    /// preserving today's behavior (every pre-feature window was effectively
    /// pinned: declared/renamed names, or the foundational "shell" name).
    #[serde(default)]
    pub auto_named: bool,
    /// The window's home base (per-window cwd). Absent in older files, where
    /// `#[serde(default)]` fills it as `None`.
    #[serde(default)]
    pub home_cwd: Option<String>,
    pub active_pane: u32,
    pub panes: Vec<PaneStateV1>,
    pub layout: LayoutStateV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PaneStateV1 {
    pub cwd: Option<String>,
    /// User-assigned pane name (schema v2+). Absent in v1 files, where
    /// `#[serde(default)]` fills it as `None`.
    #[serde(default)]
    pub name: Option<String>,
    /// Persisted scrollback (text + attributes + OSC 133 block marks), if any.
    /// Additive optional field, so old files restore blank (today's behavior).
    /// On restore the rows become the fresh pane's *scrollback history*; the
    /// new child draws into the live grid below them. See the 2026-06-12
    /// scrollback-persistence spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<PaneScrollbackV1>,
}

/// One pane's persisted scrollback: the last N rows of `scrollback ++ main
/// grid`, in display order, captured at width `cols`. Width is recorded only
/// for sanity / documentation; restore seeds rows as-is (no reflow) when the
/// spawn width differs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PaneScrollbackV1 {
    pub cols: u16,
    pub rows: Vec<RowV1>,
}

/// One persisted row. Trailing all-default cells are trimmed before serialize
/// and re-padded to `cols` on load.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RowV1 {
    pub cells: Vec<CellV1>,
    #[serde(default, skip_serializing_if = "WrapV1::is_default")]
    pub wrap: WrapV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<RowMarkV1>,
}

/// One persisted cell. Every field except `grapheme` is elided from the
/// serialized form when it holds its default value, so a plain text cell
/// serializes to just its grapheme, and that's the dominant size win for
/// mostly-plain scrollback. `hyperlink_id` is deliberately NOT mirrored: it
/// indexes the per-`Screen` `HyperlinkTable`, which is not persisted (a bare
/// index would dangle on restore). Consequence: restored scrollback keeps
/// text/styling but loses link clickability.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CellV1 {
    pub grapheme: String,
    #[serde(default, skip_serializing_if = "ColorV1::is_default")]
    pub fg: ColorV1,
    #[serde(default, skip_serializing_if = "ColorV1::is_default")]
    pub bg: ColorV1,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub attrs: u16,
    /// SGR 58/59 underline color, independent of `attrs`'s UNDERLINE bit.
    #[serde(default, skip_serializing_if = "ColorV1::is_default")]
    pub underline_color: ColorV1,
    /// SGR `4:0`..`4:5` underline shape, independent of the UNDERLINE bit and
    /// of `underline_color`. Persisted so the diff renderer re-emits
    /// `4:2`/`4:3`/`4:4`/`4:5` on restored rows instead of flattening to `4`.
    #[serde(default, skip_serializing_if = "UnderlineStyleV1::is_default")]
    pub underline_style: UnderlineStyleV1,
}

/// Persisted color (mirrors `emulator::color::Color`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum ColorV1 {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl ColorV1 {
    fn is_default(&self) -> bool {
        matches!(self, ColorV1::Default)
    }
}

/// Persisted underline shape (mirrors `emulator::attrs::UnderlineStyle`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum UnderlineStyleV1 {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl UnderlineStyleV1 {
    fn is_default(&self) -> bool {
        matches!(self, UnderlineStyleV1::None)
    }
}

/// Persisted soft-wrap continuation marker (mirrors `emulator::grid::WrapOrigin`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum WrapV1 {
    #[default]
    Hard,
    SoftFrom(u32),
}

impl WrapV1 {
    fn is_default(&self) -> bool {
        matches!(self, WrapV1::Hard)
    }
}

/// Persisted OSC 133 block annotation (mirrors the public surface of
/// `emulator::grid::RowMark`). `flags` carries only the public flag bits
/// (PROMPT_START / OUTPUT_START / BLOCK_END / PROMPT_END); the live mark's
/// private exit-presence bit is represented by `exit: Some(_)` here.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RowMarkV1 {
    pub flags: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_end_col: Option<u16>,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

// ── live ↔ DTO mappers ──────────────────────────────────────────────────
// The emulator's `Cell`/`Color`/`Row`/`RowMark`/`UnderlineStyle` stay
// serde-free; these explicit mappers convert at the persist boundary so the
// on-disk format can evolve independently of the hot emulator core.

impl From<Color> for ColorV1 {
    fn from(c: Color) -> Self {
        match c {
            Color::Default => ColorV1::Default,
            Color::Indexed(i) => ColorV1::Indexed(i),
            Color::Rgb(r, g, b) => ColorV1::Rgb(r, g, b),
        }
    }
}

impl From<ColorV1> for Color {
    fn from(c: ColorV1) -> Self {
        match c {
            ColorV1::Default => Color::Default,
            ColorV1::Indexed(i) => Color::Indexed(i),
            ColorV1::Rgb(r, g, b) => Color::Rgb(r, g, b),
        }
    }
}

impl From<UnderlineStyle> for UnderlineStyleV1 {
    fn from(s: UnderlineStyle) -> Self {
        match s {
            UnderlineStyle::None => UnderlineStyleV1::None,
            UnderlineStyle::Single => UnderlineStyleV1::Single,
            UnderlineStyle::Double => UnderlineStyleV1::Double,
            UnderlineStyle::Curly => UnderlineStyleV1::Curly,
            UnderlineStyle::Dotted => UnderlineStyleV1::Dotted,
            UnderlineStyle::Dashed => UnderlineStyleV1::Dashed,
        }
    }
}

impl From<UnderlineStyleV1> for UnderlineStyle {
    fn from(s: UnderlineStyleV1) -> Self {
        match s {
            UnderlineStyleV1::None => UnderlineStyle::None,
            UnderlineStyleV1::Single => UnderlineStyle::Single,
            UnderlineStyleV1::Double => UnderlineStyle::Double,
            UnderlineStyleV1::Curly => UnderlineStyle::Curly,
            UnderlineStyleV1::Dotted => UnderlineStyle::Dotted,
            UnderlineStyleV1::Dashed => UnderlineStyle::Dashed,
        }
    }
}

/// Convert a live cell to its DTO. `hyperlink_id` is dropped (see `CellV1`).
pub(crate) fn cell_to_dto(c: &Cell) -> CellV1 {
    CellV1 {
        grapheme: c.grapheme.as_str().to_string(),
        fg: c.fg.into(),
        bg: c.bg.into(),
        attrs: c.attrs.bits(),
        underline_color: c.underline_color.into(),
        underline_style: c.underline_style.into(),
    }
}

/// Reconstruct a live cell from its DTO. `hyperlink_id` is always `None`
/// (links are not persisted).
pub(crate) fn cell_from_dto(c: &CellV1) -> Cell {
    Cell {
        grapheme: c.grapheme.as_str().into(),
        fg: c.fg.into(),
        bg: c.bg.into(),
        underline_color: c.underline_color.into(),
        underline_style: c.underline_style.into(),
        // `from_bits_truncate` drops any bit the current build does not know, so a
        // forward-compat read of a newer file never produces a bogus `Attrs`.
        attrs: Attrs::from_bits_truncate(c.attrs),
        hyperlink_id: None,
    }
}

/// Convert a live `RowMark` to its DTO, or `None` when the row is unmarked.
/// Only the public flag bits are persisted; the live mark's private
/// exit-presence bit is captured by `exit`.
pub(crate) fn mark_to_dto(m: RowMark) -> Option<RowMarkV1> {
    if m.is_empty() {
        return None;
    }
    let mut flags = 0u8;
    for bit in [
        RowMark::PROMPT_START,
        RowMark::OUTPUT_START,
        RowMark::BLOCK_END,
        RowMark::PROMPT_END,
    ] {
        if m.contains(bit) {
            flags |= bit;
        }
    }
    Some(RowMarkV1 {
        flags,
        exit: m.exit(),
        prompt_end_col: m.prompt_end_col(),
    })
}

/// Reconstruct a live `RowMark` from its DTO.
pub(crate) fn mark_from_dto(m: &RowMarkV1) -> RowMark {
    let mut out = RowMark::default();
    for bit in [
        RowMark::PROMPT_START,
        RowMark::OUTPUT_START,
        RowMark::BLOCK_END,
        RowMark::PROMPT_END,
    ] {
        if m.flags & bit != 0 {
            out.set(bit);
        }
    }
    if let Some(col) = m.prompt_end_col {
        out.set_prompt_end(col);
    }
    // `set_exit(Some)` also sets the private `HAS_EXIT` bit; `set_exit(None)` is a
    // no-op against a fresh mark (keeps it absent).
    if let Some(code) = m.exit {
        out.set_exit(Some(code));
    }
    out
}

fn wrap_to_dto(w: WrapOrigin) -> WrapV1 {
    match w {
        WrapOrigin::Hard => WrapV1::Hard,
        WrapOrigin::SoftFrom(id) => WrapV1::SoftFrom(id),
    }
}

fn wrap_from_dto(w: WrapV1) -> WrapOrigin {
    match w {
        WrapV1::Hard => WrapOrigin::Hard,
        WrapV1::SoftFrom(id) => WrapOrigin::SoftFrom(id),
    }
}

/// Convert a live row to its DTO, trimming trailing all-default cells (re-padded
/// to `cols` on load). A wide grapheme's trailing `Cell::wide_spacer()` is an
/// empty-grapheme cell, which is NOT default (default is a single space), so
/// spacers are preserved by the trim.
pub(crate) fn row_to_dto(row: &Row) -> RowV1 {
    let default = Cell::default();
    let keep = row
        .cells
        .iter()
        .rposition(|c| *c != default)
        .map(|i| i + 1)
        .unwrap_or(0);
    let cells = row.cells[..keep].iter().map(cell_to_dto).collect();
    RowV1 {
        cells,
        wrap: wrap_to_dto(row.wrap_origin),
        mark: mark_to_dto(row.mark),
    }
}

/// Reconstruct a live row from its DTO, re-padding to `cols` with default cells
/// (the trim dropped the trailing defaults on save). Rows wider than `cols`
/// (a width-mismatch restore) keep their captured cells; the first resize
/// normalizes them.
pub(crate) fn row_from_dto(row: &RowV1, cols: u16) -> Row {
    let mut cells: Vec<Cell> = row.cells.iter().map(cell_from_dto).collect();
    if cells.len() < cols as usize {
        cells.resize(cols as usize, Cell::default());
    }
    Row {
        cells,
        wrap_origin: wrap_from_dto(row.wrap),
        mark: row.mark.as_ref().map(mark_from_dto).unwrap_or_default(),
    }
}

/// Default number of persisted rows per pane (scrollback tail + main grid, in
/// display order). Restore shows the most recent `SCROLLBACK_PERSIST_ROWS`.
pub const SCROLLBACK_PERSIST_ROWS: usize = 1000;

/// Capture a pane's scrollback for persistence: the last `n` rows of
/// `scrollback ++ main_grid.rows`, in display order, where the **main** grid is
/// `screen.alt.as_ref().unwrap_or(&screen.active)`. When a pane is on the alt
/// screen, `enter_alt_screen` parked the main grid in `screen.alt`, so the alt
/// (transient) grid in `screen.active` is never persisted. Trailing all-default
/// cells are trimmed per row (by `row_to_dto`) and blank trailing rows are
/// dropped. Returns `None` when there is nothing worth persisting (no scrollback
/// and a blank grid).
pub(crate) fn capture_scrollback(
    screen: &plexy_glass_emulator::Screen,
    n: usize,
) -> Option<PaneScrollbackV1> {
    let main_grid = screen.alt.as_ref().unwrap_or(&screen.active);
    let cols = main_grid.cols;
    // A live row that maps to a blank DTO row (no kept cells, no mark, Hard wrap),
    // the exact predicate `row_to_dto` would produce, so the count below matches.
    let default_cell = Cell::default();
    let is_blank = |row: &Row| {
        row.cells.iter().all(|c| *c == default_cell)
            && mark_to_dto(row.mark).is_none()
            && wrap_to_dto(row.wrap_origin) == WrapV1::Hard
    };
    // Unified display-order row sequence: scrollback first, then the main grid.
    // Drop blank trailing rows (the empty bottom of the live grid) BEFORE the N
    // cap, so blank live-grid padding doesn't eat the budget and silently push
    // the oldest real scrollback off the top. Count the blank tail without
    // allocating DTOs, then map only the kept window (skip keeps the cost ~n).
    let seq = || screen.scrollback.rows().iter().chain(main_grid.rows.iter());
    let total = screen.scrollback.len() + main_grid.rows.len();
    let blank_tail = seq().rev().take_while(|r| is_blank(r)).count();
    let meaningful = total - blank_tail;
    let skip = meaningful.saturating_sub(n);
    let rows: Vec<RowV1> = seq().take(meaningful).skip(skip).map(row_to_dto).collect();
    if rows.is_empty() {
        return None;
    }
    Some(PaneScrollbackV1 { cols, rows })
}

/// Reconstruct live preseed rows from a persisted scrollback. Each DTO row is
/// re-padded to `sb.cols` (its captured width); a width-mismatch restore (spawn
/// cols != saved cols) seeds the rows as-is, and the first resize normalizes
/// them.
pub(crate) fn scrollback_to_rows(sb: &PaneScrollbackV1) -> Vec<Row> {
    sb.rows.iter().map(|r| row_from_dto(r, sb.cols)).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum LayoutStateV1 {
    /// Pane index into the window's `panes` vec (DFS order).
    Leaf(u32),
    Split {
        dir: LayoutDirV1,
        ratio: f32,
        first: Box<LayoutStateV1>,
        second: Box<LayoutStateV1>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum LayoutDirV1 {
    Vertical,
    Horizontal,
}

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported schema version {0}")]
    Schema(u32),
}

/// Return the per-session directory. Honors `XDG_STATE_HOME`, falls back to
/// `$HOME/.local/state/plexy-glass/sessions`.
pub fn sessions_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("plexy-glass").join("sessions");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".local/state/plexy-glass/sessions")
}

fn session_path(name: &str) -> PathBuf {
    sessions_dir().join(format!("{name}.json"))
}

pub fn save_session(state: &SessionStateV1) -> Result<(), PersistError> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)?;
    let final_path = dir.join(format!("{}.json", state.name));
    let tmp_path = dir.join(format!("{}.json.tmp", state.name));
    // Compact (not pretty): the state file is machine-written and never
    // hand-edited, so pretty-printing is pure overhead, and with persisted
    // scrollback a styled pane is single-digit-to-tens of MB pretty vs a small
    // fraction of that compact. Combined with per-cell default-field elision
    // (see `CellV1`), most scrollback cells serialize to just their grapheme.
    let json = serde_json::to_vec(state)?;
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

pub fn load_session(name: &str) -> Result<Option<SessionStateV1>, PersistError> {
    let path = session_path(name);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(PersistError::Io(e)),
    };
    let state: SessionStateV1 = serde_json::from_slice(&bytes)?;
    // Accept any schema in the supported range (additive-only fields use
    // serde defaults, so older files still deserialize). Saves always write
    // the current SCHEMA_VERSION.
    if !(MIN_SUPPORTED_SCHEMA..=SCHEMA_VERSION).contains(&state.schema) {
        return Err(PersistError::Schema(state.schema));
    }
    Ok(Some(state))
}

pub fn delete_session(name: &str) -> Result<(), PersistError> {
    let path = session_path(name);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PersistError::Io(e)),
    }
}

/// Enumerate saved sessions on disk: (name, window count, total pane count).
/// Files that fail to parse / mismatch schema are skipped silently. Sorted by name.
pub fn list_saved() -> Vec<(String, u8, u8)> {
    let dir = sessions_dir();
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Ok(Some(state)) = load_session(stem) {
            let windows = state.windows.len().min(u8::MAX as usize) as u8;
            let panes: usize = state.windows.iter().map(|w| w.panes.len()).sum();
            out.push((state.name, windows, panes.min(u8::MAX as usize) as u8));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_env::isolate;

    fn sample_state(name: &str) -> SessionStateV1 {
        SessionStateV1 {
            schema: SCHEMA_VERSION,
            name: name.into(),
            created: chrono::Utc::now(),
            active_window: 0,
            windows: vec![WindowStateV1 {
                name: "shell".into(),
                auto_named: false,
                sync_input: false,
                home_cwd: None,
                active_pane: 0,
                panes: vec![PaneStateV1 { cwd: Some("/tmp".into()), name: None, scrollback: None }],
                layout: LayoutStateV1::Leaf(0),
            }],
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let _g = isolate();
        let s = sample_state("foo");
        save_session(&s).expect("save");
        let loaded = load_session("foo").expect("load").expect("present");
        assert_eq!(loaded.name, s.name);
        assert_eq!(loaded.windows.len(), 1);
        assert_eq!(loaded.windows[0].panes[0].cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn load_missing_returns_none() {
        let _g = isolate();
        assert!(load_session("nope").expect("ok").is_none());
    }

    #[test]
    fn load_bad_json_errors() {
        let _g = isolate();
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.json"), b"{not json").unwrap();
        let err = load_session("bad").expect_err("should fail");
        assert!(matches!(err, PersistError::Json(_)));
    }

    #[test]
    fn load_with_wrong_schema_errors() {
        let _g = isolate();
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("v9.json"),
            br#"{"schema":9,"name":"v9","created":"2026-05-24T12:00:00Z","active_window":0,"windows":[]}"#,
        )
        .unwrap();
        let err = load_session("v9").expect_err("schema mismatch");
        assert!(matches!(err, PersistError::Schema(9)));
    }

    #[test]
    fn delete_removes_file() {
        let _g = isolate();
        save_session(&sample_state("zap")).expect("save");
        delete_session("zap").expect("delete");
        assert!(load_session("zap").expect("load").is_none());
    }

    #[test]
    fn delete_missing_is_ok() {
        let _g = isolate();
        delete_session("never-saved").expect("delete");
    }

    #[test]
    fn loads_v1_file_without_name_field() {
        let _g = isolate();
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // A schema-1 file: the pane has `cwd` but no `name` key at all.
        std::fs::write(
            dir.join("old.json"),
            br#"{"schema":1,"name":"old","created":"2026-05-24T12:00:00Z","active_window":0,"windows":[{"name":"shell","sync_input":false,"active_pane":0,"panes":[{"cwd":"/tmp"}],"layout":{"Leaf":0}}]}"#,
        )
        .unwrap();
        let loaded = load_session("old").expect("load").expect("present");
        assert_eq!(loaded.windows[0].panes[0].name, None, "missing name defaults to None");
        assert_eq!(loaded.windows[0].panes[0].cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn pane_name_round_trips_in_v2() {
        let _g = isolate();
        let mut s = sample_state("named");
        s.windows[0].panes[0].name = Some("logs".into());
        save_session(&s).expect("save");
        let loaded = load_session("named").expect("load").expect("present");
        assert_eq!(loaded.schema, SCHEMA_VERSION);
        assert_eq!(loaded.schema, 2, "saves write the current schema");
        assert_eq!(loaded.windows[0].panes[0].name.as_deref(), Some("logs"));
    }

    #[test]
    fn auto_named_round_trips() {
        let _g = isolate();
        let mut s = sample_state("auto");
        s.windows[0].auto_named = true;
        save_session(&s).expect("save");
        let loaded = load_session("auto").expect("load").expect("present");
        assert!(loaded.windows[0].auto_named, "auto_named must survive save/load");
    }

    #[test]
    fn missing_auto_named_defaults_to_pinned() {
        let _g = isolate();
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // A file with no `auto_named` key on the window (older format).
        std::fs::write(
            dir.join("legacy.json"),
            br#"{"schema":2,"name":"legacy","created":"2026-05-24T12:00:00Z","active_window":0,"windows":[{"name":"shell","sync_input":false,"active_pane":0,"panes":[{"cwd":"/tmp"}],"layout":{"Leaf":0}}]}"#,
        )
        .unwrap();
        let loaded = load_session("legacy").expect("load").expect("present");
        assert!(!loaded.windows[0].auto_named, "missing auto_named ⇒ false ⇒ pinned");
    }

    #[test]
    fn list_saved_enumerates_and_sorts() {
        let _g = isolate();
        let mut a = sample_state("alpha");
        a.windows = vec![a.windows[0].clone(), a.windows[0].clone()]; // 2 windows
        save_session(&a).unwrap();
        save_session(&sample_state("beta")).unwrap();
        let listed = list_saved();
        let names: Vec<_> = listed.iter().map(|(n, _, _)| n.clone()).collect();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(listed[0].1, 2, "alpha has 2 windows");
        assert_eq!(listed[0].2, 2, "alpha has 2 panes total (1 per window)");
        assert_eq!(listed[1].1, 1, "beta has 1 window");
    }

    #[test]
    fn list_saved_skips_bad_files() {
        let _g = isolate();
        save_session(&sample_state("ok")).unwrap();
        std::fs::write(sessions_dir().join("broken.json"), b"{not json").unwrap();
        let listed = list_saved();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "ok");
    }

    #[test]
    fn save_replaces_existing() {
        let _g = isolate();
        save_session(&sample_state("rep")).expect("first save");
        let mut second = sample_state("rep");
        second.windows[0].panes[0].cwd = Some("/var".into());
        save_session(&second).expect("second save");
        let loaded = load_session("rep").expect("load").expect("present");
        assert_eq!(loaded.windows[0].panes[0].cwd.as_deref(), Some("/var"));
    }

    // ── scrollback DTO round-trip tests ────────────────────────────────────

    /// Build a styled live cell with every persisted dimension set.
    fn styled_cell(g: &str, ul: UnderlineStyle) -> Cell {
        Cell {
            grapheme: g.into(),
            fg: Color::Rgb(10, 20, 30),
            bg: Color::Indexed(42),
            underline_color: Color::Rgb(200, 100, 50),
            underline_style: ul,
            attrs: Attrs::BOLD | Attrs::UNDERLINE | Attrs::ITALIC,
            // Persistence is link-free: a non-`None` id here would break the
            // equality compare since restore always produces `None`.
            hyperlink_id: None,
        }
    }

    #[test]
    fn cell_dto_round_trips_styled_curly_and_dotted() {
        for ul in [UnderlineStyle::Curly, UnderlineStyle::Dotted] {
            let cell = styled_cell("Z", ul);
            let dto = cell_to_dto(&cell);
            let back = cell_from_dto(&dto);
            assert_eq!(back, cell, "styled cell with {ul:?} must round-trip exactly");
        }
    }

    #[test]
    fn row_dto_round_trips_wide_grapheme_with_spacer() {
        // A wide grapheme + its trailing spacer must both survive the trim.
        let mut row = Row::blank(8);
        row.cells[0] = Cell {
            grapheme: "世".into(),
            ..Cell::default()
        };
        row.cells[1] = Cell::wide_spacer();
        let dto = row_to_dto(&row);
        // Trim keeps through the spacer (col 1, non-default) and drops the
        // trailing default spaces.
        assert_eq!(dto.cells.len(), 2, "wide grapheme + spacer preserved, trailing trimmed");
        let back = row_from_dto(&dto, 8);
        assert_eq!(back.cells[0].grapheme.as_str(), "世");
        assert!(back.cells[1].is_wide_spacer(), "spacer survives round-trip");
        assert_eq!(back, row, "re-padded row equals the original (8 cols)");
    }

    #[test]
    fn row_dto_round_trips_soft_and_hard_wrap() {
        let mut hard = Row::blank(4);
        hard.cells[0].grapheme = "a".into();
        hard.wrap_origin = WrapOrigin::Hard;
        let back_hard = row_from_dto(&row_to_dto(&hard), 4);
        assert_eq!(back_hard.wrap_origin, WrapOrigin::Hard);
        assert_eq!(back_hard, hard);

        let mut soft = Row::blank(4);
        soft.cells[0].grapheme = "b".into();
        soft.wrap_origin = WrapOrigin::SoftFrom(7);
        let back_soft = row_from_dto(&row_to_dto(&soft), 4);
        assert_eq!(back_soft.wrap_origin, WrapOrigin::SoftFrom(7));
        assert_eq!(back_soft, soft);
    }

    #[test]
    fn row_mark_dto_round_trips_with_and_without_exit() {
        // BLOCK_END + exit code.
        let mut with_exit = Row::blank(4);
        with_exit.cells[0].grapheme = "x".into();
        with_exit.mark.set(RowMark::BLOCK_END);
        with_exit.mark.set_exit(Some(7));
        let back = row_from_dto(&row_to_dto(&with_exit), 4);
        assert!(back.mark.contains(RowMark::BLOCK_END));
        assert_eq!(back.mark.exit(), Some(7));
        assert_eq!(back, with_exit);

        // BLOCK_END with no parseable exit (D arrived without a code).
        let mut no_exit = Row::blank(4);
        no_exit.cells[0].grapheme = "y".into();
        no_exit.mark.set(RowMark::BLOCK_END);
        let back2 = row_from_dto(&row_to_dto(&no_exit), 4);
        assert!(back2.mark.contains(RowMark::BLOCK_END));
        assert_eq!(back2.mark.exit(), None);
        assert_eq!(back2, no_exit);

        // PROMPT_START + PROMPT_END with a column.
        let mut prompt = Row::blank(4);
        prompt.cells[0].grapheme = "$".into();
        prompt.mark.set(RowMark::PROMPT_START);
        prompt.mark.set_prompt_end(3);
        let back3 = row_from_dto(&row_to_dto(&prompt), 4);
        assert!(back3.mark.contains(RowMark::PROMPT_START));
        assert_eq!(back3.mark.prompt_end_col(), Some(3));
        assert_eq!(back3, prompt);
    }

    #[test]
    fn plain_cell_serializes_to_just_its_grapheme() {
        // A plain text cell elides every styled field, so only `grapheme` remains.
        let dto = cell_to_dto(&Cell {
            grapheme: "h".into(),
            ..Cell::default()
        });
        let json = serde_json::to_string(&dto).expect("serialize");
        assert_eq!(json, r#"{"grapheme":"h"}"#, "plain cell compacts to just its grapheme");
        // And it still round-trips to an equal `Cell`.
        let back: CellV1 = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cell_from_dto(&back), Cell { grapheme: "h".into(), ..Cell::default() });
    }

    #[test]
    fn plain_row_omits_wrap_and_mark_keys() {
        let row = RowV1 {
            cells: vec![cell_to_dto(&Cell { grapheme: "h".into(), ..Cell::default() })],
            wrap: WrapV1::Hard,
            mark: None,
        };
        let json = serde_json::to_string(&row).expect("serialize");
        assert_eq!(json, r#"{"cells":[{"grapheme":"h"}]}"#);
    }

    #[test]
    fn trailing_default_cells_trim_and_re_pad_losslessly() {
        let mut row = Row::blank(10);
        row.cells[0].grapheme = "a".into();
        row.cells[1].grapheme = "b".into();
        let dto = row_to_dto(&row);
        assert_eq!(dto.cells.len(), 2, "trailing default cells trimmed");
        let back = row_from_dto(&dto, 10);
        assert_eq!(back, row, "re-padded to 10 cols equals the original");
    }

    #[test]
    fn pane_scrollback_round_trips_through_save_load() {
        let _g = isolate();
        let mut row0 = Row::blank(8);
        row0.cells[0] = styled_cell("A", UnderlineStyle::Curly);
        row0.mark.set(RowMark::PROMPT_START);
        let mut row1 = Row::blank(8);
        row1.cells[0].grapheme = "o".into();
        row1.mark.set(RowMark::OUTPUT_START);
        let sb = PaneScrollbackV1 {
            cols: 8,
            rows: vec![row_to_dto(&row0), row_to_dto(&row1)],
        };
        let mut s = sample_state("sb");
        s.windows[0].panes[0].scrollback = Some(sb.clone());
        save_session(&s).expect("save");
        let loaded = load_session("sb").expect("load").expect("present");
        let loaded_sb = loaded.windows[0].panes[0].scrollback.as_ref().expect("scrollback present");
        assert_eq!(loaded_sb, &sb, "scrollback DTO round-trips through save/load");
        // And the live rows reconstruct exactly.
        assert_eq!(row_from_dto(&loaded_sb.rows[0], 8), row0);
        assert_eq!(row_from_dto(&loaded_sb.rows[1], 8), row1);
    }

    // ── RowMarkV1 exit Some(0) vs None presence test ───────────────────────
    // The danger: `exit_code` is 0 for BOTH `Some(0)` (`HAS_EXIT` set, code=0)
    // and `None` (`HAS_EXIT` absent, code=0 by default). Only the DTO's
    // `exit: Some(0)` vs absent (`skip_serializing_if = Option::is_none`)
    // distinguishes them. This test proves the JSON payload byte 0 is not
    // confused with the absent case.

    #[test]
    fn row_mark_dto_exit_some_zero_is_not_none() {
        // BLOCK_END + exit code 0 (success): `Some(0)` must survive the round-trip
        // as `Some(0)`, NOT be collapsed to `None`.
        let mut mark = RowMark::default();
        mark.set(RowMark::BLOCK_END);
        mark.set_exit(Some(0));

        let dto = mark_to_dto(mark).expect("BLOCK_END mark must produce a DTO");
        // The DTO must carry the `exit` field (`Some(0)`, not `None`).
        assert_eq!(dto.exit, Some(0), "Some(0) must be present in the DTO");
        // Serialized JSON must contain the `exit` key with value 0.
        let json = serde_json::to_string(&dto).expect("serialize");
        assert!(
            json.contains("\"exit\":0"),
            "Some(0) must serialize as '\"exit\":0', got: {json}"
        );
        // Deserialize back: exit must still be `Some(0)`, not `None`.
        let dto2: RowMarkV1 = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(dto2.exit, Some(0), "Some(0) must survive JSON round-trip");
        // Reconstruct the live mark: `exit()` must return `Some(0)`, not `None`.
        let live = mark_from_dto(&dto2);
        assert!(live.contains(RowMark::BLOCK_END), "BLOCK_END flag must survive");
        assert_eq!(live.exit(), Some(0), "live mark must have exit Some(0)");
    }

    #[test]
    fn row_mark_dto_exit_none_round_trips_as_none() {
        // BLOCK_END with no exit (D arrived without a code): must remain `None`.
        let mut mark = RowMark::default();
        mark.set(RowMark::BLOCK_END);
        // Explicitly leave exit unset (`None`).

        let dto = mark_to_dto(mark).expect("BLOCK_END mark must produce a DTO");
        assert_eq!(dto.exit, None, "absent exit must be None in the DTO");
        // Serialized JSON must NOT contain the `exit` key.
        let json = serde_json::to_string(&dto).expect("serialize");
        assert!(
            !json.contains("\"exit\""),
            "None exit must be omitted from JSON, got: {json}"
        );
        // Deserialize back: exit must still be `None`.
        let dto2: RowMarkV1 = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(dto2.exit, None, "None must survive JSON round-trip");
        // Reconstruct the live mark: `exit()` must return `None`.
        let live = mark_from_dto(&dto2);
        assert!(live.contains(RowMark::BLOCK_END), "BLOCK_END flag must survive");
        assert_eq!(live.exit(), None, "live mark must have exit None");
    }

    // ── capture_scrollback direct tests ───────────────────────────────────
    // Tests for the N-cap, blank-row drop, and None (empty/blank pane) branches.

    /// Build a minimal `Screen` with `n` non-blank rows in the scrollback and
    /// a blank main grid (rows × cols), suitable for testing `capture_scrollback`.
    fn screen_with_scrollback(scrollback_rows: usize, grid_rows: u16, cols: u16) -> plexy_glass_emulator::Screen {
        use plexy_glass_emulator::Screen;
        let mut s = Screen::new(grid_rows, cols);
        for i in 0..scrollback_rows {
            let mut row = Row::blank(cols);
            // Give each row a distinct non-default grapheme so it is NOT trimmed.
            row.cells[0].grapheme = format!("{}", (b'A' + (i % 26) as u8) as char).into();
            s.scrollback.push(row);
        }
        s
    }

    #[test]
    fn capture_scrollback_n_cap_returns_newest_n() {
        // 30 non-blank rows in scrollback only; grid is 1 row (also non-blank
        // to avoid the trailing-blank trim). n = 10 → capture the last 10 of 31
        // total rows. The blank-trailing-drop does NOT fire because all captured
        // rows carry a non-default grapheme.
        let mut screen = screen_with_scrollback(30, 1, 20);
        // Make the single main-grid row non-blank so it is not dropped by the
        // trailing-blank trim and counts toward the cap.
        screen.active.rows[0].cells[0].grapheme = "Z".into();

        let sb = capture_scrollback(&screen, 10).expect("non-empty screen must yield Some");
        assert_eq!(sb.rows.len(), 10, "exactly n=10 rows captured");
        // Total rows = 30 scrollback + 1 grid = 31; skip = 31-10 = 21.
        // Captured scrollback rows: indices 21..29 (8 rows), then the 1 grid row.
        // Row 21 → grapheme 'V' (21 % 26 = 21 → ord('A')+21).
        let first_grapheme = &sb.rows[0].cells[0].grapheme;
        assert_eq!(first_grapheme.as_str(), "V", "oldest captured scrollback row must be index 21 → 'V'");
        // Last row is the main-grid row.
        let last_grapheme = &sb.rows[9].cells[0].grapheme;
        assert_eq!(last_grapheme.as_str(), "Z", "last captured row must be the main-grid row → 'Z'");
    }

    #[test]
    fn capture_scrollback_drops_blank_trailing_rows() {
        // 3 non-blank rows followed by 2 blank grid rows (default = space, no mark).
        // The blank grid rows must be trimmed from the tail.
        let mut screen = screen_with_scrollback(0, 5, 10);
        // Put 3 non-blank rows in the main grid and leave the rest blank.
        for i in 0..3usize {
            screen.active.rows[i].cells[0].grapheme =
                format!("{}", (b'X' + i as u8) as char).into();
        }
        // Rows 3 and 4 remain blank (all-default, no mark).
        let sb = capture_scrollback(&screen, 1000).expect("non-blank rows must yield Some");
        assert_eq!(sb.rows.len(), 3, "blank trailing rows must be dropped, leaving 3");
    }

    #[test]
    fn capture_scrollback_cap_keeps_n_meaningful_rows_despite_blank_tail() {
        // 30 non-blank scrollback rows + a 5-row grid with only the top row
        // non-blank (4 blank trailing). n = 10. The blank tail must be dropped
        // BEFORE the cap, so exactly 10 MEANINGFUL rows survive, not 10 minus
        // the blank tail (the old cap-then-trim order returned only 6).
        let mut screen = screen_with_scrollback(30, 5, 20);
        screen.active.rows[0].cells[0].grapheme = "Z".into();
        // rows 1..5 remain blank.

        let sb = capture_scrollback(&screen, 10).expect("non-empty screen must yield Some");
        assert_eq!(sb.rows.len(), 10, "blank tail must not consume the N budget");
        // meaningful = 30 scrollback + 1 grid row = 31; skip = 21 → oldest kept
        // is scrollback[21] → 'A'+21 = 'V'.
        assert_eq!(sb.rows[0].cells[0].grapheme.as_str(), "V");
        // Last kept row is the non-blank grid row.
        assert_eq!(sb.rows[9].cells[0].grapheme.as_str(), "Z");
    }

    #[test]
    fn capture_scrollback_blank_pane_returns_none() {
        // A completely blank screen (no scrollback, default main grid) should
        // return None, there is nothing worth persisting.
        let screen = screen_with_scrollback(0, 5, 20);
        let result = capture_scrollback(&screen, 1000);
        assert!(result.is_none(), "a blank pane must return None, got: {result:?}");
    }
}
