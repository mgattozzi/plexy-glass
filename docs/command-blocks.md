# Command blocks

Scrollback is normally just a wall of lines, but once your shell emits OSC 133
prompt marks plexy-glass understands the **command-block structure** of its
output. A block is the unit from one shell prompt through its command and
output to the next prompt, and with blocks active you can navigate your
scrollback by command rather than by line, yank a command's output without
manual selection, and capture the last command's output from a script.

## What is a block?

When a shell emits OSC 133 marks, each command forms a **block**:

```
OSC 133;A  — prompt start (the row becomes a block boundary)
  $ your command here
OSC 133;B  — prompt end (end of the prompt text)
OSC 133;C  — output start (command began executing)
  ...command output...
OSC 133;D;0 — command end (exit code 0)
```

Marks ride on their rows through scrollback, eviction, and terminal resize, so
there is no index to go stale. The **last completed block** is the most recent
block with an exit code (an OSC 133;D); a command that is still running has no
D yet and is not counted.

Note that marks are **not** recorded on the alternate screen (full-screen
programs like editors and pagers).

Marks live in the pane's scrollback for the life of the session (in memory), so
block navigation, exit-status borders, and `capture --last-command` keep working
as you scroll back through earlier commands. They are **not** persisted to disk:
when the daemon stops, the session (and its marks) are gone, since we
deliberately don't save sessions across restarts.

## Shell integration

Blocks light up only when your shell emits OSC 133. Several popular terminal
emulators ship shell-integration scripts that already do this.

**The easy path:** `plexy-glass shell-integration <bash|zsh|fish|nu>` prints a
ready-to-eval snippet, so add `eval "$(plexy-glass shell-integration zsh)"` (or
the fish/bash equivalent) to your shell rc and you're done. The snippets below
are the same thing, for reference or hand-installation.

### Ghostty / iTerm2 / kitty / VS Code

If you use **bash**, **zsh**, or **fish**, the shell-integration scripts from
Ghostty, iTerm2, kitty, or VS Code all emit OSC 133 already. Source whichever
you prefer and command blocks work for free, no further configuration needed.

### Nushell

Nushell ships OSC 133 support built in, so there is no snippet to add. It's
the `shell_integration.osc133` flag, on by default:

```nu
# config.nu — on by default; this line just makes it explicit.
$env.config.shell_integration.osc133 = true
```

nushell emits all four marks itself, independent of your prompt, so starship
and oh-my-posh don't interfere, and you should *not* try to set the marks
manually via `PROMPT_COMMAND` or `pre_execution`/`pre_prompt` hooks. Doing so
fights the built-in (and if a later `$env.config = { … }` reassignment or a
prompt-framework `init` runs after your hooks, it silently wipes them), and
the symptom is `capture --last-command` reporting *no command blocks*. If you
added a manual snippet from an older version of these docs, remove it and
rely on the built-in.

> One gotcha: nushell emits the prompt/exec markers only after the terminal
> answers its startup cursor-position query (`ESC[6n`). Real terminals (Ghostty,
> iTerm, kitty, wezterm, and plexy-glass itself) answer it, so this is automatic;
> it only bites synthetic/bare PTYs that don't reply.

### bash / zsh

A minimal snippet if you're not already running a terminal integration script:

```bash
# bash: add to ~/.bashrc
__osc133_pre()  { printf '\033]133;C\007'; }
__osc133_post() { printf '\033]133;D;%s\007' "$?"; }
__osc133_ps1()  {
    PS1="\[\033]133;A\007\]${PS1}\[\033]133;B\007\]"
}
PROMPT_COMMAND="__osc133_post; __osc133_ps1"
trap '__osc133_pre' DEBUG
```

```zsh
# zsh: add to ~/.zshrc
preexec() { printf '\033]133;C\007'; }
precmd()  { printf '\033]133;D;%s\007' "$?"; }
PS1=$'%{\033]133;A\007%}'"$PS1"$'%{\033]133;B\007%}'
```

## Navigation

### Outside copy mode — viewport scroll

Two bindable verbs scroll the pane viewport so the target prompt row lands at
the *top* of the screen:

| Keys | Action |
|---|---|
| `Ctrl+a <` | Previous prompt (scroll back one command) |
| `Ctrl+a >` | Next prompt (scroll forward one command) |

`next` past the newest prompt snaps back to live, the same as ordinary wheel-
scroll behavior. Any other keystroke also snaps back to live, consistent with
wheel scrollback.

The same moves work as prompt verbs, in the command prompt and from
`plexy-glass cmd`: `:prev-prompt` / `:next-prompt`.

### Inside copy mode

Copy mode has dedicated block-navigation keys:

