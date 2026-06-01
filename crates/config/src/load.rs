use crate::{built_in_default, kdl_config, Config};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("config parse: {0}")]
    Kdl(String),
    #[error("XDG config home not found")]
    NoHome,
}

fn config_path() -> Result<PathBuf, ConfigError> {
    let dirs = directories::ProjectDirs::from("", "", "plexy-glass").ok_or(ConfigError::NoHome)?;
    Ok(dirs.config_dir().join("config.kdl"))
}

/// Load `~/.config/plexy-glass/config.kdl`. Missing file => built-in default,
/// no error. Parse/decode error => built-in default + Some(err).
pub fn load_or_default() -> (Config, Option<ConfigError>) {
    let path = match config_path() {
        Ok(p) => p,
        Err(e) => return (built_in_default(), Some(e)),
    };
    let bytes = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (built_in_default(), None);
        }
        Err(e) => return (built_in_default(), Some(ConfigError::Io(e))),
    };
    match kdl_config::parse_config(&bytes) {
        Ok(cfg) => (cfg, None),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "invalid config; using built-in default");
            (built_in_default(), Some(e))
        }
    }
}

/// Load from a specific path (used by tests + e2e).
#[doc(hidden)]
pub fn load_from_path(path: &std::path::Path) -> Result<Config, ConfigError> {
    let bytes = std::fs::read_to_string(path)?;
    kdl_config::parse_config(&bytes)
}
