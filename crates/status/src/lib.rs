//! Status-bar widgets and rendering engine.

mod engine;
mod glyphs;
mod style;
mod widget;
mod widgets;

pub use engine::{
    ClickAction, EngineInner, EvalContext, SegmentSnapshot, SnapshotCtx, StatusEngine, StatusHit,
    WindowSummary,
};
pub use glyphs::{Cluster, GlyphSet, powerline_zone};
pub use style::{ResolvedStyle, Rgb, resolve_color, resolve_style};
pub use widget::{Segment, StyledText, Widget};
pub use widgets::{
    AttachedClientsWidget, BatteryWidget, CpuLoadWidget, CwdWidget, GitBranchWidget,
    HostnameWidget, MemoryWidget, PrefixIndicatorWidget, SeparatorWidget, SessionWidget,
    ShellWidget, TextWidget, TimeWidget, WindowListWidget,
};
