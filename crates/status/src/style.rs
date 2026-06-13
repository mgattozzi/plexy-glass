use plexy_glass_config::{PaletteConfig, StyleConfig};
use plexy_glass_emulator::Attrs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
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

#[derive(Debug, Clone, Copy, Default)]
pub struct ResolvedStyle {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub attrs: Attrs,
}

/// Resolve a `StyleConfig` by looking up palette names (or accepting
/// `#rrggbb` literals).
pub fn resolve_style(style: &StyleConfig, palette: &PaletteConfig) -> ResolvedStyle {
    let mut attrs = Attrs::empty();
    if style.bold {
        attrs |= Attrs::BOLD;
    }
    if style.italic {
        attrs |= Attrs::ITALIC;
    }
    if style.underline {
        attrs |= Attrs::UNDERLINE;
    }
    if style.reverse {
        attrs |= Attrs::REVERSE;
    }
    ResolvedStyle {
        fg: style.fg.as_deref().and_then(|name| lookup(name, palette)),
        bg: style.bg.as_deref().and_then(|name| lookup(name, palette)),
        attrs,
    }
}

/// Resolve a palette name or `#rrggbb` hex literal to an `Rgb` color.
///
/// Returns `None` when the name is absent from the palette or the hex is
/// malformed.
pub fn resolve_color(name_or_hex: &str, palette: &PaletteConfig) -> Option<Rgb> {
    lookup(name_or_hex, palette)
}

fn lookup(name_or_hex: &str, palette: &PaletteConfig) -> Option<Rgb> {
    if let Some(literal) = Rgb::parse_hex(name_or_hex) {
        return Some(literal);
    }
    palette
        .entries
        .get(name_or_hex)
        .and_then(|s| Rgb::parse_hex(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn palette() -> PaletteConfig {
        let mut e = HashMap::new();
        e.insert("accent".to_string(), "#7e9cd8".to_string());
        e.insert("bg".to_string(), "#1f1f28".to_string());
        PaletteConfig { entries: e }
    }

    #[test]
    fn resolves_palette_name() {
        let s = StyleConfig {
            fg: Some("accent".to_string()),
            ..Default::default()
        };
        let r = resolve_style(&s, &palette());
        assert_eq!(r.fg, Some(Rgb { r: 0x7e, g: 0x9c, b: 0xd8 }));
    }

    #[test]
    fn resolves_hex_literal() {
        let s = StyleConfig {
            fg: Some("#abcdef".to_string()),
            ..Default::default()
        };
        let r = resolve_style(&s, &PaletteConfig::default());
        assert_eq!(r.fg, Some(Rgb { r: 0xab, g: 0xcd, b: 0xef }));
    }

    #[test]
    fn unknown_name_resolves_to_none() {
        let s = StyleConfig {
            fg: Some("nonexistent".to_string()),
            ..Default::default()
        };
        let r = resolve_style(&s, &PaletteConfig::default());
        assert_eq!(r.fg, None);
    }

    #[test]
    fn bold_flag_sets_attrs() {
        let s = StyleConfig {
            bold: true,
            ..Default::default()
        };
        let r = resolve_style(&s, &PaletteConfig::default());
        assert!(r.attrs.contains(Attrs::BOLD));
    }

    #[test]
    fn resolve_color_hex_literal() {
        let result = resolve_color("#ff0000", &PaletteConfig::default());
        assert_eq!(result, Some(Rgb { r: 0xff, g: 0x00, b: 0x00 }));
    }

    #[test]
    fn resolve_color_palette_name() {
        let result = resolve_color("accent", &palette());
        assert_eq!(result, Some(Rgb { r: 0x7e, g: 0x9c, b: 0xd8 }));
    }

    #[test]
    fn resolve_color_unknown_name_is_none() {
        let result = resolve_color("nonexistent", &PaletteConfig::default());
        assert_eq!(result, None);
    }

    #[test]
    fn resolve_color_malformed_hex_is_none() {
        let result = resolve_color("#gggggg", &PaletteConfig::default());
        assert_eq!(result, None);
    }

    #[test]
    fn parse_hex_multibyte_char_is_none_not_panic() {
        // "#aébc1": the body is exactly 6 bytes but `é` is 2 bytes straddling the
        // &s[0..2] boundary, so this must return None, not panic.
        assert_eq!(Rgb::parse_hex("#aébc1"), None);
        // A 4-char body padded to 6 bytes by a multi-byte char, too.
        assert_eq!(Rgb::parse_hex("#1234é"), None);
    }
}
