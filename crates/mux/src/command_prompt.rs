//! Pure parser for the command prompt (`Ctrl+a :`).
//!
//! Translates a typed line into a `PromptCommand`. No daemon/session
//! dependencies, so the daemon maps `PromptCommand` onto effects (see
//! `Session::handle_prompt_command` and the connection-level `switch_session`).
//! Also provides verb-name completion used by the command overlay.

use crate::{Direction, SplitDir};

/// Which pane `focus` targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    Dir(Direction),
    Next,
    Prev,
    Last,
}

/// Which neighbor a same-window `swap` targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapTarget {
    Prev,
    Next,
}

/// A parsed, validated command-prompt command.
///
/// Arg-carrying variants hold their already-bounds-checked arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptCommand {
    NewWindow,
    NextWindow,
    PrevWindow,
    SelectWindow(u8),
    LastWindow,
    SplitH,
    SplitV,
    Zoom,
    KillPane,
    KillWindow,
    Focus(FocusTarget),
    Resize(Direction, u16),
    RenameWindow(String),
    RenamePane(String),
    CopyMode,
    ToggleSync,
    Reload,
    Detach,
    Help,
    Switch(String),
    ChooseSession,
    ChooseTree,
    MarkPane,
    BreakPane,
    JoinPane(SplitDir),
    SwapPane(SwapTarget),
    SwapMarked,
    /// Paste a paste buffer: the named one, or the newest (`None`).
    PasteBuffer(Option<String>),
    ChooseBuffer,
    /// Push literal text as a new paste buffer.
    SetBuffer { text: String },
    /// Write a buffer (named, or the newest with `None`) to a file.
    SaveBuffer { name: Option<String>, path: String },
    /// Read a file into a new paste buffer.
    LoadBuffer { path: String },
    ToggleMonitorActivity,
    ToggleMonitorBell,
    ToggleMonitorCommand,
    /// Open a floating popup running the given command line (`None` = scratch shell).
    Popup(Option<String>),
    /// Close the floating popup.
    ClosePopup,
    /// Stream the target pane's raw output to a command (`None` = stop the pipe).
    PipePane(Option<String>),
    /// Rearrange the active window's panes into a preset layout.
    Layout(crate::LayoutPreset),
    /// Scroll the viewport back to the previous OSC 133 prompt.
    PrevPrompt,
    /// Scroll the viewport forward to the next OSC 133 prompt (or live).
    NextPrompt,
    /// Copy the last completed command block's output.
    CopyOutput,
}

/// A human-readable parse failure.
///
/// `Display` is verbatim the transient status-line text shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

/// Static verb names, sorted, for Tab-completion of the first token.
pub const VERBS: &[&str] = &[
    "break", "buffers", "close-popup", "copy", "copy-output", "detach", "focus",
    "help", "join", "kill", "last", "layout", "load-buffer", "mark",
    "monitor-activity", "monitor-bell", "monitor-command", "new", "next", "next-prompt", "paste",
    "pipe-pane", "popup", "prev", "prev-prompt", "reload", "rename", "rename-pane", "resize",
    "save-buffer", "sessions", "set-buffer", "split", "swap", "switch", "sync",
    "tree", "win", "zoom",
];

fn err(msg: impl Into<String>) -> ParseError {
    ParseError(msg.into())
}

