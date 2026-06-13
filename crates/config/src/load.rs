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

/// Load `config.kdl` from the platform config directory (`~/.config/plexy-glass/config.kdl`
/// on Linux, `~/Library/Application Support/plexy-glass/config.kdl` on macOS).
/// Missing file => built-in default, no error. Parse/decode error => built-in default + Some(err).
pub fn load_or_default() -> (Config, Option<ConfigError>) {
    let path = match config_path() {
        Ok(p) => p,
        Err(e) => return (built_in_default(), Some(e)),
    };
    load_or_default_at(&path)
}

/// The file-reading + parsing core of [`load_or_default`], split out so it can
/// be tested against an injected path (the real `config_path` depends on the
/// platform config dir). Missing file → default + None; IO/parse error →
/// default + Some(err); valid file → (parsed, None).
fn load_or_default_at(path: &std::path::Path) -> (Config, Option<ConfigError>) {
    let bytes = match std::fs::read_to_string(path) {
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

#[cfg(test)]
mod load_tests {
    use super::*;

    #[test]
    fn missing_file_yields_default_and_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let (_cfg, err) = load_or_default_at(&dir.path().join("nope.kdl"));
        assert!(err.is_none(), "a missing file is not an error");
    }

    #[test]
    fn invalid_file_falls_back_to_default_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.kdl");
        std::fs::write(&path, "this is not valid kdl {{{").unwrap();
        let (_cfg, err) = load_or_default_at(&path);
        assert!(matches!(err, Some(ConfigError::Kdl(_))), "parse error surfaced: {err:?}");
    }

    #[test]
    fn valid_file_parses_with_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.kdl");
        std::fs::write(&path, "session \"s\" { window \"w\" { pane } }\n").unwrap();
        let (_cfg, err) = load_or_default_at(&path);
        assert!(err.is_none(), "a valid config parses cleanly: {err:?}");
    }
}
