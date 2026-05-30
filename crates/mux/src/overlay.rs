//! Session-scoped interactive overlays: a modal text-input (rename) and a
//! scrollable display (help). Pure logic: the daemon owns the state on its
//! `WindowManager`, captures keys at the connection layer (mirroring copy
//! mode), and the compositor renders the overlay. This module only decides how
//! one key event mutates an overlay and what the caller should do next.

use crate::{Key, KeyEvent, Modifiers};

/// What a rename overlay targets. The concrete window/pane is resolved by the
/// daemon at open time, not stored here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameTarget {
    Window,
    Pane,
}

/// An active overlay. `None` (on the holder) means no overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// A single-line text prompt editing a name.
    Rename { target: RenameTarget, buf: String },
    /// A scrollable read-only page (e.g. the keybinding list). `scroll` is the
    /// top line index; the renderer clamps it to the content length.
    Help { scroll: u16 },
}

/// The caller's follow-up after feeding a key to an overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayAction {
    /// State changed; recompose the frame.
    Redraw,
    /// A rename was confirmed with this (already-trimmed) text.
    Commit(String),
    /// The overlay was dismissed with no effect.
    Cancel,
    /// Key ignored; nothing changed.
    None,
}

/// Lines scrolled by a page-style key. The handler does not know the viewport
/// height, so it uses a fixed step and the renderer clamps the result.
const PAGE_STEP: u16 = 10;

pub struct OverlayHandler;

impl OverlayHandler {
    /// Apply one key event to `overlay`. Pure: mutates `overlay` in place and
    /// returns the action for the caller to act on.
    pub fn handle(event: &KeyEvent, overlay: &mut Overlay) -> OverlayAction {
        match overlay {
            Overlay::Rename { buf, .. } => handle_rename(event, buf),
            Overlay::Help { scroll } => handle_help(event, scroll),
        }
    }
}

fn handle_rename(event: &KeyEvent, buf: &mut String) -> OverlayAction {
    match (event.mods, event.key) {
        // Esc cancels. Note: unlike the help overlay, `q` is a normal character
        // here and must NOT dismiss.
        (m, Key::Escape) if m.is_empty() => OverlayAction::Cancel,
        (_, Key::Enter) | (_, Key::KeypadEnter) => OverlayAction::Commit(buf.trim().to_string()),
        (m, Key::Backspace) if m.is_empty() => {
            if buf.pop().is_some() {
                OverlayAction::Redraw
            } else {
                OverlayAction::None
            }
        }
        // Printable scalar (plain or shifted). Reject control combos so e.g.
        // Ctrl+C doesn't insert a stray glyph.
        (m, Key::Char(c)) if m.is_empty() || m == Modifiers::SHIFT => {
            buf.push(c);
            OverlayAction::Redraw
        }
        _ => OverlayAction::None,
    }
}

