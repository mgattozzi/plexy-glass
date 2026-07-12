//! VT/xterm/SS3/kitty key-event parser, key/command spec parsers, and a
//! legacy re-encoder.

mod build;
mod encode;
mod parser;
mod paste;
mod spec;

pub use build::{build_keymap, build_keymap_with_skips};
pub use encode::{KeyboardTarget, KittyFlags, ModifyOtherKeysLevel, encode};
pub use parser::{KeyParseOutput, KeyParser, KeyboardProtocol};
pub use paste::{PasteParseOutput, PasteParser};
pub use spec::{
    ChordSpec, KeyParseError, parse_chord, parse_chord_seq, parse_chord_seq_with_prefix,
    parse_command,
};
