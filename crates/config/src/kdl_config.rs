//! KDL v2 config decoder. Hand-walks the `kdl` crate's AST into the in-memory
//! `Config` model. Replaces the former serde/TOML loader; the `Config` shape and
//! every downstream consumer are unchanged.

use crate::{
    Config, ConfigError, KeymapBinding, KeymapConfig, Padding, PaletteConfig, Position,
    StatusConfig, StyleConfig, WidgetSpec,
};
use kdl::{KdlDocument, KdlNode, KdlValue};
use std::time::Duration;

/// Parse a KDL v2 document into a `Config`. Syntax errors and decode errors both
/// surface as `ConfigError::Kdl` with a message; this never panics.
pub fn parse_config(src: &str) -> Result<Config, ConfigError> {
    let doc = KdlDocument::parse(src).map_err(|e| ConfigError::Kdl(e.to_string()))?;
    let mut config = Config::default();
    let mut seen_palette = false;
    let mut seen_status = false;
    let mut seen_keymap = false;
    for node in doc.nodes() {
        match node.name().value() {
            "palette" => {
                dup_check(seen_palette, "palette", node, src)?;
                seen_palette = true;
                config.palette = decode_palette(node, src)?;
            }
            "status" => {
                dup_check(seen_status, "status", node, src)?;
                seen_status = true;
                config.status = decode_status(node, src)?;
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

// --- property access (widgets) ---

/// Last-occurrence-wins property lookup (KDL's own convention).
fn prop_val<'a>(node: &'a KdlNode, key: &str) -> Option<&'a KdlValue> {
    node.entries()
        .iter()
        .rev()
        .find(|e| e.name().map(|n| n.value()) == Some(key))
        .map(|e| e.value())
}

fn prop_str<'a>(node: &'a KdlNode, key: &str) -> Option<&'a str> {
    prop_val(node, key).and_then(|v| v.as_string())
}

fn require_prop_str<'a>(node: &'a KdlNode, key: &str, src: &str) -> Result<&'a str, ConfigError> {
    prop_str(node, key)
        .ok_or_else(|| decode_err(src, node, &format!("`{}` requires string property `{key}`", node.name().value())))
}

fn prop_u8(node: &KdlNode, key: &str, src: &str) -> Result<Option<u8>, ConfigError> {
    match prop_val(node, key) {
        None => Ok(None),
        Some(v) => {
            let i = v
                .as_integer()
                .ok_or_else(|| decode_err(src, node, &format!("`{key}` must be an integer")))?;
            u8::try_from(i)
                .map(Some)
                .map_err(|_| decode_err(src, node, &format!("`{key}` out of range (0-255)")))
        }
    }
}

fn prop_dur(node: &KdlNode, key: &str, src: &str) -> Result<Option<Duration>, ConfigError> {
    match prop_str(node, key) {
        None => Ok(None),
        Some(s) => humantime::parse_duration(s)
            .map(Some)
            .map_err(|e| decode_err(src, node, &format!("invalid duration `{s}` for `{key}`: {e}"))),
    }
}

fn prop_char(node: &KdlNode, key: &str, src: &str) -> Result<Option<char>, ConfigError> {
    match prop_str(node, key) {
        None => Ok(None),
        Some(s) => {
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Ok(Some(c)),
                _ => Err(decode_err(src, node, &format!("`{key}` must be exactly one character, got `{s}`"))),
            }
        }
    }
}

/// Error on any property whose key isn't in `allowed`. Positional args are ignored.
fn ensure_only_props(node: &KdlNode, allowed: &[&str], src: &str) -> Result<(), ConfigError> {
    for e in node.entries() {
        if let Some(name) = e.name()
            && !allowed.contains(&name.value())
        {
            return Err(decode_err(
                src,
                node,
                &format!("unknown property `{}` on `{}`", name.value(), node.name().value()),
            ));
        }
    }
    Ok(())
}

