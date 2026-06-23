use std::collections::HashMap;
use std::time::Duration;

/// Which glyph repertoire the status surface and widgets use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GlyphTier {
    /// Box-drawing + simple symbols; renders on any font. Default.
    #[default]
    Unicode,
    /// Nerd Font icons + powerline separators.
    Nerd,
    /// Lowest-common-denominator ASCII fallback.
    Ascii,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub palette: PaletteConfig,
    pub status: StatusConfig,
    pub keymap: KeymapConfig,
    pub sessions: Vec<SessionTemplate>,
    pub blocks: BlocksConfig,
    pub notifications: NotificationsConfig,
    pub glyph_tier: GlyphTier,
    /// tmux's `automatic-rename`: when true, unpinned windows auto-name from
    /// their active pane (command → cwd → shell). Default true.
    pub auto_rename: bool,
}

/// Configuration for the block exit-status border feature.
#[derive(Debug, Clone, PartialEq)]
pub struct BlocksConfig {
    /// When `false`, no block-status border painting is performed.
    pub enabled: bool,
    /// Palette name or `#rrggbb` hex color for succeeded-command border segments.
    pub ok_color: String,
    /// Palette name or `#rrggbb` hex color for failed-command border segments.
    pub fail_color: String,
    /// Palette name or `#rrggbb` hex for the block-mode selection bracket.
    pub select_color: String,
    /// Pin the command line at the pane top when its block's output has scrolled
    /// above the viewport (live view only).
    pub sticky_header: bool,
    /// Show a block's wall-clock duration inline (right-aligned on the command row).
    pub duration: bool,
    /// Minimum duration (millis) to display; `0` shows every completed block.
    pub duration_threshold_ms: u32,
}

impl Default for BlocksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ok_color: "ok".to_string(),
            fail_color: "alert".to_string(),
            select_color: "#dca561".to_string(),
            sticky_header: true,
            duration: true,
            duration_threshold_ms: 2000,
        }
    }
}

/// Desktop notifications on command completion (long + unattended).
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationsConfig {
    /// Master switch.
    pub enabled: bool,
    /// Only notify for commands that ran at least this long (millis).
    pub min_duration_ms: u32,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { enabled: true, min_duration_ms: 30_000 }
    }
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
        /// Format in UTC (so `%Z` renders `UTC`) instead of the local timezone.
        utc: bool,
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

/// A declarative default session (Feature B): built fresh at daemon boot.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTemplate {
    pub name: String,
    pub cwd: Option<String>,
    /// Session-level env, inherited by every window/pane (overridable per
    /// window then per pane). Empty = no session-level overlay.
    pub env: Vec<(String, String)>,
    pub windows: Vec<WindowTemplate>, // invariant: non-empty (enforced by the decoder)
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowTemplate {
    pub name: String,
    pub cwd: Option<String>,
    /// `active=#true`: this window is the session's focused window on build.
    /// At most one window per session may be active (enforced by the decoder);
    /// default `false` means window 0 is active.
    pub active: bool,
    /// Window-level env, inherited by the window's panes (overlays the session
    /// env, overridden per pane).
    pub env: Vec<(String, String)>,
    pub layout: PaneNode,
}

/// A window's layout: a single pane, or a split of >= 2 child layouts.
#[derive(Debug, Clone, PartialEq)]
pub enum PaneNode {
    Leaf(PaneTemplate),
    Split {
        dir: SplitDirection,
        children: Vec<PaneNode>, // invariant: len() >= 2 (enforced by the decoder)
        /// Relative split weights aligned to `children`.
        // invariant: weights.len() == children.len() >= 2, every weight >= 1
        // (enforced by the decoder: `ratio=` defaults to 1 and `ratio=0` is a
        // decode error).
        weights: Vec<u32>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PaneTemplate {
    /// Shell command string (run via the default shell `-c`); None = interactive shell.
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub name: Option<String>,
    /// `active=#true`: this pane is its window's focused pane on build. At most
    /// one pane per window may be active (enforced by the decoder); default
    /// `false` means the DFS-leftmost pane is active.
    pub active: bool,
    /// Pane-level env, overlaying the window/session env (pane wins per key).
    pub env: Vec<(String, String)>,
}

/// Orientation of a config split. `Vertical` = side-by-side; `Horizontal` =
/// stacked (matches the engine's `SplitDir` and the `split_v`/`split_h` keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Vertical,
    Horizontal,
}
