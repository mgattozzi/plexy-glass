mod simple;
mod time;
mod window_list;

pub use simple::{
    AttachedClientsWidget, CwdWidget, HostnameWidget, PrefixIndicatorWidget, SeparatorWidget,
    SessionWidget, TextWidget,
};
pub use time::TimeWidget;
pub use window_list::WindowListWidget;
