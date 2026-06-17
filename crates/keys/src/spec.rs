//! Parsers for the `keys` and `command` strings in `[[keymap.bindings]]`.

use plexy_glass_mux::{Command, Direction, Key, Modifiers, SplitDir};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyParseError {
    #[error("empty chord")]
    Empty,
    #[error("unknown token: {0}")]
    UnknownToken(String),
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    #[error("invalid argument for command {command}: {arg}")]
    BadArg { command: String, arg: String },
    #[error("missing argument for command {command}")]
    MissingArg { command: String },
}

pub type ChordSpec = (Modifiers, Key);

pub fn parse_chord(s: &str) -> Result<ChordSpec, KeyParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(KeyParseError::Empty);
    }
    // A literal `+` key. `+` is the modifier separator, so the key token "+"
    // alone (or after modifiers, e.g. "Ctrl++") would otherwise split into empty
    // tokens and fail. Peel a trailing "+" off as the key and parse the prefix
    // as modifiers.
    if s == "+" {
        return Ok((Modifiers::empty(), parse_named_key("+")?));
    }
    if let Some(prefix) = s.strip_suffix("++") {
        let mut mods = Modifiers::empty();
        for m in prefix.split('+') {
            match Modifiers::alias_meta_as_alt(m.trim()) {
                Some(flag) => mods |= flag,
                None => return Err(KeyParseError::UnknownToken(m.to_string())),
            }
        }
        return Ok((mods, parse_named_key("+")?));
    }
    let mut mods = Modifiers::empty();
    let parts: Vec<&str> = s.split('+').collect();
    // invariant: split on a non-empty string always yields at least one element
    let (key_part, mod_parts) = parts.split_last().expect("split always yields >= 1 element");
    for m in mod_parts {
        match Modifiers::alias_meta_as_alt(m.trim()) {
            Some(flag) => mods |= flag,
            None => return Err(KeyParseError::UnknownToken((*m).to_string())),
        }
    }
    let key = parse_named_key(key_part.trim())?;
    Ok((mods, key))
}

pub fn parse_chord_seq(s: &str) -> Result<Vec<ChordSpec>, KeyParseError> {
    parse_chord_seq_impl(s, None)
}

/// Like [`parse_chord_seq`] but also recognizes the bare word `prefix`
/// (ASCII-case-insensitive) as a chord alias for the supplied `prefix` chord.
/// The token is valid at any position in the sequence.
pub fn parse_chord_seq_with_prefix(
    s: &str,
    prefix: ChordSpec,
) -> Result<Vec<ChordSpec>, KeyParseError> {
    parse_chord_seq_impl(s, Some(prefix))
}

fn parse_chord_seq_impl(
    s: &str,
    prefix: Option<ChordSpec>,
) -> Result<Vec<ChordSpec>, KeyParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(KeyParseError::Empty);
    }
    s.split_whitespace()
        .map(|tok| {
            if let Some(p) = prefix
                && tok.eq_ignore_ascii_case("prefix")
            {
                Ok(p)
            } else {
                parse_chord(tok)
            }
        })
        .collect()
}

fn parse_named_key(s: &str) -> Result<Key, KeyParseError> {
    // F1..F12 (case-insensitive prefix)
    if let Some(rest) = s.strip_prefix(['F', 'f'])
        && let Ok(n) = rest.parse::<u8>()
    {
        if (1..=12).contains(&n) {
            return Ok(Key::Function(n));
        }
        // Out-of-range function key, so fall through to the error below.
        return Err(KeyParseError::UnknownToken(s.to_string()));
    }
    let normalized = s.to_lowercase();
    let key = match normalized.as_str() {
        "right" => Key::Arrow(Direction::Right),
        "left" => Key::Arrow(Direction::Left),
        "up" => Key::Arrow(Direction::Up),
        "down" => Key::Arrow(Direction::Down),
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" | "pgdown" => Key::PageDown,
        "insert" | "ins" => Key::Insert,
        "delete" | "del" => Key::Delete,
        "tab" => Key::Tab,
        "enter" | "return" => Key::Enter,
        "backspace" | "bs" => Key::Backspace,
        "escape" | "esc" => Key::Escape,
        "space" => Key::Char(' '),
        _ => {
            // Single Unicode scalar.
            if s.chars().count() == 1 {
                // invariant: count == 1 guarantees next() returns Some
                let c = s.chars().next().expect("count is 1");
                return Ok(Key::Char(c));
            }
            return Err(KeyParseError::UnknownToken(s.to_string()));
        }
    };
    Ok(key)
}

