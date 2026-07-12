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
- **Error at daemon start**: the daemon logs a warning, runs on the built-in
  defaults, and **the next client to attach sees** `config error — running
  defaults; run plexy-glass reload for details` on the status line (cleared by
  the first clean reload). The error message varies by kind:
  - *Decode error* (unknown node, wrong type, duplicate section): names the
    problem with line/column, e.g. `unknown node "foo" (at line 12:1)`.
  - *KDL syntax error* (e.g. bare `true` instead of `#true`): now reports the
    `line:col` and the parser's message/help (e.g. `line 7:13: …`); when the
    parser gives no location, it appends the hint
    `KDL v2: booleans are #true/#false; strings must be quoted`.
- **Skipped keymap bindings**: a binding whose chord or verb won't parse is
  dropped (the rest still apply), and the attaching/reloading client sees
  `N keymap binding(s) skipped` so a dead key reads as a config mistake, not a
  bug. Details (which bindings) are in the log.
- **Reload**: three triggers, all equivalent on the daemon side:
  - `Ctrl+a R` (the default `reload_config` binding),
  - `:reload` in the command prompt,
  - `plexy-glass reload` from a shell.

  A reload re-reads the file and applies it to **every** live session
  (status bar, palette, keybindings; the reloading client's keymap is rebuilt
  immediately). It also **builds any newly-declared `session`** so it becomes
  attachable; live sessions are never rebuilt (see
  [Reload and switch](#reload-and-switch)).
- **Error on reload**: the daemon **keeps the last-known-good config**, so a
  parse error leaves every live session's palette, status bar, and keybindings
  exactly as they were, and it does *not* revert to the built-in defaults. All
  three triggers surface the error: `plexy-glass reload` prints `config reload
  error: …` and **exits non-zero** (so `reload && …` guards halt), and the
  in-session triggers (`Ctrl+a R`, `:reload`) show `reload failed: …` in the
  status line (the daemon log also records it). Note that the same
  message-fidelity limits apply: decode errors include line/column, but a raw
  KDL syntax error gives only `Failed to parse KDL document`.

Other top-level rules, all enforced by the decoder: unknown top-level nodes
are errors; `palette`, `status`, and `keymap` may each appear at most once;
`session` may appear any number of times but session names must be unique.

### Where the daemon logs

The daemon writes to a log file, not your terminal. When the docs above say
"the daemon log records it," this is where to look:

- **macOS**: `~/Library/Logs/plexy-glass/daemon.log`
- **Linux**: `$XDG_STATE_HOME/plexy-glass/daemon.log` (falls back to
  `~/.local/state/plexy-glass/daemon.log`)

Set `RUST_LOG` before the daemon first spawns to raise the verbosity, e.g.
`RUST_LOG=plexy_glass=debug plexy-glass`. With `PLEXY_GLASS_DIR` set, logs live
under `<dir>/logs/daemon.log` instead.

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
  digits). There is no short form, no `rgb()`, and no CSS color names, and a
  palette value may not reference another palette name. The hex is parsed once
  at load, so a malformed value (`accent "#zz"`) is a hard config error with a
  `line:col`, not a silent fallback.

Palette names are usable in every widget style's `fg`/`bg` (see
[Styles and padding](#styles-and-padding)). Note that a style color splits two
ways: an unknown palette *name* silently resolves to *no color* (the terminal
default), since names are resolved late and a custom palette can define
anything, but a `#`-prefixed value that isn't a valid `#rrggbb` literal is a
hard config error at load. The `fg` and `bg` entries (and `cursor`, falling
back to `accent`) also answer OSC 10/11/12 color queries from applications
running inside panes.

Beyond the status bar, several palette keys drive the rest of the chrome, so
the whole UI moves together when you retheme (override these):

- **Pane border rings**: the focused pane uses `highlight`, a marked pane uses
  `warn`, and a pane-swap drag uses `info` (source) / `ok` (target).
- **Overlay boxes** (help, choose-tree, history, buffers, and the
  [session picker](#session-picker-ctrla-w)): the border is `accent`, the title
  `highlight`, the footer `muted`, and the interior `bg_bar`. The session picker
  is client-rendered (as of protocol v12, so it renders outside the daemon
  compositor), but it reads the same roles from the client's own `config.kdl` and
  draws the same box, so it matches; its status glyphs take `ok` (live) / `warn`
  (version mismatch) / `alert` (unreachable). A v11 client falls back to the old
  daemon-rendered overlay.
- The default status bar puts the **active window tab** on `highlight` so it
  stands apart from the `accent` session pill and the `bg_bar` inactive tabs.

(The crisp powerline look with segment-shaped separators is opt-in via
`glyphs "nerd"` with a Nerd Font, see [`glyphs`](#glyphs).)

## Status-line messages

Transient feedback (copying, reloading, switching sessions, errors, and the
monitor alerts) appears as a one-line bar on the status row for ~3 seconds.
Each message carries a **severity** that selects a leading glyph and a color
from the palette:

| Severity | Glyph (unicode / nerd) | Glyph (ascii) | Palette key |
|----------|------------------------|---------------|-------------|
| Info     | `ℹ`                    | `i`           | `info`      |
| Success  | `✓`                    | `+`           | `ok`        |
| Warning  | `⚠`                    | `!`           | `warn`      |
| Error    | `✗`                    | `x`           | `alert`     |

The glyph is the primary, color-independent cue (success vs error reads even on
a monochrome terminal) and degrades to a plain letter on `glyphs "ascii"`. The
bar background is the `bg_bar` palette color. Examples you'll see: `✓ copied 3
lines` / `✓ copied "…"` on any yank or hint-mode copy, `✓ config reloaded`,
`✓ marked pane` / `ℹ mark cleared`, and `✓ killed window 2 (api)` (the leading
glyph is always present, so `i mark cleared` on the `ascii` tier). Recolor any
of these by overriding the `info` / `ok` / `warn` / `alert` / `bg_bar` entries
in [`palette`](#palette).

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

#### `ssh`

A marker shown when any client attached to the session reached the daemon over
`-H`/SSH (see [docs/ssh.md](ssh.md)); it's a session-level cue, so it renders
nothing on a purely local session. Both props are optional: `content` (default
`ssh`) and a `style` child (default the theme default; the shipped config uses
`accent`). It ships first in the default left cluster.

```kdl
ssh content="ssh" { style { fg "accent" } }
```

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
| `next_layout` | Cycle to the next preset (remembered per window); default `Ctrl+a i` (moved off `Ctrl+a Space` to make room for `command_palette`) |

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
| `history` | Structured history palette (cross-session block finder) |
| `hints` | Hint mode: overlay labels on URLs, paths, and git hashes, press a label key to copy |
| `command_prompt` | Open the `:` command prompt |
| `command_palette` | Open the command palette: a fuzzy finder over every command, default `Ctrl+a Space` |
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

Controls the command-block annotations: the exit-status border (see
[Exit status on the border](command-blocks.md#exit-status-on-the-border) in
docs/command-blocks.md), the
[command duration](command-blocks.md#command-duration), and the
[sticky command header](command-blocks.md#sticky-command-header).

```kdl
blocks {
    enabled #true               // master switch: #false disables ALL block annotations
    ok-color "ok"               // palette name or #rrggbb; default: palette `ok`
    fail-color "alert"          // palette name or #rrggbb; default: palette `alert`
    select-color "#dca561"      // palette name or #rrggbb; the block-mode bracket
    sticky-header #true         // pin the command line (dimmed) while scrolled back
    duration #true              // show each block's wall-clock duration inline
    duration-threshold "2s"     // minimum duration to show; "0" times everything
}
```

- `enabled`: `#true` (default) or `#false`. The master switch for every block
  annotation: when `#false`, no block-status border coloring happens (the left
  border is always plain) and the command duration and sticky command header
  are off too. All other `blocks` properties still decode normally, so
  `enabled #false` is the way to opt out of the whole feature without ripping
  out the rest of the node.
- `ok-color`: the foreground color for border rows belonging to a block that
  exited with code 0. Takes a palette name (e.g. `"ok"`, `"accent"`) or a
  `#rrggbb` hex literal (e.g. `"#87a987"`). Default: `"ok"` (the built-in
  palette entry `#87a987`).
- `fail-color`: the foreground color for rows belonging to a block that exited
  with a nonzero code. Same value forms as `ok-color`. Default: `"alert"` (the
  built-in palette entry `#c4746e`).
- `select-color`: the color of the block-mode selection bracket (`┏┃┗`) drawn
  around the selected command block (see
  [Block mode](command-blocks.md#block-mode) in docs/command-blocks.md). Same
  value forms as `ok-color`. Default: `"#dca561"`. Note that this one is
  independent of `enabled`: the bracket is part of block-mode navigation, so
  it's drawn even when block-status border coloring is turned off.
- `sticky-header`: `#true` (default) or `#false`. While the pane is scrolled
  back, pins a block's command line (dimmed, so it blends with the theme) on
  the pane's top row when that block's output fills the top of the viewport.
  Live view only; it never appears at the live bottom. Gated by `enabled`.
- `duration`: `#true` (default) or `#false`. Shows each completed block's
  wall-clock duration (`C`→`D`) as a dim, right-aligned note on the command
  row (and on the sticky header). Runtime-only, so it's not persisted across a
  restart. Gated by `enabled`.
- `duration-threshold`: the minimum duration worth displaying, as `"<int>ms"`,
  `"<float>s"`, or `"0"` (e.g. `"500ms"`, `"1.5s"`, `"2s"`). Default `"2s"`.
  `"0"` shows every completed block; anything faster than the threshold is
  hidden otherwise. An unparseable value is a hard config error.

An unknown palette *name* falls back to the built-in default for that field, so
it never disables the feature and is not a hard error. A malformed `#rrggbb`
literal, on the other hand, is a hard config error at load (the hex is parsed
once, up front). Both fields resolve through the same color lookup the
status-bar widget styles use, so custom `palette` entries are valid values.

The `blocks` node live-reloads with the config (`Ctrl+a R` /
`plexy-glass reload`), same as `palette` and `status`. A reload that supplies
new colors shows up on the next rendered frame.

## `hints`

Hint mode (`prefix f` / `:hints`) scans the focused pane's live visible grid
and labels every detected span (URLs, file paths including `file:line:col`
references, git SHAs, IP addresses, UUIDs, hex colors, email addresses, and
OSC 8 hyperlinks) with short keyboard labels, so you can act on them without
reaching for the mouse.

```kdl
hints {
    enabled #true
    alphabet "asdfghjkl"   // label characters (>= 2 distinct chars)
    label-fg "bg"          // label text color (palette name or #rrggbb)
    label-bg "warn"        // label background color
    match-fg "ok"          // typed-prefix highlight color
}
```

**Action model:** type a label to select a span.

- **Lowercase label**: copies the span to the clipboard and the paste buffer
  (the same path as a copy-mode yank, so `prefix ]` pastes it). For a
  `file:line:col` target the `:line:col` suffix is *kept* so editors can jump
  to the exact location. An OSC 8 hyperlink to a local file (a `file://` URL,
  as Claude Code / `eza` and friends emit) is copied as its filesystem path,
  not the URL, and percent escapes like `%20` are decoded. Real `http(s)` URLs
  copy verbatim.
- **Uppercase label (Shift held on the final key)**: opens the span via the OS
  opener (`xdg-open` / `open`). For a `file:line:col` target the `:line:col`
  suffix is *stripped* so the opener receives a plain file path.

The overlay dims the pane content and draws the labels in `label-bg` /
`label-fg`. As you type, the matching label prefix is highlighted in
`match-fg` and non-matching labels are filtered out. Press `Esc` or a
non-label key to cancel without acting. Note that v1 scans the live active
grid only; scrollback-wide hint mode is deferred.

- `enabled`: `#true` (default) or `#false`. The master switch; when `#false`,
  the `prefix f` chord and the `:hints` verb are no-ops.
- `alphabet`: the characters used to build labels. Needs at least 2 distinct
  chars; shorter or duplicate-only values fall back to `"asdfghjkl"`. Default:
  `"asdfghjkl"` (the standard home row).
- `label-fg`: foreground color of the label text. Takes a palette name (e.g.
  `"bg"`, `"fg"`) or a `#rrggbb` hex literal. Default: `"bg"` (the theme
  background, so the label text reads dark-on-light against `label-bg`).
- `label-bg`: background color of the label box. Same value forms. Default:
  `"warn"` (the built-in warning/yellow palette entry).
- `match-fg`: foreground color for the already-typed prefix within a label (so
  you can tell matched from still-to-type characters). Default: `"ok"` (the
  built-in green palette entry).

An unknown palette *name* falls back to the built-in default for that field and
is not a hard error; a malformed `#rrggbb` literal is a hard config error at
load (the hex is parsed up front, not at render).

The `hints` node live-reloads with the config (`Ctrl+a R` / `plexy-glass
reload`). New colors and alphabet are picked up on the next overlay open.

## `notifications`

Desktop notifications when a command finishes running **long** and you're **not
watching it**, so a build you walked away from (or that finished in a
background window) reaches you — plus a toast for any program that explicitly
asks for one via `OSC 9` / `OSC 777`. The daemon shells out to the platform
notifier: **`osascript`** on macOS (toasts show under "Script Editor" because a
bare CLI binary has no app bundle for macOS to attribute its own toasts to) and
**`notify-send`** (libnotify) on Linux. Both are present by default on a
desktop macOS / Linux. Note that if the notifier is missing or there's no
desktop session (a headless / SSH daemon), the attempt is logged and silently
skipped, never an error.

```kdl
notifications {
    enabled #true             // master switch (default on)
    min-duration "30s"        // only commands at least this long; "<int>ms" | "<float>s" | "0"
    in-band #true             // OSC 9 / OSC 777 toasts from child programs (default on)
}
```

- `enabled`: `#true` (default) or `#false`. Master switch for both the
  command-completion and in-band notifications below.
- `min-duration`: minimum command duration to notify, default `"30s"`. Same
  grammar as `blocks`'s `duration-threshold` (`"500ms"`, `"2s"`, `"0"`); `"0"`
  notifies for every unattended completion. An unparseable value is a hard
  error.
- `in-band`: `#true` (default) or `#false`. Toggles the `OSC 9` / `OSC 777`
  path independently of `min-duration` (an explicit request has no
  "too short to bother" case); still gated by the master `enabled`.

**When the completion toast fires:** on a command block completing, if
`enabled` **and** its duration ≥ `min-duration` **and** the completion is
*unattended*. Unattended means any of: the session is **detached**; the
completing window is **not the active one**; or the **terminal isn't focused**
on your machine (you switched to another app). A command you're actively
watching (attached, in the active window, terminal focused) never notifies.
(Terminal focus uses `?1004` focus reporting; a terminal that doesn't report
focus is treated as focused, so it never produces a false toast, and you still
get the detached / other-window cases.) Independent of the per-window
[`monitor-command`](command-blocks.md) flag (that's the in-terminal
status-flag channel; this is the desktop channel).

The completion notification reads e.g. `plexy-glass: <session>` / `✓ cargo
build · exit 0 · 2m03s`. It works while detached (the daemon fires it). On a
host with no desktop / no notification bus (a headless or SSH daemon), the
attempt is logged and silently skipped, never an error. Note that it requires
OSC 133 shell integration, like all command-block features.

**In-band notifications (`OSC 9` / `OSC 777`):** any program can ask for a
desktop toast directly, no shell integration required — `OSC 9 ; <text> ST`
(growl / iTerm2 form; body only, plexy-glass fills the title) or
`OSC 777 ; notify ; <title> ; <body> ST` (urxvt / tmux form; explicit title).
With `enabled` and `in-band` both on, it fires unless you're looking right at
the firing pane — same attended check as the completion toast above,
evaluated against the pane that actually requested it (so a background
window's `notify-send`-alike still reaches you even while you're watching a
different pane). `OSC 9 ; 4 ; …` (the ConEmu progress-bar form) is a distinct,
unrelated protocol and does not raise a toast.

## `mouse`

Mouse behavior for gestures that go beyond plain click-to-focus.

```kdl
mouse {
    drag-modifier "alt"   // "alt" | "ctrl"; "shift" is rejected
}
```

**Tab reorder gesture:** hold the configured modifier and **drag a window tab** in
the status-bar window list to reorder it. Release over another tab to drop the
dragged window into that position (drop-to-position), or release to the right of
all tabs to send it to the end. A plain click (no modifier) still just selects
the window.

**Pane swap gesture:** hold the configured modifier and **drag from inside a pane**
to another pane in the **same window**. Release over the target pane to swap their
positions. Focus follows the dragged pane after the swap. A plain click (no
modifier) is unchanged. Dragging to a pane in a different window or releasing
outside any pane aborts the gesture with no change.

- `drag-modifier`: the keyboard modifier that must be held during a status-bar
  tab drag (reorder) or an in-pane drag (pane swap) to activate the gesture.
  Accepted values: `"alt"` (default) and `"ctrl"`. We reject `"shift"` outright
  because terminals reserve Shift+drag for native text selection, so it never
  reaches the mux.

  **Why `"ctrl"` exists:** some terminal emulators (notably those that map the
  macOS Option key to Meta) intercept Alt+drag before it reaches the application.
  If Alt+drag does nothing for you, switch to `"ctrl"`:

  ```kdl
  mouse {
      drag-modifier "ctrl"
  }
  ```

The reorder takes effect immediately and lasts for the life of the session
(in memory), so there is nothing extra to configure.

The `mouse` node live-reloads with the config (`Ctrl+a R` / `plexy-glass
reload`). The new modifier takes effect on the next drag.

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

`auto-rename` is read fresh on every status-bar render, so toggling it via
reload (`Ctrl+a R` / `plexy-glass reload`) takes effect immediately.

## `welcome`

```kdl
welcome #true   // default; #false skips the one-time welcome modal
```

When `welcome #true` (the default), plexy-glass shows a **welcome modal** on the
first attach to the daemon: a centered box with the prefix, a few essential
keys, how to open full help (`Ctrl+a ?`) and detach (`Ctrl+a d`), and where
`config.kdl` lives. Press any key to dismiss it. It shows **once per daemon
run** (an in-memory flag, not an on-disk marker, since the memory-only daemon
keeps no session state on disk), so a fresh daemon shows it once again.

Set `welcome #false` to turn it off for good (the modal itself tells you
this). Note that a broken config takes precedence: you see the config-error
notice instead, and the welcome is deferred to the next clean attach.

`session` nodes declare sessions that the daemon builds fresh at boot (and on
first attach to a declared name). The template is the only source of truth:
we deliberately don't persist sessions to disk, so a declared session is
rebuilt identically every time the daemon (re)starts.

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

When a `command` pane's command exits, plexy-glass **drops to an interactive
`$SHELL` in that same slot** rather than closing the window: the window (and,
if it was the last one, the session) survives, and you land at a shell prompt
where the command was. The fallback shell closes its window normally when you
exit it, as any interactive pane does. This applies to any command pane, a
window's sole pane or a declared `split` child alike; only that pane's slot
is replaced, its siblings are untouched. (An interactive pane with no
`command` is unaffected: exiting its shell closes the window as before.)

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

## Sessions are in-memory (no on-disk persistence)

The daemon holds sessions in memory only; it does **not** save them to disk.
You can detach and reattach freely while the daemon runs (windows, panes, and
scrollback are preserved live), but when the daemon stops (`plexy-glass kill`,
a reboot, a crash) its sessions are gone, and the next attach spawns a fresh
daemon with fresh sessions. To get the same layout back every time, **declare**
it in `config.kdl` (see [`session`](#session--declarative-sessions)): declared
sessions are built fresh at boot and on first attach. (The only thing the
daemon keeps on disk is its log and the one-time [`welcome`](#welcome)
marker.)

## `remotes`

The `remotes` node is the **roster of remote hosts the session picker spans**:

```kdl
remotes {
    host "prod"
    host "user@build.internal"
}
```

Each `host` is an ssh target, the same string you would pass to `plexy-glass -H
<host>` (see [ssh.md](ssh.md)) or to `ssh` itself, so `user@host`,
`host.internal`, and `~/.ssh/config` aliases all work. It's read by the
**client** (not the daemon) when you open the picker with `Ctrl+a w`, and each
listed host becomes a section the picker queries for its running sessions. A
host you don't list here still shows up **ad-hoc** the moment you `-H` attach to
it once (see the picker section below). Config `remotes` are the stable,
always-listed hosts; ad-hoc hosts are the ones you've visited.

A parse error in `config.kdl` doesn't disable the picker. The roster falls back
to just the ad-hoc file and the local daemon, and the error surfaces on the next
attach like any other config error.

## Session picker (`Ctrl+a w`)

`Ctrl+a w` (or `:sessions`) opens a full-screen picker that spans **every daemon
in your roster at once**: the daemon you're attached to, every host in
[`remotes`](#remotes), and every host you've `-H` attached to before. As of
protocol v12 the picker is **client-rendered**: the client reads its own
roster, queries each remote daemon over SSH in parallel, and draws and drives
the list itself, so filtering and moving the cursor are local and never
round-trip to a daemon. (An older v11 client falls back to the daemon-rendered,
local-only overlay this replaced.)

### Sections

Rows are grouped by daemon, each under a selectable **host anchor**:

- The **current daemon** first, with its anchor (`local`, or the ssh target when
  you're attached remotely) followed by its sessions, one row per session
  (`name — N win, M panes, K clients`).
- Each **configured** host from `remotes`, then a horizontal **divider**, then
  each **ad-hoc** host, the hosts you've `-H` attached to that aren't in
  `remotes`. (When you're attached to a remote, the `local` daemon appears here
  as just another host to jump back to.)

A host anchor carries a status glyph, filled in as its query resolves:

| Glyph | Meaning |
|---|---|
| `…` | Pending, the query is still in flight |
| `●` | Live: the daemon answered, and its sessions are listed under the anchor |
| `○` | Empty: the daemon answered but has no sessions |
| `⚠` | Unreachable: connect failed or timed out |
| `⚠ vN` | Version mismatch: the remote runs protocol `vN`, run with `--install` to upgrade it |

Remote queries stream in independently, so a slow or unreachable host never
blocks the rest of the list. There's no animated connect spinner yet; a
pending host just shows `…` until it resolves, and that polish is deferred.

### Keys

The picker is **action-first**: in the default Navigate mode letters are
actions, not filter input. Press `/` to enter an explicit filter mode where
typing narrows the list.

#### Navigate (default)

| Key | Action |
|---|---|
| `↓` / `Ctrl+n` / `Ctrl+j` | Move selection down |
| `↑` / `Ctrl+p` / `Ctrl+k` | Move selection up |
| `/` | Enter filter mode (type to narrow) |
| `Enter` on a session on the daemon you're attached to | Switch to it in place (fast, same connection, no reconnect) |
| `Enter` on a session on another daemon | Reconnect: re-attach this client to that daemon and session |
| `Enter` on the current daemon's own anchor | No-op; just closes the picker (you're already here) |
| `Enter` on another daemon's anchor | Reconnect to that daemon's default session |
| `Enter` on `＋ Connect to a host…` (last row) | Opens a prompt; type an ssh target and `Enter` to connect (remembered as ad-hoc on success) |
| `n` | New session on the host under the cursor (prompts for a name); no-op off a host row |
| `i` | Toggle connect-with-install for the next host connect (always, regardless of the cursor row or the filter) |
| `x` | Forget the ad-hoc host under the cursor; no-op elsewhere |
| `Esc` | Cancel and return to the current session |
| Any other key | No-op — letters don't filter here; press `/` first |

#### Filter mode (press `/`)

| Key | Action |
|---|---|
| Any printable | Narrow the list (case-insensitive substring on the row) |
| Backspace | Remove the last filter character |
| `Enter` | Done: return to Navigate with the filter still applied |
| `↑` / `↓` / `Ctrl+p` / `Ctrl+n` | Return to Navigate and move the selection |
| `Esc` | Clear the filter and return to Navigate |

`Enter` routes by whether the row lives on the daemon you're currently attached
to. A session on that daemon is a fast in-place switch over the live connection;
a session (or an anchor) on a **different** daemon does a brief reconnect, the
live attach tears down and re-attaches to that host over SSH, so the picker
doubles as a cross-host jump. Note that the current daemon's own anchor has
nothing to jump to, so `Enter` on it is a no-op that just closes the picker
rather than reconnecting you to where you already are. `n` works on any host
anchor, **including the current daemon's**, so it's also the fastest way to spin
up a fresh named session right where you are; the new session is created when
the reconnect lands (an empty name is refused, so a bare `n` then `Enter` stays
in the prompt).

`n` and `x` are always actions in Navigate. `n` opens the new-session prompt on
the host anchor under the cursor and is a no-op anywhere else. `x` only forgets
**ad-hoc** hosts (removing them from the client-side roster file); it's a no-op
on configured or local anchors and on session rows, since those come from
`config.kdl` and the live connection. To type a name that happens to contain
`n`, `x`, or `i`, press `/` first: inside filter mode every printable is filter
input. Filter mode narrows the list fzf-style: every keystroke (and every
backspace) snaps the cursor back to the top match, rather than trying to
preserve your row position in a list that just got shorter or longer. The filter
you type persists after you leave filter mode (the list stays narrowed), so you
can `/` to narrow, `Enter` to stop typing, then arrow through the matches; `Esc`
while filtering clears it.

The last row is always `＋ Connect to a host…`, pinned past every section and
exempt from the filter, so it's reachable even when a search matches nothing
else. `Enter` on it opens a one-line prompt; type any ssh target (the same
syntax `-H` takes) and `Enter` connects, or `Esc` abandons it. It's how you
reach a host you haven't listed in `remotes` and haven't `-H`'d into before,
without leaving the picker. Once the attach lands, the host joins the ad-hoc
roster exactly like a `-H` attach does, so it shows up as its own anchor the
next time you open the picker; a host that fails to connect is not
remembered.

`i` toggles a persistent connect-with-install flag **unconditionally** — it's a
global toggle, so it fires no matter which row the cursor is on or whether a
filter is applied. It applies to the *next* host connect, whether that's `Enter`
on an existing remote anchor or the target you type into the `＋` prompt, and
provisions or updates the remote `plexy-glass` binary over SSH before attaching,
the same effect as `plexy-glass -H host --install` on the CLI (see
[docs/ssh.md](ssh.md)). The footer shows its current state (`i install: on`/`off`)
so it's never silently on for a host you didn't mean to install to.

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
`session` template in `config.kdl` decouples it from the template: the renamed
session lives on in memory under its new name, and the template name gets
built fresh again at the next daemon boot. Renaming any session **to** a
declared name is refused with `'<name>' is a declared session name — choose
another`.

## The command prompt

`Ctrl+a :` opens a one-line command prompt. Type a verb (Tab completes it)
and arguments separated by spaces; Enter runs it, Esc or an empty Enter
cancels. Parse errors appear as a transient status-line message.

| Verb | Arguments | Action |
|---|---|---|
| `new` | — | New window |
| `next` / `prev` / `last` | — | Next / previous / last window |
| `win` | `<n>` | Jump to window *n* (**one-based**, 1–256) |
| `split` | `h` \| `v` | Split stacked (`h`) or side-by-side (`v`) |
| `zoom` | — | Toggle pane zoom |
| `kill` | nothing \| `win` \| `window` | Kill the active pane, or the window |
| `focus` | `l`\|`r`\|`u`\|`d`\|`next`\|`prev`\|`last` | Focus a pane |
| `resize` | `l`\|`r`\|`u`\|`d` `[n]` | Resize by *n* cells (default 1) |
| `layout` | `<name>` | Apply a preset layout: `even-horizontal`, `even-vertical`, `main-horizontal`, `main-vertical`, `tiled` |
| `rename` | `<name…>` | Rename the active window (spaces allowed) |
| `rename-pane` | `<name…>` | Rename the active pane |
| `mark` | — | Mark / unmark the active pane |
| `break` | — | Break the active pane to its own window |
| `join` (alias `join-pane`) | nothing \| `h` \| `v` | Join the marked pane here (default `v`, side-by-side) |
| `swap` (alias `swap-pane`) | nothing \| `prev` \| `next` | Swap with the marked pane (across windows of the same session), or with a layout neighbor |
| `prev-prompt` | — | Scroll viewport back to the previous prompt |
| `next-prompt` | — | Scroll viewport forward to the next prompt |
| `copy-output` | — | Yank the last completed command block's output |
| `copy` | — | Enter copy mode |
| `paste` | `[bufferN]` | Paste the newest buffer, or the named one |
| `buffers` | — | Choose-buffer overlay |
| `set-buffer` | `<text…>` | Push literal text as a new paste buffer (verbatim; no newlines) |
| `save-buffer` | `[bufferN] <path…>` | Write a buffer (default: the newest) to a file, bytes verbatim |
| `load-buffer` | `<path…>` | Read a file into a new paste buffer (regular files only, 10 MiB cap) |
| `sync` | — | Toggle sync-panes |
| `monitor-activity` / `monitor-bell` / `monitor-command` | — | Toggle activity / bell / command-completion monitoring for the window |
| `monitor-silence` | `[secs]` | Arm silence monitoring after `secs` of no output (`0` or no arg = off) |
| `popup` | `[command line…]` | Open a popup (scratch shell if no command) |
| `close-popup` | — | Close the popup |
| `pipe-pane` | `[command line…]` | Stream the pane's raw output to a command (no command stops the pipe) |
| `sessions` | — | Session picker |
| `tree` | — | Choose-tree |
| `history` | — | Structured history palette (cross-session block finder) |
| `hints` | — | Hint mode (labels on-screen URLs/paths/hashes; key copies) |
| `palette` | — | Command palette (fuzzy finder over every command) |
| `switch` | `<session>` | Switch to a session in place |
| `reload` | — | Reload the config |
| `detach` | — | Detach |
| `help` | — | Help overlay |

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
