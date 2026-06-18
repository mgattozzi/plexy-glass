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
pub use glyphs::GlyphSet;
pub use style::{resolve_color, resolve_style, ResolvedStyle, Rgb};
pub use widget::{Segment, StyledText, Widget};
pub use widgets::{
    AttachedClientsWidget, BatteryWidget, CpuLoadWidget, CwdWidget, GitBranchWidget, HostnameWidget,
    MemoryWidget, PrefixIndicatorWidget, SeparatorWidget, SessionWidget, ShellWidget, TextWidget,
    TimeWidget, WindowListWidget,
};
