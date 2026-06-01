//! KDL v2 config decoder. Hand-walks the `kdl` crate's AST into the in-memory
//! `Config` model. Replaces the former serde/TOML loader; the `Config` shape and
//! every downstream consumer are unchanged.

use crate::{Config, ConfigError, KeymapBinding, KeymapConfig, PaletteConfig};
use kdl::{KdlDocument, KdlNode, KdlValue};

/// Parse a KDL v2 document into a `Config`. Syntax errors and decode errors both
/// surface as `ConfigError::Kdl` with a message; this never panics.
pub fn parse_config(src: &str) -> Result<Config, ConfigError> {
    let doc = KdlDocument::parse(src).map_err(|e| ConfigError::Kdl(e.to_string()))?;
    let mut config = Config::default();
    let mut seen_palette = false;
    let mut seen_keymap = false;
    for node in doc.nodes() {
        match node.name().value() {
            "palette" => {
                dup_check(seen_palette, "palette", node, src)?;
                seen_palette = true;
                config.palette = decode_palette(node, src)?;
            }
            "keymap" => {
                dup_check(seen_keymap, "keymap", node, src)?;
                seen_keymap = true;
                config.keymap = decode_keymap(node, src)?;
            }
            other => {
                return Err(decode_err(src, node, &format!("unknown top-level node `{other}`")));
            }
        }
    }
    Ok(config)
}

// --- error helpers ---

fn decode_err(src: &str, node: &KdlNode, msg: &str) -> ConfigError {
    let (line, col) = offset_to_line_col(src, node.span().offset());
    ConfigError::Kdl(format!("{msg} (at line {line}:{col})"))
}

/// `KdlNode::span()` gives a byte offset; turn it into 1-based line/column.
fn offset_to_line_col(src: &str, offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in src.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn dup_check(seen: bool, name: &str, node: &KdlNode, src: &str) -> Result<(), ConfigError> {
    if seen {
        Err(decode_err(src, node, &format!("duplicate `{name}` node")))
    } else {
        Ok(())
    }
}

// --- entry access ---

/// The `idx`-th positional argument value (entries with no name), in order.
fn pos_arg(node: &KdlNode, idx: usize) -> Option<&KdlValue> {
    node.entries()
        .iter()
        .filter(|e| e.name().is_none())
        .nth(idx)
        .map(|e| e.value())
}

fn string_arg<'a>(node: &'a KdlNode, idx: usize, src: &str, what: &str) -> Result<&'a str, ConfigError> {
    pos_arg(node, idx)
        .and_then(|v| v.as_string())
        .ok_or_else(|| decode_err(src, node, &format!("expected string {what}")))
}

fn bool_arg(node: &KdlNode, idx: usize, src: &str, what: &str) -> Result<bool, ConfigError> {
    pos_arg(node, idx)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| decode_err(src, node, &format!("expected boolean {what} (use #true / #false)")))
}

/// Error if the node carries any property (named entry). Used by nodes that take
/// only positional args and/or child nodes (`palette`, `keymap`, `status`, zones).
fn ensure_no_props(node: &KdlNode, src: &str) -> Result<(), ConfigError> {
    for e in node.entries() {
        if let Some(name) = e.name() {
            return Err(decode_err(
                src,
                node,
                &format!("`{}` takes no properties (found `{}`)", node.name().value(), name.value()),
            ));
        }
    }
    Ok(())
}

// --- palette ---

fn decode_palette(node: &KdlNode, src: &str) -> Result<PaletteConfig, ConfigError> {
    ensure_no_props(node, src)?;
    let mut entries = std::collections::HashMap::new();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            let key = child.name().value().to_string();
            let val = string_arg(child, 0, src, &format!("color value for `{key}`"))?.to_string();
            entries.insert(key, val);
        }
    }
    Ok(PaletteConfig { entries })
}

// --- keymap ---

fn decode_keymap(node: &KdlNode, src: &str) -> Result<KeymapConfig, ConfigError> {
    ensure_no_props(node, src)?;
    // Start from the Default (prefix "Ctrl+a", inherit_defaults true, no binds)
    // and override what the document specifies.
    let mut km = KeymapConfig::default();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            match child.name().value() {
                "prefix" => km.prefix = string_arg(child, 0, src, "prefix")?.to_string(),
                "inherit-defaults" => km.inherit_defaults = bool_arg(child, 0, src, "inherit-defaults")?,
                "bind" => {
                    let keys = string_arg(child, 0, src, "bind keys")?.to_string();
                    let command = string_arg(child, 1, src, "bind command")?.to_string();
                    km.bindings.push(KeymapBinding { keys, command });
                }
                other => {
                    return Err(decode_err(src, child, &format!("unknown keymap node `{other}`")));
                }
            }
        }
    }
    Ok(km)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::built_in_keymap;

    #[test]
    fn decodes_palette_entries() {
        let cfg = parse_config(r##"palette { bg "#1D1C19"; fg "#c8c093" }"##).unwrap();
        assert_eq!(cfg.palette.entries.get("bg").map(String::as_str), Some("#1D1C19"));
        assert_eq!(cfg.palette.entries.get("fg").map(String::as_str), Some("#c8c093"));
        assert_eq!(cfg.palette.entries.len(), 2);
    }

    #[test]
    fn decodes_keymap_prefix_inherit_and_binds() {
        let cfg = parse_config(
            r##"keymap { prefix "Ctrl+a"; inherit-defaults #true; bind "Ctrl+a c" "new_window"; bind "Ctrl+a v" "split_v" }"##,
        )
        .unwrap();
        assert_eq!(cfg.keymap.prefix, "Ctrl+a");
        assert!(cfg.keymap.inherit_defaults);
        assert_eq!(cfg.keymap.bindings.len(), 2);
        assert_eq!(
            cfg.keymap.bindings[0],
            KeymapBinding { keys: "Ctrl+a c".into(), command: "new_window".into() }
        );
    }

    #[test]
    fn keymap_defaults_when_node_absent() {
        // An empty document yields `Config::default()`; keymap mirrors built-in
        // *shape* defaults (prefix + inherit), with no inherited binds yet.
        let cfg = parse_config("").unwrap();
        assert_eq!(cfg.keymap.prefix, built_in_keymap().prefix);
        assert!(cfg.keymap.inherit_defaults);
        assert!(cfg.keymap.bindings.is_empty());
    }

    #[test]
    fn inherit_defaults_false_is_respected() {
        let cfg = parse_config(r##"keymap { inherit-defaults #false }"##).unwrap();
        assert!(!cfg.keymap.inherit_defaults);
    }

    #[test]
    fn bare_bool_is_a_v2_syntax_error() {
        // KDL v2 requires `#true`/`#false`; bare `true` is a parse error.
        assert!(parse_config(r##"keymap { inherit-defaults true }"##).is_err());
    }

    #[test]
    fn unknown_top_level_node_errors() {
        assert!(parse_config("bogus { }").is_err());
    }

    #[test]
    fn duplicate_palette_node_errors() {
        assert!(parse_config(r##"palette { bg "#000000" } palette { fg "#ffffff" }"##).is_err());
    }

    #[test]
    fn palette_property_form_is_rejected() {
        // palette entries are child nodes, not properties.
        assert!(parse_config(r##"palette bg="#000000""##).is_err());
    }
}
