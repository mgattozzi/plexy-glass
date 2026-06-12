//! KDL v2 config decoder. Hand-walks the `kdl` crate's AST into the in-memory
//! `Config` model. Replaces the former serde/TOML loader; the `Config` shape and
//! every downstream consumer are unchanged.

use crate::{
    BlocksConfig, Config, ConfigError, KeymapBinding, KeymapConfig, Padding, PaletteConfig,
    PaneNode, PaneTemplate, Position, SessionTemplate, SplitDirection, StatusConfig, StyleConfig,
    WidgetSpec, WindowTemplate,
};
use kdl::{KdlDocument, KdlNode, KdlValue};
use std::time::Duration;

/// Parse a KDL v2 document into a `Config`. Syntax errors and decode errors both
/// surface as `ConfigError::Kdl` with a message; this never panics.
pub fn parse_config(src: &str) -> Result<Config, ConfigError> {
    let doc = KdlDocument::parse(src).map_err(|e| ConfigError::Kdl(e.to_string()))?;
    // Start from the built-in default and treat the document as overrides: a
    // section the user omits keeps its default (so a config with only a
    // `session` still gets the default palette, status bar, and keymap, not
    // empty ones).
    let mut config = crate::built_in_default();
    let mut seen_palette = false;
    let mut seen_status = false;
    let mut seen_keymap = false;
    let mut seen_blocks = false;
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
            "blocks" => {
                dup_check(seen_blocks, "blocks", node, src)?;
                seen_blocks = true;
                config.blocks = decode_blocks(node, src)?;
            }
            "session" => {
                let template = decode_session(node, src)?;
                if config.sessions.iter().any(|s| s.name == template.name) {
                    return Err(decode_err(src, node, &format!("duplicate session `{}`", template.name)));
                }
                config.sessions.push(template);
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
    // Merge onto the default palette: a present `palette` node overrides the
    // named entries it lists and keeps the rest of the built-in colors.
    let mut entries = crate::built_in_default().palette.entries;
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

// --- blocks ---

fn decode_blocks(node: &KdlNode, src: &str) -> Result<BlocksConfig, ConfigError> {
    ensure_no_props(node, src)?;
    // Start from the built-in default and override only the fields the node specifies.
    let mut blocks = BlocksConfig::default();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            match child.name().value() {
                "enabled" => blocks.enabled = bool_arg(child, 0, src, "enabled")?,
                "ok-color" => blocks.ok_color = string_arg(child, 0, src, "ok-color")?.to_string(),
                "fail-color" => blocks.fail_color = string_arg(child, 0, src, "fail-color")?.to_string(),
                other => return Err(decode_err(src, child, &format!("unknown blocks node `{other}`"))),
            }
        }
    }
    Ok(blocks)
}

// --- sessions (declarative defaults, Feature B) ---

fn decode_session(node: &KdlNode, src: &str) -> Result<SessionTemplate, ConfigError> {
    let name = string_arg(node, 0, src, "session name")?.to_string();
    ensure_only_props(node, &["cwd"], src)?;
    let cwd = prop_str(node, "cwd").map(str::to_string);
    let mut windows = Vec::new();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            match child.name().value() {
                "window" => windows.push(decode_window(child, src)?),
                other => {
                    return Err(decode_err(
                        src,
                        child,
                        &format!("unknown session node `{other}` (expected `window`)"),
                    ));
                }
            }
        }
    }
    if windows.is_empty() {
        return Err(decode_err(src, node, &format!("session `{name}` has no windows")));
    }
    Ok(SessionTemplate { name, cwd, windows })
}

fn decode_window(node: &KdlNode, src: &str) -> Result<WindowTemplate, ConfigError> {
    let name = string_arg(node, 0, src, "window name")?.to_string();
    ensure_only_props(node, &["cwd"], src)?;
    let cwd = prop_str(node, "cwd").map(str::to_string);
    let nodes: Vec<&KdlNode> = node.children().map(|d| d.nodes().iter().collect()).unwrap_or_default();
    let layout = match nodes.as_slice() {
        [single] => decode_layout_node(single, src)?,
        [] => {
            return Err(decode_err(src, node, &format!("window `{name}` has no layout (expected one `pane` or `split`)")));
        }
        _ => {
            return Err(decode_err(src, node, &format!("window `{name}` must contain exactly one layout node; wrap multiple panes in a `split`")));
        }
    };
    Ok(WindowTemplate { name, cwd, layout })
}