/// Whether `s` matches the machine-generated paste-buffer name shape
/// (`^buffer[0-9]+$`), the state-independent test `save-buffer` uses to
/// split a leading buffer name from the path.
fn is_buffer_name(s: &str) -> bool {
    s.strip_prefix("buffer")
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

fn dir_from_letter(s: &str) -> Option<Direction> {
    match s {
        "l" => Some(Direction::Left),
        "r" => Some(Direction::Right),
        "u" => Some(Direction::Up),
        "d" => Some(Direction::Down),
        _ => None,
    }
}

/// Parse a committed command line.
///
/// The caller guarantees the line is non-empty after trimming (the overlay
/// maps empty-Enter to Cancel).
pub fn parse(line: &str) -> Result<PromptCommand, ParseError> {
    let line = line.trim();
    let mut it = line.split_whitespace();
    let verb = it.next().ok_or_else(|| err("empty command"))?;
    let args: Vec<&str> = it.collect();
    // Remainder after the verb, internal spaces preserved (for rename/switch).
    let rest = line
        .split_once(char::is_whitespace)
        .map(|(_, r)| r.trim())
        .unwrap_or("");

    let no_args = |cmd: PromptCommand| {
        if args.is_empty() {
            Ok(cmd)
        } else {
            Err(ParseError(format!("{verb}: takes no arguments")))
        }
    };

    match verb {
        "new" => no_args(PromptCommand::NewWindow),
        "next" => no_args(PromptCommand::NextWindow),
        "prev" => no_args(PromptCommand::PrevWindow),
        "last" => no_args(PromptCommand::LastWindow),
        "zoom" => no_args(PromptCommand::Zoom),
        "copy" => no_args(PromptCommand::CopyMode),
        "sync" => no_args(PromptCommand::ToggleSync),
        "reload" => no_args(PromptCommand::Reload),
        "detach" => no_args(PromptCommand::Detach),
        "help" => no_args(PromptCommand::Help),
        "sessions" => no_args(PromptCommand::ChooseSession),
        "tree" => no_args(PromptCommand::ChooseTree),
        "mark" => no_args(PromptCommand::MarkPane),
        "break" => no_args(PromptCommand::BreakPane),
        "paste" => match args.as_slice() {
            [] => Ok(PromptCommand::PasteBuffer(None)),
            [name] => Ok(PromptCommand::PasteBuffer(Some((*name).to_string()))),
            _ => Err(err("paste: expected a buffer name or no argument")),
        },
        "buffers" => no_args(PromptCommand::ChooseBuffer),
        "set-buffer" => {
            if rest.is_empty() {
                Err(err("set-buffer: expected text"))
            } else {
                Ok(PromptCommand::SetBuffer { text: rest.to_string() })
            }
        }
        "save-buffer" => {
            // Shape-based split: a first token matching the machine-generated
            // buffer-name shape (`bufferN`) names the source buffer and the
            // rest is the path; otherwise the WHOLE tail is the path (newest
            // buffer). State-independent: a path whose first word is
            // literally `bufferN ` is pathological and not supported.
            match rest.split_once(char::is_whitespace) {
                Some((first, tail)) if is_buffer_name(first) => Ok(PromptCommand::SaveBuffer {
                    name: Some(first.to_string()),
                    path: tail.trim_start().to_string(),
                }),
                Some(_) => Ok(PromptCommand::SaveBuffer { name: None, path: rest.to_string() }),
                // A lone `bufferN` is a missing path; a lone anything else is
                // the path. Empty rest is a missing path either way.
                None if !rest.is_empty() && !is_buffer_name(rest) => {
                    Ok(PromptCommand::SaveBuffer { name: None, path: rest.to_string() })
                }
                None => Err(err("save-buffer: expected a path")),
            }
        }
        "load-buffer" => {
            if rest.is_empty() {
                Err(err("load-buffer: expected a path"))
            } else {
                Ok(PromptCommand::LoadBuffer { path: rest.to_string() })
            }
        }
        "monitor-activity" => no_args(PromptCommand::ToggleMonitorActivity),
        "monitor-bell" => no_args(PromptCommand::ToggleMonitorBell),
        "monitor-command" => no_args(PromptCommand::ToggleMonitorCommand),
        "prev-prompt" => no_args(PromptCommand::PrevPrompt),
        "next-prompt" => no_args(PromptCommand::NextPrompt),
        "copy-output" => no_args(PromptCommand::CopyOutput),
        "join" | "join-pane" => match args.as_slice() {
            [] | ["v"] => Ok(PromptCommand::JoinPane(SplitDir::Vertical)),
            ["h"] => Ok(PromptCommand::JoinPane(SplitDir::Horizontal)),
            _ => Err(err("join: expected h or v")),
        },
        "swap" | "swap-pane" => match args.as_slice() {
            [] => Ok(PromptCommand::SwapMarked),
            ["next"] => Ok(PromptCommand::SwapPane(SwapTarget::Next)),
            ["prev"] => Ok(PromptCommand::SwapPane(SwapTarget::Prev)),
            _ => Err(err("swap: expected prev, next, or no argument")),
        },
        "win" => {
            let [n] = args.as_slice() else {
                return Err(err("win: expected a window number"));
            };
            let n: u32 = n.parse().map_err(|_| err("win: expected a window number"))?;
            if n == 0 {
                return Err(err("win: window numbers start at 1"));
            }
            if n > 256 {
                return Err(err("win: no such window"));
            }
            Ok(PromptCommand::SelectWindow((n - 1) as u8))
        }
        "split" => match args.as_slice() {
            ["h"] => Ok(PromptCommand::SplitH),
            ["v"] => Ok(PromptCommand::SplitV),
            _ => Err(err("split: expected h or v")),
        },
        "kill" => match args.as_slice() {
            [] => Ok(PromptCommand::KillPane),
            ["win"] | ["window"] => Ok(PromptCommand::KillWindow),
            _ => Err(err("kill: expected nothing or 'win'")),
        },
        "focus" => {
            let [t] = args.as_slice() else {
                return Err(err("focus: expected l/r/u/d/next/prev/last"));
            };
            let ft = match *t {
                "l" => FocusTarget::Dir(Direction::Left),
                "r" => FocusTarget::Dir(Direction::Right),
                "u" => FocusTarget::Dir(Direction::Up),
                "d" => FocusTarget::Dir(Direction::Down),
                "next" => FocusTarget::Next,
                "prev" => FocusTarget::Prev,
                "last" => FocusTarget::Last,
                _ => return Err(err("focus: expected l/r/u/d/next/prev/last")),
            };
            Ok(PromptCommand::Focus(ft))
        }
        "resize" => {
            let (dir_s, n) = match args.as_slice() {
                [d] => (*d, 1u16),
                [d, n] => {
                    let n: u16 = n.parse().map_err(|_| err("resize: count must be a number"))?;
                    (*d, n)
                }
                _ => return Err(err("resize: expected a direction l/r/u/d")),
            };
            let dir =
                dir_from_letter(dir_s).ok_or_else(|| err("resize: expected a direction l/r/u/d"))?;
            if n == 0 {
                return Err(err("resize: count must be >= 1"));
            }
            Ok(PromptCommand::Resize(dir, n))
        }
        "rename" => {
            if rest.is_empty() {
                Err(err("rename: expected a name"))
            } else {
                Ok(PromptCommand::RenameWindow(rest.to_string()))
            }
        }
        "rename-pane" => {
            if rest.is_empty() {
                Err(err("rename-pane: expected a name"))
            } else {
                Ok(PromptCommand::RenamePane(rest.to_string()))
            }
        }
        "switch" => {
            if rest.is_empty() {
                Err(err("switch: expected a session name"))
            } else {
                Ok(PromptCommand::Switch(rest.to_string()))
            }
        }
        "popup" => Ok(PromptCommand::Popup(if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        })),
        // Popup-style free-text tail: the rest of the line is the consumer
        // command verbatim; no tail means "stop the pipe".
        "pipe-pane" => Ok(PromptCommand::PipePane(if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        })),
        "close-popup" => no_args(PromptCommand::ClosePopup),
        "layout" => {
            const NAMES: &str =
                "even-horizontal, even-vertical, main-horizontal, main-vertical, tiled";
            if rest.is_empty() {
                return Err(err(format!("layout: expected one of {NAMES}")));
            }
            match crate::LayoutPreset::parse(rest) {
                Some(p) => Ok(PromptCommand::Layout(p)),
                None => Err(err(format!("layout: unknown layout `{rest}`; expected one of {NAMES}"))),
            }
        }
        other => Err(err(format!("unknown command: {other}"))),
    }
}

