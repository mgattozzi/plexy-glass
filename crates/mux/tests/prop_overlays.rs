//! Selection-consistency + totality of the three overlay input state machines
//! (`choose-tree`, history palette, hint mode). Each was hand-rolled with a
//! `selected`/`typed` cursor kept valid by hand at every mutation site — the
//! exact reference-apart-from-its-target smell. These props feed a drawn
//! `Vec<KeyEvent>` and, after every event, assert the selection stays inside the
//! visible set (or is consistently empty) and the handler never panics. Mirrors
//! `prop_finder`'s shape. A failure here is a CODE bug, not a property to weaken.

use hegel::{TestCase, generators as gs};
use plexy_glass_mux::{
    Direction, HintKind, HintState, HintTarget, HistoryEntry, HistoryState, Key, KeyEvent,
    Modifiers, PaneId, TreeNode, TreeState, WindowId, handle_history, handle_hint, handle_tree,
    pane_label, session_label, window_label,
};

/// A char biased toward the keys the overlays actually branch on (navigation,
/// filter-mode toggles, kill/rename, label letters), with an arbitrary-scalar
/// tail so filter/label-typing paths see junk too.
fn draw_char(tc: &TestCase) -> char {
    const MEANINGFUL: &[char] = &[
        '/', 'j', 'k', 'h', 'l', 'g', 'G', 'x', 'y', 'n', 'r', 'a', 's', 'd', 'f', ' ',
    ];
    let pick = tc.draw(
        gs::integers::<usize>()
            .min_value(0)
            .max_value(MEANINGFUL.len()),
    );
    if pick < MEANINGFUL.len() {
        MEANINGFUL[pick]
    } else {
        let cp = tc.draw(gs::integers::<u32>().min_value(0x20).max_value(0x2fff));
        char::from_u32(cp).unwrap_or('a')
    }
}

/// One arbitrary `KeyEvent`: a variant across the keys the overlays handle, with
/// empty / SHIFT / CTRL modifiers (the three the classifiers distinguish).
fn draw_key(tc: &TestCase) -> KeyEvent {
    let mods = match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
        0 => Modifiers::empty(),
        1 => Modifiers::CTRL,
        _ => Modifiers::SHIFT,
    };
    let key = match tc.draw(gs::integers::<u8>().min_value(0).max_value(11)) {
        0 => Key::Enter,
        1 => Key::Escape,
        2 => Key::Backspace,
        3 => Key::Arrow(Direction::Up),
        4 => Key::Arrow(Direction::Down),
        5 => Key::Arrow(Direction::Left),
        6 => Key::Arrow(Direction::Right),
        7 => Key::Home,
        8 => Key::End,
        9 => Key::KeypadEnter,
        _ => Key::Char(draw_char(tc)),
    };
    KeyEvent::new(key, mods)
}

fn draw_keys(tc: &TestCase) -> Vec<KeyEvent> {
    let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(30));
    (0..n).map(|_| draw_key(tc)).collect()
}

/// A realistic pre-order DFS tree (sessions -> windows -> panes), the shape the
/// daemon's `build_tree_nodes` produces — so collapse/filter/prune exercise real
/// structure, not arbitrary node soup.
fn draw_tree(tc: &TestCase) -> Vec<TreeNode> {
    let mut nodes = Vec::new();
    let n_sessions = tc.draw(gs::integers::<u32>().min_value(0).max_value(3));
    for si in 0..n_sessions {
        let session = format!("s{si}");
        nodes.push(TreeNode {
            session: session.clone(),
            window: None,
            pane: None,
            depth: 0,
            label: session_label(&session, 1, 1),
            name: session.clone(),
            index: 0,
            is_current: si == 0,
        });
        let n_windows = tc.draw(gs::integers::<u32>().min_value(0).max_value(3));
        for wi in 0..n_windows {
            let wname = format!("w{wi}");
            nodes.push(TreeNode {
                session: session.clone(),
                window: Some(WindowId(wi)),
                pane: None,
                depth: 1,
                label: window_label(wi + 1, &wname),
                name: wname,
                index: wi + 1,
                is_current: false,
            });
            let n_panes = tc.draw(gs::integers::<u32>().min_value(0).max_value(3));
            for pi in 0..n_panes {
                nodes.push(TreeNode {
                    session: session.clone(),
                    window: Some(WindowId(wi)),
                    pane: Some(PaneId(pi)),
                    depth: 2,
                    label: pane_label(pi + 1, ""),
                    name: String::new(),
                    index: pi + 1,
                    is_current: false,
                });
            }
        }
    }
    nodes
}

fn draw_history(tc: &TestCase) -> Vec<HistoryEntry> {
    let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(12));
    (0..n)
        .map(|i| {
            let command = tc.draw(gs::text().max_size(8));
            let output = tc.draw(gs::text().max_size(8));
            HistoryEntry {
                session: format!("s{i}"),
                window: WindowId(0),
                window_idx: 0,
                pane: PaneId(0),
                prompt_line: i as u32,
                command: command.clone(),
                exit: Some(0),
                duration: None,
                haystack: format!("{command}\n{output}").to_lowercase(),
            }
        })
        .collect()
}

fn draw_hints(tc: &TestCase) -> Vec<HintTarget> {
    let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
    (0..n)
        .map(|i| HintTarget {
            start: (0, i as u16),
            text: format!("target-{i}"),
            kind: HintKind::Sha,
        })
        .collect()
}

#[hegel::test(test_cases = 400)]
fn tree_selection_stays_visible(tc: TestCase) {
    let mut state = TreeState::new(draw_tree(&tc));
    for ev in draw_keys(&tc) {
        let _ = handle_tree(&ev, &mut state);
        // The raw index never escapes the node list...
        assert!(
            state.nodes.is_empty() || state.selected < state.nodes.len(),
            "tree selected index in range"
        );
        // ...and it references a visible row, unless nothing is visible.
        let vis = state.visible_indices();
        assert!(
            vis.is_empty() || vis.contains(&state.selected),
            "tree selection stays within the visible set"
        );
    }
}

#[hegel::test(test_cases = 400)]
fn history_selection_stays_visible(tc: TestCase) {
    let mut state = HistoryState::new(draw_history(&tc));
    for ev in draw_keys(&tc) {
        let _ = handle_history(&ev, &mut state);
        let vis = state.visible_indices();
        match state.selected() {
            None => assert!(vis.is_empty(), "history: None only when nothing matches"),
            Some(i) => assert!(vis.contains(&i), "history: selected must be a filtered index"),
        }
    }
}

#[hegel::test(test_cases = 400)]
fn hint_typed_prefix_always_matches(tc: TestCase) {
    let mut state = HintState::new(draw_hints(&tc), "asdfghjkl");
    for ev in draw_keys(&tc) {
        let _ = handle_hint(&ev, &mut state);
        // `typed` only grows when it stays a live label prefix, so the visible
        // set is empty only when there are no targets at all.
        assert!(
            state.labeled.is_empty() || state.visible().count() > 0,
            "hint: typed prefix must match at least one label"
        );
        assert!(
            state.typed.is_ascii(),
            "hint: typed prefix stays ASCII: {:?}",
            state.typed
        );
    }
}
