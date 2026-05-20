//! Parsers for the `keys` and `command` strings in `[[keymap.bindings]]`.

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

pub type ChordSpec = (plexy_glass_mux::Modifiers, plexy_glass_mux::Key);

#[derive(Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub command: plexy_glass_mux::Command,
}

pub fn parse_chord(_s: &str) -> Result<ChordSpec, KeyParseError> {
    // Placeholder: real impl in Task 7.
    Err(KeyParseError::Empty)
}

pub fn parse_chord_seq(_s: &str) -> Result<Vec<ChordSpec>, KeyParseError> {
    Err(KeyParseError::Empty)
}

pub fn parse_command(_s: &str) -> Result<CommandSpec, KeyParseError> {
    Err(KeyParseError::Empty)
}
