use plexy_glass_config::StatusConfig;

pub struct StatusEngine;

impl StatusEngine {
    pub fn new(_cfg: &StatusConfig) -> Self {
        Self
    }
}

#[derive(Debug, Clone)]
pub struct WindowSummary {
    pub name: String,
    pub active: bool,
}

pub struct EvalContext<'a> {
    pub session_name: &'a str,
    pub windows: &'a [WindowSummary],
    pub active_window: usize,
    pub attached_clients: u8,
    pub prefix_active: bool,
    pub active_pane_cwd: Option<&'a str>,
}
