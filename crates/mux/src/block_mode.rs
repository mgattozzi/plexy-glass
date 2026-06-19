//! Per-pane block-mode state and a pure handler that consumes typed key events
//! to navigate OSC 133 command blocks, yank them, and re-run their commands.
//!
//! Mirrors `copy_mode`: the handler is pure (mutates state, reads a cloned
//! `Screen`, returns a `BlockModeAction`); the connection layer applies the
//! action (clipboard / paste buffer / inject / exit).

use crate::{Direction, Key, KeyEvent, Modifiers};
use plexy_glass_emulator::Screen;

/// Block-mode state. `selected` is the absolute line of the selected block's
/// `PROMPT_START`; `viewport_top` is the absolute line shown at viewport row 0
/// (block mode owns its own viewport, like copy mode, so the pane's wheel
/// scroll offset is left untouched and exit returns to the prior view).
#[derive(Debug, Clone)]
pub struct BlockMode {
    pub selected: u32,
    pub viewport_top: u32,
    pub pane_rows: u16,
    pub total_lines: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockModeAction {
    /// State changed: repaint, stay in mode.
    Render,
    /// Leave block mode.
    Exit,
    /// Copy text to the clipboard + paste-buffer stack; STAY in mode.
    Yank(String),
    /// Inject `command + Enter` into the pane, then exit + snap to live.
    ReRun(String),
    /// Key not handled: swallow it (modal isolation), no repaint.
    Ignore,
}

impl BlockMode {
    /// Try to open block mode on `screen`. Returns `None` (refuse) when a
    /// full-screen app is active (`alt`) or the pane has no command blocks
    /// (no `PROMPT_START` anywhere). On success the newest block is selected
    /// and scrolled to the top of the viewport.
    pub fn new_for(screen: &Screen, pane_rows: u16) -> Option<Self> {
        if screen.alt.is_some() {
            return None;
        }
        let selected = crate::blocks::last_prompt_line(screen)?;
        let mut state = Self {
            selected,
            viewport_top: 0,
            pane_rows,
            total_lines: crate::blocks::total_lines(screen),
        };
        state.recenter();
        Some(state)
    }

    /// Put the selected prompt at the top of the viewport, clamped so we never
    /// scroll past the live bottom (matching the `NextPrompt` snap-to-live).
    fn recenter(&mut self) {
        let max_top = self.total_lines.saturating_sub(u32::from(self.pane_rows));
        self.viewport_top = self.selected.min(max_top);
    }