pub fn parse_command(s: &str) -> Result<Command, KeyParseError> {
    let s = s.trim();
    let mut parts = s.splitn(2, ':');
    // invariant: splitn(2, …) on any string always yields >= 1 element
    let name = parts.next().expect("splitn always yields >= 1").trim();
    let arg = parts.next().map(str::trim);
    let command = match name {
        "new_window" => Command::NewWindow,
        "split_v" => Command::SplitV,
        "split_h" => Command::SplitH,
        "kill_pane" => Command::KillPane,
        "kill_window" => Command::KillWindow,
        "zoom_toggle" => Command::ZoomToggle,
        "next_window" => Command::NextWindow,
        "prev_window" => Command::PrevWindow,
        "detach" => Command::Detach,
        "cancel" => Command::Cancel,
        "enter_copy_mode" => Command::EnterCopyMode,
        "toggle_sync_panes" => Command::ToggleSyncPanes,
        "reload_config" => Command::ReloadConfig,
        "select_next_pane" => Command::SelectNextPane,
        "select_prev_pane" => Command::SelectPrevPane,
        "select_pane_left" => Command::SelectPane(Direction::Left),
        "select_pane_right" => Command::SelectPane(Direction::Right),
        "select_pane_up" => Command::SelectPane(Direction::Up),
        "select_pane_down" => Command::SelectPane(Direction::Down),
        "resize_pane_left" => Command::ResizePane(Direction::Left),
        "resize_pane_right" => Command::ResizePane(Direction::Right),
        "resize_pane_up" => Command::ResizePane(Direction::Up),
        "resize_pane_down" => Command::ResizePane(Direction::Down),
        "select_last_window" => Command::SelectLastWindow,
        "select_last_pane" => Command::SelectLastPane,
        "rename_window" => Command::RenameWindow,
        "rename_pane" => Command::RenamePane,
        "show_help" => Command::ShowHelp,
        "command_prompt" => Command::CommandPrompt,
        "choose_session" => Command::ChooseSession,
        "choose_tree" => Command::ChooseTree,
        "mark_pane" => Command::MarkPane,
        "break_pane" => Command::BreakPane,
        "swap_pane_next" => Command::SwapPane(true),
        "swap_pane_prev" => Command::SwapPane(false),
        "join_pane" => Command::JoinPane(SplitDir::Vertical),
        "swap_marked_pane" => Command::SwapMarkedPane,
        "paste_buffer" => Command::PasteBuffer,
        "choose_buffer" => Command::ChooseBuffer,
        "toggle_monitor_activity" => Command::ToggleMonitorActivity,
        "toggle_monitor_bell" => Command::ToggleMonitorBell,
        "toggle_monitor_command" => Command::ToggleMonitorCommand,
        "set_monitor_silence" => {
            // `set_monitor_silence:30` arms a 30s threshold; no arg or `:0`
            // disables it.
            let secs = match arg.filter(|a| !a.is_empty()) {
                Some(a) => {
                    let n: u64 = a.parse().map_err(|_| KeyParseError::BadArg {
                        command: name.to_string(),
                        arg: a.to_string(),
                    })?;
                    (n > 0).then_some(n)
                }
                None => None,
            };
            Command::SetMonitorSilence(secs)
        }
        "popup" => Command::OpenPopup {
            command: arg.filter(|a| !a.is_empty()).map(str::to_string),
        },
        "close_popup" => Command::ClosePopup,
        "next_layout" => Command::NextLayout,
        "prev_prompt" => Command::PrevPrompt,
        "next_prompt" => Command::NextPrompt,
        "copy_output" => Command::CopyOutput,
        "layout" => {
            let arg_str = arg.filter(|a| !a.is_empty()).ok_or_else(|| {
                KeyParseError::MissingArg { command: name.to_string() }
            })?;
            let preset = plexy_glass_mux::LayoutPreset::parse(arg_str).ok_or_else(|| {
                KeyParseError::BadArg {
                    command: name.to_string(),
                    arg: arg_str.to_string(),
                }
            })?;
            Command::SelectLayout(preset)
        }
        "select_window" => {
            let arg_str = arg.ok_or_else(|| KeyParseError::MissingArg {
                command: name.to_string(),
            })?;
            let n: u8 = arg_str.parse().map_err(|_| KeyParseError::BadArg {
                command: name.to_string(),
                arg: arg_str.to_string(),
            })?;
            Command::SelectWindow(n)
        }
        other => return Err(KeyParseError::UnknownCommand(other.to_string())),
    };
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_mux::{Direction, Key, Modifiers};

    #[test]
    fn parses_bare_letter() {
        assert_eq!(
            parse_chord("a").unwrap(),
            (Modifiers::empty(), Key::Char('a'))
        );
    }

    #[test]
    fn parses_ctrl_plus_letter() {
        assert_eq!(
            parse_chord("Ctrl+a").unwrap(),
            (Modifiers::CTRL, Key::Char('a'))
        );
    }

    #[test]
    fn parses_multi_modifier() {
        let (mods, key) = parse_chord("Ctrl+Shift+Right").unwrap();
        assert_eq!(mods, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(key, Key::Arrow(Direction::Right));
    }

    #[test]
    fn parses_literal_plus_key() {
        // `+` is the modifier separator; binding it as a key must still work.
        assert_eq!(parse_chord("+").unwrap(), (Modifiers::empty(), Key::Char('+')));
        assert_eq!(parse_chord("Ctrl++").unwrap(), (Modifiers::CTRL, Key::Char('+')));
    }

    #[test]
    fn parses_meta_as_alt() {
        assert_eq!(
            parse_chord("Meta+a").unwrap(),
            (Modifiers::ALT, Key::Char('a'))
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(
            parse_chord("F1").unwrap(),
            (Modifiers::empty(), Key::Function(1))
        );
        assert_eq!(
            parse_chord("F12").unwrap(),
            (Modifiers::empty(), Key::Function(12))
        );
        assert!(parse_chord("F13").is_err());
    }

    #[test]
    fn unknown_modifier_errors() {
        assert!(parse_chord("Hyper2+a").is_err());
    }

    #[test]
    fn unknown_key_errors() {
        assert!(parse_chord("Wat").is_err());
    }

    #[test]
    fn chord_seq_parses_multiple() {
        let v = parse_chord_seq("Ctrl+a c").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], (Modifiers::CTRL, Key::Char('a')));
        assert_eq!(v[1], (Modifiers::empty(), Key::Char('c')));
    }

    #[test]
    fn command_no_arg() {
        let c = parse_command("new_window").unwrap();
        assert_eq!(c, Command::NewWindow);
    }

    #[test]
    fn command_with_arg() {
        let c = parse_command("select_window:0").unwrap();
        assert_eq!(c, Command::SelectWindow(0));
    }

    #[test]
    fn parses_resize_pane_commands() {
        assert_eq!(parse_command("resize_pane_right").unwrap(), Command::ResizePane(Direction::Right));
        assert_eq!(parse_command("resize_pane_up").unwrap(), Command::ResizePane(Direction::Up));
    }

    #[test]
    fn parses_last_window_pane_commands() {
        assert_eq!(parse_command("select_last_window").unwrap(), Command::SelectLastWindow);
        assert_eq!(parse_command("select_last_pane").unwrap(), Command::SelectLastPane);
    }

    #[test]
    fn parses_overlay_commands() {
        assert_eq!(parse_command("rename_window").unwrap(), Command::RenameWindow);
        assert_eq!(parse_command("rename_pane").unwrap(), Command::RenamePane);
        assert_eq!(parse_command("show_help").unwrap(), Command::ShowHelp);
    }

    #[test]
    fn parses_overlay_chords() {
        // The default bindings use comma / period / question chords.
        assert_eq!(parse_chord(",").unwrap(), (Modifiers::empty(), Key::Char(',')));
        assert_eq!(parse_chord(".").unwrap(), (Modifiers::empty(), Key::Char('.')));
        assert_eq!(parse_chord("?").unwrap(), (Modifiers::empty(), Key::Char('?')));
    }

    #[test]
    fn command_missing_arg_errors() {
        assert!(parse_command("select_window").is_err());
    }

    #[test]
    fn command_bad_arg_errors() {
        assert!(parse_command("select_window:abc").is_err());
    }

    #[test]
    fn unknown_command_errors() {
        assert!(parse_command("frobnicate").is_err());
    }

    #[test]
    fn parses_enter_copy_mode_command() {
        let c = parse_command("enter_copy_mode").unwrap();
        assert_eq!(c, Command::EnterCopyMode);
    }

    #[test]
    fn parses_toggle_sync_panes_command() {
        let c = parse_command("toggle_sync_panes").unwrap();
        assert_eq!(c, Command::ToggleSyncPanes);
    }

    #[test]
    fn parses_reload_config_command() {
        let c = parse_command("reload_config").unwrap();
        assert_eq!(c, Command::ReloadConfig);
    }

    #[test]
    fn parses_popup_bare() {
        let c = parse_command("popup").unwrap();
        assert_eq!(c, Command::OpenPopup { command: None });
    }

    #[test]
    fn parses_popup_with_command_preserving_spaces_and_colons() {
        let c = parse_command("popup:git log --oneline").unwrap();
        assert_eq!(
            c,
            Command::OpenPopup { command: Some("git log --oneline".into()) }
        );
        // splitn(2, ':') keeps later colons intact.
        let c = parse_command("popup:rg foo:bar").unwrap();
        assert_eq!(c, Command::OpenPopup { command: Some("rg foo:bar".into()) });
    }

    #[test]
    fn parses_popup_empty_arg_as_bare() {
        let c = parse_command("popup:").unwrap();
        assert_eq!(c, Command::OpenPopup { command: None });
    }

    #[test]
    fn parses_close_popup() {
        let c = parse_command("close_popup").unwrap();
        assert_eq!(c, Command::ClosePopup);
    }

    #[test]
    fn parses_layout_with_name() {
        use plexy_glass_mux::LayoutPreset;
        let c = parse_command("layout:tiled").unwrap();
        assert_eq!(c, Command::SelectLayout(LayoutPreset::Tiled));
        let c = parse_command("layout:even-horizontal").unwrap();
        assert_eq!(c, Command::SelectLayout(LayoutPreset::EvenHorizontal));
    }

    #[test]
    fn layout_requires_a_valid_name() {
        assert!(matches!(
            parse_command("layout"),
            Err(KeyParseError::MissingArg { .. })
        ));
        assert!(matches!(
            parse_command("layout:bogus"),
            Err(KeyParseError::BadArg { .. })
        ));
    }

    #[test]
    fn parses_next_layout() {
        let c = parse_command("next_layout").unwrap();
        assert_eq!(c, Command::NextLayout);
    }

    #[test]
    fn parses_block_scroll_verbs() {
        assert_eq!(parse_command("prev_prompt").unwrap(), Command::PrevPrompt);
        assert_eq!(parse_command("next_prompt").unwrap(), Command::NextPrompt);
        assert_eq!(parse_command("copy_output").unwrap(), Command::CopyOutput);
    }

    #[test]
    fn parses_monitor_verbs() {
        // toggle_monitor_command: bare verb, no arg.
        assert_eq!(
            parse_command("toggle_monitor_command").unwrap(),
            Command::ToggleMonitorCommand,
        );
        // set_monitor_silence: no arg → None (disable).
        assert_eq!(
            parse_command("set_monitor_silence").unwrap(),
            Command::SetMonitorSilence(None),
        );
        // set_monitor_silence:0 → None (zero also disables).
        assert_eq!(
            parse_command("set_monitor_silence:0").unwrap(),
            Command::SetMonitorSilence(None),
        );
        // set_monitor_silence:30 → Some(30).
        assert_eq!(
            parse_command("set_monitor_silence:30").unwrap(),
            Command::SetMonitorSilence(Some(30)),
        );
        // set_monitor_silence with a non-numeric arg is an error.
        assert!(parse_command("set_monitor_silence:abc").is_err());
    }

    // ── prefix token tests ──────────────────────────────────────────

    fn ctrl_a() -> ChordSpec {
        (Modifiers::CTRL, Key::Char('a'))
    }

    fn ctrl_b() -> ChordSpec {
        (Modifiers::CTRL, Key::Char('b'))
    }

    #[test]
    fn prefix_token_resolves_in_sequences() {
        // "prefix c" with default Ctrl+a prefix == "Ctrl+a c"
        assert_eq!(
            parse_chord_seq_with_prefix("prefix c", ctrl_a()).unwrap(),
            parse_chord_seq("Ctrl+a c").unwrap()
        );

        // Custom prefix: Ctrl+b substitutes
        assert_eq!(
            parse_chord_seq_with_prefix("prefix c", ctrl_b()).unwrap(),
            vec![ctrl_b(), (Modifiers::empty(), Key::Char('c'))]
        );

        // Token at second position: "Ctrl+x prefix"
        let ctrl_x = (Modifiers::CTRL, Key::Char('x'));
        assert_eq!(
            parse_chord_seq_with_prefix("Ctrl+x prefix", ctrl_a()).unwrap(),
            vec![ctrl_x, ctrl_a()]
        );

        // "prefix prefix" resolves to two prefix chords
        assert_eq!(
            parse_chord_seq_with_prefix("prefix prefix", ctrl_a()).unwrap(),
            vec![ctrl_a(), ctrl_a()]
        );
    }

    #[test]
    fn prefix_token_case_insensitive() {
        let expected = vec![ctrl_a(), (Modifiers::empty(), Key::Char('c'))];
        assert_eq!(
            parse_chord_seq_with_prefix("Prefix c", ctrl_a()).unwrap(),
            expected
        );
        assert_eq!(
            parse_chord_seq_with_prefix("PREFIX c", ctrl_a()).unwrap(),
            expected
        );
    }

    #[test]
    fn absolute_sequences_unchanged() {
        // A representative set of sequences parses byte-identically through
        // both entry points (no prefix token in the string).
        let cases = ["Ctrl+a c", "Alt+Left", "Ctrl+a Ctrl+a", "Ctrl+b x"];
        for s in cases {
            assert_eq!(
                parse_chord_seq_with_prefix(s, ctrl_a()).unwrap(),
                parse_chord_seq(s).unwrap(),
                "mismatch for {s:?}"
            );
        }
    }

    #[test]
    fn plain_parse_chord_seq_rejects_the_token() {
        // parse_chord_seq (no prefix) must NOT silently resolve "prefix", since
        // it is not a valid chord name in the prefix-less path.
        let err = parse_chord_seq("prefix c").unwrap_err();
        assert!(
            matches!(err, KeyParseError::UnknownToken(ref t) if t == "prefix"),
            "expected UnknownToken(\"prefix\"), got {err:?}"
        );
    }
}
