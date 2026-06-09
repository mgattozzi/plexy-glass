//! Keymap: a chord trie that consumes typed `KeyEvent`s and emits `Command`
//! or `PassThrough`.

use crate::{Direction, Key, KeyEvent, KeyEventKind, Modifiers, SplitDir};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Command {
    NewWindow,
    NextWindow,
    PrevWindow,
    KillWindow,
    SelectWindow(u8),
    SplitV,
    SplitH,
    KillPane,
    ZoomToggle,
    SelectPane(Direction),
    SelectNextPane,
    SelectPrevPane,
    ResizePane(Direction),
    SelectLastWindow,
    SelectLastPane,
    /// Toggle the session-wide marked pane (set to the active pane, or clear it).
    MarkPane,
    /// Move the active pane into a new window of its own.
    BreakPane,
    /// Swap the active pane with its next (`true`) or previous (`false`) DFS
    /// neighbor in the same window.
    SwapPane(bool),
    /// Join the marked pane into the active window, splitting the active pane in
    /// the given direction.
    JoinPane(SplitDir),
    /// Swap the active pane with the marked pane (same window only in v1).
    SwapMarkedPane,
    /// Toggle monitor-activity on the active window.
    ToggleMonitorActivity,
    /// Toggle monitor-bell on the active window.
    ToggleMonitorBell,
    RenameWindow,
    RenamePane,
    ShowHelp,
    CommandPrompt,
    ChooseSession,
    ChooseTree,
    /// Paste the most-recent paste buffer into the active pane.
    PasteBuffer,
    /// Open the choose-buffer overlay.
    ChooseBuffer,
    Detach,
    Cancel,
    EnterCopyMode,
    ToggleSyncPanes,
    ReloadConfig,
    /// Open a floating popup pane running `command` via `$SHELL -c` (`None` =
    /// the default interactive shell), centered over the layout. Last-wins if
    /// a popup is already open.
    OpenPopup { command: Option<String> },
    /// Close the floating popup, killing its child.
    ClosePopup,
}

pub type Chord = (Modifiers, Key);

#[derive(Debug, Default, Clone)]
struct TrieNode {
    children: HashMap<Chord, TrieNode>,
    terminal: Option<Command>,
}

#[derive(Debug, Clone)]
pub struct Keymap {
    root: TrieNode,
    pending: Vec<Chord>,
    pending_bytes: Vec<u8>,
    /// The full last pending `KeyEvent` (the trie only keeps the `Chord`, which
    /// loses kind/text/alternates). `tick()` flushes this on timeout so the
    /// passed-through event is faithful, not a `kind=Press` reconstruction.
    pending_last_event: Option<KeyEvent>,
    pending_since: Option<Instant>,
    timeout: Duration,
}

#[derive(Debug, Clone)]
pub enum KeymapAction {
    /// Key bubbled all the way through; deliver to the active pane.
    PassThrough(KeyEvent, Vec<u8>),
    /// A binding fired.
    Command(Command),
    /// We're inside a chord sequence; hold until next chord.
    Pending,
    /// Pending sequence cancelled (timeout / non-matching key).
    Cancel,
}

impl Keymap {
    pub fn new() -> Self {
        Self {
            root: TrieNode::default(),
            pending: Vec::new(),
            pending_bytes: Vec::new(),
            pending_last_event: None,
            pending_since: None,
            timeout: Duration::from_secs(1),
        }
    }

    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    /// Add a binding. Later bindings with the same chord-sequence override earlier ones.
    pub fn bind(&mut self, chords: &[Chord], command: Command) {
        let mut node = &mut self.root;
        for chord in chords.iter() {
            node = node.children.entry(*chord).or_default();
        }
        node.terminal = Some(command);
    }

