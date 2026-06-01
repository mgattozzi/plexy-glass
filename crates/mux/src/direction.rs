//! Direction (Up/Down/Left/Right) for neighbor traversal and SplitDir for
//! arranging children in a Split.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// Orientation of a split. `Horizontal` means the split bar lies horizontally,
/// so children stack top/bottom. `Vertical` means the split bar is vertical,
/// so children sit side by side. (tmux convention.)
// `Hash` so `SplitDir` can be a payload of `Command` (which derives `Hash` as a
// chord-trie terminal); `Command::JoinPane(SplitDir)` needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

impl Direction {
    pub fn axis(self) -> SplitDir {
        match self {
            Direction::Up | Direction::Down => SplitDir::Horizontal,
            Direction::Left | Direction::Right => SplitDir::Vertical,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_maps_directions_to_split_orientation() {
        assert_eq!(Direction::Up.axis(), SplitDir::Horizontal);
        assert_eq!(Direction::Down.axis(), SplitDir::Horizontal);
        assert_eq!(Direction::Left.axis(), SplitDir::Vertical);
        assert_eq!(Direction::Right.axis(), SplitDir::Vertical);
    }
}
