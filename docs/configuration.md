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
  immediately).
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

The window strip: index + name per window, with the activity/bell flags
folded in. Needs both `active-style` (the focused window) and
`inactive-style`. No properties.

```kdl
window-list { active-style fg="fg" bg="accent"; inactive-style fg="muted" bg="bg_bar" }
```

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

The local time. `format` is a string in `strftime` syntax (default `"%H:%M"`);
`interval` is an optional duration; `style` optional.

```kdl
time format="%a %H:%M" interval="30s" { style fg="fg" bg="bg_bar" }
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
chords separated by spaces: `"Ctrl+a c"` means press `Ctrl+a`, then `c`.

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
| `swap_marked_pane` | Swap the active pane with the marked pane |

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

**Misc**

| Command | Action |
|---|---|
| `toggle_sync_panes` | Toggle synchronized input to all panes in the window |
| `toggle_monitor_activity` | Toggle activity monitoring for the window |
| `toggle_monitor_bell` | Toggle bell monitoring for the window |
| `reload_config` | Reload `config.kdl` |
| `cancel` | Do nothing (an explicit no-op, e.g. to neuter a default chord) |

## `session` — declarative sessions

`session` nodes declare sessions that the daemon builds fresh at boot. For a
declared name the config wins over the saved on-disk state, so the layout
always matches the template, and undeclared session names still restore
from disk as usual.

```kdl
session "dev" cwd="~/projects/app" {
    window "edit" {
        pane command="hx ."
    }
    window "run" cwd="~/projects/app/svc" {
        split vertical {
            pane command="cargo watch -x check" name="check"
            pane name="shell"
        }
    }
}
```

Node by node:

- `session "<name>" [cwd="<dir>"]` must contain at least one `window`.
  Duplicate session names are errors.
- `window "<name>" [cwd="<dir>"]` must contain exactly one layout node
  (`pane` or `split`), so to put several panes in a window, wrap them in a
  `split`.
- `split <direction> { … }` takes a direction, `vertical` (children
  side-by-side) or `horizontal` (children stacked), plus two or more
  children, each a `pane` or a nested `split`. Splits divide space evenly,
  and ratios are not configurable in templates. No properties.
- `pane [command="…"] [cwd="<dir>"] [name="…"]` is a leaf. `command` is a
  shell command line, run via your default shell's `-c` (`$SHELL`, falling
  back to `/bin/sh`), and without it the pane is an interactive shell. No
  children.

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
| `swap` (alias `swap-pane`) | nothing \| `prev` \| `next` | Swap with the marked pane, or a neighbor |
| `prev-prompt` | nothing | Scroll viewport back to the previous prompt |
| `next-prompt` | nothing | Scroll viewport forward to the next prompt |
| `copy-output` | nothing | Yank the last completed command block's output |
| `copy` | nothing | Enter copy mode |
| `paste` | nothing | Paste the newest buffer |
| `buffers` | nothing | Choose-buffer overlay |
| `sync` | nothing | Toggle sync-panes |
| `monitor-activity` / `monitor-bell` | nothing | Toggle window monitoring |
| `popup` | `[command line…]` | Open a popup (scratch shell if no command) |
| `close-popup` | nothing | Close the popup |
| `sessions` | nothing | Session picker |
| `tree` | nothing | Choose-tree |
| `switch` | `<session>` | Switch to a session in place |
| `reload` | nothing | Reload the config |
| `detach` | nothing | Detach |
| `help` | nothing | Help overlay |

Verbs marked “—” reject arguments. `rename`, `rename-pane`, `switch`, and
`popup` take the rest of the line verbatim (internal spaces preserved).

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
