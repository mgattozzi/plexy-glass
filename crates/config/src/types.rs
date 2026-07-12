use std::collections::HashMap;
use std::num::NonZeroU32;
use std::time::Duration;

use crate::{ColorSource, Rgb};

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
    pub hints: HintsConfig,
    pub mouse: MouseConfig,
    pub notifications: NotificationsConfig,
    pub glyph_tier: GlyphTier,
    /// tmux's `automatic-rename`: when true, unpinned windows auto-name from
    /// their active pane (command → cwd → shell). Default true.
    pub auto_rename: bool,
    /// Show the one-time welcome overlay on a user's first ever attach (gated by
    /// a state-dir marker, so it appears once). Default true; set `welcome
    /// #false` to skip it. nushell's `show_banner`, as a modal.
    pub welcome: bool,
    /// Roster of remote hosts (`remotes { host "x" }`) the session picker spans
    /// alongside the local daemon. Empty by default (local-only).
    pub remotes: Vec<String>,
}

/// Configuration for the block exit-status border feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlocksConfig {
    /// When `false`, no block-status border painting is performed.
    pub enabled: bool,
    /// Palette name or `#rrggbb` hex color for succeeded-command border segments.
    pub ok_color: ColorSource,
    /// Palette name or `#rrggbb` hex color for failed-command border segments.
    pub fail_color: ColorSource,
    /// Palette name or `#rrggbb` hex for the block-mode selection bracket.
    pub select_color: ColorSource,
    /// Pin the command line at the pane top when its block's output has scrolled
    /// above the viewport (live view only).
    pub sticky_header: bool,
    /// Show a block's wall-clock duration inline (right-aligned on the command row).
    pub duration: bool,
    /// Minimum duration to display; `Duration::ZERO` shows every completed block.
    pub duration_threshold: Duration,
}

impl Default for BlocksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ok_color: ColorSource::Name("ok".to_string()),
            fail_color: ColorSource::Name("alert".to_string()),
            select_color: ColorSource::Literal(Rgb {
                r: 0xdc,
                g: 0xa5,
                b: 0x61,
            }),
            sticky_header: true,
            duration: true,
            duration_threshold: Duration::from_secs(2),
        }
    }
}

/// Configuration for hint mode (`prefix f`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintsConfig {
    pub enabled: bool,
    /// Label characters (home row by default). Must be >= 2 distinct chars;
    /// shorter values fall back to the default at use time.
    pub alphabet: String,
    /// Palette name or `#rrggbb` for the label text / background / highlighted
    /// match foreground.
    pub label_fg: ColorSource,
    pub label_bg: ColorSource,
    pub match_fg: ColorSource,
}

impl Default for HintsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            alphabet: "asdfghjkl".to_string(),
            label_fg: ColorSource::Name("bg".to_string()),
            label_bg: ColorSource::Name("warn".to_string()),
            match_fg: ColorSource::Name("ok".to_string()),
        }
    }
}

/// Which keyboard modifier must be held to drag-reorder window tabs or
/// drag-swap panes. `Shift` is intentionally unavailable because terminals
/// reserve Shift+drag for native text selection, so it never reaches the mux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragModifier {
    Alt,
    Ctrl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseConfig {
    pub drag_modifier: DragModifier,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            drag_modifier: DragModifier::Alt,
        }
    }
}

/// Desktop notifications on command completion (long + unattended) and
/// in-band requests (OSC 9 / OSC 777).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationsConfig {
    /// Master switch.
    pub enabled: bool,
    /// Only notify for commands that ran at least this long.
    pub min_duration: Duration,
    /// Raise a toast for OSC 9 / OSC 777 requests from child programs, unless
    /// you're looking right at the firing pane. Under `enabled`.
    pub in_band: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_duration: Duration::from_secs(30),
            in_band: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapBinding {
    pub keys: String,
    pub command: String,
}

fn default_prefix() -> String {
    "Ctrl+a".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaletteConfig {
    /// Role name → color, parsed from `#rrggbb` hex at decode. A malformed hex
    /// is a loud decode error, so every value here is a valid color.
    pub entries: HashMap<String, Rgb>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Position {
    #[default]
    Bottom,
    Top,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

const fn default_refresh() -> Duration {
    Duration::from_secs(5)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyleConfig {
    pub fg: Option<ColorSource>,
    pub bg: Option<ColorSource>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Padding {
    pub left: u8,
    pub right: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    Ssh {
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
        children: Vec<SplitChild>, // invariant: len() >= 2 (enforced by the decoder)
    },
}

/// A direct child of a `PaneNode::Split`: a child layout plus its relative split
/// weight. Folding the weight into the child (as a `NonZeroU32`) makes the two
/// invariants that used to be hand-maintained across the decoder and the
/// ratio-preorder math into type facts: a length mismatch between `children` and
/// a parallel `weights` vec, and a zero weight (which would divide-by-zero into
/// a NaN ratio) are both now unrepresentable.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitChild {
    /// Relative weight in the parent split (`ratio=`, default 1).
    pub weight: NonZeroU32,
    pub node: PaneNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