fn handle_help(event: &KeyEvent, scroll: &mut u16) -> OverlayAction {
    use crate::Direction;
    // Dismiss keys (mirrors copy mode's escape chain).
    if event.mods.is_empty() && matches!(event.key, Key::Escape | Key::Char('q') | Key::Enter) {
        return OverlayAction::Cancel;
    }
    let before = *scroll;
    let next = match (event.mods, event.key) {
        (m, Key::Char('j')) | (m, Key::Arrow(Direction::Down)) if m.is_empty() => {
            scroll.saturating_add(1)
        }
        (m, Key::Char('k')) | (m, Key::Arrow(Direction::Up)) if m.is_empty() => {
            scroll.saturating_sub(1)
        }
        (m, Key::PageDown) if m.is_empty() => scroll.saturating_add(PAGE_STEP),
        (m, Key::PageUp) if m.is_empty() => scroll.saturating_sub(PAGE_STEP),
        (m, Key::Char('d')) if m == Modifiers::CTRL => scroll.saturating_add(PAGE_STEP),
        (m, Key::Char('u')) if m == Modifiers::CTRL => scroll.saturating_sub(PAGE_STEP),
        (m, Key::Char('g')) | (m, Key::Home) if m.is_empty() => 0,
        // `G` arrives shifted; jump to the bottom (renderer clamps to content).
        (m, Key::Char('G')) if m == Modifiers::SHIFT => u16::MAX,
        (m, Key::End) if m.is_empty() => u16::MAX,
        _ => return OverlayAction::None,
    };
    *scroll = next;
    if next == before {
        OverlayAction::None
    } else {
        OverlayAction::Redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(mods: Modifiers, key: Key) -> KeyEvent {
        KeyEvent::new(key, mods)
    }

    fn rename() -> Overlay {
        Overlay::Rename { target: RenameTarget::Window, buf: String::new() }
    }

    #[test]
    fn rename_appends_printable_chars() {
        let mut o = rename();
        assert_eq!(OverlayHandler::handle(&ev(Modifiers::empty(), Key::Char('h')), &mut o), OverlayAction::Redraw);
        OverlayHandler::handle(&ev(Modifiers::SHIFT, Key::Char('I')), &mut o);
        let Overlay::Rename { buf, .. } = &o else { panic!("expected rename") };
        assert_eq!(buf, "hI");
    }

    #[test]
    fn rename_backspace_pops_and_is_noop_when_empty() {
        let mut o = Overlay::Rename { target: RenameTarget::Pane, buf: "ab".into() };
        assert_eq!(OverlayHandler::handle(&ev(Modifiers::empty(), Key::Backspace), &mut o), OverlayAction::Redraw);
        OverlayHandler::handle(&ev(Modifiers::empty(), Key::Backspace), &mut o);
        let Overlay::Rename { buf, .. } = &o else { panic!() };
        assert!(buf.is_empty());
        // Backspace on empty: no change.
        assert_eq!(OverlayHandler::handle(&ev(Modifiers::empty(), Key::Backspace), &mut o), OverlayAction::None);
    }

    #[test]
    fn rename_enter_commits_trimmed() {
        let mut o = Overlay::Rename { target: RenameTarget::Window, buf: "  build  ".into() };
        assert_eq!(
            OverlayHandler::handle(&ev(Modifiers::empty(), Key::Enter), &mut o),
            OverlayAction::Commit("build".into())
        );
    }

    #[test]
    fn rename_escape_cancels_but_q_is_a_character() {
        let mut o = rename();
        // 'q' must be typed, not treated as dismiss.
        assert_eq!(OverlayHandler::handle(&ev(Modifiers::empty(), Key::Char('q')), &mut o), OverlayAction::Redraw);
        let Overlay::Rename { buf, .. } = &o else { panic!() };
        assert_eq!(buf, "q");
        assert_eq!(OverlayHandler::handle(&ev(Modifiers::empty(), Key::Escape), &mut o), OverlayAction::Cancel);
    }

    #[test]
    fn rename_ignores_control_combos() {
        let mut o = rename();
        assert_eq!(OverlayHandler::handle(&ev(Modifiers::CTRL, Key::Char('c')), &mut o), OverlayAction::None);
        let Overlay::Rename { buf, .. } = &o else { panic!() };
        assert!(buf.is_empty());
    }

    #[test]
    fn help_scrolls_and_saturates_at_top() {
        let mut o = Overlay::Help { scroll: 0 };
        // At the top, scrolling up is a no-op.
        assert_eq!(OverlayHandler::handle(&ev(Modifiers::empty(), Key::Char('k')), &mut o), OverlayAction::None);
        assert_eq!(OverlayHandler::handle(&ev(Modifiers::empty(), Key::Char('j')), &mut o), OverlayAction::Redraw);
        let Overlay::Help { scroll } = &o else { panic!() };
        assert_eq!(*scroll, 1);
    }

    #[test]
    fn help_page_and_jump_keys() {
        let mut o = Overlay::Help { scroll: 0 };
        OverlayHandler::handle(&ev(Modifiers::empty(), Key::PageDown), &mut o);
        let Overlay::Help { scroll } = &o else { panic!() };
        assert_eq!(*scroll, PAGE_STEP);
        OverlayHandler::handle(&ev(Modifiers::SHIFT, Key::Char('G')), &mut o);
        let Overlay::Help { scroll } = &o else { panic!() };
        assert_eq!(*scroll, u16::MAX);
        OverlayHandler::handle(&ev(Modifiers::empty(), Key::Char('g')), &mut o);
        let Overlay::Help { scroll } = &o else { panic!() };
        assert_eq!(*scroll, 0);
    }

    #[test]
    fn help_dismiss_keys() {
        for key in [Key::Escape, Key::Char('q'), Key::Enter] {
            let mut o = Overlay::Help { scroll: 3 };
            assert_eq!(OverlayHandler::handle(&ev(Modifiers::empty(), key), &mut o), OverlayAction::Cancel);
        }
    }
}
