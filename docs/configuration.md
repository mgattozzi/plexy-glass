# Configuration

plexy-glass is configured by a single [KDL v2](https://kdl.dev/) file,
`config.kdl`. Everything in it is optional: the file itself, every section,
and every property. Whatever you omit keeps its built-in default, so a config
containing only a `session` node still gets the default palette, status bar,
and keymap.

## The config file

### Location

The path is platform-dependent (resolved via the OS project-directories
convention):

| Platform | Path |
|---|---|
| Linux | `~/.config/plexy-glass/config.kdl` (honors `$XDG_CONFIG_HOME`) |
| macOS | `~/Library/Application Support/plexy-glass/config.kdl` |

### Syntax

The file is KDL **v2**. The practical differences from v1 that bite people:

- Booleans are written `#true` / `#false`, and bare `true` is a **parse
  error**.
- Strings are double-quoted; bare identifiers are allowed where a single
  word suffices (e.g. `split vertical`).
- Multiple nodes on one line are separated with `;`.

### Loading, errors, and live reload

- **Missing file**: the daemon silently uses the built-in defaults. Not an
  error.
- **Error at daemon start**: the daemon logs a warning and runs on the
  built-in defaults. The error message varies by kind:
  - *Decode error* (unknown node, wrong type, duplicate section): names the
    problem with line/column, e.g. `unknown node "foo" (at line 12:1)`.
  - *KDL syntax error* (e.g. bare `true` instead of `#true`): produces only
    the generic `Failed to parse KDL document`, with no location and no
    description, so if your config stops applying after an edit and the
    message names nothing, suspect a v2 syntax slip.
- **Reload**: three triggers, all equivalent on the daemon side:
  - `Ctrl+a R` (the default `reload_config` binding),
  - `:reload` in the command prompt,
  - `plexy-glass reload` from a shell.

  A reload re-reads the file and applies it to **every** live session
  (status bar, palette, keybindings; the reloading client's keymap is rebuilt
  immediately). It also **builds any newly-declared `session`** so it becomes
  attachable; live sessions are never rebuilt (see
  [Reload and switch](#reload-and-switch)).
- **Error on reload**: the daemon does *not* keep the previous config. It
  falls back to the built-in defaults everywhere, since we'd rather run on a
  known-good config than on stale state. All three triggers surface the
  error: `plexy-glass reload` prints `config reload error: …` to the shell,
  and the in-session triggers (`Ctrl+a R`, `:reload`) show `reload failed: …`
  in the status line (the daemon log also records it). Note that the same
  message-fidelity limits apply: decode errors include line/column, and a raw
  KDL syntax error gives only `Failed to parse KDL document`.

Other top-level rules, all enforced by the decoder: unknown top-level nodes
are errors; `palette`, `status`, and `keymap` may each appear at most once;
`session` may appear any number of times but session names must be unique.

## `palette`

Named colors, usable anywhere a style takes a color. Each entry is a child
node: the node name is the color's name, the single argument is its value.

```kdl
palette {
    accent "#7aa2f7"
    alert "#f7768e"
}
```

- The property form (`palette accent="#7aa2f7"`) is rejected; entries are
  child nodes.
- A `palette` node **merges onto** the built-in palette: entries you name are
  overridden, the rest keep their defaults. You can also invent new names.
- Color values must be hex literals of exactly the form `#rrggbb` (six hex
  digits). There is no short form, no `rgb()`, and no CSS color names.
  Palette values may not reference other palette names.

Palette names are usable in every widget style's `fg`/`bg` (see
[Styles and padding](#styles-and-padding)). Note that a style color that is
neither a known palette name nor a `#rrggbb` literal silently resolves to *no
color* (the terminal default). The `fg` and `bg` entries (and `cursor`, falling
back to `accent`) also answer OSC 10/11/12 color queries from applications
running inside panes.

The built-in palette (Kanagawa Dragon):

| Name | Value | | Name | Value |
|---|---|---|---|---|
| `bg` | `#1D1C19` | | `info` | `#949fb5` |
| `bg_bar` | `#282727` | | `alert` | `#c4746e` |
| `fg` | `#c8c093` | | `warn` | `#c4b28a` |
| `accent` | `#737c73` | | `muted` | `#b6927b` |
| `highlight` | `#b6927b` | | `ok` | `#87a987` |
| `selection` | `#393836` | | | |

## `status`

The status bar. A present `status` node overrides only what it specifies, so
`status { position "top" }` moves the default bar to the top without touching
its widgets.

```kdl
status {
    position "bottom"   // "top" or "bottom"; default "bottom"
    refresh "5s"        // duration string; default 5s
    left { /* widgets */ }
    middle { /* widgets */ }
    right { /* widgets */ }
}
```

- `position` is `"top"` or `"bottom"`. Default: `"bottom"`.
- `refresh` is the bar's base refresh period, as a
  [humantime](https://docs.rs/humantime) duration string (`"5s"`, `"500ms"`,
  `"2m"`, `"1h 30m"`). Default: `5s`. Every duration-valued property below
  uses the same format.
- `left` / `middle` / `right` are the three zones. Each contains widget
  nodes, rendered in order. Zones take no properties.

Widgets that declare an `interval` are refreshed on their own period by a
background task; widgets without one are re-evaluated when the bar redraws.

### Styles and padding

Most widgets accept a `style` child node with up to six fields:

| Field | Type | Meaning |
|---|---|---|
| `fg` | string | Foreground: a palette name or `#rrggbb` literal |
| `bg` | string | Background: a palette name or `#rrggbb` literal |
| `bold` | `#true`/`#false` | Bold |
| `italic` | `#true`/`#false` | Italic |
| `underline` | `#true`/`#false` | Underline |
| `reverse` | `#true`/`#false` | Reverse video |

Two equivalent spellings (children override like-named properties if you mix
them):

```kdl
session { style fg="bg" bg="accent" bold=#true }            // property form
session { style { fg "bg"; bg "accent"; bold #true } }      // child form
```

`style` is *required* on `prefix-indicator` and `attached-clients`, and
`window-list` requires both `active-style` and `inactive-style`. On every
other widget it is optional (absent = unstyled). Unknown style fields are
errors.

`padding` is accepted by the `session` widget *only*: a child node with two
integer arguments (0–255), `padding <left> <right>`. Absent = `0 0`.

### Widgets

Unknown widget names, unknown properties, and unknown child nodes are all
decode errors, so typos fail loudly (and on reload, loudly means falling
back to the default config, see above).

#### `session`

The current session name. Optional `style`, and the only widget that takes
`padding`.

```kdl
session { style fg="bg" bg="accent" bold=#true; padding 1 1 }
```

#### `window-list`

The window strip: index + name per window, with monitor flags appended after
the name. Requires both `active-style` (the focused window) and
`inactive-style`. No properties.

```kdl
window-list { active-style fg="fg" bg="accent"; inactive-style fg="muted" bg="bg_bar" }
```

Monitor flags (only ever shown on a non-current window; cleared when you view
the window):

| Flag | Meaning |
|------|---------|
| `!`  | bell (`monitor-bell`) |
| `#`  | activity (`monitor-activity`) |
| `~`  | silence, no output for the configured period (`monitor-silence`) |
| `✓`  | a command completed with exit 0, or no exit code (`monitor-command`) |
| `✗`  | a command completed with a nonzero exit (`monitor-command`) |

Each flagged event also raises a transient status-line message (e.g.
`activity in window 2 (api)`, `bell in window 3 (logs)`,
`silence in window 1 (build)`, `done in window 2 (api): exit 1`). Messages are
edge-triggered, so they fire once when the flag turns on, not continuously.

#### `prefix-indicator`

Shows `content` while the prefix key is armed (by any attached client),
nothing otherwise. It doubles as the mode badge, which takes precedence over
the armed indicator: ` COPY ` while the active pane is in copy mode
(click to exit), ` SYNC ` while sync-panes is on (click to toggle), and
` Z ` while the active pane is zoomed. Both `content` (string property) and
`style` are required.

```kdl
prefix-indicator content=" PFX " { style fg="bg" bg="highlight" bold=#true }
```

#### `attached-clients`

The number of attached clients, hidden while fewer than `min-count` (integer,
default `2`) are attached. `style` is required.

```kdl
attached-clients min-count=2 { style fg="fg" bg="bg_bar" }
```

#### `time`

The current time. `format` is a string in `strftime` syntax (default `"%H:%M"`);
`interval` is an optional duration; `style` optional. `utc=#true` formats in UTC
(so `%Z` renders `UTC`) instead of the local timezone.

```kdl
time format="%a %H:%M" interval="30s" { style fg="fg" bg="bg_bar" }
time format="%H:%M %Z" utc=#true { style fg="fg" bg="bg_bar" }   // 14:32 UTC
```

#### `hostname`

The machine's hostname. Optional `interval` (duration) and `style`.

```kdl
hostname interval="5m"
```

#### `cwd`

The active pane's working directory (live, via OSC 7). `max-components`
(integer 0–255, optional) keeps only the last N path components; absent shows
the full path. Optional `style`.

```kdl
cwd max-components=3 { style fg="fg" bg="bg_bar" }
```

#### `git-branch`

The current git branch of the active pane's cwd. Optional `interval`
(duration) and `style`.

```kdl
git-branch interval="10s" { style fg="ok" bg="bg_bar" }
```

#### `battery`

Battery charge state. Optional `interval` (duration) and `style`.

```kdl
battery { style fg="fg" bg="bg_bar" }
```

#### `cpu-load`

CPU load. Optional `interval` (duration) and `style`.

```kdl
cpu-load interval="2s" { style fg="fg" bg="bg_bar" }
```

#### `memory`

Memory usage. Optional `interval` (duration) and `style`.

```kdl
memory interval="2s" { style fg="fg" bg="bg_bar" }
```

#### `text`

A literal string. `value` (string property) is required and `style` is
optional. The default bar uses `text value=" | "` as its separators.

```kdl
text value=" | " { style fg="muted" bg="bg_bar" }
```

#### `separator`

A single separator character. `char` must be exactly one character (string
property, default `"|"`) and `style` is optional.

```kdl
separator char="•" { style fg="muted" }
```

#### `shell`

Runs an external command and shows its output. `command` (string property) is
required, and positional string arguments go in an `args` child node. Optional
`interval` (duration), `timeout` (duration, default `1s`), and `style`.

```kdl
shell command="uname" interval="1m" timeout="2s" { args "-sr"; style fg="info" }
```

## `keymap`

```kdl
keymap {
    prefix "Ctrl+a"          // default "Ctrl+a"
    inherit-defaults #true   // default #true
    bind "prefix g" "popup:lazygit"
}
```

- `inherit-defaults`: when `#true` (the default), we load the built-in
  binding table first and apply your `bind` lines on top, so a `bind` with
  the same chord sequence **overrides** that single default. When `#false`,
  only your `bind` lines exist.
- `prefix`: a single chord (e.g. `prefix "Ctrl+b"`). The word `prefix` in a
  `bind` chord sequence resolves to this chord, and the built-in defaults are
  declared prefix-relative (`prefix c`, `prefix v`, …), so changing `prefix`
  retargets every inherited default and every token-form binding at once. A
  binding that spells out a literal chord (`bind "Ctrl+a g" …`) is absolute
  and does not follow `prefix`. If the value is empty, unparseable, or more
  than one chord, the daemon logs a warning and falls back to `Ctrl+a`, so a
  config typo never bricks the session.
- `bind "<chord sequence>" "<command>"`: two string arguments. A binding
  whose chord or command fails to parse is **skipped with a logged warning**,
  and it does not fail the whole config.

### Chord grammar

A *chord* is zero or more modifiers and one key, joined with `+`:
`Ctrl+a`, `Alt+Left`, `Ctrl+Shift+F5`, `x`. A *chord sequence* is one or more
chords separated by spaces: `"Ctrl+a c"` means press `Ctrl+a`, then `c`. The
literal `+` key can be bound too, write it as the last token, e.g. `"+"` or
`"Ctrl++"` (the trailing `+` is the key, not a separator).

The bare word `prefix` (case-insensitive: `prefix`, `Prefix`, `PREFIX`) is a
chord alias that resolves to `keymap.prefix`. It is valid at any position in
a sequence (`"prefix c"`, `"prefix Space"`, `"Ctrl+x prefix"`), and the
built-in defaults all use it, which is what makes them follow a custom
prefix.

An armed prefix **waits indefinitely** for the rest of its chord (tmux
semantics: there is no timeout, and the `prefix-indicator` status widget
stays lit while armed). Pressing a key that doesn't continue any binding
cancels the pending chord; the cancelling key is consumed, not forwarded.

**Modifiers** (each token is accepted capitalized, lowercase, or uppercase,
so `Ctrl`, `ctrl`, and `CTRL` all work; the long form is `Control`/`control`
only):

| Token | Aliases | Meaning |
|---|---|---|
| `Ctrl` | `Control` | Control |
| `Alt` | `Meta` | Alt (Meta is treated as Alt) |
| `Shift` | | Shift |
| `Super` | `Cmd` | Super / Command |
| `Hyper` | | Hyper |

**Keys** (names are case-insensitive):

- Arrows: `Up`, `Down`, `Left`, `Right`
- `Home`, `End`
- `PageUp` (alias `PgUp`), `PageDown` (aliases `PgDn`, `PgDown`)
- `Insert` (alias `Ins`), `Delete` (alias `Del`)
- `Tab`, `Enter` (alias `Return`), `Backspace` (alias `BS`),
  `Escape` (alias `Esc`), `Space`
- Function keys `F1`–`F12`
- Any single character: `a`, `?`, `{`, … Single characters are
  **case-sensitive** (`Ctrl+a H` and `Ctrl+a h` are different bindings; an
  uppercase letter implies Shift on the terminal side).

### Binding commands

The full command vocabulary. Three commands take an argument after a colon
(`layout:<name>`, `popup:<command line>`, `select_window:<n>`); everything
after the **first** colon is the argument, so popup command lines may
themselves contain colons and spaces.

**Windows**

| Command | Action |
|---|---|
| `new_window` | New window (at the session cwd) |
| `kill_window` | Kill the active window |
| `next_window` / `prev_window` | Cycle windows forward / back |
| `select_window:<n>` | Jump to window *n* (**zero-based**: the default `Ctrl+a 1` is `select_window:0`) |
| `select_last_window` | Toggle to the previously active window |
| `rename_window` | Open the window-rename overlay |

**Panes and splits**

| Command | Action |
|---|---|
| `split_v` | Split side-by-side (vertical divider) |
| `split_h` | Split stacked (horizontal divider) |
| `kill_pane` | Kill the active pane |
| `zoom_toggle` | Zoom / unzoom the active pane |
| `select_pane_left` / `select_pane_right` / `select_pane_up` / `select_pane_down` | Focus the pane in that direction |
| `select_next_pane` / `select_prev_pane` | Focus the next / previous pane in layout order |
| `select_last_pane` | Toggle to the previously focused pane |
| `resize_pane_left` / `resize_pane_right` / `resize_pane_up` / `resize_pane_down` | Move the nearest split border |
| `rename_pane` | Open the pane-rename overlay |

**Layouts**

| Command | Action |
|---|---|
| `layout:<name>` | Apply a preset layout: `even-horizontal`, `even-vertical`, `main-horizontal`, `main-vertical`, or `tiled` (for the `main-*` presets the active pane takes the main slot) |
| `next_layout` | Cycle to the next preset (remembered per window); default `Ctrl+a Space` |

**Pane mobility**

| Command | Action |
|---|---|
| `mark_pane` | Mark / unmark the active pane |
| `break_pane` | Break the active pane out to its own window |
| `join_pane` | Join the marked pane into the active window (side-by-side) |
| `swap_pane_next` / `swap_pane_prev` | Swap the active pane with its layout neighbor |
| `swap_marked_pane` | Swap the active pane with the marked pane (works across windows of the same session; focus and zoom follow the slot, mark is preserved) |

**Popups**

| Command | Action |
|---|---|
| `popup` | Open a floating scratch-shell popup |
| `popup:<command line>` | Open a popup running that command (e.g. `popup:lazygit`) |
| `close_popup` | Close the popup |

**Command blocks**

| Command | Action |
|---|---|
| `prev_prompt` | Scroll the viewport back to the previous prompt (default `prefix <`) |
| `next_prompt` | Scroll the viewport forward to the next prompt (default `prefix >`) |
| `copy_output` | Yank the last completed block's output to clipboard + paste buffer (no default chord) |

**Copy and paste**

| Command | Action |
|---|---|
| `enter_copy_mode` | Enter copy mode |
| `paste_buffer` | Paste the newest paste buffer |
| `choose_buffer` | Open the choose-buffer overlay |

**Sessions and overlays**

| Command | Action |
|---|---|
| `choose_session` | Visual session picker |
| `choose_tree` | Choose-tree (session → window → pane) |
| `command_prompt` | Open the `:` command prompt |
| `show_help` | Keybinding help overlay |
| `detach` | Detach this client |

Every overlay (the pickers, choose-tree, help, the rename prompts, the
command prompt) cancels on `Esc`, and that works on *all* terminals now,
including legacy / Terminal.app-class ones that send a bare `\x1b` byte, not
just Kitty-capable terminals. While an overlay is open it is modal:
keystrokes go to the overlay, and pastes / raw bytes are swallowed, so
nothing reaches the pane's shell behind it.

**Misc**

| Command | Action |
|---|---|
| `toggle_sync_panes` | Toggle synchronized input to all panes in the window |
| `toggle_monitor_activity` | Toggle activity monitoring for the window |
| `toggle_monitor_bell` | Toggle bell monitoring for the window |
| `toggle_monitor_command` | Toggle command-completion monitoring for the window |
| `set_monitor_silence:<secs>` | Arm silence monitoring after `<secs>` (`:0` or no arg = off) |
| `reload_config` | Reload `config.kdl` |
| `cancel` | Do nothing (an explicit no-op, e.g. to neuter a default chord) |

## `blocks`

Controls the block exit-status border feature (see
[docs/command-blocks.md, Exit status on the border](command-blocks.md#exit-status-on-the-border)
for the user-facing description of what it does).

```kdl
blocks {
    enabled #true            // #false disables all block border painting
    ok-color "ok"            // palette name or #rrggbb; default: palette `ok`
    fail-color "alert"       // palette name or #rrggbb; default: palette `alert`
    select-color "#dca561"   // palette name or #rrggbb; the block-mode bracket
}
```

- `enabled` is `#true` (default) or `#false`. When `#false`, no block-status
  coloring is performed and the left border is always plain. All other
  `blocks` properties are still decoded normally, so `enabled #false` is the
  way to opt out of the feature without deleting the rest of the node.
- `ok-color` is the foreground color for border rows belonging to a block
  that exited with code 0. It takes a palette name (e.g. `"ok"`, `"accent"`)
  or a `#rrggbb` hex literal (e.g. `"#87a987"`). Default: `"ok"` (the
  built-in palette entry `#87a987`).
- `fail-color` is the foreground color for rows belonging to a block that
  exited with a nonzero code, and it also triggers the `│` → `▌` glyph on
  plain vertical segments. Same value forms as `ok-color`. Default: `"alert"`
  (the built-in palette entry `#c4746e`).
- `select-color` is the color of the block-mode selection bracket (`┏┃┗`)
  drawn around the selected command block (see
  [docs/command-blocks.md, Block mode](command-blocks.md#block-mode)). Same
  value forms as `ok-color`. Default: `"#dca561"`. Note that this one is
  independent of `enabled`: the bracket is part of block-mode navigation, so
  it is drawn even when block-status border coloring is turned off.

A bad color value (unknown palette name or malformed hex) falls back to the
built-in default for that field, so it never disables the feature and is not
a hard error. Both fields resolve through the same color lookup the
status-bar widget styles use, so custom `palette` entries are valid values.

The `blocks` node live-reloads with the config (`Ctrl+a R` /
`plexy-glass reload`), same as `palette` and `status`. A reload that supplies
new colors shows up on the next rendered frame.

## `glyphs`

```kdl
glyphs "unicode"   // "unicode" (default) | "nerd" | "ascii"
```

Controls which glyph repertoire the status bar uses for icons and separators.
Note that there is **no runtime font detection**, plexy-glass does not inspect
which font your terminal is using, so you declare the tier that matches your
setup.

| Tier | What it means |
|---|---|
| `"unicode"` | Box-drawing characters and simple Unicode symbols. Renders on any font. **Default.** |
| `"nerd"` | [Nerd Font](https://www.nerdfonts.com/) icons + powerline separators. Requires a Nerd Font patched font to be active in your terminal; unpatched fonts render these codepoints as tofu (☐). |
| `"ascii"` | Lowest-common-denominator ASCII fallback. All icons are short text labels; no special characters. |

### Icon table

The icons each widget renders in each tier:

| Role | `unicode` | `nerd` (codepoint) | `ascii` |
|---|---|---|---|
| Separator (left-cluster cap) | *(none)* | `` U+E0B0 | *(none)* |
| Separator (right-cluster cap) | *(none)* | `` U+E0B2 | *(none)* |
| `session` | `◆` U+25C6 | `` U+EBC8 | `*` |
| `prefix-indicator` | `▲` U+25B2 | `` U+F4A1 | `^` |
| `git-branch` | `⎇` U+2387 | `` U+E0A0 | `git:` |
| `cwd` | `▸` U+25B8 | `` U+F07B | `>` |
| `time` | `◷` U+25F7 | `` U+F017 | *(empty)* |
| `cpu-load` | `λ` U+03BB | `` U+F2DB | `cpu:` |
| `memory` | `≣` U+2263 | `` U+EFC5 | `mem:` |
| `battery` | `▮` U+25AE | `` U+F240 | `bat:` |
| `hostname` | `@` | `` U+F108 | `@` |
| `attached-clients` | `^` | `` U+F0C0 | `cl:` |

Icons lead each widget's content and are separated from it by a space in the
rendered output. On the `ascii` tier, the `time` widget renders no prefix icon
at all.

### Powerline separators

**Powerline separators are only inserted on the `nerd` tier.** On `unicode` and
`ascii` tiers the left, middle, and right zones are plain flat segments with no
arrows between widget groups.

On the `nerd` tier, adjacent widget groups with different background colors get
a powerline arrow between them, the filled-triangle chevron (`` / ``) that
points in the direction of the cluster. A trailing cap is also added at the
outer edge of the left cluster, and a leading cap at the outer edge of the
right cluster.

To use powerline separators, set `glyphs "nerd"` in your config and give your
status-bar widget `style` nodes background colors. Widget groups that share a
background color are joined directly, and the separator between them is
omitted (it would be invisible anyway).

### Worked example

```kdl
// ~/.config/plexy-glass/config.kdl  (Linux)
// ~/Library/Application Support/plexy-glass/config.kdl  (macOS)

glyphs "nerd"   // requires a Nerd Font in your terminal

status {
    left {
        session { style fg="bg" bg="accent" bold=#true; padding 1 1 }
        prefix-indicator content=" PFX " { style fg="bg" bg="highlight" bold=#true }
    }
    middle {
        window-list {
            active-style fg="fg" bg="accent" bold=#true
            inactive-style fg="muted" bg="bg_bar"
        }
    }
    right {
        // The shipped default right cluster (offline, stable width).
        cpu-load { style fg="fg" bg="selection" }
        battery { style fg="fg" bg="bg_bar" }
        hostname { style fg="fg" bg="selection" }
        // Optional weather (NOT shipped by default — it makes a network call):
        // self-contained wttr.in curl with condition icon, temperature, and the
        // IP-resolved city. `&u` selects °F (drop it for °C); no API key.
        shell command="bash" interval="30m" timeout="5s" {
            args "-c" "curl -sfL 'wttr.in/?format=%c+%t+%l&u' | tr -d '+'"
            style fg="bg" bg="accent"
        }
        // Far-right clock: 24-hour LOCAL time with the location's UTC offset
        // (e.g. `14:42 UTC-04:00`). Use `utc=#true` for absolute UTC instead.
        time format="%H:%M UTC%:z" { style fg="fg" bg="bg_bar" }
    }
}
```

> The built-in default right cluster is **CPU · battery · hostname · clock** (the
> clock shows local time plus the location's UTC offset). We deliberately don't
> ship weather by default because the `shell` widget makes a periodic outbound
> request to `wttr.in` (which infers your location from your IP), so add it
> yourself as shown above. Widget groups get a space of internal padding on
> each side so content doesn't crowd the powerline arrows.

See [`auto-rename`](#auto-rename) for the companion setting that controls
whether unpinned windows derive their name from the active pane.

## `auto-rename`

```kdl
auto-rename #true   // default; #false pins names to their literal value
```

When `#true` (the default), a window whose name was *not* set explicitly is
**derived** from its active pane and updates live. The name is the first of:

1. the **running command**: the first word of the command currently executing
   in the active pane (basename only, e.g. `/usr/bin/cargo build` → `cargo`),
   detected via OSC 133 shell integration;
2. the **current directory**: the basename of the active pane's working
   directory (OSC 7), e.g. `~/projects/api` → `api`;
3. the **shell**: the basename of `$SHELL` (e.g. `sh`, `zsh`), as a fallback.

A window's name is **pinned** (and the toggle no longer affects it) the moment
it is given a real name, either by an explicit `:rename-window` / `Ctrl+a ,`
rename or by a declared `window "name" { … }` in a `session` node. Pinned
names are shown verbatim regardless of `auto-rename`.

With `auto-rename #false`, an unnamed window simply shows its shell basename and
never tracks the running command or directory.

The pinned/auto state is persisted with the session, so a derived-name window
restored after a daemon restart keeps deriving its name (a renamed window stays
pinned). `auto-rename` is read fresh on every status-bar render, so toggling it
via reload (`Ctrl+a R` / `plexy-glass reload`) takes effect immediately.

## `session` — declarative sessions

`session` nodes declare sessions that the daemon builds fresh at boot. For a
declared name the config wins over the saved on-disk state, so the layout
always matches the template, and undeclared session names still restore
from disk as usual.

```kdl
session "dev" cwd="~/projects/app" {
    env { RUST_LOG "debug" }              // session-level env (inherited)
    window "edit" active=#true {
        pane active=#true command="hx ."
    }
    window "run" cwd="~/projects/app/svc" {
        split vertical {
            pane ratio=2 command="cargo watch -x check" name="check"
            pane ratio=1 { env { PORT "8080" } }   // interactive, with env
        }
    }
}
```

Node by node:

- `session "<name>" [cwd="<dir>"]` must contain at least one `window`. May
  contain an `env` block. Duplicate session names are errors.
- `window "<name>" [cwd="<dir>"] [active=#true]` must contain **exactly one**
  layout node (`pane` or `split`); to put several panes in a window, wrap them
  in a `split`. May contain an `env` block. `active=#true` makes this the
  session's focused window on build, and at most one window per session may
  be active (a second is a decode error); the default is the first window.
- `split <direction> { … }` takes a direction, `vertical` (children
  side-by-side) or `horizontal` (children stacked), and two or more children,
  each a `pane` or a nested `split`. No properties of its own.
- `pane [command="…"] [cwd="<dir>"] [name="…"] [active=#true]` is a leaf.
  `command` is a shell command line, run via your default shell's `-c`
  (`$SHELL`, falling back to `/bin/sh`); without it the pane is an interactive
  shell. May contain an `env` block. `active=#true` makes this its window's
  focused pane on build, and at most one pane per window may be active (a
  second is a decode error); the default is the DFS-leftmost pane.

### Split ratios (`ratio=`)

Each **direct child of a `split`** (a `pane`, or a nested `split`) may carry
a `ratio=<n>`, a relative **weight** (a positive integer, default `1`).
Within a split the children divide space in proportion to their weights:

```kdl
split vertical {
    pane ratio=2     // gets 2/3 of the width
    pane ratio=1     // gets 1/3
}
```

- Default weights (no `ratio=`) give an **even** split: `1/N` to each of `N`
  children (so a two-pane split is 50/50, a flat three-pane split is
  33/33/33).
- A nested split's weight is its **own** `ratio=` in the parent split, *not*
  the number of leaves it contains:
  `split vertical { pane ratio=2; split horizontal ratio=1 { pane; pane } }`
  makes the outer split 2:1 regardless of the inner split's pane count.
- `ratio=0` is a decode error (zero weights have no meaning). `ratio=` is
  only valid on a direct split child; on a window's top-level pane, or on a
  `split`/`pane` not inside a split, it is rejected.
- A window too small to honor a ratio degrades gracefully (each ratio clamps
  to `[0.1, 0.9]` and each pane to at least one cell); it never panics.

### Per-pane environment (`env`)

A `session`, `window`, or `pane` may carry an `env` block, a string map of
environment variables:

```kdl
pane command="./run" {
    env {
        PORT "8080"
        RUST_LOG "debug"
    }
}
```

The effective environment for a pane is the **overlay** of session → window →
pane env (a later level overrides an earlier one per key), applied **on top
of the daemon's inherited environment**, so `PATH`, `HOME`, `TERM`, `SHELL`
and the rest survive; only the declared keys are added or overridden. Note
that unlike an inline `command="FOO=bar cmd"` (which only affects that
command), a structured `env` also applies to **interactive** panes (no
`command`).

### Working-directory precedence

Every `cwd` accepts `~` for your home directory. A pane spawns at the first
of:

1. the pane's own `cwd`,
2. the window's `cwd`,
3. the session's `cwd`,
4. the daemon's working directory.

A window's `cwd` (or, absent that, the session's) is the window's permanent
**home base**: every later split or interactively created pane in that window
spawns there too, *not* at the active pane's live (OSC 7) location. A window
created interactively with `Ctrl+a c` anchors to the session cwd. Note that
popups are the one exception: a popup spawns at the active pane's **live**
OSC-7 cwd (falling back to the home base), so that a popup acts on the
current context.

### Reload and switch

Reloading the config (`Ctrl+a R`, `:reload`, or `plexy-glass reload`) **re-reads
the templates**: any session newly declared in the edited config is built
immediately, so `:switch <new>` / `attach -n <new>` find it.

- **Live sessions are never rebuilt by a reload.** Rebuilding would kill the
  panes and processes you have running. A changed template for a session that
  is already live takes effect on its **next** build, after you kill it and
  reattach. Reload makes new templates *available*; it never destroys live
  work.
- A declared name you **remove** from the config is left alone if it is
  currently live (your running session is not killed by an edit); it simply
  stops being auto-created on future boots/reloads.

`:switch <name>` (the command prompt and pickers) **auto-creates** a declared
session that is not yet running: if `<name>` is declared in the config but no
session by that name is live, it is built from the template, then switched to.
An unknown, undeclared name still reports `no session: <name>`. (The headless
`plexy-glass cmd "switch …"` path remains interactive-only and is refused;
auto-create applies only to an attached client's switch.)

## Persistence

Each session is saved to a per-session JSON file under
`$XDG_STATE_HOME/plexy-glass/sessions/<name>.json` (falling back to
`~/.local/state/...`). Saves are atomic (write-to-temp + fsync + rename) and run
off the main loop. A daemon restart restores from these files (attach the same
name), and `plexy-glass kill` deletes the file.

What is restored:

- **Layout**: windows, panes, split directions and ratios, window/pane names,
  the per-window home cwd, the sync-input flag, and the active window/pane.
- **Per-pane cwd**: restored panes spawn a *fresh* shell at the saved working
  directory.
- **Scrollback + command-block marks**: each pane's recent history (text,
  colors/attributes, and the OSC 133 block marks) comes back as the pane's
  scrollback, and the fresh shell starts below it, so block navigation, the
  exit-status border colors, and `capture --last-command` all work on the
  restored history immediately. See [command-blocks.md](command-blocks.md).

When it saves: persistence is *opportunistic*. The session file is rewritten
at the next *structural* change (a split, a window/pane add / remove /
rename, a resize that clamps ratios), debounced by ~1.5s. It is *not*
written on every output line, and *not* flushed on detach, so scrollback is
captured with whatever history existed at the moment of the next structural
save.

Scrollback caveats:

- Only the most recent 1000 rows per pane are persisted, and older history
  is truncated on restore.
- Rows come back at their saved width and are *not* reflowed until the
  pane's first resize.
- OSC 8 hyperlinks are not persisted: restored text keeps its styling but
  loses link clickability.
- The alt screen (full-screen TUIs) is never persisted; the saved rows are
  always the main screen.

## Choose-tree (`Ctrl+a W`)

`Ctrl+a W` (or `:tree`) opens a floating session → window → pane drill-down.
Each row is indented by level; the `*` marker flags the current session path.

### Navigation

| Key | Action |
|---|---|
| `j` / `↓` / `Ctrl+n` | Move selection down |
| `k` / `↑` / `Ctrl+p` | Move selection up |
| `g` / `Home` | Jump to first row |
| `G` / `End` | Jump to last row |
| `Enter` | Switch to the selected session / window / pane |
| `h` / `←` | Collapse the selected session or window (hides descendants); on a pane row, folds its parent window |
| `l` / `→` | Expand the selected session or window |
| `/` | Enter filter mode (incremental search) |
| `x` | Kill the selected session / window / pane (prompts `y`/`n`) |
| `r` | Rename the selected session, window, or pane inline |
| `Esc` | Close the tree |

### Filter mode (`/`)

Pressing `/` enters filter mode. Typed text narrows the tree in real time
(case-insensitive substring on the row label), and ancestors of matching rows
stay visible so you can still see the path. `Enter` returns to Navigate
keeping the active filter (a `(filtered)` hint appears in the footer); `Esc`
clears the filter and returns to Navigate.

### Session rename

Pressing `r` on a session row opens the same inline editor used for window and
pane rename. The edit buffer is primed with the current session name; edit it
and press `Enter` to commit. The rename propagates live everywhere: the
registry re-keys, and the status bar, `plexy-glass list`, and `-n` resolution
all follow immediately. `Esc` cancels.

Session names must be 1–64 characters of ASCII letters, digits, `-`, or `_`,
with no spaces (unlike window/pane rename, which allows any text). An invalid
name, a collision with a live session, or a config-declared name is refused:
a transient status message explains why and the tree is left unchanged.

**Declarative sessions note:** renaming a session that was built from a
`session` template in `config.kdl` decouples it from the template, so at the
next daemon boot the template name gets built fresh as a new session and the
renamed session restores from its own saved file under the new name. The
reverse is refused outright: renaming any session *to* a declared name fails
with `'<name>' is a declared session name — choose another`, since otherwise
the template would silently shadow that session's saved state at the next
daemon boot.

## The command prompt

`Ctrl+a :` opens a one-line command prompt. Type a verb (Tab completes it)
and arguments separated by spaces; Enter runs it, Esc or an empty Enter
cancels. Parse errors appear as a transient status-line message.

| Verb | Arguments | Action |
|---|---|---|
| `new` | nothing | New window |
| `next` / `prev` / `last` | nothing | Next / previous / last window |
| `win` | `<n>` | Jump to window *n* (**one-based**, 1–256) |
| `split` | `h` \| `v` | Split stacked (`h`) or side-by-side (`v`) |
| `zoom` | nothing | Toggle pane zoom |
| `kill` | nothing \| `win` \| `window` | Kill the active pane, or the window |
| `focus` | `l`\|`r`\|`u`\|`d`\|`next`\|`prev`\|`last` | Focus a pane |
| `resize` | `l`\|`r`\|`u`\|`d` `[n]` | Resize by *n* cells (default 1) |
| `layout` | `<name>` | Apply a preset layout: `even-horizontal`, `even-vertical`, `main-horizontal`, `main-vertical`, `tiled` |
| `rename` | `<name…>` | Rename the active window (spaces allowed) |
| `rename-pane` | `<name…>` | Rename the active pane |
| `mark` | nothing | Mark / unmark the active pane |
| `break` | nothing | Break the active pane to its own window |
| `join` (alias `join-pane`) | nothing \| `h` \| `v` | Join the marked pane here (default `v`, side-by-side) |
| `swap` (alias `swap-pane`) | nothing \| `prev` \| `next` | Swap with the marked pane (across windows of the same session), or with a layout neighbor |
| `prev-prompt` | nothing | Scroll viewport back to the previous prompt |
| `next-prompt` | nothing | Scroll viewport forward to the next prompt |
| `copy-output` | nothing | Yank the last completed command block's output |
| `copy` | nothing | Enter copy mode |
| `paste` | `[bufferN]` | Paste the newest buffer, or the named one |
| `buffers` | nothing | Choose-buffer overlay |
| `set-buffer` | `<text…>` | Push literal text as a new paste buffer (verbatim; no newlines) |
| `save-buffer` | `[bufferN] <path…>` | Write a buffer (default: the newest) to a file, bytes verbatim |
| `load-buffer` | `<path…>` | Read a file into a new paste buffer (regular files only, 10 MiB cap) |
| `sync` | nothing | Toggle sync-panes |
| `monitor-activity` / `monitor-bell` / `monitor-command` | nothing | Toggle activity / bell / command-completion monitoring for the window |
| `monitor-silence` | `[secs]` | Arm silence monitoring after `secs` of no output (`0` or no arg = off) |
| `popup` | `[command line…]` | Open a popup (scratch shell if no command) |
| `close-popup` | nothing | Close the popup |
| `pipe-pane` | `[command line…]` | Stream the pane's raw output to a command (no command stops the pipe) |
| `sessions` | nothing | Session picker |
| `tree` | nothing | Choose-tree |
| `switch` | `<session>` | Switch to a session in place |
| `reload` | nothing | Reload the config |
| `detach` | nothing | Detach |
| `help` | nothing | Help overlay |

Verbs marked “—” reject arguments. `rename`, `rename-pane`, `switch`,
`popup`, `pipe-pane`, `set-buffer`, `load-buffer`, and `save-buffer`'s path
take the rest of the line verbatim (internal spaces preserved). `save-buffer`
and `load-buffer` paths must be absolute or `~`-prefixed, since they resolve
daemon-side; see [docs/scripting.md](scripting.md#path-policy) for the path
policy and limits.

## Scripting from the CLI

The `cmd` / `send` / `capture` CLI verbs reuse the command-prompt grammar
above to drive sessions from scripts. They have their own document:
[docs/scripting.md](scripting.md).

## A complete worked example

Everything below is decoder-valid (it is parsed verbatim by a test in the
repo, `docs_worked_example_parses`, so it cannot rot silently):

```kdl
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
```