/// Error on any child node whose name isn't in `allowed`.
fn ensure_only_children(node: &KdlNode, allowed: &[&str], src: &str) -> Result<(), ConfigError> {
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            if !allowed.contains(&child.name().value()) {
                return Err(decode_err(
                    src,
                    child,
                    &format!("unknown child node `{}` on `{}`", child.name().value(), node.name().value()),
                ));
            }
        }
    }
    Ok(())
}

/// First child node with the given name, if any.
fn find_child<'a>(node: &'a KdlNode, name: &str) -> Option<&'a KdlNode> {
    node.children().and_then(|doc| doc.nodes().iter().find(|n| n.name().value() == name))
}

// --- style ---

fn decode_style(node: &KdlNode, src: &str) -> Result<StyleConfig, ConfigError> {
    let mut style = StyleConfig::default();
    if pos_arg(node, 0).is_some() {
        return Err(decode_err(
            src,
            node,
            &format!("`{}` takes named style fields, not positional args", node.name().value()),
        ));
    }
    // Property form.
    for e in node.entries() {
        if let Some(name) = e.name() {
            set_style_field(&mut style, name.value(), e.value(), node, src)?;
        }
    }
    // Child name-value form (overrides like-named properties).
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            let key = child.name().value();
            let val = pos_arg(child, 0)
                .ok_or_else(|| decode_err(src, child, &format!("style field `{key}` needs a value")))?;
            set_style_field(&mut style, key, val, child, src)?;
        }
    }
    Ok(style)
}

fn set_style_field(
    style: &mut StyleConfig,
    key: &str,
    val: &KdlValue,
    node: &KdlNode,
    src: &str,
) -> Result<(), ConfigError> {
    let as_str = |v: &KdlValue| -> Result<String, ConfigError> {
        v.as_string()
            .map(str::to_string)
            .ok_or_else(|| decode_err(src, node, &format!("`{key}` must be a string")))
    };
    let as_flag = |v: &KdlValue| -> Result<bool, ConfigError> {
        v.as_bool()
            .ok_or_else(|| decode_err(src, node, &format!("`{key}` must be #true/#false")))
    };
    match key {
        "fg" => style.fg = Some(as_str(val)?),
        "bg" => style.bg = Some(as_str(val)?),
        "bold" => style.bold = as_flag(val)?,
        "italic" => style.italic = as_flag(val)?,
        "underline" => style.underline = as_flag(val)?,
        "reverse" => style.reverse = as_flag(val)?,
        other => return Err(decode_err(src, node, &format!("unknown style field `{other}`"))),
    }
    Ok(())
}

/// A required style child (`prefix-indicator`/`attached-clients` `style`,
/// `window-list` `active-style`/`inactive-style`).
fn require_style(node: &KdlNode, child_name: &str, src: &str) -> Result<StyleConfig, ConfigError> {
    match find_child(node, child_name) {
        Some(n) => decode_style(n, src),
        None => Err(decode_err(src, node, &format!("`{}` requires a `{child_name}` node", node.name().value()))),
    }
}

/// An optional style child (most widgets); absent ⇒ `StyleConfig::default()`.
fn opt_style(node: &KdlNode, child_name: &str, src: &str) -> Result<StyleConfig, ConfigError> {
    match find_child(node, child_name) {
        Some(n) => decode_style(n, src),
        None => Ok(StyleConfig::default()),
    }
}

// --- padding ---

fn decode_padding(node: &KdlNode, src: &str) -> Result<Padding, ConfigError> {
    Ok(Padding {
        left: padding_arg(node, 0, src)?,
        right: padding_arg(node, 1, src)?,
    })
}

fn padding_arg(node: &KdlNode, idx: usize, src: &str) -> Result<u8, ConfigError> {
    let v = pos_arg(node, idx)
        .ok_or_else(|| decode_err(src, node, "`padding` needs two integer args: <left> <right>"))?;
    let i = v
        .as_integer()
        .ok_or_else(|| decode_err(src, node, "`padding` args must be integers"))?;
    u8::try_from(i).map_err(|_| decode_err(src, node, "`padding` value out of range (0-255)"))
}

