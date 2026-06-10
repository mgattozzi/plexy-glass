//! VT/xterm/SS3/kitty key-event parser, key/command spec parsers, and a
//! legacy re-encoder.

mod build;
mod encode;
mod parser;
mod paste;
mod spec;

pub use build::build_keymap;
pub use encode::{encode, legacy_bytes, KeyboardTarget};
pub use parser::{KeyboardProtocol, KeyParseOutput, KeyParser};
pub use paste::{PasteParseOutput, PasteParser};
pub use spec::{
    parse_chord, parse_chord_seq, parse_chord_seq_with_prefix, parse_command, ChordSpec,
    CommandSpec, KeyParseError,
};
