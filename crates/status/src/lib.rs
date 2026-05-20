//! Status-bar widgets and rendering engine.

mod engine;
mod style;
mod widget;
mod widgets;

pub use engine::{EvalContext, StatusEngine, WindowSummary};
pub use style::{resolve_style, ResolvedStyle, Rgb};
pub use widget::{Segment, StyledText, Widget};
pub use widgets::{
    AttachedClientsWidget, CwdWidget, HostnameWidget, PrefixIndicatorWidget, SeparatorWidget,
    SessionWidget, TextWidget,
};
