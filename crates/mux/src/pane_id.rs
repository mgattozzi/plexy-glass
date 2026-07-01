//! Opaque newtype IDs for panes and windows.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u32);

impl PaneId {
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl WindowId {
    pub const fn raw(self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_ids_equal_when_inner_matches() {
        assert_eq!(PaneId(1), PaneId(1));
        assert_ne!(PaneId(1), PaneId(2));
    }

    #[test]
    fn ids_are_hashable() {
        use std::collections::HashMap;
        let mut m: HashMap<PaneId, &str> = HashMap::new();
        m.insert(PaneId(7), "seven");
        assert_eq!(m.get(&PaneId(7)), Some(&"seven"));
    }

    #[test]
    fn raw_unwraps() {
        assert_eq!(PaneId(42).raw(), 42);
        assert_eq!(WindowId(3).raw(), 3);
    }
}
