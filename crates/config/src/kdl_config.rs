//! KDL v2 config decoder. Hand-walks the `kdl` crate's AST into the in-memory
//! `Config` model. Replaces the former serde/TOML loader; the `Config` shape and
//! every downstream consumer are unchanged.

use crate::{
    BlocksConfig, Config, ConfigError, DragModifier, GlyphTier, HintsConfig, KeymapBinding,
    KeymapConfig, MouseConfig, NotificationsConfig, Padding, PaletteConfig, PaneNode, PaneTemplate,
    Position, SessionTemplate, SplitDirection, StatusConfig, StyleConfig, WidgetSpec, WindowTemplate,
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
    let mut seen_hints = false;
    let mut seen_mouse = false;
    let mut seen_notifications = false;
    let mut seen_glyphs = false;
    let mut seen_auto_rename = false;
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
            "hints" => {
                dup_check(seen_hints, "hints", node, src)?;
                seen_hints = true;
                config.hints = decode_hints(node, src)?;
            }
            "mouse" => {
                dup_check(seen_mouse, "mouse", node, src)?;
                seen_mouse = true;
                config.mouse = decode_mouse(node, src)?;
            }
            "notifications" => {
                dup_check(seen_notifications, "notifications", node, src)?;
                seen_notifications = true;
                config.notifications = decode_notifications(node, src)?;
            }
            "glyphs" => {
                dup_check(seen_glyphs, "glyphs", node, src)?;
                seen_glyphs = true;
                config.glyph_tier = decode_glyph_tier(node, src)?;
            }
            "auto-rename" => {
                dup_check(seen_auto_rename, "auto-rename", node, src)?;
                seen_auto_rename = true;
                config.auto_rename = bool_arg(node, 0, src, "#true or #false")?;
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
    // named entries it lists and keeps the rest of the built-in colors. Build
    // just the palette, not the whole default `Config`.
    let mut entries = crate::kanagawa_dragon_palette().entries;
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

/// Parse a duration threshold: `"<int>ms"`, `"<float>s"`, or `"0"` → millis.
/// Returns `None` for unparseable or negative values (caller maps to an error).
fn parse_duration_threshold(s: &str) -> Option<u32> {
    let s = s.trim();
    if s == "0" {
        return Some(0);
    }
    if let Some(ms) = s.strip_suffix("ms") {
        return ms.trim().parse::<u32>().ok();
    }
    if let Some(secs) = s.strip_suffix('s') {
        let secs: f64 = secs.trim().parse().ok()?;
        if secs < 0.0 || !secs.is_finite() {
            return None;
        }
        return Some((secs * 1000.0).round() as u32);
    }
    None
}

fn decode_notifications(node: &KdlNode, src: &str) -> Result<NotificationsConfig, ConfigError> {
    ensure_no_props(node, src)?;
    let mut n = NotificationsConfig::default();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            match child.name().value() {
                "enabled" => n.enabled = bool_arg(child, 0, src, "enabled")?,
                "min-duration" => {
                    let s = string_arg(child, 0, src, "min-duration")?;
                    n.min_duration_ms = parse_duration_threshold(s).ok_or_else(|| {
                        decode_err(
                            src,
                            child,
                            "invalid min-duration (use e.g. \"30s\", \"500ms\", or \"0\")",
                        )
                    })?;
                }
                other => {
                    return Err(decode_err(
                        src,
                        child,
                        &format!("unknown notifications node `{other}`"),
                    ));
                }
            }
        }
    }
    Ok(n)
}

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
                "select-color" => {
                    blocks.select_color = string_arg(child, 0, src, "select-color")?.to_string()
                }
                "sticky-header" => {
                    blocks.sticky_header = bool_arg(child, 0, src, "sticky-header")?
                }
                "duration" => blocks.duration = bool_arg(child, 0, src, "duration")?,
                "duration-threshold" => {
                    let s = string_arg(child, 0, src, "duration-threshold")?;
                    blocks.duration_threshold_ms = parse_duration_threshold(s).ok_or_else(|| {
                        decode_err(
                            src,
                            child,
                            "invalid duration-threshold (use e.g. \"2s\", \"500ms\", or \"0\")",
                        )
                    })?;
                }
                other => return Err(decode_err(src, child, &format!("unknown blocks node `{other}`"))),
            }
        }
    }
    Ok(blocks)
}

