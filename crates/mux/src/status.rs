//! Stub for the status-bar module. Real implementation lands in D7 (Task 12).

pub struct StatusLine;
pub struct WindowEntry;

pub fn build(_status: &StatusLine, _cols: u16) -> Vec<plexy_glass_emulator::Cell> {
    Vec::new()
}
