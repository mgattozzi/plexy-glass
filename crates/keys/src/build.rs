//! Compile a `KeymapConfig` into a runtime `Keymap`.

use crate::spec::{ChordSpec, parse_chord_seq, parse_chord_seq_with_prefix, parse_command};
use plexy_glass_config::{KeymapConfig, built_in_keymap};
use plexy_glass_mux::{Key, Keymap, Modifiers};

/// The default fallback prefix: `Ctrl+a`.
///
/// Used when `keymap.prefix` is invalid, empty, or resolves to more than one
/// chord. A config typo must never brick the session, so this follows the same
/// policy as invalid bindings (warn-and-skip).
const DEFAULT_PREFIX: ChordSpec = (Modifiers::CTRL, Key::Char('a'));

/// Resolve `keymap.prefix` to a single [`ChordSpec`].
///
/// `s` must be a single-chord string (e.g. `"Ctrl+b"`). If it is empty,
/// unparseable, or parses to more than one chord, a warning is emitted and
/// the function falls back to `Ctrl+a`.
fn resolve_prefix(s: &str) -> ChordSpec {
    match parse_chord_seq(s) {
        Ok(chords) if chords.len() == 1 => chords[0],
        Ok(_) => {
            tracing::warn!(
                value = s,
                "keymap.prefix must be a single chord; falling back to Ctrl+a"
            );
            DEFAULT_PREFIX
        }
        Err(e) => {
            tracing::warn!(
                value = s,
                error = %e,
                "keymap.prefix is invalid; falling back to Ctrl+a"
            );
            DEFAULT_PREFIX
        }
    }
}

pub fn build_keymap(cfg: &KeymapConfig) -> Keymap {
    // Resolve prefix; see `resolve_prefix` for the fallback policy.
    let prefix = resolve_prefix(&cfg.prefix);
    let mut km = Keymap::new();
    if cfg.inherit_defaults {
        apply(&mut km, &built_in_keymap().bindings, prefix);
    }
    apply(&mut km, &cfg.bindings, prefix);
    km
}

fn apply(km: &mut Keymap, bindings: &[plexy_glass_config::KeymapBinding], prefix: ChordSpec) {
    for (i, b) in bindings.iter().enumerate() {
        match (parse_chord_seq_with_prefix(&b.keys, prefix), parse_command(&b.command)) {
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
    fn default_bindings_include_next_layout_on_space() {
        let km_cfg = plexy_glass_config::built_in_keymap();
        let mut km = build_keymap(&km_cfg);
        // Ctrl+a Space → NextLayout
        let e1 = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        assert!(matches!(km.consume(e1, vec![0x01]), KeymapAction::Pending));
        let e2 = KeyEvent::new(Key::Char(' '), Modifiers::empty());
        assert!(matches!(
            km.consume(e2, b" ".to_vec()),
            KeymapAction::Command(Command::NextLayout)
        ));
    }

    #[test]
    fn default_bindings_include_block_scroll_chords() {
        let km_cfg = plexy_glass_config::built_in_keymap();
        let mut km = build_keymap(&km_cfg);
        // Ctrl+a < → PrevPrompt
        let e1 = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        assert!(matches!(km.consume(e1, vec![0x01]), KeymapAction::Pending));
        let e2 = KeyEvent::new(Key::Char('<'), Modifiers::empty());
        assert!(matches!(
            km.consume(e2, b"<".to_vec()),
            KeymapAction::Command(Command::PrevPrompt)
        ));
        // Ctrl+a > → NextPrompt
        let e3 = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        assert!(matches!(km.consume(e3, vec![0x01]), KeymapAction::Pending));
        let e4 = KeyEvent::new(Key::Char('>'), Modifiers::empty());
        assert!(matches!(
            km.consume(e4, b">".to_vec()),
            KeymapAction::Command(Command::NextPrompt)
        ));
    }

    #[test]
    fn invalid_prefix_falls_back_to_ctrl_a() {
        // "NotAKey", "", "Ctrl+a Ctrl+b", and "prefix" (circular) are all
        // invalid prefix values; each must fall back to Ctrl+a.
        // The warn itself is log-only (not observable here), but the binding fires.
        for bad_prefix in &["NotAKey", "", "Ctrl+a Ctrl+b", "prefix"] {
            let cfg = KeymapConfig {
                prefix: (*bad_prefix).into(),
                inherit_defaults: true,
                bindings: vec![],
            };
            let mut km = build_keymap(&cfg);
            let e1 = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
            assert!(
                matches!(km.consume(e1, vec![0x01]), KeymapAction::Pending),
                "Pending after Ctrl+a (bad prefix={bad_prefix:?})"
            );
            let e2 = KeyEvent::new(Key::Char('c'), Modifiers::empty());
            assert!(
                matches!(km.consume(e2, b"c".to_vec()), KeymapAction::Command(Command::NewWindow)),
                "NewWindow after Ctrl+a c (bad prefix={bad_prefix:?})"
            );
        }
    }

    #[test]
    fn custom_prefix_retargets_inherited_defaults() {
        // When the configured prefix is Ctrl+b, all defaults that used `prefix X`
        // (stored as "prefix X" in the binding strings) must resolve to Ctrl+b X,
        // NOT Ctrl+a X. This test fails before P2 because the defaults still store
        // literal "Ctrl+a X" strings which always resolve to Ctrl+a regardless of cfg.prefix.
        let cfg = KeymapConfig {
            prefix: "Ctrl+b".into(),
            inherit_defaults: true,
            bindings: vec![KeymapBinding {
                keys: "prefix H".into(),
                command: "resize_pane_left".into(),
            }],
        };
        let mut km = build_keymap(&cfg);

        // Ctrl+b c → NewWindow (default retargeted to new prefix)
        let e1 = KeyEvent::new(Key::Char('b'), Modifiers::CTRL);
        assert!(
            matches!(km.consume(e1, vec![0x02]), KeymapAction::Pending),
            "Ctrl+b should arm the prefix"
        );
        let e2 = KeyEvent::new(Key::Char('c'), Modifiers::empty());
        assert!(
            matches!(km.consume(e2, b"c".to_vec()), KeymapAction::Command(Command::NewWindow)),
            "Ctrl+b c should fire NewWindow"
        );

        // Ctrl+a should be PassThrough now (nothing bound under it)
        let e3 = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        let a3 = km.consume(e3, vec![0x01]);
        assert!(
            matches!(a3, KeymapAction::PassThrough(..)),
            "Ctrl+a should be PassThrough (no binding under Ctrl+a): got {a3:?}"
        );

        // Ctrl+b H → ResizePane(Left) (user binding using `prefix` token)
        let e4 = KeyEvent::new(Key::Char('b'), Modifiers::CTRL);
        assert!(
            matches!(km.consume(e4, vec![0x02]), KeymapAction::Pending),
            "Ctrl+b should arm the prefix (second time)"
        );
        let e5 = KeyEvent::new(Key::Char('H'), Modifiers::empty());
        assert!(
            matches!(
                km.consume(e5, b"H".to_vec()),
                KeymapAction::Command(Command::ResizePane(plexy_glass_mux::Direction::Left))
            ),
            "Ctrl+b H should fire ResizePane(Left)"
        );
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
