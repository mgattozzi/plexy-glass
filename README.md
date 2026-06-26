# plexy-glass

**The multiplexer that doesn't downgrade your terminal to 1990.**

A terminal multiplexer (in the tmux/zellij family) written in Rust, built as a
daemon/client pair: a daemon owns the sessions and PTYs, and lightweight
clients attach, detach, and reattach to them. Other multiplexers strip or
mangle the modern terminal, so plexy-glass treats rich escape-sequence traffic
as a first-class concern instead: inline images (Kitty/Sixel/iTerm2), OSC 7
(working directory), OSC 8 (hyperlinks), OSC 52 (clipboard), OSC 133 (prompt
marks), OSC 0/1/2 (titles), and OSC 10/11/12 (colors) are parsed and routed
per pane rather than stripped, and it negotiates keyboard protocols (the Kitty
keyboard protocol and xterm's modifyOtherKeys) in both directions, so panes
can ask for the richer key encodings and the client negotiates the same with
the outer terminal. It's designed alongside Ghostty-style terminal
integration.

## Features

- Sessions, windows, and panes with horizontal/vertical splits, pane zoom, and
  keyboard or mouse resize
- Detach and reattach to a running session: the daemon keeps your windows,
  panes, and scrollback alive in memory while you're away. Note that sessions
  are *not* persisted to disk, so when the daemon stops its sessions are gone
  and a fresh daemon starts clean. Declare sessions in `config.kdl` to get the
  same layout built fresh every time (see
  [declarative sessions](docs/configuration.md#session--declarative-sessions)).
- Multiple clients attached to the same session
- Copy mode with search
- Full mouse support, including click-to-focus, drag-resize on split borders,
  **Alt+drag a window tab to reorder windows**, and **Alt+drag a pane onto
  another pane in the same window to swap their positions** (plain click still
  selects; the modifier is configurable, see
  [`mouse { drag-modifier }`](docs/configuration.md#mouse))
- Popup panes: `Ctrl+a P` or `:popup [cmd]` opens a transient floating pane
- Pipe-pane: stream a pane's raw output to an external command with
  `:pipe-pane <cmd>` (e.g. `:pipe-pane tee -a session.log`), scriptable via
  `plexy-glass cmd`
- Preset layouts: five tmux-style presets (`even-horizontal`, `even-vertical`,
  `main-horizontal`, `main-vertical`, `tiled`), cycled with `Ctrl+a Space` or
  applied by name with `:layout <name>`
- Paste buffers: copy-mode yanks push a named-buffer stack; paste the newest
  with `Ctrl+a ]` or a named one with `:paste bufferN`, pick one with
  `Ctrl+a =`, and bridge text/files with `:set-buffer`, `:save-buffer`, and
  `:load-buffer` (scriptable via `plexy-glass cmd`)
- Per-window monitoring: activity (`#`), bell (`!`), silence (`~`,
  `:monitor-silence <secs>`), and command completion (`✓`/`✗`,
  `:monitor-command`), all surfaced as status-bar window-list flags and
  edge-triggered status-line alert messages
- Configurable status bar with live config reload
- Dynamic window names: an unnamed window derives its name from the active pane
  (running command → directory → shell) and updates live; an explicit or
  declared rename pins it. Toggle with `auto-rename #true|#false`
  ([reference](docs/configuration.md#auto-rename))
- Configurable glyph tier: `glyphs "unicode"` (default, any font) /
  `glyphs "nerd"` (Nerd Font icons + powerline separators) /
  `glyphs "ascii"` (ASCII-only fallback)
  ([reference](docs/configuration.md#glyphs))
- KDL v2 configuration (`config.kdl`)
- Declarative sessions: recursive `session → window → split/pane` layouts in
  the config, with per-pane commands and working directories, split ratios
  (`ratio=` weights), an active window/pane (`active=#true`), and per-pane
  `env` overlays; reload re-reads the templates (building newly-declared
  sessions, never rebuilding live ones) and `:switch` auto-creates a
  declared-but-not-running session
- A visual session picker (`Ctrl+a w`) and a choose-tree
  (session → window → pane drill-down with incremental filter `/`,
  collapse/expand `h`/`l`, and session rename `r`, `Ctrl+a W`)
- Pane mobility: break a pane to its own window, join it elsewhere, swap
  panes (including cross-window swap with the marked pane), and a marked pane
  for cross-window moves; focus and zoom follow the slot, mark is preserved
- Keyboard-protocol negotiation: Kitty keyboard protocol and modifyOtherKeys,
  per pane, with graceful fallback and clean teardown of the outer terminal
- Colored underlines (SGR 58/59), advertised to applications
- Command-block awareness (OSC 133, [needs shell
  integration](#shell-integration-recommended)): navigate scrollback by
  prompt with `Ctrl+a <` / `>`, jump prompts in copy mode with `[` / `]`,
  click a prompt row while scrolled back to jump there, select a command's
  output with `o` then `y`, yank it with `:copy-output`, or capture it from a
  script with `plexy-glass capture --last-command` (plain text) or
  `plexy-glass capture --last-command --json` (structured `{"output",
  "exit_code", "command_line"}`); `plexy-glass run --json` returns the same
  structure for synchronous runs; each pane's left border (popup pane borders
  included) is color-coded per row by block exit status (ok color `│` /
  fail color `▌`), viewport-tracked and live-reloading with the `blocks`
  config node

## Quick start

Requires Rust 1.85+ (edition 2024), so `rustup update` if you're behind.

```sh
git clone https://github.com/mgattozzi/plexy-glass
cd plexy-glass
cargo install --path .        # installs to ~/.cargo/bin (already on your PATH)
plexy-glass                   # attach to (or create) the default session "main"
```

Prefer not to install? Build it and run from the target directory instead:

```sh
cargo build --release         # binary lands at target/release/plexy-glass
./target/release/plexy-glass  # or add target/release to your PATH
```

Then:

```sh
plexy-glass                   # attach to (or create) the default session "main"
plexy-glass attach -n work    # attach to (or create) the session "work"
```

Running `plexy-glass` with no subcommand is the same as `plexy-glass attach`.
The daemon is spawned automatically on first attach. The first time you attach
to a fresh daemon, a **welcome modal** shows the essentials: the prefix
(`Ctrl+a`), a few key bindings, how to open help (`Ctrl+a ?`) and detach
(`Ctrl+a d`), and how to turn it off (`welcome #false` in `config.kdl`). Press
any key to dismiss it. Detach with `Ctrl+a d` and the session keeps running in
the daemon (in memory) until you reattach or the daemon stops.

Other subcommands:

| Command | What it does |
|---|---|
| `plexy-glass list` | List all running sessions |
| `plexy-glass kill -n NAME` | Kill a single session by name |
| `plexy-glass kill` | Stop this runtime dir's daemon |
| `plexy-glass kill --all` | Stop every plexy-glass daemon for the current user |
| `plexy-glass reload` | Reload `config.kdl` from the platform config dir |
| `plexy-glass cmd [-n NAME] <LINE>...` | Run one or more command-prompt lines against a session |
| `plexy-glass send [-n NAME] [--enter] <TEXT>...` | Type text into the focused pane (popup-aware) |
| `plexy-glass capture [-n NAME]` | Print the focused pane's visible screen text (popup-aware) |
| `plexy-glass run [-n NAME] [--timeout SECS] <COMMAND>...` | Type a command into the focused pane, wait for OSC 133 completion, print the output, and exit with the command's exit code (requires shell integration) |
| `plexy-glass shell-integration <bash\|zsh\|fish\|nu>` | Print an OSC 133 shell-integration snippet for your shell (see below) |

(`plexy-glass daemon` exists but auto-spawn runs it for you internally, so
the only time you'd type it is with `--foreground` for development.)

### Shell integration (recommended)

Several headline features light up only when your shell emits **OSC 133**
prompt marks: exit-status pane borders, prompt navigation (`Ctrl+a <` / `>`),
block mode, the history palette's output search, `plexy-glass run`, and
command-completion notifications. Wiring it up is one line in your shell's rc
file:

```sh
# bash (~/.bashrc) / zsh (~/.zshrc) / fish (~/.config/fish/config.fish)
eval "$(plexy-glass shell-integration zsh)"
```

If you use **Ghostty, iTerm2, kitty, or VS Code**, their shell-integration
scripts already emit OSC 133 and you don't need this. **Nushell** has it built
in (`plexy-glass shell-integration nu` just prints the config line). Without
shell integration plexy-glass still works fully, the block-aware features are
simply inert. See [docs/command-blocks.md](docs/command-blocks.md) for
details.

### Running a second, isolated instance

By default every invocation by the same user shares one daemon (one socket,
one set of live sessions). To run a fully separate instance, for example to
test a build without touching your daily-driver daemon, set `PLEXY_GLASS_DIR`
to a directory of your choice:

```sh
PLEXY_GLASS_DIR=~/.plexy-test plexy-glass        # spawns/attaches an isolated daemon
PLEXY_GLASS_DIR=~/.plexy-test plexy-glass list   # lists only that instance's sessions
PLEXY_GLASS_DIR=~/.plexy-test plexy-glass kill   # stops only that instance's daemon
```

When set, the daemon roots its runtime files (`run/`) and logs (`logs/`) under
that directory, so the two instances never collide. Note that it deliberately
does *not* override the config location, both instances read the same
`config.kdl`. Because the variable is inherited by the auto-spawned daemon,
every subcommand run with it set targets the same isolated instance.
(`plexy-glass kill --all` remains a UID-wide sweep across *all* instances, so
use the scoped `kill` above to leave your daily driver alone.)

### Scripting

The `cmd`, `send`, `capture`, and `run` verbs let you drive a running session
from a script or another tool, no terminal attachment required:

```sh
# Apply a structural command, then run a test and check the output
plexy-glass cmd -n work "split v" "layout main-vertical"
plexy-glass send -n work --enter "cargo test"
plexy-glass capture -n work | grep "test result: ok"

# Gate a commit on the test suite — synchronous, exit code passthrough
plexy-glass run -n work "cargo test" && plexy-glass run -n work "jj commit -m wip"
```

`cmd` reuses the command-prompt grammar verbatim. `run` injects a command and
waits for the OSC 133 completion mark (so it needs shell integration). `cmd`,
`send`, and `capture` exit 0 on success and 1 on any failure. `run` exits
with the command's own exit code (0–255), 124 on timeout, and 1 for
plexy-glass failures. `-n NAME` targets a session, and without `-n` the sole
running session is used (error if zero or more than one). Both
`capture --last-command` and `run` accept `--json` to print a structured
`{"output", "exit_code", "command_line", …}` object instead of plain text.
See [docs/scripting.md](docs/scripting.md) for the full reference.

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
| `Ctrl+a /` | History palette (cross-session block finder) |
| `Ctrl+a f` | Hint mode (label on-screen URLs/paths/hashes; key to copy) |
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

### Command blocks

| Keys | Action |
|---|---|
| `Ctrl+a <` | Previous prompt (scroll back one command) |
| `Ctrl+a >` | Next prompt (scroll forward one command) |
| `Ctrl+a b` | Block mode (navigate / yank / re-run / fold; `/` filter, `J`/`K` jump to failures) |

These work outside copy mode. **Block mode** (`Ctrl+a b`) outlines the selected
block and adds `j`/`k`/`g`/`G` navigation (wrapping), `y`/`o`/`c` yanks (whole /
output / command), `r` to re-run, `Tab` to fold/unfold a block's output
(`Z`/`O` fold/unfold all, and a folded block keeps a `▸ N lines ✓` summary and
stays folded in the live view), `/` to filter by command + output (dims
non-matches, highlights the match), and `J`/`K` to jump to failed blocks within
the filter. Inside copy mode: `[` / `]` jump to the previous / next prompt, and
`o` selects the current block's output region (then `y` to yank). See
[docs/command-blocks.md](docs/command-blocks.md).

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

Other topic docs:
- [docs/scripting.md](docs/scripting.md) covers the `cmd`, `send`, `capture`,
  and `run` CLI verbs
- [docs/command-blocks.md](docs/command-blocks.md) covers OSC 133
  command-block navigation and capture
- [docs/inline-graphics.md](docs/inline-graphics.md) covers inline images in
  panes (Kitty graphics, Sixel, and iTerm2; per-client placeholder box
  fallback)

## Troubleshooting

The daemon logs to a file, not your terminal, so if something misbehaves the
first place to look is:

- **macOS:** `~/Library/Logs/plexy-glass/daemon.log`
- **Linux:** `$XDG_STATE_HOME/plexy-glass/daemon.log` (falls back to
  `~/.local/state/plexy-glass/daemon.log`)

Raise the verbosity by setting `RUST_LOG` before the daemon first spawns, e.g.
`RUST_LOG=plexy_glass=debug plexy-glass`. Note that with `PLEXY_GLASS_DIR` set,
logs live under `<dir>/logs/daemon.log` instead.

## Status

Under active development and closing in on a first public release. The design
spec for each feature lives in `docs/superpowers/specs/`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in this
project by you, as defined in the Apache-2.0 license, shall be dual licensed as
above, without any additional terms or conditions.
