//! On-disk session persistence. Each Session serializes to a per-session
//! JSON file at `$XDG_STATE_HOME/plexy-glass/sessions/<name>.json`.
//! Writes are atomic (tempfile + fsync + rename). Restore is on-demand
//! from `SessionRegistry::attach_or_create`.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// Schema version. Bump on any non-additive on-disk format change.
pub const SCHEMA_VERSION: u32 = 1;

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
    pub active_pane: u32,
    pub panes: Vec<PaneStateV1>,
    pub layout: LayoutStateV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PaneStateV1 {
    pub cwd: Option<String>,
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
    if state.schema != SCHEMA_VERSION {
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
