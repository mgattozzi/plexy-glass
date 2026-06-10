//! On-disk session persistence. Each Session serializes to a per-session
//! JSON file at `$XDG_STATE_HOME/plexy-glass/sessions/<name>.json`.
//! Writes are atomic (tempfile + fsync + rename). Restore is on-demand
//! from `SessionRegistry::attach_or_create`.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// Schema version. Bump on any non-additive on-disk format change.
///
/// v1 -> v2: added `PaneStateV1.name` (optional, `#[serde(default)]`). v1 files
/// still load (the missing field defaults to `None`) so loads accept either.
///
/// Additive optional fields added since v2 do not bump the version (older files
/// default them to `None` via `#[serde(default)]`): `WindowStateV1.home_cwd`.
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
    let json = serde_json::to_vec_pretty(state)?;
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
                sync_input: false,
                home_cwd: None,
                active_pane: 0,
                panes: vec![PaneStateV1 { cwd: Some("/tmp".into()), name: None }],
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
}