    /// Called by `Pane::on_size_changed` on resize / scrollback growth.
    pub fn set_pane_rows(&mut self, pane_rows: u16, total_lines: u32) {
        self.pane_rows = pane_rows;
        self.total_lines = total_lines;
        if self.selected >= total_lines {
            self.selected = total_lines.saturating_sub(1);
        }
        self.recenter();
    }
}

/// Consume one key event, mutate state, return the action the caller applies.
pub fn handle(event: &KeyEvent, state: &mut BlockMode, screen: &Screen) -> BlockModeAction {
    use BlockModeAction::*;

    // Keep total_lines fresh (background output may have grown the screen) and
    // re-anchor the selection onto a surviving prompt (eviction / drift safety).
    state.total_lines = crate::blocks::total_lines(screen);
    match crate::blocks::prompt_at_or_above(screen, state.selected) {
        Some(p) => state.selected = p,
        None => match crate::blocks::first_prompt_line(screen) {
            Some(p) => state.selected = p,
            None => return Exit, // no blocks left at all
        },
    }

    // Esc / q exits.
    if event.mods.is_empty() && matches!(event.key, Key::Escape | Key::Char('q')) {
        return Exit;
    }

    match (event.mods, event.key) {
        (m, Key::Char('j')) | (m, Key::Arrow(Direction::Down)) if m.is_empty() => {
            match crate::blocks::next_prompt_line(screen, state.selected) {
                Some(p) => {
                    state.selected = p;
                    state.recenter();
                    Render
                }
                None => Ignore,
            }
        }
        (m, Key::Char('k')) | (m, Key::Arrow(Direction::Up)) if m.is_empty() => {
            match crate::blocks::prev_prompt_line(screen, state.selected) {
                Some(p) => {
                    state.selected = p;
                    state.recenter();
                    Render
                }
                None => Ignore,
            }
        }
        (m, Key::Char('g')) if m.is_empty() => {
            if let Some(p) = crate::blocks::first_prompt_line(screen) {
                state.selected = p;
                state.recenter();
            }
            Render
        }
        // Shifted `G` arrives with empty mods on legacy / modifyOtherKeys clients
        // and with SHIFT under Kitty, so accept both (matches copy mode).
        (m, Key::Char('G')) if m.is_empty() || m == Modifiers::SHIFT => {
            if let Some(p) = crate::blocks::last_prompt_line(screen) {
                state.selected = p;
                state.recenter();
            }
            Render
        }
        (m, Key::Char('y')) if m.is_empty() => {
            let range = crate::blocks::block_extent(screen, state.selected);
            Yank(crate::blocks::block_text(screen, range))
        }
        (m, Key::Char('o')) if m.is_empty() => {
            match crate::blocks::block_output_range(screen, state.selected) {
                Some(range) => Yank(crate::blocks::block_text(screen, range)),
                None => Ignore,
            }
        }
        (m, Key::Char('c')) if m.is_empty() => {
            match crate::blocks::block_command_line(screen, state.selected) {
                Some(cmd) => Yank(cmd),
                None => Ignore,
            }
        }
        (m, Key::Char('r')) if m.is_empty() => {
            match crate::blocks::block_command_line(screen, state.selected) {
                Some(cmd) => ReRun(cmd),
                None => Ignore,
            }
        }
        _ => Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_emulator::Emulator;

    fn screen_from(rows: u16, cols: u16, bytes: &[u8]) -> Screen {
        let mut e = Emulator::new(rows, cols);
        e.advance(bytes);
        e.advance(b"\x1b[m");
        e.screen().clone()
    }

    /// Two complete blocks: prompts at lines 0 and 3 (D+A share line 3). Each
    /// block has a `133;B` command mark so command extraction works.
    fn two_blocks() -> Screen {
        screen_from(
            8,
            20,
            b"\x1b]133;A\x07$ \x1b]133;B\x07one\r\n\
              \x1b]133;C\x07out1\r\n\
              out2\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07two\r\n\
              \x1b]133;C\x07out3",
        )
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::plain(Key::Char(c))
    }

    #[test]
    fn new_for_selects_newest_block() {
        let s = two_blocks();
        let bm = BlockMode::new_for(&s, 8).expect("opens");
        // Newest prompt is line 3 (the D+A row that starts block 2).
        assert_eq!(bm.selected, 3);
    }

    #[test]
    fn new_for_refuses_without_prompts() {
        let s = screen_from(4, 20, b"just text");
        assert!(BlockMode::new_for(&s, 4).is_none());
    }

    #[test]
    fn new_for_refuses_on_alt_screen() {
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ x\r\n\x1b[?1049h\x1b]133;A\x07$ alt");
        assert!(BlockMode::new_for(&s, 4).is_none());
    }

    #[test]
    fn k_then_j_move_selection_between_prompts() {
        let s = two_blocks();
        let mut bm = BlockMode::new_for(&s, 8).unwrap(); // selected = 3
        assert_eq!(handle(&key('k'), &mut bm, &s), BlockModeAction::Render);
        assert_eq!(bm.selected, 0, "k moves to older block");
        assert_eq!(handle(&key('k'), &mut bm, &s), BlockModeAction::Ignore);
        assert_eq!(bm.selected, 0, "k at oldest is a no-op");
        assert_eq!(handle(&key('j'), &mut bm, &s), BlockModeAction::Render);
        assert_eq!(bm.selected, 3, "j moves to newer block");
        assert_eq!(handle(&key('j'), &mut bm, &s), BlockModeAction::Ignore);
    }

    #[test]
    fn g_and_shift_g_jump_to_ends() {
        let s = two_blocks();
        let mut bm = BlockMode::new_for(&s, 8).unwrap();
        handle(&key('g'), &mut bm, &s);
        assert_eq!(bm.selected, 0);
        handle(&key('G'), &mut bm, &s);
        assert_eq!(bm.selected, 3);
    }

    #[test]
    fn yank_whole_block_includes_prompt_and_output() {
        let s = two_blocks();
        let mut bm = BlockMode::new_for(&s, 8).unwrap();
        handle(&key('k'), &mut bm, &s); // select block 1 (lines 0..=2)
        let action = handle(&key('y'), &mut bm, &s);
        match action {
            BlockModeAction::Yank(t) => {
                assert!(t.contains("one"), "command line present: {t:?}");
                assert!(t.contains("out1") && t.contains("out2"), "output present: {t:?}");
            }
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    #[test]
    fn yank_output_only_excludes_command() {
        let s = two_blocks();
        let mut bm = BlockMode::new_for(&s, 8).unwrap();
        handle(&key('k'), &mut bm, &s);
        match handle(&key('o'), &mut bm, &s) {
            BlockModeAction::Yank(t) => {
                assert_eq!(t, "out1\nout2");
            }
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    #[test]
    fn yank_command_only() {
        let s = two_blocks();
        let mut bm = BlockMode::new_for(&s, 8).unwrap();
        handle(&key('k'), &mut bm, &s);
        match handle(&key('c'), &mut bm, &s) {
            BlockModeAction::Yank(t) => assert_eq!(t, "one"),
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    #[test]
    fn rerun_returns_command_line() {
        let s = two_blocks();
        let mut bm = BlockMode::new_for(&s, 8).unwrap();
        handle(&key('k'), &mut bm, &s);
        assert_eq!(handle(&key('r'), &mut bm, &s), BlockModeAction::ReRun("one".to_string()));
    }

    #[test]
    fn esc_and_q_exit() {
        let s = two_blocks();
        let mut bm = BlockMode::new_for(&s, 8).unwrap();
        assert_eq!(
            handle(&KeyEvent::plain(Key::Escape), &mut bm, &s),
            BlockModeAction::Exit
        );
        assert_eq!(handle(&key('q'), &mut bm, &s), BlockModeAction::Exit);
    }

    #[test]
    fn unhandled_key_is_ignored() {
        let s = two_blocks();
        let mut bm = BlockMode::new_for(&s, 8).unwrap();
        assert_eq!(handle(&key('z'), &mut bm, &s), BlockModeAction::Ignore);
    }

    #[test]
    fn recenter_pins_to_live_bottom_for_newest() {
        let s = two_blocks(); // total_lines = 8
        let bm = BlockMode::new_for(&s, 8).unwrap(); // selected = 3, pane_rows = 8
        // max_top = 8 - 8 = 0, so newest selection clamps viewport_top to 0.
        assert_eq!(bm.viewport_top, 0);
    }
}
