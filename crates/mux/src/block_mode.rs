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
/// An active block-mode filter: the lens that restricts navigation to blocks
/// whose command+output contains `query` (case-insensitive). `matches` holds the
/// matching `PROMPT_START` line indices, ascending. `prompt_active` is true while
/// the user is typing the query (input goes to the query, not motions).
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub query: String,
    pub prompt_active: bool,
    pub matches: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct BlockMode {
    pub selected: u32,
    pub viewport_top: u32,
    pub pane_rows: u16,
    pub total_lines: u32,
    /// `None` = no filter (the full block set). `Some` = the lens is active.
    pub filter: Option<Filter>,
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
            filter: None,
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

    // A full-screen app took over the pane (alt screen) while block mode was
    // open. The OSC 133 marks live on the MAIN grid's scrollback, not on what
    // is now displayed, so navigating/yanking would act on stale content. Leave
    // block mode (the caller's Exit arm clears the per-pane state), the same
    // alt policy as the entry guard in `new_for`.
    if screen.alt.is_some() {
        return Exit;
    }

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

    // While the filter prompt is open, every key edits the query.
    if state.filter.as_ref().is_some_and(|f| f.prompt_active) {
        return handle_filter_prompt(event, state, screen);
    }

    // `/` opens the filter prompt (keeps any existing query for editing). An
    // empty query matches all blocks, so seed `matches` with the full set,
    // otherwise the first typed char would narrow from the empty default.
    if event.mods.is_empty() && event.key == Key::Char('/') {
        let all = crate::blocks::all_prompt_lines(screen);
        let f = state.filter.get_or_insert_with(Filter::default);
        f.prompt_active = true;
        if f.query.is_empty() {
            f.matches = all;
        }
        return Render;
    }

    // Esc clears an active (committed) filter first, then exits. `q` always exits.
    if event.mods.is_empty() && matches!(event.key, Key::Escape | Key::Char('q')) {
        if event.key == Key::Escape && state.filter.is_some() {
            state.filter = None;
            return Render;
        }
        return Exit;
    }

    // Motions operate over the active set (filtered matches, or all prompts),
    // wrapping at the ends.
    let set = active_set(state, screen);
    match (event.mods, event.key) {
        (m, Key::Char('j')) | (m, Key::Arrow(Direction::Down)) if m.is_empty() => {
            move_to(state, next_in(&set, state.selected, true))
        }
        (m, Key::Char('k')) | (m, Key::Arrow(Direction::Up)) if m.is_empty() => {
            move_to(state, next_in(&set, state.selected, false))
        }
        // `J`/`K` (failed-jump) arrive like `G`: empty mods on legacy /
        // modifyOtherKeys, SHIFT under Kitty.
        (m, Key::Char('J')) if m.is_empty() || m == Modifiers::SHIFT => {
            move_to(state, next_failed(&set, screen, state.selected, true))
        }
        (m, Key::Char('K')) if m.is_empty() || m == Modifiers::SHIFT => {
            move_to(state, next_failed(&set, screen, state.selected, false))
        }
        (m, Key::Char('g')) if m.is_empty() => move_to(state, set.first().copied()),
        (m, Key::Char('G')) if m.is_empty() || m == Modifiers::SHIFT => {
            move_to(state, set.last().copied())
        }
        (m, Key::Char('y')) if m.is_empty() => {
            let range = crate::blocks::block_extent(screen, state.selected);
            Yank(crate::blocks::block_text(screen, range))
        }
        (m, Key::Char('o')) if m.is_empty() => {
            // `selected` is always re-anchored to a real prompt above, so
            // block_output_range returns Some (it falls back to the prompt row
            // when the block has no OUTPUT_START). The None arm is defensive.
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

/// Apply a motion result: move + recenter + Render, or Ignore when there is no
/// target (empty active set).
fn move_to(state: &mut BlockMode, target: Option<u32>) -> BlockModeAction {
    match target {
        Some(p) => {
            state.selected = p;
            state.recenter();
            BlockModeAction::Render
        }
        None => BlockModeAction::Ignore,
    }
}

/// The ordered set of selectable prompt lines: the filter's matches when a
/// committed filter has a non-empty query, otherwise every prompt.
fn active_set(state: &BlockMode, screen: &Screen) -> Vec<u32> {
    match &state.filter {
        Some(f) if !f.query.is_empty() => f.matches.clone(),
        _ => crate::blocks::all_prompt_lines(screen),
    }
}

/// Next (`forward`) or previous prompt in `set` relative to `selected`, wrapping.
/// `set` is ascending. Returns `None` only for an empty set.
fn next_in(set: &[u32], selected: u32, forward: bool) -> Option<u32> {
    if set.is_empty() {
        return None;
    }
    if forward {
        set.iter().copied().find(|&p| p > selected).or_else(|| set.first().copied())
    } else {
        set.iter().copied().rev().find(|&p| p < selected).or_else(|| set.last().copied())
    }
}

/// Next/previous FAILED block (nonzero exit) within `set`, wrapping. `None` when
/// the set has no failed members.
fn next_failed(set: &[u32], screen: &Screen, selected: u32, forward: bool) -> Option<u32> {
    let failed: Vec<u32> = set
        .iter()
        .copied()
        .filter(|&p| matches!(crate::blocks::closing_exit(screen, p), Some(c) if c != 0))
        .collect();
    next_in(&failed, selected, forward)
}

/// Recompute the matching prompt lines for `query` (case-insensitive substring
/// over each block's command+output). When `prior` is `Some`, narrow that set (a
/// longer query's matches are a subset, so this is the fast incremental path);
/// when `None`, scan every block (first char, or a backspace that can grow the
/// set).
fn recompute_matches(screen: &Screen, query: &str, prior: Option<&[u32]>) -> Vec<u32> {
    if query.is_empty() {
        return crate::blocks::all_prompt_lines(screen);
    }
    let q = query.to_lowercase();
    let candidates = match prior {
        Some(p) => p.to_vec(),
        None => crate::blocks::all_prompt_lines(screen),
    };
    candidates
        .into_iter()
        .filter(|&prompt| {
            let text =
                crate::blocks::block_text(screen, crate::blocks::block_extent(screen, prompt));
            text.to_lowercase().contains(&q)
        })
        .collect()
}

/// After a filter edit, if the selection no longer matches, snap it to the
/// nearest match (first at-or-after, else last before). Holds when the set is
/// empty or the selection still matches.
fn snap_after_filter(state: &mut BlockMode) {
    let target = {
        let Some(f) = state.filter.as_ref() else { return };
        if f.query.is_empty() || f.matches.is_empty() || f.matches.contains(&state.selected) {
            return;
        }
        f.matches
            .iter()
            .copied()
            .find(|&p| p >= state.selected)
            .or_else(|| f.matches.iter().copied().rev().find(|&p| p < state.selected))
    };
    if let Some(t) = target {
        state.selected = t;
        state.recenter();
    }
}

/// Route a key to the filter-query editor while the prompt is active.
fn handle_filter_prompt(
    event: &KeyEvent,
    state: &mut BlockMode,
    screen: &Screen,
) -> BlockModeAction {
    use BlockModeAction::*;
    match (event.mods, event.key) {
        (m, Key::Enter) if m.is_empty() => {
            if let Some(f) = state.filter.as_mut() {
                f.prompt_active = false;
                if f.query.is_empty() {
                    state.filter = None; // empty query == no filter
                }
            }
            Render
        }
        (m, Key::Escape) if m.is_empty() => {
            state.filter = None;
            Render
        }
        (m, Key::Backspace) if m.is_empty() => {
            let query = match state.filter.as_mut() {
                Some(f) => {
                    f.query.pop();
                    f.query.clone()
                }
                None => return Ignore,
            };
            // A shrinking query can ADD matches → full rescan.
            let matches = recompute_matches(screen, &query, None);
            if let Some(f) = state.filter.as_mut() {
                f.matches = matches;
            }
            snap_after_filter(state);
            Render
        }
        (m, Key::Char(ch)) if m.is_empty() || m == Modifiers::SHIFT => {
            let (query, prior) = match state.filter.as_mut() {
                Some(f) => {
                    f.query.push(ch);
                    (f.query.clone(), f.matches.clone())
                }
                None => return Ignore,
            };
            // Appending a char only ever narrows → re-check the prior matches.
            let matches = recompute_matches(screen, &query, Some(&prior));
            if let Some(f) = state.filter.as_mut() {
                f.matches = matches;
            }
            snap_after_filter(state);
            Render
        }
        _ => Ignore, // arrows etc. while typing: swallow
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

    fn shift_key(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), Modifiers::SHIFT)
    }

    /// Three completed blocks: ok ("alpha") / fail ("beta", D;1) / ok ("gamma").
    /// The D+A rows share, so prompts land at lines 0, 2, 4 (fail at 2). Each
    /// block has a 133;B command mark.
    fn three_blocks_ok_fail_ok() -> Screen {
        screen_from(
            12,
            20,
            b"\x1b]133;A\x07$ \x1b]133;B\x07alpha\r\n\
              \x1b]133;C\x07out-a\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07beta\r\n\
              \x1b]133;C\x07BOOM\r\n\
              \x1b]133;D;1\x07\x1b]133;A\x07$ \x1b]133;B\x07gamma\r\n\
              \x1b]133;C\x07out-c\r\n\
              \x1b]133;D;0\x07",
        )
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
    fn j_k_wrap_at_the_edges() {
        let s = two_blocks();
        let mut bm = BlockMode::new_for(&s, 8).unwrap(); // selected = 3 (newest)
        // k from newest → older block.
        assert_eq!(handle(&key('k'), &mut bm, &s), BlockModeAction::Render);
        assert_eq!(bm.selected, 0);
        // k again wraps to the newest.
        assert_eq!(handle(&key('k'), &mut bm, &s), BlockModeAction::Render);
        assert_eq!(bm.selected, 3, "k at oldest wraps to newest");
        // j from newest wraps to oldest.
        assert_eq!(handle(&key('j'), &mut bm, &s), BlockModeAction::Render);
        assert_eq!(bm.selected, 0, "j at newest wraps to oldest");
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

    #[test]
    fn handle_exits_on_alt_screen() {
        // Block mode was open on the main screen; the child then enters the alt
        // screen. `handle` must Exit (marks live on the main grid, not on screen).
        let main = two_blocks();
        let mut bm = BlockMode::new_for(&main, 8).unwrap();
        let alt = screen_from(4, 20, b"\x1b]133;A\x07$ x\r\n\x1b[?1049h\x1b]133;A\x07$ alt");
        assert_eq!(handle(&key('j'), &mut bm, &alt), BlockModeAction::Exit);
        assert_eq!(handle(&key('y'), &mut bm, &alt), BlockModeAction::Exit);
    }

    #[test]
    fn handle_exits_when_no_prompts_remain() {
        // The selected prompt was evicted and no prompt survives anywhere.
        let s = screen_from(4, 20, b"plain text");
        let mut bm = BlockMode { selected: 0, viewport_top: 0, pane_rows: 4, total_lines: 1, filter: None };
        assert_eq!(handle(&key('j'), &mut bm, &s), BlockModeAction::Exit);
    }

    #[test]
    fn handle_reanchors_when_selected_is_non_prompt() {
        // `selected` drifted onto a non-prompt output row (e.g. eviction shifted
        // indices). handle re-anchors to the governing prompt before acting.
        let s = two_blocks(); // prompts at lines 0 and 3
        let mut bm = BlockMode::new_for(&s, 8).unwrap();
        bm.selected = 5; // an output row inside block 2 (lines 3..=7)
        match handle(&key('y'), &mut bm, &s) {
            BlockModeAction::Yank(t) => {
                assert_eq!(bm.selected, 3, "re-anchored to governing prompt");
                assert!(t.contains("two"), "yanked block 2: {t:?}");
            }
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    #[test]
    fn set_pane_rows_clamps_selection_and_recenters() {
        let s = two_blocks(); // total = 8
        let mut bm = BlockMode::new_for(&s, 8).unwrap(); // selected = 3
        bm.set_pane_rows(4, 6);
        assert_eq!(bm.pane_rows, 4);
        assert_eq!(bm.total_lines, 6);
        assert_eq!(bm.selected, 3, "still valid, not clamped");
        // recenter: viewport_top = selected.min(total - pane_rows) = 3.min(2) = 2
        assert_eq!(bm.viewport_top, 2);
        // Shrink total below the selection → clamp to total-1.
        bm.set_pane_rows(4, 2);
        assert_eq!(bm.selected, 1, "selection clamped to total-1");
    }

    #[test]
    fn g_g_return_render_even_at_edge() {
        // Distinct from j/k, which Ignore at the edge: g/G always Render.
        let s = two_blocks();
        let mut bm = BlockMode::new_for(&s, 8).unwrap(); // selected = 3 (newest)
        assert_eq!(handle(&key('G'), &mut bm, &s), BlockModeAction::Render);
        assert_eq!(bm.selected, 3, "G at newest is a no-op move but still Render");
        handle(&key('g'), &mut bm, &s); // → 0
        assert_eq!(handle(&key('g'), &mut bm, &s), BlockModeAction::Render);
        assert_eq!(bm.selected, 0, "g at oldest is a no-op move but still Render");
    }

    #[test]
    fn recompute_matches_command_and_output_hits() {
        let s = three_blocks_ok_fail_ok();
        // "alpha" is a command → block at line 0.
        assert_eq!(recompute_matches(&s, "alpha", None), vec![0]);
        // "BOOM" is OUTPUT of the second block → block at line 2 (case-insensitive).
        assert_eq!(recompute_matches(&s, "boom", None), vec![2]);
        // "out" appears in two blocks' output.
        assert_eq!(recompute_matches(&s, "out", None), vec![0, 4]);
        // No match.
        assert!(recompute_matches(&s, "zzz", None).is_empty());
    }

    #[test]
    fn recompute_matches_narrows_from_prior() {
        let s = three_blocks_ok_fail_ok();
        let prior = recompute_matches(&s, "out", None); // [0, 4]
        // Narrowing "out" → "out-c" re-checks only the prior set and keeps [4].
        assert_eq!(recompute_matches(&s, "out-c", Some(&prior)), vec![4]);
    }

    #[test]
    fn slash_opens_filter_then_typing_narrows_and_snaps() {
        let s = three_blocks_ok_fail_ok();
        let mut bm = BlockMode::new_for(&s, 12).unwrap(); // selected = 6 (newest)
        assert_eq!(handle(&key('/'), &mut bm, &s), BlockModeAction::Render);
        assert!(bm.filter.as_ref().unwrap().prompt_active);
        for ch in "alpha".chars() {
            handle(&key(ch), &mut bm, &s);
        }
        assert_eq!(bm.filter.as_ref().unwrap().matches, vec![0]);
        assert_eq!(bm.selected, 0, "selection snapped to the only match");
        assert_eq!(handle(&KeyEvent::plain(Key::Enter), &mut bm, &s), BlockModeAction::Render);
        assert!(!bm.filter.as_ref().unwrap().prompt_active);
        assert_eq!(bm.filter.as_ref().unwrap().query, "alpha");
    }

    #[test]
    fn j_k_restricted_to_filtered_set() {
        let s = three_blocks_ok_fail_ok();
        let mut bm = BlockMode::new_for(&s, 12).unwrap();
        handle(&key('/'), &mut bm, &s);
        for ch in "out".chars() {
            handle(&key(ch), &mut bm, &s);
        }
        handle(&KeyEvent::plain(Key::Enter), &mut bm, &s);
        assert_eq!(bm.filter.as_ref().unwrap().matches, vec![0, 4]);
        bm.selected = 0;
        handle(&key('j'), &mut bm, &s);
        assert_eq!(bm.selected, 4, "j skips the non-matching block at line 2");
        handle(&key('j'), &mut bm, &s);
        assert_eq!(bm.selected, 0, "j wraps within the filtered set");
    }

    #[test]
    fn failed_jump_visits_only_failed_blocks_wrapping() {
        let s = three_blocks_ok_fail_ok(); // fail at line 2
        let mut bm = BlockMode::new_for(&s, 12).unwrap(); // selected = 4 (newest)
        assert_eq!(handle(&shift_key('J'), &mut bm, &s), BlockModeAction::Render);
        assert_eq!(bm.selected, 2);
        // Only one failed block → J wraps back to 2 (stays).
        assert_eq!(handle(&shift_key('J'), &mut bm, &s), BlockModeAction::Render);
        assert_eq!(bm.selected, 2);
    }

    #[test]
    fn failed_jump_no_failures_is_ignored() {
        let s = two_blocks(); // both blocks D;0 (ok)
        let mut bm = BlockMode::new_for(&s, 8).unwrap();
        assert_eq!(handle(&shift_key('J'), &mut bm, &s), BlockModeAction::Ignore);
    }

    #[test]
    fn failed_jump_respects_active_filter() {
        let s = three_blocks_ok_fail_ok();
        let mut bm = BlockMode::new_for(&s, 12).unwrap();
        // Filter to "alpha" (block 0, OK) → no failed blocks in the set.
        handle(&key('/'), &mut bm, &s);
        for ch in "alpha".chars() {
            handle(&key(ch), &mut bm, &s);
        }
        handle(&KeyEvent::plain(Key::Enter), &mut bm, &s);
        assert_eq!(handle(&shift_key('J'), &mut bm, &s), BlockModeAction::Ignore);
    }

    #[test]
    fn esc_layering_clears_filter_then_exits() {
        let s = three_blocks_ok_fail_ok();
        let mut bm = BlockMode::new_for(&s, 12).unwrap();
        handle(&key('/'), &mut bm, &s);
        for ch in "out".chars() {
            handle(&key(ch), &mut bm, &s);
        }
        handle(&KeyEvent::plain(Key::Enter), &mut bm, &s);
        assert!(bm.filter.is_some());
        assert_eq!(handle(&KeyEvent::plain(Key::Escape), &mut bm, &s), BlockModeAction::Render);
        assert!(bm.filter.is_none());
        assert_eq!(handle(&KeyEvent::plain(Key::Escape), &mut bm, &s), BlockModeAction::Exit);
    }

    #[test]
    fn esc_while_typing_clears_filter() {
        let s = three_blocks_ok_fail_ok();
        let mut bm = BlockMode::new_for(&s, 12).unwrap();
        handle(&key('/'), &mut bm, &s);
        handle(&key('x'), &mut bm, &s);
        assert!(bm.filter.as_ref().unwrap().prompt_active);
        assert_eq!(handle(&KeyEvent::plain(Key::Escape), &mut bm, &s), BlockModeAction::Render);
        assert!(bm.filter.is_none(), "Esc while typing clears the filter");
    }

    #[test]
    fn backspace_to_empty_query_normalizes_to_no_filter_on_commit() {
        let s = three_blocks_ok_fail_ok();
        let mut bm = BlockMode::new_for(&s, 12).unwrap();
        handle(&key('/'), &mut bm, &s);
        handle(&key('a'), &mut bm, &s);
        handle(&KeyEvent::plain(Key::Backspace), &mut bm, &s); // query now ""
        assert_eq!(bm.filter.as_ref().unwrap().query, "");
        handle(&KeyEvent::plain(Key::Enter), &mut bm, &s);
        assert!(bm.filter.is_none(), "empty query commits to no filter");
    }

    #[test]
    fn q_while_typing_is_a_query_char_not_exit() {
        let s = three_blocks_ok_fail_ok();
        let mut bm = BlockMode::new_for(&s, 12).unwrap();
        handle(&key('/'), &mut bm, &s);
        assert_eq!(handle(&key('q'), &mut bm, &s), BlockModeAction::Render);
        assert_eq!(bm.filter.as_ref().unwrap().query, "q", "q typed into the query");
    }
}
