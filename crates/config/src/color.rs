//! Color primitives shared across the config + render layers.
//!
//! `Rgb` used to live in `plexy-glass-status`, but that made hex a value the
//! render path re-parsed with `unwrap_or(fallback)` at every site and never
//! validated at load. It moved down here so the KDL decoder can parse hex into
//! `Rgb` ONCE at config decode (a bad hex is a loud `line:col` error, not a
//! silent render-time fallback); `status` re-exports `Rgb` so every existing
//! `plexy_glass_status::Rgb` user keeps compiling.

/// A 24-bit RGB color, parsed once from a `#rrggbb` hex literal at config decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Parse a `#rrggbb` hex literal. Returns `None` for a missing `#`, wrong
    /// length, non-ASCII, or non-hex digits.
    pub fn parse_hex(s: &str) -> Option<Self> {
        let s = s.strip_prefix('#')?;
        // Require 6 ASCII bytes before byte-slicing: a 6-byte string can hold a
        // multi-byte UTF-8 char straddling a slice boundary (e.g. "#aébc1"),
        // and slicing off a char boundary panics. ASCII-only makes the byte
        // indices safe and falls into the existing None path for bad input.
        if s.len() != 6 || !s.is_ascii() {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Self { r, g, b })
    }
}

/// A color SPEC as written in config: either a `#rrggbb` literal parsed to `Rgb`
/// at decode, or a palette role NAME resolved late at render time. Names are
/// user-definable and merge onto the built-in palette, so they can't be
/// validated at decode; a `#`-prefixed literal that doesn't parse IS a loud
/// decode error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColorSource {
    Literal(Rgb),
    Name(String),
}

/// A `#`-prefixed color spec that isn't a valid `#rrggbb` hex. The decoder wraps
/// this in a located `ConfigError`; on its own it just names the bad value.
#[derive(Debug, thiserror::Error)]
#[error("invalid hex color `{0}` (expected #rrggbb)")]
pub struct HexColorError(pub String);

impl ColorSource {
    /// Parse a color spec. A leading `#` means a hex literal that MUST parse
    /// (bad hex is an error); anything else is a palette role name resolved at
    /// render time (unknown roles fall through to the terminal default).
    pub fn parse(s: &str) -> Result<Self, HexColorError> {
        if let Some(rgb) = Rgb::parse_hex(s) {
            Ok(Self::Literal(rgb))
        } else if s.starts_with('#') {
            Err(HexColorError(s.to_string()))
        } else {
            Ok(Self::Name(s.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_valid() {
        assert_eq!(Rgb::parse_hex("#abcdef"), Some(Rgb { r: 0xab, g: 0xcd, b: 0xef }));
        assert_eq!(Rgb::parse_hex("#000000"), Some(Rgb { r: 0, g: 0, b: 0 }));
    }

    #[test]
    fn parse_hex_bad_is_none() {
        assert_eq!(Rgb::parse_hex("accent"), None); // no `#`
        assert_eq!(Rgb::parse_hex("#zzz"), None); // wrong length + non-hex
        assert_eq!(Rgb::parse_hex("#gggggg"), None); // non-hex digits
    }

    #[test]
    fn parse_hex_multibyte_char_is_none_not_panic() {
        // "#aébc1": the body is exactly 6 bytes but `é` is 2 bytes straddling the
        // &s[0..2] boundary, so this must return None, not panic.
        assert_eq!(Rgb::parse_hex("#aébc1"), None);
        // A 4-char body padded to 6 bytes by a multi-byte char, too.
        assert_eq!(Rgb::parse_hex("#1234é"), None);
    }

    #[test]
    fn color_source_parse_literal_name_and_bad_hex() {
        assert_eq!(
            ColorSource::parse("#7e9cd8").unwrap(),
            ColorSource::Literal(Rgb { r: 0x7e, g: 0x9c, b: 0xd8 })
        );
        assert_eq!(
            ColorSource::parse("accent").unwrap(),
            ColorSource::Name("accent".to_string())
        );
        // A `#`-prefixed spec that isn't valid hex is a hard error, not a name.
        assert!(ColorSource::parse("#zzz").is_err());
    }
}