fn decode_hints(node: &KdlNode, src: &str) -> Result<HintsConfig, ConfigError> {
    ensure_no_props(node, src)?;
    let mut hints = HintsConfig::default();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            match child.name().value() {
                "enabled" => hints.enabled = bool_arg(child, 0, src, "enabled")?,
                "alphabet" => hints.alphabet = string_arg(child, 0, src, "alphabet")?.to_string(),
                "label-fg" => hints.label_fg = string_arg(child, 0, src, "label-fg")?.to_string(),
                "label-bg" => hints.label_bg = string_arg(child, 0, src, "label-bg")?.to_string(),
                "match-fg" => hints.match_fg = string_arg(child, 0, src, "match-fg")?.to_string(),
                other => {
                    return Err(decode_err(
                        src,
                        child,
                        &format!("unknown hints node `{other}`"),
                    ));
                }
            }
        }
    }
    Ok(hints)
}

fn decode_mouse(node: &KdlNode, src: &str) -> Result<MouseConfig, ConfigError> {
    ensure_no_props(node, src)?;
    let mut mouse = MouseConfig::default();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            match child.name().value() {
                "drag-modifier" => {
                    let s = string_arg(child, 0, src, "drag-modifier")?;
                    mouse.drag_modifier = match s {
                        "alt" => DragModifier::Alt,
                        "ctrl" => DragModifier::Ctrl,
                        other => {
                            return Err(decode_err(
                                src,
                                child,
                                &format!(
                                    "drag-modifier must be \"alt\" or \"ctrl\" (got {other:?}; \"shift\" is unavailable — terminals reserve shift+drag for native selection)"
                                ),
                            ));
                        }
                    };
                }
                other => {
                    return Err(decode_err(src, child, &format!("unknown mouse node `{other}`")));
                }
            }
        }
    }
    Ok(mouse)
}

// --- glyph tier ---

fn decode_glyph_tier(node: &KdlNode, src: &str) -> Result<GlyphTier, ConfigError> {
    let v = node
        .entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
        .ok_or_else(|| decode_err(src, node, "`glyphs` takes one string: unicode | nerd | ascii"))?;
    match v {
        "unicode" => Ok(GlyphTier::Unicode),
        "nerd" => Ok(GlyphTier::Nerd),
        "ascii" => Ok(GlyphTier::Ascii),
        other => Err(decode_err(
            src,
            node,
            &format!("`glyphs`: unknown tier `{other}` (expected unicode | nerd | ascii)"),
        )),
    }
}

// --- sessions (declarative defaults, Feature B) ---

fn decode_session(node: &KdlNode, src: &str) -> Result<SessionTemplate, ConfigError> {
    let name = string_arg(node, 0, src, "session name")?.to_string();
    ensure_only_props(node, &["cwd"], src)?;
    ensure_only_children(node, &["window", "env"], src)?;
    let cwd = opt_prop_str(node, "cwd", src)?.map(str::to_string);
    let env = decode_env_child(node, src)?;
    let mut windows = Vec::new();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            match child.name().value() {
                "window" => windows.push(decode_window(child, src)?),
                "env" => {} // already collected via `decode_env_child`
                other => {
                    return Err(decode_err(
                        src,
                        child,
                        &format!("unknown session node `{other}` (expected `window` or `env`)"),
                    ));
                }
            }
        }
    }
    if windows.is_empty() {
        return Err(decode_err(src, node, &format!("session `{name}` has no windows")));
    }
    // At most one active window per session (deterministic config, not last-wins).
    if windows.iter().filter(|w| w.active).count() > 1 {
        return Err(decode_err(src, node, &format!("session `{name}` has more than one active window")));
    }
    Ok(SessionTemplate { name, cwd, env, windows })
}

