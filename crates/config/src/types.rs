use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub palette: PaletteConfig,
    pub status: StatusConfig,
    pub keymap: KeymapConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeymapConfig {
    pub prefix: String,
    pub inherit_defaults: bool,
    pub bindings: Vec<KeymapBinding>,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            prefix: default_prefix(),
            inherit_defaults: true,
            bindings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeymapBinding {
    pub keys: String,
    pub command: String,
}

fn default_prefix() -> String {
    "Ctrl+a".to_string()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaletteConfig {
    pub entries: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Position {
    #[default]
    Bottom,
    Top,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusConfig {
    pub position: Position,
    pub refresh: Duration,
    pub left: Vec<WidgetSpec>,
    pub middle: Vec<WidgetSpec>,
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StyleConfig {
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
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

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Padding {
    pub left: u8,
    pub right: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WidgetSpec {
    Session {
        style: StyleConfig,
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
        min_count: u8,
    },
    Time {
        format: String,
        interval: Option<Duration>,
        style: StyleConfig,
    },
    Hostname {
        style: StyleConfig,
        interval: Option<Duration>,
    },
    Cwd {
        style: StyleConfig,
        max_components: Option<u8>,
    },
    GitBranch {
        style: StyleConfig,
        interval: Option<Duration>,
    },
    Battery {
        style: StyleConfig,
        interval: Option<Duration>,
    },
    CpuLoad {
        style: StyleConfig,
        interval: Option<Duration>,
    },
    Memory {
        style: StyleConfig,
        interval: Option<Duration>,
    },
    Text {
        value: String,
        style: StyleConfig,
    },
    Separator {
        char: char,
        style: StyleConfig,
    },
    Shell {
        command: String,
        args: Vec<String>,
        interval: Option<Duration>,
        timeout: Duration,
        style: StyleConfig,
    },
}