fn decode_layout_node(node: &KdlNode, src: &str) -> Result<PaneNode, ConfigError> {
    match node.name().value() {
        "pane" => Ok(PaneNode::Leaf(decode_pane(node, src)?)),
        "split" => decode_split(node, src),
        other => Err(decode_err(src, node, &format!("expected `pane` or `split`, got `{other}`"))),
    }
}

fn decode_pane(node: &KdlNode, src: &str) -> Result<PaneTemplate, ConfigError> {
    ensure_only_props(node, &["command", "cwd", "name"], src)?;
    ensure_only_children(node, &[], src)?;
    Ok(PaneTemplate {
        command: prop_str(node, "command").map(str::to_string),
        cwd: prop_str(node, "cwd").map(str::to_string),
        name: prop_str(node, "name").map(str::to_string),
    })
}

fn decode_split(node: &KdlNode, src: &str) -> Result<PaneNode, ConfigError> {
    let dir = match string_arg(node, 0, src, "split direction")? {
        "vertical" => SplitDirection::Vertical,
        "horizontal" => SplitDirection::Horizontal,
        other => {
            return Err(decode_err(src, node, &format!("split direction must be `vertical` or `horizontal`, got `{other}`")));
        }
    };
    ensure_only_props(node, &[], src)?;
    let mut children = Vec::new();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            children.push(decode_layout_node(child, src)?);
        }
    }
    if children.len() < 2 {
        return Err(decode_err(src, node, "`split` needs at least two child layout nodes"));
    }
    Ok(PaneNode::Split { dir, children })
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
    // Start from the built-in default bar and override only the fields/zones the
    // node specifies, so e.g. `status { position "top" }` keeps the default
    // widgets but moves the bar to the top.
    let mut status = crate::built_in_default().status;
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
    fn palette_merges_onto_defaults() {
        // A present `palette` node overrides the entries it lists and keeps the
        // rest of the 11 built-in colors (config = overrides on defaults).
        let cfg = parse_config(r##"palette { accent "#ff0000" }"##).unwrap();
        assert_eq!(cfg.palette.entries.get("accent").map(String::as_str), Some("#ff0000"), "override applied");
        assert_eq!(cfg.palette.entries.get("bg").map(String::as_str), Some("#1D1C19"), "default bg retained");
        assert_eq!(cfg.palette.entries.len(), 11, "merged with the built-in palette");
    }

    #[test]
    fn omitted_sections_fall_back_to_built_in_defaults() {
        // A config with only a session must still get the default palette,
        // status bar, and keymap, not empty ones. (Regression: `parse_config`
        // used to start from `Config::default()`, yielding an empty status bar.)
        let cfg = parse_config(r##"session "dev" { window "w" { pane } }"##).unwrap();
        let d = built_in_default();
        assert_eq!(cfg.palette, d.palette, "omitted palette → default palette");
        assert_eq!(cfg.status, d.status, "omitted status → default status bar");
        assert_eq!(cfg.keymap, d.keymap, "omitted keymap → default keymap");
        assert_eq!(cfg.sessions.len(), 1);
    }

    #[test]
    fn partial_status_keeps_default_zones() {
        // Overriding one field keeps the rest of the default bar.
        let cfg = parse_config(r##"status { position "top" }"##).unwrap();
        let d = built_in_default();
        assert_eq!(cfg.status.position, Position::Top);
        assert_eq!(cfg.status.left.len(), d.status.left.len(), "omitted zones keep defaults");
        assert_eq!(cfg.status.middle.len(), d.status.middle.len());
        assert_eq!(cfg.status.right.len(), d.status.right.len());
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
        // An empty document yields the built-in default config, so the keymap is
        // the full built-in keymap (config = overrides on defaults; an omitted
        // `keymap` node keeps every default binding).
        let cfg = parse_config("").unwrap();
        assert_eq!(cfg.keymap, built_in_keymap());
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
    bind "prefix c" "new_window"
    bind "prefix v" "split_v"
    bind "prefix s" "split_h"
    bind "prefix x" "kill_pane"
    bind "prefix z" "zoom_toggle"
    bind "prefix n" "next_window"
    bind "prefix p" "prev_window"
    bind "prefix &" "kill_window"
    bind "prefix d" "detach"
    bind "prefix [" "enter_copy_mode"
    bind "prefix y" "toggle_sync_panes"
    bind "prefix R" "reload_config"
    bind "prefix 1" "select_window:0"
    bind "prefix 2" "select_window:1"
    bind "prefix 3" "select_window:2"
    bind "prefix 4" "select_window:3"
    bind "prefix 5" "select_window:4"
    bind "prefix 6" "select_window:5"
    bind "prefix 7" "select_window:6"
    bind "prefix 8" "select_window:7"
    bind "prefix 9" "select_window:8"
    bind "prefix h" "select_pane_left"
    bind "prefix j" "select_pane_down"
    bind "prefix k" "select_pane_up"
    bind "prefix l" "select_pane_right"
    bind "Alt+Left" "select_pane_left"
    bind "Alt+Down" "select_pane_down"
    bind "Alt+Up" "select_pane_up"
    bind "Alt+Right" "select_pane_right"
    bind "prefix H" "resize_pane_left"
    bind "prefix J" "resize_pane_down"
    bind "prefix K" "resize_pane_up"
    bind "prefix L" "resize_pane_right"
    bind "prefix Tab" "select_last_window"
    bind "prefix ;" "select_last_pane"
    bind "prefix ," "rename_window"
    bind "prefix ." "rename_pane"
    bind "prefix ?" "show_help"
    bind "prefix :" "command_prompt"
    bind "prefix w" "choose_session"
    bind "prefix W" "choose_tree"
    bind "prefix m" "mark_pane"
    bind "prefix !" "break_pane"
    bind "prefix {" "swap_pane_prev"
    bind "prefix }" "swap_pane_next"
    bind "prefix ]" "paste_buffer"
    bind "prefix =" "choose_buffer"
    bind "prefix M" "toggle_monitor_activity"
    bind "prefix P" "popup"
    bind "prefix q" "close_popup"
    bind "prefix Space" "next_layout"
    bind "prefix <" "prev_prompt"
    bind "prefix >" "next_prompt"
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

    // --- sessions (Feature B) ---

    #[test]
    fn decodes_single_pane_session() {
        let cfg = parse_config(r##"session "dev" { window "main" { pane command="htop" } }"##).unwrap();
        assert_eq!(cfg.sessions.len(), 1);
        let s = &cfg.sessions[0];
        assert_eq!(s.name, "dev");
        assert_eq!(s.windows.len(), 1);
        assert_eq!(s.windows[0].name, "main");
        match &s.windows[0].layout {
            PaneNode::Leaf(p) => assert_eq!(p.command.as_deref(), Some("htop")),
            other => panic!("expected Leaf, got {other:?}"),
        }
    }

    #[test]
    fn decodes_session_cwd_and_pane_overrides() {
        let cfg = parse_config(
            r##"session "x" cwd="~/p" { window "w" { pane cwd="~/p/sub" name="left" } }"##,
        )
        .unwrap();
        assert_eq!(cfg.sessions[0].cwd.as_deref(), Some("~/p"));
        match &cfg.sessions[0].windows[0].layout {
            PaneNode::Leaf(p) => {
                assert_eq!(p.cwd.as_deref(), Some("~/p/sub"));
                assert_eq!(p.name.as_deref(), Some("left"));
            }
            other => panic!("expected Leaf, got {other:?}"),
        }
    }

    #[test]
    fn decodes_split_two_and_three_children() {
        let cfg = parse_config(
            r##"session "s" { window "w" { split vertical { pane; pane command="a"; pane } } }"##,
        )
        .unwrap();
        match &cfg.sessions[0].windows[0].layout {
            PaneNode::Split { dir, children } => {
                assert_eq!(*dir, SplitDirection::Vertical);
                assert_eq!(children.len(), 3);
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn decodes_nested_split() {
        let cfg = parse_config(
            r##"session "s" { window "w" { split vertical { pane; split horizontal { pane; pane } } } }"##,
        )
        .unwrap();
        match &cfg.sessions[0].windows[0].layout {
            PaneNode::Split { children, .. } => match &children[1] {
                PaneNode::Split { dir, children } => {
                    assert_eq!(*dir, SplitDirection::Horizontal);
                    assert_eq!(children.len(), 2);
                }
                other => panic!("expected nested Split, got {other:?}"),
            },
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn session_errors() {
        // no name
        assert!(parse_config(r##"session { window "w" { pane } }"##).is_err());
        // no windows
        assert!(parse_config(r##"session "s" { }"##).is_err());
        // window with zero layout nodes
        assert!(parse_config(r##"session "s" { window "w" { } }"##).is_err());
        // window with two layout nodes (must wrap in a split)
        assert!(parse_config(r##"session "s" { window "w" { pane; pane } }"##).is_err());
        // split with one child
        assert!(parse_config(r##"session "s" { window "w" { split vertical { pane } } }"##).is_err());
        // bad split direction
        assert!(parse_config(r##"session "s" { window "w" { split sideways { pane; pane } } }"##).is_err());
        // unknown pane property
        assert!(parse_config(r##"session "s" { window "w" { pane bogus="x" } }"##).is_err());
        // duplicate session name
        assert!(parse_config(r##"session "s" { window "w" { pane } } session "s" { window "w" { pane } }"##).is_err());
    }

    #[test]
    fn no_sessions_by_default() {
        assert!(parse_config("").unwrap().sessions.is_empty());
        assert!(parse_config(r##"palette { bg "#000000" }"##).unwrap().sessions.is_empty());
    }

    #[test]
    fn decodes_window_cwd() {
        let cfg = parse_config(
            r##"session "x" cwd="~/p" {
                window "api" cwd="~/p/api" { pane }
                window "logs" { pane }
            }"##,
        )
        .unwrap();
        assert_eq!(cfg.sessions[0].windows[0].cwd.as_deref(), Some("~/p/api"));
        assert_eq!(cfg.sessions[0].windows[1].cwd, None);
    }

    /// The complete worked example from `docs/configuration.md` ("A complete
    /// worked example"). Keep the two verbatim-identical: this test is what
    /// stops the doc example from rotting silently when the decoder changes.
    const DOCS_WORKED_EXAMPLE: &str = r##"
// ~/.config/plexy-glass/config.kdl  (Linux)
// ~/Library/Application Support/plexy-glass/config.kdl  (macOS)

palette {
    // Override two built-in names; everything else keeps its default.
    accent "#7aa2f7"
    alert "#f7768e"
}

status {
    position "top"
    refresh "2s"
    left {
        session { style fg="bg" bg="accent" bold=#true; padding 1 1 }
        prefix-indicator content=" PREFIX " { style fg="bg" bg="alert" bold=#true }
        text value=" "
    }
    middle {
        window-list {
            active-style fg="bg" bg="accent" bold=#true
            inactive-style fg="muted" bg="bg_bar"
        }
    }
    right {
        git-branch interval="10s" { style fg="ok" bg="bg_bar" }
        text value=" | " { style fg="muted" bg="bg_bar" }
        cwd max-components=3 { style fg="fg" bg="bg_bar" }
        text value=" | " { style fg="muted" bg="bg_bar" }
        shell command="uname" interval="1m" timeout="2s" { args "-sr"; style fg="info" bg="bg_bar" }
        text value=" | " { style fg="muted" bg="bg_bar" }
        time format="%a %H:%M" interval="30s" { style fg="fg" bg="bg_bar" }
        text value=" " { style fg="muted" bg="bg_bar" }
    }
}

keymap {
    prefix "Ctrl+a"
    inherit-defaults #true
    // New bindings on top of the defaults. The `prefix` token resolves to
    // the chord configured above, so these follow a prefix change:
    bind "prefix g" "popup:lazygit"
    bind "prefix t" "layout:tiled"
    // A second chord for an existing command: F5 also reloads.
    bind "prefix F5" "reload_config"
}

session "dev" cwd="~/projects/app" {
    window "edit" {
        pane command="hx ."
    }
    window "run" {
        split vertical {
            pane command="cargo watch -x check" name="check"
            split horizontal {
                pane name="shell"
                pane command="tail -f log/dev.log" cwd="~/projects/app/log" name="logs"
            }
        }
    }
    window "db" cwd="~/projects/app/db" {
        pane
    }
}
"##;

    #[test]
    fn docs_worked_example_parses() {
        let cfg = parse_config(DOCS_WORKED_EXAMPLE).expect("docs/configuration.md example must parse");
        // Palette: overrides merged onto the built-in entries.
        assert_eq!(cfg.palette.entries.get("accent").map(String::as_str), Some("#7aa2f7"));
        assert_eq!(cfg.palette.entries.get("alert").map(String::as_str), Some("#f7768e"));
        assert_eq!(cfg.palette.entries.get("bg").map(String::as_str), Some("#1D1C19"));
        // Status: position, refresh, and the documented zone shapes.
        assert_eq!(cfg.status.position, Position::Top);
        assert_eq!(cfg.status.refresh, Duration::from_secs(2));
        assert_eq!(cfg.status.left.len(), 3);
        assert_eq!(cfg.status.middle.len(), 1);
        assert_eq!(cfg.status.right.len(), 8);
        match &cfg.status.right[4] {
            WidgetSpec::Shell { command, args, interval, timeout, .. } => {
                assert_eq!(command, "uname");
                assert_eq!(args, &vec!["-sr".to_string()]);
                assert_eq!(*interval, Some(Duration::from_secs(60)));
                assert_eq!(*timeout, Duration::from_secs(2));
            }
            other => panic!("expected Shell, got {other:?}"),
        }
        // Keymap: defaults inherited plus the three documented binds.
        assert!(cfg.keymap.inherit_defaults);
        assert_eq!(cfg.keymap.bindings.len(), 3);
        assert_eq!(
            cfg.keymap.bindings[0],
            KeymapBinding { keys: "prefix g".into(), command: "popup:lazygit".into() }
        );
        // Session template: three windows, nested split in "run".
        assert_eq!(cfg.sessions.len(), 1);
        let s = &cfg.sessions[0];
        assert_eq!(s.name, "dev");
        assert_eq!(s.cwd.as_deref(), Some("~/projects/app"));
        assert_eq!(s.windows.len(), 3);
        assert_eq!(s.windows[2].cwd.as_deref(), Some("~/projects/app/db"));
        match &s.windows[1].layout {
            PaneNode::Split { dir: SplitDirection::Vertical, children } => {
                assert_eq!(children.len(), 2);
                assert!(matches!(
                    &children[1],
                    PaneNode::Split { dir: SplitDirection::Horizontal, children } if children.len() == 2
                ));
            }
            other => panic!("expected vertical Split, got {other:?}"),
        }
    }

    #[test]
    fn window_rejects_unknown_prop() {
        let err = parse_config(r##"session "x" { window "w" bogus="1" { pane } }"##);
        assert!(err.is_err(), "unknown window property must error");
    }

    // --- blocks ---

    #[test]
    fn blocks_defaults_when_absent() {
        let cfg = parse_config("").unwrap();
        let d = BlocksConfig::default();
        assert_eq!(cfg.blocks, d);
        assert!(cfg.blocks.enabled);
        assert_eq!(cfg.blocks.ok_color, "ok");
        assert_eq!(cfg.blocks.fail_color, "alert");
    }

    #[test]
    fn blocks_round_trip_custom_values() {
        let cfg = parse_config(
            r##"blocks { enabled #true; ok-color "#87a987"; fail-color "#c4746e" }"##,
        )
        .unwrap();
        assert!(cfg.blocks.enabled);
        assert_eq!(cfg.blocks.ok_color, "#87a987");
        assert_eq!(cfg.blocks.fail_color, "#c4746e");
    }

    #[test]
    fn blocks_round_trip_hex_literal() {
        let cfg = parse_config(r##"blocks { ok-color "#ff0000"; fail-color "#0000ff" }"##).unwrap();
        assert_eq!(cfg.blocks.ok_color, "#ff0000");
        assert_eq!(cfg.blocks.fail_color, "#0000ff");
    }

    #[test]
    fn blocks_round_trip_palette_names() {
        let cfg = parse_config(r##"blocks { ok-color "ok"; fail-color "alert" }"##).unwrap();
        assert_eq!(cfg.blocks.ok_color, "ok");
        assert_eq!(cfg.blocks.fail_color, "alert");
    }

    #[test]
    fn blocks_partial_node_other_fields_default() {
        // Only `fail-color` set, so `enabled` and `ok_color` stay at defaults.
        let cfg = parse_config(r##"blocks { fail-color "#ff0000" }"##).unwrap();
        assert!(cfg.blocks.enabled, "enabled defaults to true");
        assert_eq!(cfg.blocks.ok_color, "ok", "ok_color defaults to ok");
        assert_eq!(cfg.blocks.fail_color, "#ff0000");
    }

    #[test]
    fn blocks_enabled_false_decodes() {
        let cfg = parse_config(r##"blocks { enabled #false }"##).unwrap();
        assert!(!cfg.blocks.enabled);
    }

    #[test]
    fn blocks_unknown_child_errors() {
        assert!(parse_config(r##"blocks { bogus "x" }"##).is_err());
    }

    #[test]
    fn blocks_duplicate_node_errors() {
        assert!(parse_config(r##"blocks { } blocks { }"##).is_err());
    }

    #[test]
    fn blocks_property_form_rejected() {
        // blocks takes no properties on the node itself.
        assert!(parse_config(r##"blocks enabled=#true"##).is_err());
    }
}
