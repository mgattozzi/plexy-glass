//! Mouse event types, ANSI-SGR parser, and child-forwarding encoder.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Release,
    Move,
    /// Positive `delta` = wheel up, negative = wheel down.
    Wheel { delta: i16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub button: MouseButton,
    pub modifiers: MouseModifiers,
    /// 0-indexed row within the host viewport.
    pub row: u16,
    /// 0-indexed column within the host viewport.
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEncoding {
    /// `?9`: initial click only, no release/move.
    X10,
    /// `?1000`: press + release.
    ButtonEvent,
    /// `?1003`: press + release + any movement.
    AnyEvent,
    /// `?1006`: SGR encoding with extended coordinates.
    Sgr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseParseAction {
    Pending,
    Event(MouseEvent),
    Other(u8),
}

// Parser + encoder added in Tasks 2 and 3.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_default_all_false() {
        let m = MouseModifiers::default();
        assert!(!m.shift && !m.alt && !m.ctrl);
    }

    #[test]
    fn wheel_kind_sign_indicates_direction() {
        match (MouseKind::Wheel { delta: 3 }, MouseKind::Wheel { delta: -3 }) {
            (MouseKind::Wheel { delta: up }, MouseKind::Wheel { delta: down }) => {
                assert!(up > 0, "expected positive delta for wheel up");
                assert!(down < 0, "expected negative delta for wheel down");
            }
            _ => unreachable!(),
        }
    }
}