fn decode_window(node: &KdlNode, src: &str) -> Result<WindowTemplate, ConfigError> {
    let name = string_arg(node, 0, src, "window name")?.to_string();
    ensure_only_props(node, &["cwd", "active"], src)?;
    let cwd = opt_prop_str(node, "cwd", src)?.map(str::to_string);
    let active = bool_prop(node, "active", src)?.unwrap_or(false);
    let env = decode_env_child(node, src)?;
    // The window's layout is its single non-`env` child node.
    let layout_nodes: Vec<&KdlNode> = node
        .children()
        .map(|d| d.nodes().iter().filter(|n| n.name().value() != "env").collect())
        .unwrap_or_default();
    let layout = match layout_nodes.as_slice() {
        [single] => decode_layout_node(single, false, src)?,
        [] => {
            return Err(decode_err(src, node, &format!("window `{name}` has no layout (expected one `pane` or `split`)")));
        }
        _ => {
            return Err(decode_err(src, node, &format!("window `{name}` must contain exactly one layout node; wrap multiple panes in a `split`")));
        }
    };
    // At most one active pane per window (deterministic config, not last-wins).
    if count_active_leaves(&layout) > 1 {
        return Err(decode_err(src, node, &format!("window `{name}` has more than one active pane")));
    }
    Ok(WindowTemplate { name, cwd, active, env, layout })
}

/// Decode one layout node (`pane` or `split`). `allow_ratio` is true only when
/// the node is a DIRECT child of a `split`: `ratio=` is read by the parent
/// (`decode_split`) and so is permitted in the prop allowlist here; everywhere
/// else a `ratio=` is rejected by the existing `ensure_only_props`.
fn decode_layout_node(node: &KdlNode, allow_ratio: bool, src: &str) -> Result<PaneNode, ConfigError> {
    match node.name().value() {
        "pane" => Ok(PaneNode::Leaf(decode_pane(node, allow_ratio, src)?)),
        "split" => decode_split(node, allow_ratio, src),
        other => Err(decode_err(src, node, &format!("expected `pane` or `split`, got `{other}`"))),
    }
}

fn decode_pane(node: &KdlNode, allow_ratio: bool, src: &str) -> Result<PaneTemplate, ConfigError> {
    let allowed: &[&str] = if allow_ratio {
        &["command", "cwd", "name", "active", "ratio"]
    } else {
        &["command", "cwd", "name", "active"]
    };
    ensure_only_props(node, allowed, src)?;
    ensure_only_children(node, &["env"], src)?;
    Ok(PaneTemplate {
        command: opt_prop_str(node, "command", src)?.map(str::to_string),
        cwd: opt_prop_str(node, "cwd", src)?.map(str::to_string),
        name: opt_prop_str(node, "name", src)?.map(str::to_string),
        active: bool_prop(node, "active", src)?.unwrap_or(false),
        env: decode_env_child(node, src)?,
    })
}

fn decode_split(node: &KdlNode, allow_ratio: bool, src: &str) -> Result<PaneNode, ConfigError> {
    let dir = match string_arg(node, 0, src, "split direction")? {
        "vertical" => SplitDirection::Vertical,
        "horizontal" => SplitDirection::Horizontal,
        other => {
            return Err(decode_err(src, node, &format!("split direction must be `vertical` or `horizontal`, got `{other}`")));
        }
    };
    // A split itself may carry `ratio=` only when it is a direct child of an
    // outer split (its weight in that outer split); `direction` is positional.
    let allowed: &[&str] = if allow_ratio { &["ratio"] } else { &[] };
    ensure_only_props(node, allowed, src)?;
    let mut children = Vec::new();
    let mut weights = Vec::new();
    if let Some(doc) = node.children() {
        for child in doc.nodes() {
            // Each direct child carries its own `ratio=` weight (default 1).
            weights.push(split_child_ratio(child, src)?);
            children.push(decode_layout_node(child, /*allow_ratio=*/ true, src)?);
        }
    }
    if children.len() < 2 {
        return Err(decode_err(src, node, "`split` needs at least two child layout nodes"));
    }
    Ok(PaneNode::Split { dir, children, weights })
}

