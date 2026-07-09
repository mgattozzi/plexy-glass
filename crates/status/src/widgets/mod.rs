mod git_branch;
mod shell;
mod simple;
mod system;
mod time;
mod window_list;

pub use git_branch::GitBranchWidget;
pub use shell::ShellWidget;
pub use simple::{
    AttachedClientsWidget, CwdWidget, HostnameWidget, PrefixIndicatorWidget, SeparatorWidget,
    SessionWidget, SshWidget, TextWidget,
};
pub use system::{BatteryWidget, CpuLoadWidget, MemoryWidget};
pub use time::TimeWidget;
pub use window_list::WindowListWidget;