/// Optional `padding` child; absent ⇒ `Padding::default()` (0,0).
fn opt_padding(node: &KdlNode, src: &str) -> Result<Padding, ConfigError> {
    match find_child(node, "padding") {
        Some(n) => decode_padding(n, src),
        None => Ok(Padding::default()),
    }
}

// --- status + zones ---

fn decode_status(node: &KdlNode, src: &str) -> Result<StatusConfig, ConfigError> {
    ensure_no_props(node, src)?;
    // Start from defaults (position Bottom, refresh 5s, empty zones) and override.
    let mut status = StatusConfig::default();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            match child.name().value() {
                "position" => status.position = decode_position(child, src)?,
                "refresh" => {
                    let s = string_arg(child, 0, src, "refresh duration")?;
                    status.refresh = humantime::parse_duration(s)
                        .map_err(|e| decode_err(src, child, &format!("invalid refresh duration `{s}`: {e}")))?;
                }
                "left" => status.left = decode_zone(child, src)?,
                "middle" => status.middle = decode_zone(child, src)?,
                "right" => status.right = decode_zone(child, src)?,
                other => return Err(decode_err(src, child, &format!("unknown status node `{other}`"))),
            }
        }
    }
    Ok(status)
}

fn decode_position(node: &KdlNode, src: &str) -> Result<Position, ConfigError> {
    match string_arg(node, 0, src, "position")? {
        "bottom" => Ok(Position::Bottom),
        "top" => Ok(Position::Top),
        other => Err(decode_err(src, node, &format!("position must be `top` or `bottom`, got `{other}`"))),
    }
}

fn decode_zone(node: &KdlNode, src: &str) -> Result<Vec<WidgetSpec>, ConfigError> {
    ensure_no_props(node, src)?;
    let mut widgets = Vec::new();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            widgets.push(decode_widget(child, src)?);
        }
    }
    Ok(widgets)
}

// --- widgets ---

