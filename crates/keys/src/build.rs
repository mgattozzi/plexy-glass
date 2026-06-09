//! Compile a `KeymapConfig` into a runtime `Keymap`.

use crate::spec::{parse_chord_seq, parse_command};
use plexy_glass_config::{KeymapConfig, built_in_keymap};
use plexy_glass_mux::Keymap;

pub fn build_keymap(cfg: &KeymapConfig) -> Keymap {
    let mut km = Keymap::new();
    if cfg.inherit_defaults {
        apply(&mut km, &built_in_keymap().bindings);
    }
    apply(&mut km, &cfg.bindings);
    km
}

fn apply(km: &mut Keymap, bindings: &[plexy_glass_config::KeymapBinding]) {
    for (i, b) in bindings.iter().enumerate() {
        match (parse_chord_seq(&b.keys), parse_command(&b.command)) {
            (Ok(chords), Ok(cmd_spec)) => {
                km.bind(&chords, cmd_spec.command);
            }
            (Err(e), _) => {
                tracing::warn!(
                    idx = i,
                    keys = %b.keys,
                    error = %e,
                    "skipping invalid keymap binding (keys)"
                );
            }
            (_, Err(e)) => {
                tracing::warn!(
                    idx = i,
                    command = %b.command,
                    error = %e,
                    "skipping invalid keymap binding (command)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_config::{KeymapBinding, KeymapConfig};
    use plexy_glass_mux::{Command, Key, KeyEvent, KeymapAction, Modifiers};

    #[test]
    fn build_from_default_inherits_defaults() {
        let cfg = KeymapConfig::default();
        let mut km = build_keymap(&cfg);
        let e1 = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        assert!(matches!(km.consume(e1, vec![0x01]), KeymapAction::Pending));
        let e2 = KeyEvent::new(Key::Char('c'), Modifiers::empty());
        assert!(matches!(
            km.consume(e2, b"c".to_vec()),
            KeymapAction::Command(Command::NewWindow)
        ));
    }

    #[test]
    fn user_binding_overrides_default() {
        let cfg = KeymapConfig {
            prefix: "Ctrl+a".into(),
            inherit_defaults: true,
            bindings: vec![KeymapBinding {
                keys: "Ctrl+a c".into(),
                command: "kill_pane".into(),
            }],
        };
        let mut km = build_keymap(&cfg);
        let e1 = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        km.consume(e1, vec![0x01]);
        let e2 = KeyEvent::new(Key::Char('c'), Modifiers::empty());
        let action = km.consume(e2, b"c".to_vec());
        assert!(matches!(action, KeymapAction::Command(Command::KillPane)));
    }

    #[test]
    fn default_bindings_include_popup_chords() {
        let km_cfg = plexy_glass_config::built_in_keymap();
        let mut km = build_keymap(&km_cfg);
        // Ctrl+a P → OpenPopup { command: None }
        let e1 = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        assert!(matches!(km.consume(e1, vec![0x01]), KeymapAction::Pending));
        let e2 = KeyEvent::new(Key::Char('P'), Modifiers::empty());
        assert!(matches!(
            km.consume(e2, b"P".to_vec()),
            KeymapAction::Command(Command::OpenPopup { command: None })
        ));
        // Ctrl+a q → ClosePopup
        let e3 = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        assert!(matches!(km.consume(e3, vec![0x01]), KeymapAction::Pending));
        let e4 = KeyEvent::new(Key::Char('q'), Modifiers::empty());
        assert!(matches!(
            km.consume(e4, b"q".to_vec()),
            KeymapAction::Command(Command::ClosePopup)
        ));
    }

    #[test]
    fn invalid_binding_is_logged_and_skipped() {
        let cfg = KeymapConfig {
            prefix: "Ctrl+a".into(),
            inherit_defaults: false,
            bindings: vec![
                KeymapBinding {
                    keys: "Garbage+x".into(),
                    command: "new_window".into(),
                },
                KeymapBinding {
                    keys: "Alt+x".into(),
                    command: "new_window".into(),
                },
            ],
        };
        let mut km = build_keymap(&cfg);
        let e = KeyEvent::new(Key::Char('x'), Modifiers::ALT);
        assert!(matches!(
            km.consume(e, b"\x1bx".to_vec()),
            KeymapAction::Command(Command::NewWindow)
        ));
    }
}