| Key | Action |
|---|---|
| `[` | Jump to the previous prompt (moves the cursor to that row, col 0) |
| `]` | Jump to the next prompt |
| `o` | Select the current block's output region; then `y` to yank |

Note that `[` at the oldest prompt and `]` at the newest are no-ops (no
wraparound). Both keys cross the scrollback/grid boundary and scroll the
copy-mode viewport to keep the cursor visible.

**`o` (output selection)** places the selection anchor at the block's
output-start row (col 0) and the cursor at the block's last row (last col), so
the selection covers everything from the `133;C` row through the row before
the next prompt. If the block has no `133;C` mark (some shells omit it), the
prompt row itself is used as the start. Press `y` after `o` to yank the
selection into the clipboard and paste-buffer stack as usual.

## Output yank without copy mode

**`:copy-output`** (bindable verb: `copy_output`, no default chord) yanks the
last completed block's output straight to the clipboard and the paste-buffer
stack, from the output-start row through the block end, scrollback-inclusive,
so you never have to enter copy mode for it.

Status messages:

- Success: `"copied output of last command"`
- No blocks: `"no command blocks — shell integration not active? see docs/command-blocks.md"`

If you reach for this a lot, bind a chord in your config:

```kdl
keymap {
    bind "prefix o" "copy_output"
}
```

## Scrolled-view mouse: click a prompt to jump

While scrolled back (the viewport is showing history, not the live shell), a
**plain left-click on a prompt row** scrolls the viewport so that prompt sits
at the top. It's the fastest way to re-center a command you spotted in the
history.

The jump only fires while scrolled back (`scroll_offset > 0`). At the live view
(no scroll), a click on a prompt row behaves as a normal click. We deliberately
keep the jump this narrow, since at offset 0 the prompt row is already in view
and the jump would have no visible effect.

## Scripting: `capture --last-command`

```sh
plexy-glass capture --last-command [-n NAME]
```

Prints the last completed block's output (scrollback-inclusive) to stdout,
with trailing blank lines dropped. Exits 0 on success, and exits 1 with
`"no command blocks — shell integration not active? see docs/command-blocks.md"`
when no block exists.

Popup-aware: with a popup open it targets the popup's pane, same as plain
`capture`.

