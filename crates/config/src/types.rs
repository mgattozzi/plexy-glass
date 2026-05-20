use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub palette: PaletteConfig,
    #[serde(default)]
    pub status: StatusConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PaletteConfig {
    #[serde(flatten)]
    pub entries: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Bottom,
    Top,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusConfig {
    #[serde(default)]
    pub position: Position,
    #[serde(default = "default_refresh", with = "humantime_serde")]
    pub refresh: Duration,
    #[serde(default)]
    pub left: Vec<WidgetSpec>,
    #[serde(default)]
    pub middle: Vec<WidgetSpec>,
    #[serde(default)]
    pub right: Vec<WidgetSpec>,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            position: Position::Bottom,
            refresh: default_refresh(),
            left: Vec::new(),
            middle: Vec::new(),
            right: Vec::new(),
        }
    }
}

fn default_refresh() -> Duration {
    Duration::from_secs(5)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StyleConfig {
    #[serde(default)]
    pub fg: Option<String>,
    #[serde(default)]
    pub bg: Option<String>,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub reverse: bool,
}

impl StyleConfig {
    pub fn new<S: Into<String>, T: Into<String>>(fg: S, bg: T) -> Self {
        Self {
            fg: Some(fg.into()),
            bg: Some(bg.into()),
            ..Self::default()
        }
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(from = "[u8; 2]", into = "[u8; 2]")]
pub struct Padding {
    pub left: u8,
    pub right: u8,
}

impl From<[u8; 2]> for Padding {
    fn from(arr: [u8; 2]) -> Self {
        Self { left: arr[0], right: arr[1] }
    }
}

impl From<Padding> for [u8; 2] {
    fn from(p: Padding) -> Self {
        [p.left, p.right]
    }
}

impl From<(u8, u8)> for Padding {
    fn from((left, right): (u8, u8)) -> Self {
        Self { left, right }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WidgetSpec {
    Session {
        #[serde(default)]
        style: StyleConfig,
        #[serde(default)]
        padding: Padding,
    },
    WindowList {
        active_style: StyleConfig,
        inactive_style: StyleConfig,
    },
    PrefixIndicator {
        style: StyleConfig,
        content: String,
    },
    AttachedClients {
        style: StyleConfig,
        #[serde(default = "default_min_clients")]
        min_count: u8,
    },
    Time {
        #[serde(default = "default_time_fmt")]
        format: String,
        #[serde(default, with = "humantime_serde::option")]
        interval: Option<Duration>,
        #[serde(default)]
        style: StyleConfig,
    },
    Hostname {
        #[serde(default)]
        style: StyleConfig,
        #[serde(default, with = "humantime_serde::option")]
        interval: Option<Duration>,
    },
    Cwd {
        #[serde(default)]
        style: StyleConfig,
        #[serde(default)]
        max_components: Option<u8>,
    },
    GitBranch {
        #[serde(default)]
        style: StyleConfig,
        #[serde(default, with = "humantime_serde::option")]
        interval: Option<Duration>,
    },
    Battery {
        #[serde(default)]
        style: StyleConfig,
        #[serde(default, with = "humantime_serde::option")]
        interval: Option<Duration>,
    },
    CpuLoad {
        #[serde(default)]
        style: StyleConfig,
        #[serde(default, with = "humantime_serde::option")]
        interval: Option<Duration>,
    },
    Memory {
        #[serde(default)]
        style: StyleConfig,
        #[serde(default, with = "humantime_serde::option")]
        interval: Option<Duration>,
    },
    Text {
        value: String,
        #[serde(default)]
        style: StyleConfig,
    },
    Separator {
        #[serde(default = "default_sep")]
        char: char,
        #[serde(default)]
        style: StyleConfig,
    },
    Shell {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, with = "humantime_serde::option")]
        interval: Option<Duration>,
        #[serde(default = "default_shell_timeout", with = "humantime_serde")]
        timeout: Duration,
        #[serde(default)]
        style: StyleConfig,
    },
}

fn default_min_clients() -> u8 { 2 }
fn default_time_fmt() -> String { "%H:%M".to_string() }
fn default_sep() -> char { '|' }
fn default_shell_timeout() -> Duration { Duration::from_secs(1) }