/// Read a split direct-child's `ratio=` weight: a `u32` >= 1, default 1.
/// `ratio=0` is a decode error (it would make the preorder split formula
/// produce a NaN ratio that poisons persistence, see the v2 spec).
fn split_child_ratio(node: &KdlNode, src: &str) -> Result<u32, ConfigError> {
    match prop_val(node, "ratio") {
        None => Ok(1),
        Some(v) => {
            let i = v
                .as_integer()
                .ok_or_else(|| decode_err(src, node, "`ratio` must be a positive integer"))?;
            let w = u32::try_from(i)
                .map_err(|_| decode_err(src, node, "`ratio` out of range (expected a positive integer)"))?;
            if w < 1 {
                return Err(decode_err(src, node, "`ratio` must be >= 1 (zero weights are not allowed)"));
            }
            Ok(w)
        }
    }
}

/// Count the `active=#true` leaves in a layout subtree (for the at-most-one
/// active-pane-per-window decode check).
fn count_active_leaves(node: &PaneNode) -> usize {
    match node {
        PaneNode::Leaf(p) => usize::from(p.active),
        PaneNode::Split { children, .. } => children.iter().map(count_active_leaves).sum(),
    }
}

/// Decode an optional `env { KEY "value"; … }` child node into ordered
/// `(key, value)` pairs. Mirrors `decode_palette`'s string-map child shape.
fn decode_env_child(node: &KdlNode, src: &str) -> Result<Vec<(String, String)>, ConfigError> {
    // Reject more than one `env` block (session/window/pane all route here).
    // `find_child` only takes the first, so a second block would silently drop its
    // vars, and we'd rather fail loud, matching the decoder's strictness elsewhere.
    let env_count = node
        .children()
        .map(|d| d.nodes().iter().filter(|n| n.name().value() == "env").count())
        .unwrap_or(0);
    if env_count > 1 {
        return Err(decode_err(src, node, "at most one `env` block is allowed"));
    }
    let Some(env_node) = find_child(node, "env") else {
        return Ok(Vec::new());
    };
    ensure_no_props(env_node, src)?;
    let mut out = Vec::new();
    if let Some(doc) = env_node.children() {
        for entry in doc.nodes() {
            let key = entry.name().value().to_string();
            let val = string_arg(entry, 0, src, &format!("env value for `{key}`"))?.to_string();
            out.push((key, val));
        }
    }
    Ok(out)
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

/// An optional string property; absent ⇒ `None`. A present but non-string value
/// is a decode error (consistent with `bool_prop`/`prop_u8`) rather than being
/// silently dropped; a typo like `cwd=5` or `command=#true` should fail loud,
/// not produce a pane with no command / no cwd.
fn opt_prop_str<'a>(
    node: &'a KdlNode,
    key: &str,
    src: &str,
) -> Result<Option<&'a str>, ConfigError> {
    match prop_val(node, key) {
        None => Ok(None),
        Some(v) => v
            .as_string()
            .map(Some)
            .ok_or_else(|| decode_err(src, node, &format!("`{key}` must be a string"))),
    }
}

