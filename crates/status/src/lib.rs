//! Status-bar widgets and rendering engine.

mod engine;
mod style;
mod widget;
mod widgets;

pub use engine::{EngineInner, EvalContext, SegmentSnapshot, StatusEngine, WindowSummary};
pub use style::{resolve_style, ResolvedStyle, Rgb};
pub use widget::{Segment, StyledText, Widget};
pub use widgets::{
    AttachedClientsWidget, BatteryWidget, CpuLoadWidget, CwdWidget, GitBranchWidget, HostnameWidget,
    MemoryWidget, PrefixIndicatorWidget, SeparatorWidget, SessionWidget, ShellWidget, TextWidget,
    TimeWidget, WindowListWidget,
};
