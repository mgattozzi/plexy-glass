use crate::Config;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("no config home")]
    NoHome,
}

pub fn load_or_default() -> (Config, Option<ConfigError>) {
    (Config::default(), None)
}