    pub fn prefix_active(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn consume(&mut self, event: KeyEvent, bytes: Vec<u8>) -> KeymapAction {
        // Check pending timeout.
        if let Some(at) = self.pending_since
            && at.elapsed() >= self.timeout
        {
            self.cancel();
        }

        // Release/Repeat events never trigger bindings, they flow straight to
        // the re-encode stage. Only Press is matched.
        if event.kind != KeyEventKind::Press {
            if self.pending.is_empty() {
                return KeymapAction::PassThrough(event, bytes);
            }
            self.cancel();
            return KeymapAction::Cancel;
        }
        // Lock modifiers (CapsLock/NumLock) are not part of any binding, so mask
        // them before lookup and a binding matches regardless of lock state.
        let lookup_mods = event.mods.difference(Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK);
        let chord = (lookup_mods, event.key);
        let node = self.descend();
        if let Some(child) = node.children.get(&chord) {
            if !child.children.is_empty() {
                self.pending.push(chord);
                self.pending_bytes.extend_from_slice(&bytes);
                self.pending_last_event = Some(event.clone());
                self.pending_since = Some(Instant::now());
                return KeymapAction::Pending;
            }
            if let Some(cmd) = child.terminal.clone() {
                self.cancel();
                return KeymapAction::Command(cmd);
            }
            self.cancel();
            return KeymapAction::Cancel;
        }

        if self.pending.is_empty() {
            return KeymapAction::PassThrough(event, bytes);
        }
        self.cancel();
        KeymapAction::Cancel
    }

    /// Call periodically to handle prefix timeout. Returns `Some(PassThrough)` when
    /// the held sequence has timed out.
    pub fn tick(&mut self) -> Option<KeymapAction> {
        if let Some(at) = self.pending_since
            && at.elapsed() >= self.timeout
        {
            let bytes = std::mem::take(&mut self.pending_bytes);
            // Flush the FULL pending event (kind/text/alternates preserved), not
            // a `kind=Press` reconstruction from the trie `Chord`.
            let last_event = self.pending_last_event.take();
            self.cancel();
            if let Some(event) = last_event {
                return Some(KeymapAction::PassThrough(event, bytes));
            }
            return Some(KeymapAction::Cancel);
        }
        None
    }

    fn descend(&self) -> &TrieNode {
        let mut node = &self.root;
        for chord in &self.pending {
            match node.children.get(chord) {
                Some(child) => node = child,
                None => return &self.root,
            }
        }
        node
    }

    fn cancel(&mut self) {
        self.pending.clear();
        self.pending_bytes.clear();
        self.pending_last_event = None;
        self.pending_since = None;
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    fn chord(mods: Modifiers, key: Key) -> Chord {
        (mods, key)
    }

    fn ev(mods: Modifiers, key: Key, bytes: &[u8]) -> (KeyEvent, Vec<u8>) {
        (KeyEvent::new(key, mods), bytes.to_vec())
    }

    #[test]
    fn unbound_key_passes_through() {
        let mut k = Keymap::new();
        let (e, b) = ev(Modifiers::empty(), Key::Char('z'), b"z");
        let action = k.consume(e, b);
        assert!(matches!(action, KeymapAction::PassThrough(_, ref bs) if bs == b"z"));
    }

    #[test]
    fn direct_binding_fires_command() {
        let mut k = Keymap::new();
        k.bind(
            &[chord(Modifiers::ALT, Key::Arrow(Direction::Right))],
            Command::SelectPane(Direction::Right),
        );
        let (e, b) = ev(Modifiers::ALT, Key::Arrow(Direction::Right), b"\x1b[1;3C");
        let action = k.consume(e, b);
        assert!(matches!(action, KeymapAction::Command(Command::SelectPane(Direction::Right))));
    }

    #[test]
    fn binding_matches_with_lock_modifiers_set() {
        // CAPS_LOCK / NUM_LOCK are masked before trie lookup, so a Ctrl+a chord
        // still fires when CapsLock happens to be on.
        let mut k = Keymap::new();
        k.bind(
            &[chord(Modifiers::CTRL, Key::Char('a')), chord(Modifiers::empty(), Key::Char('c'))],
            Command::NewWindow,
        );
        let mut e1 = KeyEvent::new(Key::Char('a'), Modifiers::CTRL | Modifiers::CAPS_LOCK);
        e1.kind = KeyEventKind::Press;
        assert!(matches!(k.consume(e1, vec![0x01]), KeymapAction::Pending));
        let mut e2 = KeyEvent::new(Key::Char('c'), Modifiers::NUM_LOCK);
        e2.kind = KeyEventKind::Press;
        assert!(matches!(k.consume(e2, b"c".to_vec()), KeymapAction::Command(Command::NewWindow)));
    }

    #[test]
    fn release_event_never_matches_a_binding() {
        // A Release for the very same chord must NOT fire, it passes through.
        let mut k = Keymap::new();
        k.bind(
            &[chord(Modifiers::ALT, Key::Arrow(Direction::Right))],
            Command::SelectPane(Direction::Right),
        );
        let mut e = KeyEvent::new(Key::Arrow(Direction::Right), Modifiers::ALT);
        e.kind = KeyEventKind::Release;
        assert!(matches!(k.consume(e, b"\x1b[1;3C".to_vec()), KeymapAction::PassThrough(..)));
    }

    #[test]
    fn repeat_event_never_matches_a_binding() {
        let mut k = Keymap::new();
        k.bind(
            &[chord(Modifiers::ALT, Key::Arrow(Direction::Right))],
            Command::SelectPane(Direction::Right),
        );
        let mut e = KeyEvent::new(Key::Arrow(Direction::Right), Modifiers::ALT);
        e.kind = KeyEventKind::Repeat;
        assert!(matches!(k.consume(e, b"\x1b[1;3C".to_vec()), KeymapAction::PassThrough(..)));
    }

    #[test]
    fn release_during_pending_prefix_cancels() {
        // A Release arriving after the prefix chord is consumed cancels the
        // pending sequence (it can't advance the trie), rather than passing
        // through. Covers the non-Press + pending branch.
        let mut k = Keymap::new();
        k.bind(
            &[
                chord(Modifiers::CTRL, Key::Char('a')),
                chord(Modifiers::empty(), Key::Char('c')),
            ],
            Command::NewWindow,
        );
        let (e1, b1) = ev(Modifiers::CTRL, Key::Char('a'), &[0x01]);
        assert!(matches!(k.consume(e1, b1), KeymapAction::Pending));
        assert!(k.prefix_active());
        let mut e2 = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        e2.kind = KeyEventKind::Release;
        assert!(matches!(k.consume(e2, b"".to_vec()), KeymapAction::Cancel));
        assert!(!k.prefix_active());
    }

    #[test]
    fn prefix_sequence_fires_on_second_chord() {
        let mut k = Keymap::new();
        k.bind(
            &[chord(Modifiers::CTRL, Key::Char('a')), chord(Modifiers::empty(), Key::Char('c'))],
            Command::NewWindow,
        );
        let (e1, b1) = ev(Modifiers::CTRL, Key::Char('a'), &[0x01]);
        assert!(matches!(k.consume(e1, b1), KeymapAction::Pending));
        let (e2, b2) = ev(Modifiers::empty(), Key::Char('c'), b"c");
        assert!(matches!(k.consume(e2, b2), KeymapAction::Command(Command::NewWindow)));
    }

    #[test]
    fn prefix_non_matching_followup_cancels() {
        let mut k = Keymap::new();
        k.bind(
            &[chord(Modifiers::CTRL, Key::Char('a')), chord(Modifiers::empty(), Key::Char('c'))],
            Command::NewWindow,
        );
        let (e1, b1) = ev(Modifiers::CTRL, Key::Char('a'), &[0x01]);
        assert!(matches!(k.consume(e1, b1), KeymapAction::Pending));
        let (e2, b2) = ev(Modifiers::empty(), Key::Char('z'), b"z");
        assert!(matches!(k.consume(e2, b2), KeymapAction::Cancel));
    }

    #[test]
    fn pending_timeout_triggers_cancel_on_tick() {
        let mut k = Keymap::new();
        k.set_timeout(Duration::from_millis(50));
        k.bind(
            &[chord(Modifiers::CTRL, Key::Char('a')), chord(Modifiers::empty(), Key::Char('c'))],
            Command::NewWindow,
        );
        let (e1, b1) = ev(Modifiers::CTRL, Key::Char('a'), &[0x01]);
        assert!(matches!(k.consume(e1, b1), KeymapAction::Pending));
        sleep(Duration::from_millis(80));
        let tick = k.tick().expect("expected timeout flush");
        assert!(matches!(tick, KeymapAction::PassThrough(..)));
        assert!(!k.prefix_active());
    }

    #[test]
    fn pending_timeout_flush_preserves_full_event() {
        // On timeout the buffered prefix key is flushed via the FULL stored
        // event (text/shifted intact), not a `Chord`-reconstructed bare Press.
        let mut k = Keymap::new();
        k.set_timeout(Duration::from_millis(50));
        k.bind(
            &[chord(Modifiers::CTRL, Key::Char('a')), chord(Modifiers::empty(), Key::Char('c'))],
            Command::NewWindow,
        );
        let mut event = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        event.text = Some("a".into());
        event.shifted = Some('A');
        assert!(matches!(k.consume(event, vec![0x01]), KeymapAction::Pending));
        sleep(Duration::from_millis(80));
        match k.tick().expect("expected timeout flush") {
            KeymapAction::PassThrough(ev, _) => {
                assert_eq!(ev.text.as_deref(), Some("a"), "text preserved through tick");
                assert_eq!(ev.shifted, Some('A'), "shifted key preserved through tick");
            }
            other => panic!("expected PassThrough, got {other:?}"),
        }
    }
}
