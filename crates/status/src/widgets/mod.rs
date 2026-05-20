mod simple;
mod system;
mod time;
mod window_list;

pub use simple::{
    AttachedClientsWidget, CwdWidget, HostnameWidget, PrefixIndicatorWidget, SeparatorWidget,
    SessionWidget, TextWidget,
};
pub use system::{BatteryWidget, CpuLoadWidget, MemoryWidget};
pub use time::TimeWidget;
pub use window_list::WindowListWidget;
