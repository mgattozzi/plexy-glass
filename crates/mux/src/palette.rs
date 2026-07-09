//! Pure core for the command palette (`Ctrl+a Space`): a fuzzy finder over a
//! static, labeled catalog of every actionable command. Mirrors `history.rs`
//! (a finder-backed state + a pure handler); the daemon builds the entries
//! (filling each key from the active keymap) and adapts `PaletteOutcome` to its
//! dispatch. No daemon dependency, so it tests standalone.

use crate::command_prompt::{FocusTarget, SwapTarget};
use crate::finder::{self, FilterList, FinderKey};
use crate::{Direction, KeyEvent, LayoutPreset, PromptCommand, SplitDir};

/// What Enter on a palette entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteAction {
    /// Run now, through the same path a committed command-prompt line takes.
    Run(PromptCommand),
    /// Open the command prompt pre-filled with this string (e.g. `"rename "`)
    /// for a free-text argument.
    Prompt(String),
}

/// One catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    pub label: String,
    /// Extra search terms (incl. the raw verb) folded into the haystack.
    pub aliases: Vec<String>,
    /// The keymap command string this entry corresponds to (the join between
    /// the catalog and the keymap), when one exists. The daemon resolves `key`
    /// from it; `None` = no binding verb applies.
    pub binding_verb: Option<&'static str>,
    /// The resolved key chord for display, filled by the daemon at open time.
    /// Always `None` in the pure catalog.
    pub key: Option<String>,
    pub action: PaletteAction,
    /// Pre-lowercased `label + aliases`, the filter haystack.
    pub haystack: String,
}

/// history.rs-local follow-up; the daemon adapts it to its dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteOutcome {
    None,
    Redraw,
    Cancel,
    Run(PromptCommand),
    Prompt(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteState {
    pub entries: Vec<PaletteEntry>,
    pub finder: FilterList,
}

impl PaletteState {
    pub const fn new(entries: Vec<PaletteEntry>) -> Self {
        Self {
            entries,
            finder: FilterList::new(),
        }
    }

    fn haystacks(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.haystack.as_str()).collect()
    }

    pub fn visible_indices(&self) -> Vec<usize> {
        finder::filtered_indices(&self.haystacks(), &self.finder.filter)
    }

    pub fn selected(&self) -> Option<usize> {
        self.finder.selected(&self.haystacks())
    }

    pub fn filter(&self) -> &str {
        &self.finder.filter
    }
}

/// Apply one key. An empty filtered view can only `Cancel`.
pub fn handle_palette(event: &KeyEvent, state: &mut PaletteState) -> PaletteOutcome {
    let hs: Vec<&str> = state.entries.iter().map(|e| e.haystack.as_str()).collect();
    let redraw = |changed: bool| {
        if changed {
            PaletteOutcome::Redraw
        } else {
            PaletteOutcome::None
        }
    };
    match finder::classify(event) {
        FinderKey::Cancel => PaletteOutcome::Cancel,
        FinderKey::Accept => match state.finder.selected(&hs) {
            Some(i) => match &state.entries[i].action {
                PaletteAction::Run(cmd) => PaletteOutcome::Run(cmd.clone()),
                PaletteAction::Prompt(prefill) => PaletteOutcome::Prompt(prefill.clone()),
            },
            None => PaletteOutcome::None,
        },
        FinderKey::Up => redraw(state.finder.up()),
        FinderKey::Down => redraw(state.finder.down(&hs)),
        FinderKey::Home => redraw(state.finder.home()),
        FinderKey::End => redraw(state.finder.end(&hs)),
        FinderKey::Clear => redraw(state.finder.clear()),
        FinderKey::Backspace => redraw(state.finder.backspace(&hs)),
        FinderKey::Char(c) => {
            state.finder.push(c);
            PaletteOutcome::Redraw
        }
        FinderKey::Pass => PaletteOutcome::None,
    }
}

/// Build one entry, computing the haystack from label + aliases.
fn run(
    label: &str,
    binding_verb: Option<&'static str>,
    cmd: PromptCommand,
    aliases: &[&str],
) -> PaletteEntry {
    entry(label, binding_verb, PaletteAction::Run(cmd), aliases)
}

