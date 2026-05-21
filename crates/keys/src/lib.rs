//! VT/xterm/SS3/kitty key-event parser, key/command spec parsers, and a
//! legacy re-encoder.

mod build;
mod encode;
mod parser;
mod spec;

pub use build::build_keymap;
pub use encode::legacy_bytes;
pub use parser::{KeyParseOutput, KeyParser};
pub use spec::{
    parse_chord, parse_chord_seq, parse_command, ChordSpec, CommandSpec, KeyParseError,
};
