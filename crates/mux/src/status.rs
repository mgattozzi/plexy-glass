//! Status bar painting types. The actual segment data is produced by
//! plexy-glass-status; this module owns the geometry/painting only.

use plexy_glass_status::Segment;

#[derive(Debug, Clone, Default)]
pub struct StatusLine {
    pub left: Vec<Segment>,
    pub middle: Vec<Segment>,
    pub right: Vec<Segment>,
}

impl StatusLine {
    pub fn empty() -> Self {
        Self::default()
    }
}
