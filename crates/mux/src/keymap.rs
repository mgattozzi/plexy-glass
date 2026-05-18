//! Prefix-key state machine. Translates raw input bytes into either
//! pass-through bytes (sent to the active pane) or `Command` events (which
//! mutate the `WindowManager`).

use crate::direction::Direction;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    SplitH,
    SplitV,
    SelectNextPane,
    SelectPrevPane,
    SelectPane(Direction),
    KillPane,
    ZoomToggle,
    NewWindow,
    NextWindow,
    PrevWindow,
    SelectWindow(u8),
    KillWindow,
    Detach,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeymapAction {
    PassThrough(u8),
    Command(Command),
    /// Byte consumed by the state machine (e.g., prefix); no side effects on the pane.
    Consumed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeymapState {
    PassThrough,
    AwaitingCommand,
}

pub struct Keymap {
    prefix: u8,
    state: KeymapState,
    bindings: HashMap<u8, Command>,
}

impl Keymap {
    /// Default keymap:
    /// - Prefix: Ctrl-a (`0x01`).
    /// - `v` / `s` split panes (matching `bind v split-window -h` and
    ///   `bind s split-window -v` in the user's tmux.conf; `-h` is
    ///   plexy-glass's vertical split, `-v` is horizontal).
    /// - `h` / `j` / `k` / `l` select pane left / down / up / right
    ///   (vi-style; works on both QWERTY and Colemak keyboards because
    ///   the bytes are layout-independent).
    /// - `n` is next-window (its tmux default).
    pub fn default_tmux() -> Self {
        let mut bindings: HashMap<u8, Command> = HashMap::new();
        // Splits, matching the user's tmux.conf.
        bindings.insert(b'v', Command::SplitV);
        bindings.insert(b's', Command::SplitH);
        // Cycling panes.
        bindings.insert(b'o', Command::SelectNextPane);
        bindings.insert(b';', Command::SelectPrevPane);
        // Directional pane selection (vi-style hjkl).
        bindings.insert(b'h', Command::SelectPane(Direction::Left));
        bindings.insert(b'j', Command::SelectPane(Direction::Down));
        bindings.insert(b'k', Command::SelectPane(Direction::Up));
        bindings.insert(b'l', Command::SelectPane(Direction::Right));
        // Pane lifecycle.
        bindings.insert(b'x', Command::KillPane);
        bindings.insert(b'z', Command::ZoomToggle);
        // Window management.
        bindings.insert(b'c', Command::NewWindow);
        bindings.insert(b'n', Command::NextWindow);
        bindings.insert(b'p', Command::PrevWindow);
        bindings.insert(b'&', Command::KillWindow);
        bindings.insert(b'd', Command::Detach);
        for digit in 0..10u8 {
            bindings.insert(b'0' + digit, Command::SelectWindow(digit));
        }
        Self {
            prefix: 0x01, // Ctrl-a (from user's tmux.conf)
            state: KeymapState::PassThrough,
            bindings,
        }
    }

    pub fn consume(&mut self, byte: u8) -> KeymapAction {
        match self.state {
            KeymapState::PassThrough => {
                if byte == self.prefix {
                    self.state = KeymapState::AwaitingCommand;
                    KeymapAction::Consumed
                } else {
                    KeymapAction::PassThrough(byte)
                }
            }
            KeymapState::AwaitingCommand => {
                self.state = KeymapState::PassThrough;
                if byte == self.prefix {
                    return KeymapAction::PassThrough(byte);
                }
                if byte == 0x1b {
                    return KeymapAction::Command(Command::Cancel);
                }
                if let Some(cmd) = self.bindings.get(&byte).copied() {
                    return KeymapAction::Command(cmd);
                }
                tracing::trace!(byte, "unknown command after prefix");
                KeymapAction::Consumed
            }
        }
    }

    /// True if we're currently between the prefix byte and the next byte.
    pub fn prefix_active(&self) -> bool {
        matches!(self.state, KeymapState::AwaitingCommand)
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self::default_tmux()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_byte_passes_through() {
        let mut k = Keymap::default_tmux();
        assert_eq!(k.consume(b'a'), KeymapAction::PassThrough(b'a'));
    }

    #[test]
    fn prefix_then_command_emits_command() {
        let mut k = Keymap::default_tmux();
        assert_eq!(k.consume(0x01), KeymapAction::Consumed);
        assert_eq!(k.consume(b'v'), KeymapAction::Command(Command::SplitV));
    }

    #[test]
    fn split_h_binding() {
        let mut k = Keymap::default_tmux();
        assert_eq!(k.consume(0x01), KeymapAction::Consumed);
        assert_eq!(k.consume(b's'), KeymapAction::Command(Command::SplitH));
    }

    #[test]
    fn directional_pane_select_bindings() {
        let mut k = Keymap::default_tmux();
        assert_eq!(k.consume(0x01), KeymapAction::Consumed);
        assert_eq!(
            k.consume(b'h'),
            KeymapAction::Command(Command::SelectPane(Direction::Left))
        );
        assert_eq!(k.consume(0x01), KeymapAction::Consumed);
        assert_eq!(
            k.consume(b'j'),
            KeymapAction::Command(Command::SelectPane(Direction::Down))
        );
        assert_eq!(k.consume(0x01), KeymapAction::Consumed);
        assert_eq!(
            k.consume(b'k'),
            KeymapAction::Command(Command::SelectPane(Direction::Up))
        );
        assert_eq!(k.consume(0x01), KeymapAction::Consumed);
        assert_eq!(
            k.consume(b'l'),
            KeymapAction::Command(Command::SelectPane(Direction::Right))
        );
    }

    #[test]
    fn double_prefix_passes_through_literal() {
        let mut k = Keymap::default_tmux();
        assert_eq!(k.consume(0x01), KeymapAction::Consumed);
        assert_eq!(k.consume(0x01), KeymapAction::PassThrough(0x01));
    }

    #[test]
    fn unknown_command_aborts_to_pass_through() {
        let mut k = Keymap::default_tmux();
        assert_eq!(k.consume(0x01), KeymapAction::Consumed);
        assert_eq!(k.consume(b'~'), KeymapAction::Consumed);
        assert_eq!(k.consume(b'a'), KeymapAction::PassThrough(b'a'));
    }

    #[test]
    fn escape_after_prefix_cancels() {
        let mut k = Keymap::default_tmux();
        assert_eq!(k.consume(0x01), KeymapAction::Consumed);
        assert_eq!(k.consume(0x1b), KeymapAction::Command(Command::Cancel));
    }

    #[test]
    fn digits_map_to_select_window() {
        let mut k = Keymap::default_tmux();
        assert_eq!(k.consume(0x01), KeymapAction::Consumed);
        assert_eq!(k.consume(b'3'), KeymapAction::Command(Command::SelectWindow(3)));
    }

    #[test]
    fn prefix_active_flag_tracks_state() {
        let mut k = Keymap::default_tmux();
        assert!(!k.prefix_active());
        k.consume(0x01);
        assert!(k.prefix_active());
        k.consume(b'v');
        assert!(!k.prefix_active());
    }
}