If you want a synchronous alternative that also injects the command and waits
for it to finish, that's `plexy-glass run`, see
[docs/scripting.md](scripting.md#run--synchronous-command-execution).

**Example** (grab the last build output):

```sh
# Run a build, then capture the full output regardless of scroll position
plexy-glass send -n work --enter "cargo build 2>&1"
plexy-glass capture --last-command -n work > /tmp/build.log
```

### `--json` — structured output

```sh
plexy-glass capture --last-command --json [-n NAME]
```

Prints one compact JSON object + newline to stdout instead of plain text:

```json
{"command_line":"cargo test 2>&1","exit_code":0,"output":"running 42 tests\n..."}
```

Keys (`serde_json` sorts them alphabetically):

| Key | Type | Notes |
|---|---|---|
| `output` | string | The block's output text, same as the plain `capture --last-command` output |
| `exit_code` | number or null | The closing `133;D` exit code; null when the D mark carried no code |
| `command_line` | string or null | The command the user typed, recovered from the prompt-end (`133;B`) mark; null when the shell omitted `B`/`C` or when B and C share a row |

**`jq` example**:

```sh
# Fail the script if the last command did not exit 0
plexy-glass capture --last-command --json -n work \
  | jq -e '.exit_code == 0' > /dev/null
```

**Accepted edges on `command_line`**:

- **PS2 / heredoc prefixes**: if the command spans multiple lines (e.g. a
  here-doc), the shell's secondary-prompt string (`> `) appears on continuation
  rows, because the emulator sees the rendered prompt characters. The result
  includes those prefixes, so what you get is the recorded screen-scraping
  approximation.
- **RPROMPT**: some shells render a right-prompt on the `133;B` row. Because
  trailing whitespace is trimmed per physical row, the RPROMPT tail survives
  trimming (it is not at the true trailing edge of the command text). Rare in
  practice.
- **Soft-wrapped commands**: a command that wraps at the terminal margin is
  joined WITHOUT a newline, because it was one logical line the user typed. A
  typed space at the exact wrap boundary may be dropped (the terminal trims
  the trailing space from the physical row).
- **Hard line boundaries** (the user pressed Enter in a multi-line construct,
  e.g. `for`/`do`/`done`): those rows join WITH `\n`, a real newline in the
  user's input.

## Exit status on the border

When shell integration is active, each pane's **left border line** is
color-coded per visible row by the command block the row belongs to:

| Border | Meaning |
|---|---|
| Half-block `▌` in the ok color (default: `#87a987`) | Row belongs to a block that exited with code 0 |
| Half-block `▌` in the fail color (default: `#c4746e`) | Row belongs to a block that exited with a nonzero code |
| Plain `│` | Row before the first prompt, a running block (no exit code yet), or a block end (`133;D`) that arrived without an exit code |

A block row's `│` is drawn as the solid half-block `▌` whether it passed or
failed, and **color** (ok vs fail) is what distinguishes them, so a passing
block reads as a solid bar, not a faint line. The entire block (prompt row
through the row before the next prompt) takes the same status, so a glance at
the border shows which commands succeeded and which failed, even after
scrolling back.

**Viewport-tracked**: the coloring always matches what is on screen, whether
you are at the live view, scrolled back with the mouse wheel, or in copy mode.

**Popup panes**: the popup box's left border takes the same per-block exit-status
coloring. Popups always show the live grid, so the coloring reflects the popup
pane's current shell state.

**Alternate screen** (`hx`, `less`, and other full-screen programs): while the
alternate screen is active the border reverts to plain, because we deliberately
don't record marks on the alternate screen. Full-screen programs are not
command-block flows, so this is correct behavior, not a limitation. Status
coloring returns automatically when the program exits.

**Precedence**: a marked pane's ring (bright magenta) beats block status, so a
`▌` will never appear on a marked-pane border. Block status beats the active
pane's blue ring on the colored rows; plain (None) rows still show the active
ring as usual.

**Requires OSC 133 shell integration**, same as all the other block features.

### Configuration

The `blocks` node in `config.kdl` controls this feature. See the
[`blocks` section of docs/configuration.md](configuration.md#blocks) for the
full reference (colors, `enabled` flag, defaults, and live-reload behavior).

## Command duration

Each block is timed from when its command starts running (`133;C`) to when it
finishes (`133;D`), and the elapsed time shows up as a dim, right-aligned note
on the command row, right where the fold summary sits:

```
$ cargo build --release                                      2.3s
```

Timing every `ls` and `cd` would just be noise, so only commands at or above a
threshold get a note (default **2s**). Set `duration-threshold "0"` to time
everything. When a block is also folded, the duration appends to the fold
summary: `▸ 412 lines ✓ · 2.3s`.

The duration shows in the live/scrollback view and in block mode, but not in
copy mode, which renders raw text for selection. Turn it off with
`duration #false` in the `blocks` node.

**Requires OSC 133 shell integration**, same as all the other block features.

## Sticky command header

When you **scroll back** into a long command's output (a big `cargo build`, a
long `git log`), its command line scrolls off the top and you lose track of
what produced the wall of text. The **sticky header** pins it: the command
line stays on the pane's top row (a dim line that blends with your theme, not
a bright bar) for as long as that block's output fills the top of the
viewport.

```
  cargo build --release                                        2.3s     ← pinned (dim)
    Compiling plexy-glass-emulator v0.1.0
    Compiling plexy-glass-mux v0.1.0
```

The header carries the block's duration too (same threshold). It appears
**only while you are scrolled back**, since at the live bottom you're watching
fresh output and don't need it, and only in the live view (block mode already
lists every command line, and copy mode owns the selection cursor). A folded
block never triggers it (its output is collapsed, so nothing of it is on
screen to scroll into). Turn it off with `sticky-header #false` in the
`blocks` node.

**Requires OSC 133 shell integration**, same as all the other block features.

## Block mode

Press `Ctrl+a b` to enter **block mode**, a dedicated view for navigating the
command blocks in the focused pane. The selected block gets a bright capped
bracket (`┏ ┃ ┗`) on the pane's left border, and the viewport scrolls to keep
it visible as you move.

Block mode refuses to open when the pane has no command blocks, which happens
when a full-screen application is running (the alternate screen) or the shell
has no OSC 133 integration. You stay in normal mode and
`no command blocks in this pane` flashes on the status line.

| Key | Action |
|-----|--------|
| `j` / `↓` | select the next (newer) block |
| `k` / `↑` | select the previous (older) block |
| `g` / `G` | select the oldest / newest block |
| `/` | filter blocks by command + output (incremental; dims non-matches, highlights the match) |
| `J` / `K` | jump to the next / previous **failed** block (within the filter) |
| `y` | yank the whole block (prompt + command + output) |
| `o` | yank the block's output only |
| `c` | yank the block's command line only |
| `r` | re-run the block's command (injects it + Enter, then exits) |
| `Tab` | fold / unfold the selected block's output |
| `Z` | fold all completed blocks |
| `O` | unfold all blocks |
| `Esc` / `q` | leave block mode |