fn prompt(
    label: &str,
    binding_verb: Option<&'static str>,
    prefill: &str,
    aliases: &[&str],
) -> PaletteEntry {
    entry(
        label,
        binding_verb,
        PaletteAction::Prompt(prefill.to_string()),
        aliases,
    )
}

fn entry(
    label: &str,
    binding_verb: Option<&'static str>,
    action: PaletteAction,
    aliases: &[&str],
) -> PaletteEntry {
    let haystack = {
        let mut h = label.to_lowercase();
        for a in aliases {
            h.push(' ');
            h.push_str(&a.to_lowercase());
        }
        h
    };
    PaletteEntry {
        label: label.to_string(),
        aliases: aliases.iter().map(|s| (*s).to_string()).collect(),
        binding_verb,
        key: None,
        action,
        haystack,
    }
}

/// The comprehensive command catalog, in curated category order. Each entry's
/// `key` is `None`; the daemon fills it from the active keymap via `binding_verb`.
pub fn catalog() -> Vec<PaletteEntry> {
    use Direction::{Down, Left, Right, Up};
    use FocusTarget as FT;
    use LayoutPreset as LP;
    use PromptCommand as P;

    vec![
        // --- Windows ---
        run("New window", Some("new_window"), P::NewWindow, &["new"]),
        run("Next window", Some("next_window"), P::NextWindow, &["next"]),
        run(
            "Previous window",
            Some("prev_window"),
            P::PrevWindow,
            &["previous"],
        ),
        run(
            "Last window",
            Some("select_last_window"),
            P::LastWindow,
            &["last"],
        ),
        prompt("Select window…", None, "win ", &["window", "goto"]),
        prompt(
            "Rename window…",
            Some("rename_window"),
            "rename ",
            &["rename"],
        ),
        run(
            "Kill window",
            Some("kill_window"),
            P::KillWindow,
            &["kill", "close"],
        ),
        // --- Panes ---
        run("Split horizontal", Some("split_h"), P::SplitH, &["split"]),
        run("Split vertical", Some("split_v"), P::SplitV, &["split"]),
        run(
            "Zoom pane",
            Some("zoom_toggle"),
            P::Zoom,
            &["zoom", "fullscreen"],
        ),
        run(
            "Kill pane",
            Some("kill_pane"),
            P::KillPane,
            &["kill", "close"],
        ),
        run(
            "Focus pane left",
            Some("select_pane_left"),
            P::Focus(FT::Dir(Left)),
            &["focus", "move"],
        ),
        run(
            "Focus pane right",
            Some("select_pane_right"),
            P::Focus(FT::Dir(Right)),
            &["focus", "move"],
        ),
        run(
            "Focus pane up",
            Some("select_pane_up"),
            P::Focus(FT::Dir(Up)),
            &["focus", "move"],
        ),
        run(
            "Focus pane down",
            Some("select_pane_down"),
            P::Focus(FT::Dir(Down)),
            &["focus", "move"],
        ),
        run(
            "Focus next pane",
            Some("select_next_pane"),
            P::Focus(FT::Next),
            &["focus"],
        ),
        run(
            "Focus previous pane",
            Some("select_prev_pane"),
            P::Focus(FT::Prev),
            &["focus"],
        ),
        run(
            "Focus last pane",
            Some("select_last_pane"),
            P::Focus(FT::Last),
            &["focus"],
        ),
        run(
            "Resize pane left",
            Some("resize_pane_left"),
            P::Resize(Left, 1),
            &["resize"],
        ),
        run(
            "Resize pane right",
            Some("resize_pane_right"),
            P::Resize(Right, 1),
            &["resize"],
        ),
        run(
            "Resize pane up",
            Some("resize_pane_up"),
            P::Resize(Up, 1),
            &["resize"],
        ),
        run(
            "Resize pane down",
            Some("resize_pane_down"),
            P::Resize(Down, 1),
            &["resize"],
        ),
        prompt(
            "Rename pane…",
            Some("rename_pane"),
            "rename-pane ",
            &["rename", "pane"],
        ),
        // --- Pane mobility ---
        run("Mark pane", Some("mark_pane"), P::MarkPane, &["mark"]),
        run("Break pane", Some("break_pane"), P::BreakPane, &["break"]),
        run(
            "Join pane (vertical)",
            Some("join_pane"),
            P::JoinPane(SplitDir::Vertical),
            &["join"],
        ),
        run(
            "Join pane (horizontal)",
            None,
            P::JoinPane(SplitDir::Horizontal),
            &["join"],
        ),
        run(
            "Swap pane next",
            Some("swap_pane_next"),
            P::SwapPane(SwapTarget::Next),
            &["swap"],
        ),
        run(
            "Swap pane previous",
            Some("swap_pane_prev"),
            P::SwapPane(SwapTarget::Prev),
            &["swap"],
        ),
        run(
            "Swap with marked pane",
            Some("swap_marked_pane"),
            P::SwapMarked,
            &["swap", "marked"],
        ),
        // --- Layouts (concrete presets; the cycle stays a keyboard-only chord) ---
        run(
            "Layout: even-horizontal",
            Some("layout:even-horizontal"),
            P::Layout(LP::EvenHorizontal),
            &["layout"],
        ),
        run(
            "Layout: even-vertical",
            Some("layout:even-vertical"),
            P::Layout(LP::EvenVertical),
            &["layout"],
        ),
        run(
            "Layout: main-horizontal",
            Some("layout:main-horizontal"),
            P::Layout(LP::MainHorizontal),
            &["layout"],
        ),
        run(
            "Layout: main-vertical",
            Some("layout:main-vertical"),
            P::Layout(LP::MainVertical),
            &["layout"],
        ),
        run(
            "Layout: tiled",
            Some("layout:tiled"),
            P::Layout(LP::Tiled),
            &["layout"],
        ),
        // --- Sessions / navigation ---
        run(
            "Choose session",
            Some("choose_session"),
            P::ChooseSession,
            &["sessions"],
        ),
        prompt("Switch session…", None, "switch ", &["session"]),
        run("Choose tree", Some("choose_tree"), P::ChooseTree, &["tree"]),
        run(
            "History palette",
            Some("history"),
            P::History,
            &["history", "commands"],
        ),
        run("Hint mode", Some("hints"), P::Hints, &["hints", "links"]),
        // --- Modes / misc ---
        run(
            "Copy mode",
            Some("enter_copy_mode"),
            P::CopyMode,
            &["copy", "scroll"],
        ),
        run(
            "Block mode",
            Some("enter_block_mode"),
            P::BlockMode,
            &["block", "blocks"],
        ),
        run(
            "Toggle sync panes",
            Some("toggle_sync_panes"),
            P::ToggleSync,
            &["sync", "broadcast"],
        ),
        run("Detach", Some("detach"), P::Detach, &["detach"]),
        run(
            "Reload config",
            Some("reload_config"),
            P::Reload,
            &["reload", "config"],
        ),
        run("Help", Some("show_help"), P::Help, &["help", "keys"]),
        // --- Command blocks ---
        run(
            "Previous command",
            Some("prev_prompt"),
            P::PrevPrompt,
            &["prompt"],
        ),
        run(
            "Next command",
            Some("next_prompt"),
            P::NextPrompt,
            &["prompt"],
        ),
        run(
            "Copy last output",
            Some("copy_output"),
            P::CopyOutput,
            &["output", "copy"],
        ),
        // --- Monitoring ---
        run(
            "Monitor activity",
            Some("toggle_monitor_activity"),
            P::ToggleMonitorActivity,
            &["monitor"],
        ),
        run(
            "Monitor bell",
            Some("toggle_monitor_bell"),
            P::ToggleMonitorBell,
            &["monitor"],
        ),
        run(
            "Monitor command",
            Some("toggle_monitor_command"),
            P::ToggleMonitorCommand,
            &["monitor"],
        ),
        prompt("Monitor silence…", None, "monitor-silence ", &["monitor"]),
        // --- Buffers ---
        run(
            "Paste buffer",
            Some("paste_buffer"),
            P::PasteBuffer(None),
            &["paste"],
        ),
        run(
            "Choose buffer",
            Some("choose_buffer"),
            P::ChooseBuffer,
            &["buffers"],
        ),
        prompt("Set buffer…", None, "set-buffer ", &["buffer"]),
        prompt("Save buffer…", None, "save-buffer ", &["buffer"]),
        prompt("Load buffer…", None, "load-buffer ", &["buffer"]),
        // --- Popups / pipe ---
        run(
            "Popup (scratch shell)",
            Some("popup"),
            P::Popup(None),
            &["popup", "shell"],
        ),
        prompt("Open popup: command…", None, "popup ", &["popup", "run"]),
        run(
            "Close popup",
            Some("close_popup"),
            P::ClosePopup,
            &["popup", "close"],
        ),
        prompt("Pipe pane…", None, "pipe-pane ", &["pipe"]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Key, KeyEvent, Modifiers};

    fn chr(c: char) -> KeyEvent {
        KeyEvent::plain(Key::Char(c))
    }
    fn key(k: Key) -> KeyEvent {
        KeyEvent::plain(k)
    }

    #[test]
    fn catalog_is_nonempty_and_covers_all_three_kinds() {
        let c = catalog();
        assert!(c.len() > 40, "comprehensive catalog");
        // A run-immediate no-arg, an enumerable, and a free-text handoff exist.
        assert!(c.iter().any(|e| e.label == "Zoom pane"
            && matches!(e.action, PaletteAction::Run(PromptCommand::Zoom))));
        assert!(c.iter().any(|e| e.label == "Layout: tiled"));
        assert!(
            c.iter().any(|e| e.label == "Rename window…"
                && e.action == PaletteAction::Prompt("rename ".into()))
        );
    }

    #[test]
    fn catalog_haystacks_are_lowercased_label_plus_aliases() {
        let c = catalog();
        let e = c.iter().find(|e| e.label == "Toggle sync panes").unwrap();
        assert!(e.haystack.contains("toggle sync panes"));
        assert!(
            e.haystack.contains("broadcast"),
            "alias folded into haystack"
        );
        assert_eq!(e.key, None, "the pure catalog never resolves keys");
    }

    #[test]
    fn typing_filters_by_alias_and_enter_runs() {
        let mut s = PaletteState::new(catalog());
        for ch in "broadcast".chars() {
            handle_palette(&chr(ch), &mut s);
        }
        let vis = s.visible_indices();
        assert_eq!(vis.len(), 1, "only 'Toggle sync panes' matches 'broadcast'");
        match handle_palette(&key(Key::Enter), &mut s) {
            PaletteOutcome::Run(PromptCommand::ToggleSync) => {}
            other => panic!("expected Run(ToggleSync), got {other:?}"),
        }
    }

    #[test]
    fn enter_on_free_text_entry_yields_prompt_prefill() {
        let mut s = PaletteState::new(catalog());
        for ch in "rename window".chars() {
            handle_palette(&chr(ch), &mut s);
        }
        match handle_palette(&key(Key::Enter), &mut s) {
            PaletteOutcome::Prompt(p) => assert_eq!(p, "rename "),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn enter_with_no_match_is_none_and_esc_cancels() {
        let mut s = PaletteState::new(catalog());
        for ch in "zzzznope".chars() {
            handle_palette(&chr(ch), &mut s);
        }
        assert_eq!(
            handle_palette(&key(Key::Enter), &mut s),
            PaletteOutcome::None
        );
        assert_eq!(
            handle_palette(&key(Key::Escape), &mut s),
            PaletteOutcome::Cancel
        );
    }

    #[test]
    fn ctrl_jk_navigate() {
        let mut s = PaletteState::new(catalog());
        let first = s.selected();
        handle_palette(&KeyEvent::new(Key::Char('j'), Modifiers::CTRL), &mut s);
        assert_ne!(s.selected(), first, "Ctrl-j moved the selection");
        handle_palette(&KeyEvent::new(Key::Char('k'), Modifiers::CTRL), &mut s);
        assert_eq!(s.selected(), first, "Ctrl-k moved it back");
    }
}