/// A `key=#true`/`key=#false` bool property; absent ⇒ `None`. A present
/// non-bool value is a decode error (KDL v2 requires `#true`/`#false`).
fn bool_prop(node: &KdlNode, key: &str, src: &str) -> Result<Option<bool>, ConfigError> {
    match prop_val(node, key) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| decode_err(src, node, &format!("`{key}` must be #true/#false"))),
    }
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
            ensure_only_props(node, &["format", "interval", "utc"], src)?;
            ensure_only_children(node, &["style"], src)?;
            WidgetSpec::Time {
                format: prop_str(node, "format").unwrap_or("%H:%M").to_string(),
                interval: prop_dur(node, "interval", src)?,
                style: opt_style(node, "style", src)?,
                utc: bool_prop(node, "utc", src)?.unwrap_or(false),
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
        text value=" ? " { style fg="bg" bg="info" bold=#true }
    }
    middle {
        window-list { active-style fg="fg" bg="accent"; inactive-style fg="muted" bg="bg_bar" }
    }
    right {
        cpu-load { style fg="fg" bg="selection" }
        battery { style fg="fg" bg="bg_bar" }
        hostname { style fg="fg" bg="selection" }
        time format="%H:%M UTC%:z" { style fg="fg" bg="bg_bar" }
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
    bind "prefix /" "history"
    bind "prefix f" "hints"
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
    bind "prefix b" "enter_block_mode"
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
        // Separator decodes (default char '|'); the built-in default uses no
        // dividers at all (lean divider-free right cluster).
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
            PaneNode::Split { dir, children, weights } => {
                assert_eq!(*dir, SplitDirection::Vertical);
                assert_eq!(children.len(), 3);
                // default weights (no `ratio=`) are all 1, aligned to children.
                assert_eq!(weights, &vec![1, 1, 1]);
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
                PaneNode::Split { dir, children, .. } => {
                    assert_eq!(*dir, SplitDirection::Horizontal);
                    assert_eq!(children.len(), 2);
                }
                other => panic!("expected nested Split, got {other:?}"),
            },
            other => panic!("expected Split, got {other:?}"),
        }
    }

    // --- v2: ratios, active, env ---

    #[test]
    fn split_ratio_weights_parse_and_default_to_one() {
        let cfg = parse_config(
            r##"session "s" { window "w" { split vertical { pane ratio=2; pane; pane ratio=3 } } }"##,
        )
        .unwrap();
        match &cfg.sessions[0].windows[0].layout {
            PaneNode::Split { weights, children, .. } => {
                assert_eq!(weights, &vec![2, 1, 3]);
                assert_eq!(weights.len(), children.len());
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn nested_split_can_carry_its_own_ratio() {
        // A `split` that is itself a child of a split may carry `ratio=` (its
        // weight in the OUTER split); the inner children keep their own.
        let cfg = parse_config(
            r##"session "s" { window "w" { split vertical { pane ratio=2; split horizontal ratio=1 { pane; pane } } } }"##,
        )
        .unwrap();
        match &cfg.sessions[0].windows[0].layout {
            PaneNode::Split { weights, children, .. } => {
                assert_eq!(weights, &vec![2, 1], "outer weights are 2:1 (nested split contributes its own ratio=1)");
                match &children[1] {
                    PaneNode::Split { weights, .. } => assert_eq!(weights, &vec![1, 1]),
                    other => panic!("expected nested Split, got {other:?}"),
                }
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn ratio_zero_is_a_decode_error() {
        assert!(parse_config(r##"session "s" { window "w" { split vertical { pane ratio=0; pane } } }"##).is_err());
    }

    #[test]
    fn ratio_on_non_split_child_is_rejected() {
        // A top-level window pane (not a split child) with `ratio=` is rejected
        // by the standalone `decode_pane` allowlist (which omits `ratio`).
        assert!(parse_config(r##"session "s" { window "w" { pane ratio=2 } }"##).is_err());
        // A bare split (the window's top layout) with `ratio=` is likewise rejected.
        assert!(parse_config(r##"session "s" { window "w" { split vertical ratio=2 { pane; pane } } }"##).is_err());
    }

    #[test]
    fn active_window_and_pane_parse() {
        let cfg = parse_config(
            r##"session "s" {
                window "a" { pane }
                window "b" active=#true { split vertical { pane; pane active=#true } }
            }"##,
        )
        .unwrap();
        let s = &cfg.sessions[0];
        assert!(!s.windows[0].active);
        assert!(s.windows[1].active);
        match &s.windows[1].layout {
            PaneNode::Split { children, .. } => {
                assert!(matches!(&children[0], PaneNode::Leaf(p) if !p.active));
                assert!(matches!(&children[1], PaneNode::Leaf(p) if p.active));
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_active_window_is_a_decode_error() {
        assert!(parse_config(
            r##"session "s" { window "a" active=#true { pane } window "b" active=#true { pane } }"##
        )
        .is_err());
    }

    #[test]
    fn duplicate_active_pane_is_a_decode_error() {
        assert!(parse_config(
            r##"session "s" { window "w" { split vertical { pane active=#true; pane active=#true } } }"##
        )
        .is_err());
    }

    #[test]
    fn env_blocks_parse_at_all_levels() {
        let cfg = parse_config(
            r##"session "s" {
                env { RUST_LOG "debug" }
                window "w" {
                    env { TIER "win" }
                    split vertical {
                        pane { env { PORT "8080" } }
                        pane command="x"
                    }
                }
            }"##,
        )
        .unwrap();
        let s = &cfg.sessions[0];
        assert_eq!(s.env, vec![("RUST_LOG".to_string(), "debug".to_string())]);
        assert_eq!(s.windows[0].env, vec![("TIER".to_string(), "win".to_string())]);
        match &s.windows[0].layout {
            PaneNode::Split { children, .. } => match &children[0] {
                PaneNode::Leaf(p) => {
                    assert_eq!(p.env, vec![("PORT".to_string(), "8080".to_string())]);
                }
                other => panic!("expected Leaf, got {other:?}"),
            },
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn env_keeps_declared_order_and_multiple_keys() {
        let cfg = parse_config(
            r##"session "s" { window "w" { pane { env { A "1"; B "2"; C "3" } } } }"##,
        )
        .unwrap();
        match &cfg.sessions[0].windows[0].layout {
            PaneNode::Leaf(p) => assert_eq!(
                p.env,
                vec![
                    ("A".to_string(), "1".to_string()),
                    ("B".to_string(), "2".to_string()),
                    ("C".to_string(), "3".to_string()),
                ]
            ),
            other => panic!("expected Leaf, got {other:?}"),
        }
    }

    #[test]
    fn env_non_string_value_is_a_decode_error() {
        assert!(parse_config(r##"session "s" { window "w" { pane { env { PORT 8080 } } } }"##).is_err());
    }

    #[test]
    fn unknown_child_on_pane_still_rejected() {
        // `env` is now allowed on a pane, but other children are not.
        assert!(parse_config(r##"session "s" { window "w" { pane { bogus "x" } } }"##).is_err());
    }

    #[test]
    fn unknown_child_on_session_still_rejected() {
        // `window` and `env` are the only session children.
        assert!(parse_config(r##"session "s" { window "w" { pane } bogus "x" }"##).is_err());
    }

    #[test]
    fn active_non_bool_is_a_decode_error() {
        // KDL v2 requires `#true`/`#false`; a string is a decode error.
        assert!(parse_config(r##"session "s" { window "w" active="yes" { pane } }"##).is_err());
    }

    #[test]
    fn non_string_optional_props_are_decode_errors() {
        // command/cwd/name with a non-string value used to be silently dropped;
        // they must now fail loud like the other typed props.
        assert!(
            parse_config(r##"session "s" { window "w" { pane command=#true } }"##).is_err(),
            "non-string command must error"
        );
        assert!(
            parse_config(r##"session "s" { window "w" { pane cwd=5 } }"##).is_err(),
            "non-string pane cwd must error"
        );
        assert!(
            parse_config(r##"session "s" { window "w" { pane name=42 } }"##).is_err(),
            "non-string pane name must error"
        );
        assert!(
            parse_config(r##"session "s" cwd=5 { window "w" { pane } }"##).is_err(),
            "non-string session cwd must error"
        );
        assert!(
            parse_config(r##"session "s" { window "w" cwd=#false { pane } }"##).is_err(),
            "non-string window cwd must error"
        );
        // A well-typed string still parses.
        assert!(parse_config(r##"session "s" { window "w" { pane command="echo hi" } }"##).is_ok());
    }

    #[test]
    fn duplicate_env_block_is_a_decode_error() {
        // A second env block on session/window/pane silently dropped its vars;
        // it must now be rejected.
        assert!(
            parse_config(
                r##"session "s" { env { A "1" } env { B "2" } window "w" { pane } }"##
            )
            .is_err(),
            "duplicate session env must error"
        );
        assert!(
            parse_config(
                r##"session "s" { window "w" { pane { env { A "1" } env { B "2" } } } }"##
            )
            .is_err(),
            "duplicate pane env must error"
        );
        // A single env block still parses.
        assert!(
            parse_config(r##"session "s" { window "w" { pane { env { A "1" } } } }"##).is_ok()
        );
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
            PaneNode::Split { dir: SplitDirection::Vertical, children, .. } => {
                assert_eq!(children.len(), 2);
                assert!(matches!(
                    &children[1],
                    PaneNode::Split { dir: SplitDirection::Horizontal, children, .. } if children.len() == 2
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

    /// The v2 worked example from `docs/configuration.md` (~line 514). Kept
    /// verbatim-identical to the docs so a future schema tweak that breaks the
    /// most-copied declarative-sessions snippet fails here. Update both together.
    const DOCS_V2_SECTION_EXAMPLE: &str = r##"
session "dev" cwd="~/projects/app" {
    env { RUST_LOG "debug" }
    window "edit" active=#true {
        pane active=#true command="hx ."
    }
    window "run" cwd="~/projects/app/svc" {
        split vertical {
            pane ratio=2 command="cargo watch -x check" name="check"
            pane ratio=1 { env { PORT "8080" } }
        }
    }
}
"##;

    #[test]
    fn docs_v2_section_example_parses() {
        let cfg =
            parse_config(DOCS_V2_SECTION_EXAMPLE).expect("docs v2 declarative example must parse");
        assert_eq!(cfg.sessions.len(), 1);
        let s = &cfg.sessions[0];
        assert_eq!(s.env, vec![("RUST_LOG".to_string(), "debug".to_string())]);
        assert_eq!(s.windows.len(), 2);
        // window "edit" is the active window, its sole pane is active.
        assert!(s.windows[0].active);
        match &s.windows[0].layout {
            PaneNode::Leaf(p) => assert!(p.active, "edit pane is active"),
            other => panic!("expected leaf, got {other:?}"),
        }
        assert!(!s.windows[1].active);
        // window "run": a 2:1 vertical split; the second pane carries env.
        match &s.windows[1].layout {
            PaneNode::Split { dir: SplitDirection::Vertical, children, weights } => {
                assert_eq!(weights, &vec![2, 1]);
                assert_eq!(children.len(), 2);
                match &children[1] {
                    PaneNode::Leaf(p) => {
                        assert_eq!(p.env, vec![("PORT".to_string(), "8080".to_string())]);
                    }
                    other => panic!("expected leaf, got {other:?}"),
                }
            }
            other => panic!("expected vertical Split, got {other:?}"),
        }
    }

    // --- blocks ---

    #[test]
    fn notifications_defaults() {
        let d = NotificationsConfig::default();
        assert!(d.enabled);
        assert_eq!(d.min_duration_ms, 30_000);
        // Absent node keeps the defaults.
        let cfg = parse_config("").unwrap();
        assert_eq!(cfg.notifications, d);
    }

    #[test]
    fn notifications_round_trip() {
        let cfg = parse_config(r##"notifications { enabled #false; min-duration "60s" }"##).unwrap();
        assert!(!cfg.notifications.enabled);
        assert_eq!(cfg.notifications.min_duration_ms, 60_000);
    }

    #[test]
    fn notifications_min_duration_invalid_errors() {
        assert!(parse_config(r##"notifications { min-duration "soon" }"##).is_err());
    }

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
    fn blocks_annotation_defaults() {
        let d = BlocksConfig::default();
        assert!(d.sticky_header);
        assert!(d.duration);
        assert_eq!(d.duration_threshold_ms, 2000);
    }

    #[test]
    fn blocks_annotation_round_trip() {
        let cfg = parse_config(
            r##"blocks { sticky-header #false; duration #false; duration-threshold "500ms" }"##,
        )
        .unwrap();
        assert!(!cfg.blocks.sticky_header);
        assert!(!cfg.blocks.duration);
        assert_eq!(cfg.blocks.duration_threshold_ms, 500);
    }

    #[test]
    fn blocks_duration_threshold_seconds_and_zero() {
        let a = parse_config(r##"blocks { duration-threshold "1.5s" }"##).unwrap();
        assert_eq!(a.blocks.duration_threshold_ms, 1500);
        let b = parse_config(r##"blocks { duration-threshold "0" }"##).unwrap();
        assert_eq!(b.blocks.duration_threshold_ms, 0);
    }

    #[test]
    fn blocks_duration_threshold_invalid_errors() {
        assert!(parse_config(r##"blocks { duration-threshold "soon" }"##).is_err());
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
    fn blocks_select_color_decodes() {
        let cfg = parse_config(r##"blocks { select-color "#112233" }"##).unwrap();
        assert_eq!(cfg.blocks.select_color, "#112233");
        // Unspecified means the default.
        let d = parse_config(r##"blocks { ok-color "#ff0000" }"##).unwrap();
        assert_eq!(d.blocks.select_color, "#dca561");
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

    // --- glyph tier + auto-rename ---

    #[test]
    fn decodes_glyph_tier_and_auto_rename() {
        let cfg = parse_config(r#"glyphs "nerd"
auto-rename #false"#).expect("decode");
        assert_eq!(cfg.glyph_tier, crate::GlyphTier::Nerd);
        assert!(!cfg.auto_rename);
    }

    #[test]
    fn glyph_tier_defaults_unicode_and_unknown_errors() {
        let cfg = parse_config("").expect("empty decodes");
        assert_eq!(cfg.glyph_tier, crate::GlyphTier::Unicode);
        assert!(cfg.auto_rename);
        let err = parse_config(r#"glyphs "wingdings""#).unwrap_err();
        assert!(err.to_string().contains("glyphs"), "msg names the node: {err}");
    }

    #[test]
    fn decodes_hints_node() {
        let src = r##"
hints {
    alphabet "qwerty"
    label-fg "warn"
    match-fg "#abcdef"
}
"##;
        let cfg = parse_config(src).expect("parse");
        assert_eq!(cfg.hints.alphabet, "qwerty");
        assert_eq!(cfg.hints.label_fg, "warn");
        assert_eq!(cfg.hints.match_fg, "#abcdef");
        assert!(cfg.hints.enabled); // default
    }

    #[test]
    fn hints_defaults_when_absent() {
        let cfg = parse_config("").expect("parse");
        assert_eq!(cfg.hints.alphabet, "asdfghjkl");
        assert!(cfg.hints.enabled);
    }

    #[test]
    fn rejects_unknown_hints_node() {
        let src = "hints {\n  bogus \"x\"\n}\n";
        assert!(parse_config(src).is_err());
    }

    #[test]
    fn decodes_mouse_modifier_ctrl() {
        let cfg = parse_config("mouse {\n  drag-modifier \"ctrl\"\n}\n").expect("parse");
        assert_eq!(cfg.mouse.drag_modifier, DragModifier::Ctrl);
    }

    #[test]
    fn mouse_defaults_to_alt_when_absent() {
        let cfg = parse_config("").expect("parse");
        assert_eq!(cfg.mouse.drag_modifier, DragModifier::Alt);
    }

    #[test]
    fn rejects_shift_and_unknown_mouse_node() {
        assert!(parse_config("mouse {\n  drag-modifier \"shift\"\n}\n").is_err());
        assert!(parse_config("mouse {\n  bogus \"x\"\n}\n").is_err());
    }
}