/// Result of completing a token against a candidate set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// No candidate matches; leave the input unchanged.
    None,
    /// Exactly one match; replace the token with this (caller may append a space).
    Unique(String),
    /// Several matches share this prefix, longer than the input token.
    Partial(String),
}

/// Complete `prefix` against `candidates`: a unique match, the longest common
/// prefix of several matches (when it makes progress), or `None`.
pub fn complete(prefix: &str, candidates: &[&str]) -> Completion {
    let matches: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|c| c.starts_with(prefix))
        .collect();
    match matches.as_slice() {
        [] => Completion::None,
        [only] => Completion::Unique((*only).to_string()),
        many => {
            let lcp = longest_common_prefix(many);
            if lcp.len() > prefix.len() {
                Completion::Partial(lcp)
            } else {
                Completion::None
            }
        }
    }
}

fn longest_common_prefix(items: &[&str]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut end = first.len();
    for s in &items[1..] {
        let common = first
            .bytes()
            .zip(s.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        end = end.min(common);
    }
    while end > 0 && !first.is_char_boundary(end) {
        end -= 1;
    }
    first[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: &str) -> Result<PromptCommand, ParseError> {
        parse(line)
    }

    #[test]
    fn no_arg_verbs() {
        assert_eq!(p("new").unwrap(), PromptCommand::NewWindow);
        assert_eq!(p("next").unwrap(), PromptCommand::NextWindow);
        assert_eq!(p("prev").unwrap(), PromptCommand::PrevWindow);
        assert_eq!(p("last").unwrap(), PromptCommand::LastWindow);
        assert_eq!(p("zoom").unwrap(), PromptCommand::Zoom);
        assert_eq!(p("copy").unwrap(), PromptCommand::CopyMode);
        assert_eq!(p("sync").unwrap(), PromptCommand::ToggleSync);
        assert_eq!(p("reload").unwrap(), PromptCommand::Reload);
        assert_eq!(p("detach").unwrap(), PromptCommand::Detach);
        assert_eq!(p("help").unwrap(), PromptCommand::Help);
    }

    #[test]
    fn no_arg_verb_rejects_extra() {
        assert_eq!(p("zoom x").unwrap_err().to_string(), "zoom: takes no arguments");
    }

    #[test]
    fn leading_and_trailing_whitespace_ignored() {
        assert_eq!(p("   zoom   ").unwrap(), PromptCommand::Zoom);
    }

    #[test]
    fn win_one_based_to_zero_based() {
        assert_eq!(p("win 1").unwrap(), PromptCommand::SelectWindow(0));
        assert_eq!(p("win 9").unwrap(), PromptCommand::SelectWindow(8));
        assert_eq!(p("win 256").unwrap(), PromptCommand::SelectWindow(255));
    }

    #[test]
    fn win_errors() {
        assert_eq!(p("win").unwrap_err().to_string(), "win: expected a window number");
        assert_eq!(p("win x").unwrap_err().to_string(), "win: expected a window number");
        assert_eq!(p("win 0").unwrap_err().to_string(), "win: window numbers start at 1");
        assert_eq!(p("win 999").unwrap_err().to_string(), "win: no such window");
        assert_eq!(p("win 1 2").unwrap_err().to_string(), "win: expected a window number");
    }

    #[test]
    fn split_variants() {
        assert_eq!(p("split h").unwrap(), PromptCommand::SplitH);
        assert_eq!(p("split v").unwrap(), PromptCommand::SplitV);
        assert_eq!(p("split").unwrap_err().to_string(), "split: expected h or v");
        assert_eq!(p("split z").unwrap_err().to_string(), "split: expected h or v");
    }

    #[test]
    fn kill_variants() {
        assert_eq!(p("kill").unwrap(), PromptCommand::KillPane);
        assert_eq!(p("kill win").unwrap(), PromptCommand::KillWindow);
        assert_eq!(p("kill window").unwrap(), PromptCommand::KillWindow);
        assert_eq!(p("kill foo").unwrap_err().to_string(), "kill: expected nothing or 'win'");
    }

    #[test]
    fn focus_targets() {
        assert_eq!(p("focus l").unwrap(), PromptCommand::Focus(FocusTarget::Dir(Direction::Left)));
        assert_eq!(p("focus r").unwrap(), PromptCommand::Focus(FocusTarget::Dir(Direction::Right)));
        assert_eq!(p("focus u").unwrap(), PromptCommand::Focus(FocusTarget::Dir(Direction::Up)));
        assert_eq!(p("focus d").unwrap(), PromptCommand::Focus(FocusTarget::Dir(Direction::Down)));
        assert_eq!(p("focus next").unwrap(), PromptCommand::Focus(FocusTarget::Next));
        assert_eq!(p("focus prev").unwrap(), PromptCommand::Focus(FocusTarget::Prev));
        assert_eq!(p("focus last").unwrap(), PromptCommand::Focus(FocusTarget::Last));
        assert_eq!(
            p("focus x").unwrap_err().to_string(),
            "focus: expected l/r/u/d/next/prev/last"
        );
    }

    #[test]
    fn resize_with_and_without_count() {
        assert_eq!(p("resize l").unwrap(), PromptCommand::Resize(Direction::Left, 1));
        assert_eq!(p("resize r 5").unwrap(), PromptCommand::Resize(Direction::Right, 5));
        assert_eq!(p("resize u 12").unwrap(), PromptCommand::Resize(Direction::Up, 12));
    }

    #[test]
    fn resize_errors() {
        assert_eq!(p("resize").unwrap_err().to_string(), "resize: expected a direction l/r/u/d");
        assert_eq!(p("resize x").unwrap_err().to_string(), "resize: expected a direction l/r/u/d");
        assert_eq!(p("resize l 0").unwrap_err().to_string(), "resize: count must be >= 1");
        assert_eq!(p("resize l z").unwrap_err().to_string(), "resize: count must be a number");
    }

    #[test]
    fn rename_preserves_internal_spaces() {
        assert_eq!(p("rename my build").unwrap(), PromptCommand::RenameWindow("my build".into()));
        assert_eq!(p("rename-pane left log").unwrap(), PromptCommand::RenamePane("left log".into()));
    }

    #[test]
    fn rename_requires_a_name() {
        assert_eq!(p("rename").unwrap_err().to_string(), "rename: expected a name");
        assert_eq!(p("rename   ").unwrap_err().to_string(), "rename: expected a name");
        assert_eq!(p("rename-pane").unwrap_err().to_string(), "rename-pane: expected a name");
    }

    #[test]
    fn switch_takes_the_remainder() {
        assert_eq!(p("switch work").unwrap(), PromptCommand::Switch("work".into()));
        assert_eq!(p("switch").unwrap_err().to_string(), "switch: expected a session name");
    }

    #[test]
    fn sessions_verb() {
        assert_eq!(p("sessions").unwrap(), PromptCommand::ChooseSession);
        assert_eq!(p("sessions x").unwrap_err().to_string(), "sessions: takes no arguments");
    }

    #[test]
    fn tree_verb() {
        assert_eq!(p("tree").unwrap(), PromptCommand::ChooseTree);
        assert_eq!(p("tree x").unwrap_err().to_string(), "tree: takes no arguments");
    }

    #[test]
    fn pane_mobility_verbs() {
        assert_eq!(p("mark").unwrap(), PromptCommand::MarkPane);
        assert_eq!(p("break").unwrap(), PromptCommand::BreakPane);
        assert_eq!(p("join").unwrap(), PromptCommand::JoinPane(SplitDir::Vertical));
        assert_eq!(p("join v").unwrap(), PromptCommand::JoinPane(SplitDir::Vertical));
        assert_eq!(p("join h").unwrap(), PromptCommand::JoinPane(SplitDir::Horizontal));
        assert_eq!(p("join-pane h").unwrap(), PromptCommand::JoinPane(SplitDir::Horizontal));
        assert!(p("join x").is_err());
        assert_eq!(p("swap").unwrap(), PromptCommand::SwapMarked);
        assert_eq!(p("swap next").unwrap(), PromptCommand::SwapPane(SwapTarget::Next));
        assert_eq!(p("swap prev").unwrap(), PromptCommand::SwapPane(SwapTarget::Prev));
        assert_eq!(p("swap-pane next").unwrap(), PromptCommand::SwapPane(SwapTarget::Next));
        assert!(p("swap sideways").is_err());
    }

    #[test]
    fn paste_buffer_verbs() {
        assert_eq!(p("paste").unwrap(), PromptCommand::PasteBuffer(None));
        assert_eq!(
            p("paste buffer2").unwrap(),
            PromptCommand::PasteBuffer(Some("buffer2".into()))
        );
        assert_eq!(p("buffers").unwrap(), PromptCommand::ChooseBuffer);
        assert_eq!(
            p("paste a b").unwrap_err().to_string(),
            "paste: expected a buffer name or no argument"
        );
        assert!(p("buffers x").is_err());
    }

    #[test]
    fn set_buffer_takes_rest_verbatim() {
        assert_eq!(
            p("set-buffer some literal   text").unwrap(),
            PromptCommand::SetBuffer { text: "some literal   text".into() }
        );
        assert_eq!(p("set-buffer").unwrap_err().to_string(), "set-buffer: expected text");
        assert_eq!(p("set-buffer   ").unwrap_err().to_string(), "set-buffer: expected text");
    }

    #[test]
    fn load_buffer_takes_rest_as_path() {
        assert_eq!(
            p("load-buffer /tmp/my snippet.txt").unwrap(),
            PromptCommand::LoadBuffer { path: "/tmp/my snippet.txt".into() }
        );
        assert_eq!(
            p("load-buffer").unwrap_err().to_string(),
            "load-buffer: expected a path"
        );
    }

    #[test]
    fn save_buffer_shape_based_first_token_split() {
        // No leading buffer name: the whole tail is the path (spaces preserved).
        assert_eq!(
            p("save-buffer /tmp/my yank.txt").unwrap(),
            PromptCommand::SaveBuffer { name: None, path: "/tmp/my yank.txt".into() }
        );
        // First token matches `^buffer[0-9]+$`: it names the buffer, the rest
        // is the path (extra separating spaces collapse, internal ones stay).
        assert_eq!(
            p("save-buffer buffer3 /tmp/old.txt").unwrap(),
            PromptCommand::SaveBuffer { name: Some("buffer3".into()), path: "/tmp/old.txt".into() }
        );
        assert_eq!(
            p("save-buffer buffer0   /tmp/with space.txt").unwrap(),
            PromptCommand::SaveBuffer {
                name: Some("buffer0".into()),
                path: "/tmp/with space.txt".into()
            }
        );
        // Shape, not state: `bufferX` has no digits, so the whole tail is a path.
        assert_eq!(
            p("save-buffer bufferX /tmp/old.txt").unwrap(),
            PromptCommand::SaveBuffer { name: None, path: "bufferX /tmp/old.txt".into() }
        );
        // Edge: a lone `bufferN` is a missing path, not a path.
        assert_eq!(
            p("save-buffer buffer3").unwrap_err().to_string(),
            "save-buffer: expected a path"
        );
        assert_eq!(p("save-buffer").unwrap_err().to_string(), "save-buffer: expected a path");
    }

    #[test]
    fn monitor_verbs() {
        assert_eq!(p("monitor-activity").unwrap(), PromptCommand::ToggleMonitorActivity);
        assert_eq!(p("monitor-bell").unwrap(), PromptCommand::ToggleMonitorBell);
        assert_eq!(p("monitor-command").unwrap(), PromptCommand::ToggleMonitorCommand);
        assert!(p("monitor-activity x").is_err());
        assert!(p("monitor-command x").is_err());
    }

    #[test]
    fn block_navigation_verbs() {
        assert_eq!(p("prev-prompt").unwrap(), PromptCommand::PrevPrompt);
        assert_eq!(p("next-prompt").unwrap(), PromptCommand::NextPrompt);
        assert_eq!(p("copy-output").unwrap(), PromptCommand::CopyOutput);
        assert_eq!(
            p("prev-prompt x").unwrap_err().to_string(),
            "prev-prompt: takes no arguments"
        );
        assert_eq!(
            p("copy-output x").unwrap_err().to_string(),
            "copy-output: takes no arguments"
        );
    }

    #[test]
    fn verbs_are_sorted() {
        let mut sorted = VERBS.to_vec();
        sorted.sort_unstable();
        assert_eq!(VERBS, sorted.as_slice(), "VERBS must stay sorted for completion");
    }

    #[test]
    fn parses_popup_verb() {
        assert_eq!(p("popup").unwrap(), PromptCommand::Popup(None));
        assert_eq!(p("popup lazygit").unwrap(), PromptCommand::Popup(Some("lazygit".into())));
        assert_eq!(
            p("popup git log --oneline").unwrap(),
            PromptCommand::Popup(Some("git log --oneline".into()))
        );
    }

    #[test]
    fn parses_pipe_pane_verb() {
        // No tail = stop the pipe.
        assert_eq!(p("pipe-pane").unwrap(), PromptCommand::PipePane(None));
        assert_eq!(p("pipe-pane   ").unwrap(), PromptCommand::PipePane(None));
        // The tail is the consumer command line.
        assert_eq!(
            p("pipe-pane tee /tmp/session.log").unwrap(),
            PromptCommand::PipePane(Some("tee /tmp/session.log".into()))
        );
        // Internal spaces are preserved verbatim (popup's free-text convention).
        assert_eq!(
            p("pipe-pane tee -a  my log.txt").unwrap(),
            PromptCommand::PipePane(Some("tee -a  my log.txt".into()))
        );
    }

    #[test]
    fn parses_close_popup_verb() {
        assert_eq!(p("close-popup").unwrap(), PromptCommand::ClosePopup);
        assert_eq!(
            p("close-popup x").unwrap_err().to_string(),
            "close-popup: takes no arguments"
        );
    }

    #[test]
    fn parses_layout_verb() {
        use crate::LayoutPreset;
        assert_eq!(p("layout tiled").unwrap(), PromptCommand::Layout(LayoutPreset::Tiled));
        assert_eq!(
            p("layout main-vertical").unwrap(),
            PromptCommand::Layout(LayoutPreset::MainVertical)
        );
    }

    #[test]
    fn layout_verb_errors_name_the_valid_set() {
        let msg = p("layout").unwrap_err().to_string();
        assert!(msg.contains("even-horizontal") && msg.contains("tiled"), "{msg}");
        let msg = p("layout bogus").unwrap_err().to_string();
        assert!(msg.contains("bogus") && msg.contains("tiled"), "{msg}");
    }

    #[test]
    fn unknown_verb() {
        assert_eq!(p("frobnicate").unwrap_err().to_string(), "unknown command: frobnicate");
    }

    #[test]
    fn complete_unique() {
        assert_eq!(complete("zo", VERBS), Completion::Unique("zoom".into()));
    }

    #[test]
    fn complete_partial_common_prefix() {
        // "rename" and "rename-pane" share "rename".
        assert_eq!(complete("ren", VERBS), Completion::Partial("rename".into()));
        // "next" and "new" share "ne".
        assert_eq!(complete("n", &["new", "next", "prev"]), Completion::Partial("ne".into()));
    }

    #[test]
    fn complete_none_when_no_match_or_no_progress() {
        assert_eq!(complete("zzz", VERBS), Completion::None);
        // Common prefix already fully typed, so no progress (no unique winner).
        assert_eq!(complete("rename", &["rename", "rename-pane"]), Completion::None);
    }

    #[test]
    fn complete_against_session_names() {
        let names = ["work", "worktree", "web"];
        assert_eq!(complete("we", &names), Completion::Unique("web".into()));
        assert_eq!(complete("wor", &names), Completion::Partial("work".into()));
        // All three share only "w", so replacing "w" with "w" is no progress.
        assert_eq!(complete("w", &names), Completion::None);
        assert_eq!(complete("x", &names), Completion::None);
    }
}