fn decode_widget(node: &KdlNode, src: &str) -> Result<WidgetSpec, ConfigError> {
    let name = node.name().value();
    let widget = match name {
        "session" => {
            ensure_only_props(node, &[], src)?;
            ensure_only_children(node, &["style", "padding"], src)?;
            WidgetSpec::Session {
                style: opt_style(node, "style", src)?,
                padding: opt_padding(node, src)?,
            }
        }
        "window-list" => {
            ensure_only_props(node, &[], src)?;
            ensure_only_children(node, &["active-style", "inactive-style"], src)?;
            WidgetSpec::WindowList {
                active_style: require_style(node, "active-style", src)?,
                inactive_style: require_style(node, "inactive-style", src)?,
            }
        }
        "prefix-indicator" => {
            ensure_only_props(node, &["content"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::PrefixIndicator {
                style: require_style(node, "style", src)?,
                content: require_prop_str(node, "content", src)?.to_string(),
            }
        }
        "attached-clients" => {
            ensure_only_props(node, &["min-count"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::AttachedClients {
                style: require_style(node, "style", src)?,
                min_count: prop_u8(node, "min-count", src)?.unwrap_or(2),
            }
        }
        "time" => {
            ensure_only_props(node, &["format", "interval"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::Time {
                format: prop_str(node, "format").unwrap_or("%H:%M").to_string(),
                interval: prop_dur(node, "interval", src)?,
                style: opt_style(node, "style", src)?,
            }
        }
        "hostname" => {
            ensure_only_props(node, &["interval"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::Hostname {
                style: opt_style(node, "style", src)?,
                interval: prop_dur(node, "interval", src)?,
            }
        }
        "cwd" => {
            ensure_only_props(node, &["max-components"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::Cwd {
                style: opt_style(node, "style", src)?,
                max_components: prop_u8(node, "max-components", src)?,
            }
        }
        "git-branch" => {
            ensure_only_props(node, &["interval"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::GitBranch {
                style: opt_style(node, "style", src)?,
                interval: prop_dur(node, "interval", src)?,
            }
        }
        "battery" => {
            ensure_only_props(node, &["interval"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::Battery {
                style: opt_style(node, "style", src)?,
                interval: prop_dur(node, "interval", src)?,
            }
        }
        "cpu-load" => {
            ensure_only_props(node, &["interval"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::CpuLoad {
                style: opt_style(node, "style", src)?,
                interval: prop_dur(node, "interval", src)?,
            }
        }
        "memory" => {
            ensure_only_props(node, &["interval"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::Memory {
                style: opt_style(node, "style", src)?,
                interval: prop_dur(node, "interval", src)?,
            }
        }
        "text" => {
            ensure_only_props(node, &["value"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::Text {
                value: require_prop_str(node, "value", src)?.to_string(),
                style: opt_style(node, "style", src)?,
            }
        }
        "separator" => {
            ensure_only_props(node, &["char"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::Separator {
                char: prop_char(node, "char", src)?.unwrap_or('|'),
                style: opt_style(node, "style", src)?,
            }
        }
        "shell" => {
            ensure_only_props(node, &["command", "interval", "timeout"], src)?;
            ensure_only_children(node, &["style", "args"], src)?;
            WidgetSpec::Shell {
                command: require_prop_str(node, "command", src)?.to_string(),
                args: decode_shell_args(node, src)?,
                interval: prop_dur(node, "interval", src)?,
                timeout: prop_dur(node, "timeout", src)?.unwrap_or(Duration::from_secs(1)),
                style: opt_style(node, "style", src)?,
            }
        }
        other => return Err(decode_err(src, node, &format!("unknown widget `{other}`"))),
    };
    Ok(widget)
}

/// `shell`'s `args` child node: positional string args ⇒ `Vec<String>`.
fn decode_shell_args(node: &KdlNode, src: &str) -> Result<Vec<String>, ConfigError> {
    match find_child(node, "args") {
        None => Ok(Vec::new()),
        Some(args_node) => {
            let mut out = Vec::new();
            for e in args_node.entries() {
                if e.name().is_some() {
                    return Err(decode_err(src, args_node, "`args` takes positional string args only"));
                }
                let s = e
                    .value()
                    .as_string()
                    .ok_or_else(|| decode_err(src, args_node, "`args` entries must be strings"))?;
                out.push(s.to_string());
            }
            Ok(out)
        }
    }
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

    use crate::built_in_default;

    const DEFAULT_KDL: &str = r##"
palette {
    bg "#1D1C19"
    bg_bar "#282727"
    fg "#c8c093"
    accent "#737c73"
    highlight "#b6927b"
    selection "#393836"
    info "#949fb5"
    alert "#c4746e"
    warn "#c4b28a"
    muted "#b6927b"
    ok "#87a987"
}

status {
    position "bottom"
    refresh "5s"
    left {
        session { style fg="bg" bg="accent" bold=#true; padding 1 1 }
        prefix-indicator content=" PFX " { style fg="bg" bg="highlight" bold=#true }
        text value=" "
    }
    middle {
        window-list { active-style fg="fg" bg="accent"; inactive-style fg="muted" bg="bg_bar" }
    }
    right {
        attached-clients min-count=2 { style fg="fg" bg="bg_bar" }
        text value="  " { style fg="muted" bg="bg_bar" }
        cpu-load { style fg="fg" bg="bg_bar" }
        text value=" | " { style fg="muted" bg="bg_bar" }
        battery { style fg="fg" bg="bg_bar" }
        text value=" | " { style fg="muted" bg="bg_bar" }
        time format="%H:%M" { style fg="fg" bg="bg_bar" }
        text value=" " { style fg="muted" bg="bg_bar" }
    }
}

keymap {
    prefix "Ctrl+a"
    inherit-defaults #true
    bind "Ctrl+a c" "new_window"
    bind "Ctrl+a v" "split_v"
    bind "Ctrl+a s" "split_h"
    bind "Ctrl+a x" "kill_pane"
    bind "Ctrl+a z" "zoom_toggle"
    bind "Ctrl+a n" "next_window"
    bind "Ctrl+a p" "prev_window"
    bind "Ctrl+a &" "kill_window"
    bind "Ctrl+a d" "detach"
    bind "Ctrl+a [" "enter_copy_mode"
    bind "Ctrl+a y" "toggle_sync_panes"
    bind "Ctrl+a R" "reload_config"
    bind "Ctrl+a 1" "select_window:0"
    bind "Ctrl+a 2" "select_window:1"
    bind "Ctrl+a 3" "select_window:2"
    bind "Ctrl+a 4" "select_window:3"
    bind "Ctrl+a 5" "select_window:4"
    bind "Ctrl+a 6" "select_window:5"
    bind "Ctrl+a 7" "select_window:6"
    bind "Ctrl+a 8" "select_window:7"
    bind "Ctrl+a 9" "select_window:8"
    bind "Ctrl+a h" "select_pane_left"
    bind "Ctrl+a j" "select_pane_down"
    bind "Ctrl+a k" "select_pane_up"
    bind "Ctrl+a l" "select_pane_right"
    bind "Alt+Left" "select_pane_left"
    bind "Alt+Down" "select_pane_down"
    bind "Alt+Up" "select_pane_up"
    bind "Alt+Right" "select_pane_right"
    bind "Ctrl+a H" "resize_pane_left"
    bind "Ctrl+a J" "resize_pane_down"
    bind "Ctrl+a K" "resize_pane_up"
    bind "Ctrl+a L" "resize_pane_right"
    bind "Ctrl+a Tab" "select_last_window"
    bind "Ctrl+a ;" "select_last_pane"
    bind "Ctrl+a ," "rename_window"
    bind "Ctrl+a ." "rename_pane"
    bind "Ctrl+a ?" "show_help"
    bind "Ctrl+a :" "command_prompt"
    bind "Ctrl+a w" "choose_session"
    bind "Ctrl+a W" "choose_tree"
    bind "Ctrl+a m" "mark_pane"
    bind "Ctrl+a !" "break_pane"
    bind "Ctrl+a {" "swap_pane_prev"
    bind "Ctrl+a }" "swap_pane_next"
    bind "Ctrl+a ]" "paste_buffer"
    bind "Ctrl+a =" "choose_buffer"
    bind "Ctrl+a M" "toggle_monitor_activity"
}
"##;

    #[test]
    fn default_kdl_parses_to_built_in_default() {
        let cfg = parse_config(DEFAULT_KDL).expect("default KDL must parse");
        assert_eq!(cfg, built_in_default());
    }

    #[test]
    fn position_top_and_refresh_parse() {
        let cfg = parse_config(r##"status { position "top"; refresh "10s" }"##).unwrap();
        assert_eq!(cfg.status.position, Position::Top);
        assert_eq!(cfg.status.refresh, Duration::from_secs(10));
    }

    #[test]
    fn text_requires_value() {
        assert!(parse_config(r##"status { left { text } }"##).is_err());
    }

    #[test]
    fn shell_requires_command_and_parses_args() {
        assert!(parse_config(r##"status { left { shell } }"##).is_err());
        let cfg = parse_config(
            r##"status { left { shell command="echo" interval="30s" { args "hi" "there" } } }"##,
        )
        .unwrap();
        match &cfg.status.left[0] {
            WidgetSpec::Shell { command, args, interval, timeout, .. } => {
                assert_eq!(command, "echo");
                assert_eq!(args, &vec!["hi".to_string(), "there".to_string()]);
                assert_eq!(*interval, Some(Duration::from_secs(30)));
                assert_eq!(*timeout, Duration::from_secs(1)); // default
            }
            other => panic!("expected Shell, got {other:?}"),
        }
    }

    #[test]
    fn window_list_requires_both_styles() {
        // Missing inactive-style is a decode error (window-list requires both styles).
        assert!(parse_config(r##"status { middle { window-list { active-style fg="fg" } } }"##).is_err());
    }

    #[test]
    fn attached_clients_min_count_defaults_to_two() {
        let cfg = parse_config(r##"status { right { attached-clients { style fg="fg" } } }"##).unwrap();
        match &cfg.status.right[0] {
            WidgetSpec::AttachedClients { min_count, .. } => assert_eq!(*min_count, 2),
            other => panic!("expected AttachedClients, got {other:?}"),
        }
    }

    #[test]
    fn style_child_node_form_matches_property_form() {
        let props = parse_config(r##"status { left { session { style fg="bg" bg="accent" bold=#true } } }"##).unwrap();
        let children = parse_config(r##"status { left { session { style { fg "bg"; bg "accent"; bold #true } } } }"##).unwrap();
        assert_eq!(props.status.left, children.status.left);
    }

    #[test]
    fn separator_is_supported_but_not_in_default() {
        // Separator decodes (default char '|'), even though built_in_default uses
        // `text " | "` for the bars instead.
        let cfg = parse_config(r##"status { right { separator char="*" } }"##).unwrap();
        match &cfg.status.right[0] {
            WidgetSpec::Separator { char, .. } => assert_eq!(*char, '*'),
            other => panic!("expected Separator, got {other:?}"),
        }
        assert!(parse_config(r##"status { right { separator char="ab" } }"##).is_err());
    }

    #[test]
    fn unknown_widget_and_unknown_property_error() {
        assert!(parse_config(r##"status { left { not-a-widget } }"##).is_err());
        assert!(parse_config(r##"status { left { text value="x" bogus="y" } }"##).is_err());
    }

    #[test]
    fn bad_duration_errors() {
        assert!(parse_config(r##"status { refresh "not-a-duration" }"##).is_err());
    }

    #[test]
    fn prefix_indicator_requires_content_and_style() {
        // Both fields are required (no defaults).
        assert!(parse_config(r##"status { left { prefix-indicator { style fg="fg" } } }"##).is_err());
        assert!(parse_config(r##"status { left { prefix-indicator content=" PFX " } }"##).is_err());
    }

    #[test]
    fn attached_clients_requires_style() {
        // Missing `style` child is a decode error (attached-clients requires it).
        assert!(parse_config(r##"status { right { attached-clients min-count=2 } }"##).is_err());
    }

    #[test]
    fn duplicate_status_node_errors() {
        assert!(parse_config("status { } status { }").is_err());
    }

    #[test]
    fn duplicate_keymap_node_errors() {
        assert!(parse_config("keymap { } keymap { }").is_err());
    }

    #[test]
    fn unknown_child_node_on_widget_errors() {
        assert!(parse_config(r##"status { left { session { bad-child { } } } }"##).is_err());
    }

    #[test]
    fn padding_out_of_range_or_malformed_errors() {
        assert!(parse_config(r##"status { left { session { padding 256 1 } } }"##).is_err());
        assert!(parse_config(r##"status { left { session { padding -1 1 } } }"##).is_err());
        assert!(parse_config(r##"status { left { session { padding 1 } } }"##).is_err());
    }

    #[test]
    fn cwd_max_components_optional() {
        let with = parse_config(r##"status { left { cwd max-components=2 } }"##).unwrap();
        match &with.status.left[0] {
            WidgetSpec::Cwd { max_components, .. } => assert_eq!(*max_components, Some(2)),
            other => panic!("expected Cwd, got {other:?}"),
        }
        let without = parse_config(r##"status { left { cwd } }"##).unwrap();
        match &without.status.left[0] {
            WidgetSpec::Cwd { max_components, .. } => assert_eq!(*max_components, None),
            other => panic!("expected Cwd, got {other:?}"),
        }
    }

    #[test]
    fn hostname_git_branch_memory_decode() {
        let cfg = parse_config(
            r##"status { left { hostname interval="30s"; git-branch; memory interval="2s" } }"##,
        )
        .unwrap();
        assert!(matches!(cfg.status.left[0], WidgetSpec::Hostname { .. }));
        assert!(matches!(cfg.status.left[1], WidgetSpec::GitBranch { .. }));
        assert!(matches!(cfg.status.left[2], WidgetSpec::Memory { .. }));
    }
}
