// `Rgb` moved down into `plexy-glass-config` (parsed from hex at decode); re-export
// so every existing `plexy_glass_status::Rgb` user keeps compiling.
pub use plexy_glass_config::Rgb;
use plexy_glass_config::{ColorSource, PaletteConfig, StyleConfig};
use plexy_glass_emulator::Attrs;

#[derive(Debug, Clone, Copy, Default)]
pub struct ResolvedStyle {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub attrs: Attrs,
}

/// Resolve a `StyleConfig` against the (pre-parsed) palette.
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
        fg: style
            .fg
            .as_ref()
            .and_then(|src| resolve_color(src, palette)),
        bg: style
            .bg
            .as_ref()
            .and_then(|src| resolve_color(src, palette)),
        attrs,
    }
}

/// Resolve a `ColorSource` to an `Rgb`: a literal is returned as-is; a role name
/// is a lookup in the pre-parsed palette. An unknown role → `None` → the terminal
/// default (the same silent-fallback behavior the old string parse had for an
/// unknown name), but the hex is no longer re-parsed here — it was validated at
/// config decode.
pub fn resolve_color(source: &ColorSource, palette: &PaletteConfig) -> Option<Rgb> {
    match source {
        ColorSource::Literal(rgb) => Some(*rgb),
        ColorSource::Name(role) => palette.entries.get(role).copied(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn palette() -> PaletteConfig {
        let mut e = HashMap::new();
        e.insert(
            "accent".to_string(),
            Rgb {
                r: 0x7e,
                g: 0x9c,
                b: 0xd8,
            },
        );
        e.insert(
            "bg".to_string(),
            Rgb {
                r: 0x1f,
                g: 0x1f,
                b: 0x28,
            },
        );
        PaletteConfig { entries: e }
    }

    fn name(s: &str) -> ColorSource {
        ColorSource::Name(s.to_string())
    }

    #[test]
    fn resolves_palette_name() {
        let s = StyleConfig {
            fg: Some(name("accent")),
            ..Default::default()
        };
        let r = resolve_style(&s, &palette());
        assert_eq!(
            r.fg,
            Some(Rgb {
                r: 0x7e,
                g: 0x9c,
                b: 0xd8
            })
        );
    }

    #[test]
    fn resolves_hex_literal() {
        let s = StyleConfig {
            fg: Some(ColorSource::Literal(Rgb {
                r: 0xab,
                g: 0xcd,
                b: 0xef,
            })),
            ..Default::default()
        };
        let r = resolve_style(&s, &PaletteConfig::default());
        assert_eq!(
            r.fg,
            Some(Rgb {
                r: 0xab,
                g: 0xcd,
                b: 0xef
            })
        );
    }

    #[test]
    fn unknown_name_resolves_to_none() {
        let s = StyleConfig {
            fg: Some(name("nonexistent")),
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
    fn resolve_color_literal() {
        let result = resolve_color(
            &ColorSource::Literal(Rgb {
                r: 0xff,
                g: 0,
                b: 0,
            }),
            &PaletteConfig::default(),
        );
        assert_eq!(
            result,
            Some(Rgb {
                r: 0xff,
                g: 0,
                b: 0
            })
        );
    }

    #[test]
    fn resolve_color_palette_name() {
        let result = resolve_color(&ColorSource::Name("accent".to_string()), &palette());
        assert_eq!(
            result,
            Some(Rgb {
                r: 0x7e,
                g: 0x9c,
                b: 0xd8
            })
        );
    }

    #[test]
    fn resolve_color_unknown_name_is_none() {
        let result = resolve_color(
            &ColorSource::Name("nonexistent".to_string()),
            &PaletteConfig::default(),
        );
        assert_eq!(result, None);
    }
}