Yanks go to the paste-buffer stack (paste with `Ctrl+a ]`) **and** the system
clipboard, the same destinations as `copy-output`. The three yanks stay in
block mode so you can copy several blocks in a row, while `r` and `Esc`/`q`
leave it. Any other key is swallowed (block mode is modal, nothing leaks to
the pane's shell), but `Ctrl+a` prefix chords still work, so you can switch
panes or detach without leaving block mode first.

### Filter and failed-jump

`/` makes the filter the **lens**: type a query and the navigable set narrows to
blocks whose **command + output** contains it (case-insensitive, incremental).
While a filter is active, `j`/`k`/`g`/`G` only visit matching blocks,
non-matching blocks are **dimmed**, and the query is **highlighted** inside each
match; the prompt shows a live count (`filter: cargo (3/7)`). `Enter` commits
the filter; `Esc` while typing clears it; with a committed filter, `Esc` clears
it and `Esc` again (no filter) leaves block mode.

`J` / `K` jump to the next / previous **failed** block (nonzero exit), within
the current filter if one is active, for a fast "take me to what broke" loop.
All relative motions (`j`/`k`, `J`/`K`) **wrap** around the ends of the set.

`:block-mode` opens block mode from the command prompt. The bracket color is
the `blocks` node's `select-color` (see
[the `blocks` node in docs/configuration.md](configuration.md#blocks)), and
the entry chord is configurable like any other binding (`enter_block_mode`).

**Requires OSC 133 shell integration**, same as all the other block features.

### Folding

`Tab` collapses the selected block's **output**, keeping the command line
visible; `Tab` again expands it. `Z` folds every completed block at once, `O`
unfolds them all. The fold takes effect **instantly in block mode** (the output
collapses as you press `Tab`), and a folded block's command line is **dimmed**
with a dim, right-aligned `▸ N lines ✓`/`✗` summary (the hidden line count and
the command's exit status), so a fold reads clearly as folded and not as a
command with no output.

Folds **persist after you leave block mode** and across re-entering it, and
that's the whole point: declutter your working view, then keep typing in it.
In both the live view and block mode the output rows vanish and the rows below
shift up (the prompt stays at the bottom; older history fills in at the top).
Note that only **completed** blocks fold, the running command and the prompt
you're typing at never collapse, and a command with no output isn't foldable.

Notes / current limits:

- Copy mode renders blocks **expanded** (raw text for selection); the fold
  markers still show, and the collapse re-applies when you leave copy mode.
- Folds are **runtime-only**, so they don't survive a daemon restart.
- Scrolled navigation is fold-exact: wheel scroll moves by visible lines (folds
  skipped, no dead zone), and `Ctrl+a <`/`>` and click-a-prompt-to-jump land the
  target at the viewport top through the visible-line projection.

## History palette

`Ctrl+a /` (or `:history`) opens the **structured history palette**, a finder
over your command blocks across **every session**, not just the focused pane.
Unlike a shell history search (`Ctrl+R`, atuin), it searches a block's **output**
as well as its command, and shows the exit status and duration:

```
 History
 filter: refused█                                    1/14
 ✗ 45s    web/1   cargo test --workspace
 ✓ 2.3s   api/2   docker compose up -d
 ↑/↓ select · enter jump · esc cancel
```

- **Type** to filter incrementally over **command + output** (case-insensitive);
  the count updates live.
- **↑/↓** (or **Ctrl-P/Ctrl-N**) move the selection; **Home/End** jump to ends.
- **Enter** jumps to the block: it switches to the block's session/window/pane
  (as needed) and opens **block mode** on it, where the usual keys (`y`/`o`/`c`
  yank, `r` re-run, `Tab` fold, `J`/`K` failed-jump) take over. The palette is a
  *finder*; block mode is where you act.
- **Esc** cancels.

Each row shows a status glyph (`✓` exit 0, `✗` nonzero), the duration, the
`session/window` it came from, and the command. Rows are ordered with the
**current pane's** blocks first (newest first), then the rest of the session,
then other sessions.

The palette is built fresh when you open it, by reading every pane's live grid
**and scrollback**, so there is no separate history database to keep in sync.
If a command has run in several places, the jump re-finds it by command text at
jump time, so it lands correctly even if the pane has scrolled since you opened
the palette.

**Requires OSC 133 shell integration**, same as all the other block features.

## Limitations

- **Scrollback cap on restore**: only the most recent 5000 rows per pane are
  persisted, and older history is truncated. Restored rows keep their saved
  width (no reflow until the first resize), and OSC 8 hyperlinks are not
  persisted (text and styling survive; link clickability does not). Note that
  persistence is opportunistic: it's captured at the next *structural* save,
  not continuously and not on detach (see
  [Persistence](configuration.md#persistence)).
