# plexy-glass

A terminal multiplexer (in the tmux/zellij family) written in Rust, built as a
daemon/client pair: a daemon owns the sessions and PTYs, and lightweight
clients attach, detach, and reattach to them. It treats modern terminal
escape-sequence traffic as a first-class concern, so OSC 7 (working
directory), OSC 8 (hyperlinks), OSC 52 (clipboard), OSC 133 (prompt marks),
OSC 0/1/2 (titles), and OSC 10/11/12 (colors) are parsed and routed per pane
rather than stripped, and it negotiates keyboard protocols (the Kitty keyboard
protocol and xterm's modifyOtherKeys) in both directions: panes can ask for
enhanced keys, and the client negotiates the same with the outer terminal. It
is designed alongside Ghostty-style terminal integration.

## Features

- Sessions, windows, and panes with horizontal/vertical splits, pane zoom, and
  keyboard or mouse resize
- Detach/reattach with on-disk session persistence and restore (split ratios
  are restored faithfully, not reset)
- Multiple clients attached to the same session
- Copy mode with search
- Full mouse support, including click-to-focus and drag-resize on split borders
- Popup panes: `Ctrl+a P` or `:popup [cmd]` opens a transient floating pane
- Preset layouts: five tmux-style presets (`even-horizontal`, `even-vertical`,
  `main-horizontal`, `main-vertical`, `tiled`), cycled with `Ctrl+a Space` or
  applied by name with `:layout <name>`
- Paste buffers: copy-mode yanks push a named-buffer stack; paste the newest
  with `Ctrl+a ]`, or pick one with `Ctrl+a =`
- Per-window activity and bell monitoring, surfaced as flags in the status bar
- Configurable status bar with live config reload
- KDL v2 configuration (`config.kdl`)
- Declarative sessions: recursive `session → window → split/pane` layouts in
  the config, with per-pane commands and working directories
- A visual session picker (`Ctrl+a w`) and a choose-tree
  (session → window → pane drill-down, `Ctrl+a W`)
- Pane mobility: break a pane to its own window, join it elsewhere, swap
  panes, and a marked pane for cross-window moves
- Keyboard-protocol negotiation: Kitty keyboard protocol and modifyOtherKeys,
  per pane, with graceful fallback and clean teardown of the outer terminal
- Colored underlines (SGR 58/59), advertised to applications

## Quick start

```sh
cargo build --release
```

The binary lands at `target/release/plexy-glass`.

```sh
plexy-glass attach            # attach to the only session, or create "main"
plexy-glass attach -n work    # attach to (or create) the session "work"
```

Running `plexy-glass` with no subcommand is the same as `plexy-glass attach`.
The daemon is spawned automatically on first attach. Detach with `Ctrl+a d`.
The session keeps running, and it's also saved to disk so it survives a
daemon restart.

Other subcommands:

| Command | What it does |
|---|---|
| `plexy-glass list` | List all running sessions |
| `plexy-glass list-saved` | List sessions saved on disk (running or not) |
| `plexy-glass kill -n NAME` | Kill a single session by name |
| `plexy-glass kill` | Stop this runtime dir's daemon |
| `plexy-glass kill --all` | Stop every plexy-glass daemon for the current user |
| `plexy-glass reload` | Reload `config.kdl` from the platform config dir |
| `plexy-glass cmd [-n NAME] <LINE>...` | Run one or more command-prompt lines against a session |
| `plexy-glass send [-n NAME] [--enter] <TEXT>...` | Type text into the focused pane (popup-aware) |
| `plexy-glass capture [-n NAME]` | Print the focused pane's visible screen text (popup-aware) |

(`plexy-glass daemon` exists but auto-spawn runs it for you internally, so
the only time you'd type it is with `--foreground` for development.)

### Scripting

The `cmd`, `send`, and `capture` verbs let you drive a running session from a
script or another tool, no terminal attachment required:

```sh
# Apply a structural command, then run a test and check the output
plexy-glass cmd -n work "split v" "layout main-vertical"
plexy-glass send -n work --enter "cargo test"
plexy-glass capture -n work | grep "test result: ok"
```

`cmd` reuses the command-prompt grammar verbatim. All three verbs exit 0 on
success and 1 on any failure; `-n NAME` targets a session, and without `-n`
the sole running session is used (error if zero or more than one). See
[docs/scripting.md](docs/scripting.md) for the full reference.

## Default keybindings

The prefix defaults to `Ctrl+a` and is configurable (`keymap { prefix "Ctrl+b" }`).
The built-in bindings are declared with a `prefix` chord token so they all
follow a prefix change, and every binding below is also individually
rebindable via the `keymap` block in `config.kdl` (see the
[configuration reference](docs/configuration.md)).

### Sessions and client

| Keys | Action |
|---|---|
| `Ctrl+a d` | Detach |
| `Ctrl+a w` | Choose session |
| `Ctrl+a W` | Choose tree |
| `Ctrl+a :` | Command prompt |
| `Ctrl+a ?` | Help |
| `Ctrl+a R` | Reload config |

### Windows

| Keys | Action |
|---|---|
| `Ctrl+a c` | New window |
| `Ctrl+a n` | Next window |
| `Ctrl+a p` | Previous window |
| `Ctrl+a 1` … `Ctrl+a 9` | Select window 1–9 |
| `Ctrl+a Tab` | Last window |
| `Ctrl+a ,` | Rename window |
| `Ctrl+a &` | Kill window |
| `Ctrl+a M` | Monitor activity |

### Panes

| Keys | Action |
|---|---|
| `Ctrl+a v` | Split vertical |
| `Ctrl+a s` | Split horizontal |
| `Ctrl+a x` | Kill pane |
| `Ctrl+a z` | Zoom pane |
| `Ctrl+a h` / `j` / `k` / `l` | Focus pane left / down / up / right |
| `Alt+Left` / `Down` / `Up` / `Right` | Focus pane left / down / up / right |
| `Ctrl+a H` / `J` / `K` / `L` | Resize pane left / down / up / right |
| `Ctrl+a ;` | Last pane |
| `Ctrl+a .` | Rename pane |
| `Ctrl+a m` | Mark pane |
| `Ctrl+a !` | Break pane |
| `Ctrl+a {` | Swap pane prev |
| `Ctrl+a }` | Swap pane next |

### Layouts and popups

| Keys | Action |
|---|---|
| `Ctrl+a Space` | Next layout |
| `Ctrl+a P` | Popup (scratch shell) |
| `Ctrl+a q` | Close popup |

### Modes and buffers

| Keys | Action |
|---|---|
| `Ctrl+a [` | Copy mode |
| `Ctrl+a ]` | Paste buffer |
| `Ctrl+a =` | Choose buffer |
| `Ctrl+a y` | Toggle sync panes |

## Configuration

plexy-glass reads `config.kdl` (KDL v2) from your platform's config directory,
`~/.config/plexy-glass/config.kdl` on Linux and
`~/Library/Application Support/plexy-glass/config.kdl` on macOS, and it covers
the color palette, the status bar and its widgets, the keymap, and declarative
session templates. The config reloads live (`Ctrl+a R` or `plexy-glass
reload`) so you don't have to restart the daemon to see a change. See
[docs/configuration.md](docs/configuration.md) for the full reference.

## Status

A personal project under active development. Design specs for each feature
live in `docs/superpowers/specs/`.
