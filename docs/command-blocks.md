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
programs like editors and pagers) and are **not** persisted across daemon
restarts, so a restored pane starts without blocks.

## Shell integration

Blocks light up only when your shell emits OSC 133. Several popular terminal
emulators ship shell-integration scripts that already do this.

### Ghostty / iTerm2 / kitty / VS Code

If you use **bash**, **zsh**, or **fish**, the shell-integration scripts from
Ghostty, iTerm2, kitty, or VS Code all emit OSC 133 already. Source whichever
you prefer and command blocks work for free, no further configuration needed.

### Nushell

Nushell doesn't bundle OSC 133 support, but you can add it directly in your
`config.nu`. I checked the snippet below against nu's hook API and smoke-tested
it with `nu -c` on a local nu install:

```nu
# OSC 133 shell integration for plexy-glass
# Add this to your config.nu

# 133;A (prompt start) and 133;B (prompt end) wrap the rendered prompt.
$env.PROMPT_COMMAND = {
    print -n "\u{001b}]133;A\u{0007}"
    # Insert your actual prompt here, e.g.:
    $"(ansi green)(pwd)(ansi reset) "
}
$env.PROMPT_INDICATOR = "\u{001b}]133;B\u{0007}> "

# 133;C fires just before the command runs.
# 133;D fires before the next prompt, recording the previous exit code.
$env.config = ($env.config | upsert hooks {
    pre_execution: (
        ($env.config.hooks.pre_execution? | default []) ++
        [{|| print -n "\u{001b}]133;C\u{0007}"}]
    )
    pre_prompt: (
        ($env.config.hooks.pre_prompt? | default []) ++
        [{|| print -n $"\u{001b}]133;D;($env.LAST_EXIT_CODE)\u{0007}"}]
    )
})
```

**Ordering note**: `pre_prompt` runs immediately before nu renders the next
prompt, so the 133;D (command end) is emitted there, before the following
133;A, and that is the correct sequence. The `++ []` append form preserves any
hooks you already have.

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

## Exit status on the border

When shell integration is active, each pane's **left border line** is
color-coded per visible row by the command block the row belongs to:

| Border | Meaning |
|---|---|
| Colored `│` in the ok color (default: `#87a987`) | Row belongs to a block that exited with code 0 |
| Colored `▌` in the fail color (default: `#c4746e`) | Row belongs to a block that exited with a nonzero code |
| Plain `│` | Row before the first prompt, a running block (no exit code yet), or a block end (`133;D`) that arrived without an exit code |

The entire block (prompt row through the row before the next prompt) takes the
same status, so a glance at the border shows which commands succeeded and which
failed, even after scrolling back.

**Viewport-tracked**: the coloring always matches what is on screen, whether
you are at the live view, scrolled back with the mouse wheel, or in copy mode.

**Alternate screen** (`hx`, `less`, and other full-screen programs): while the
alternate screen is active the border reverts to plain. Status coloring comes
back on its own when the program exits.

**Precedence**: a marked pane's ring (bright magenta) beats block status, so a
`▌` will never appear on a marked-pane border. Block status beats the active
pane's blue ring on the colored rows; plain (None) rows still show the active
ring as usual.

**Requires OSC 133 shell integration**, same as all the other block features.

### Configuration

The `blocks` node in `config.kdl` controls this feature. See the
[`blocks` section of docs/configuration.md](configuration.md#blocks) for the
full reference (colors, `enabled` flag, defaults, and live-reload behavior).

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

## Limitations (v1)

We track the following as future work:

- **No `--json` flag** on `capture --last-command`: the flag that would return
  `{ text, exit_code, command_line }` as JSON is deferred.
- **No block-aware mouse**: clicking on a prompt does not jump to it.
- **No mark persistence**: marks are not saved on disk; a restored pane starts
  without block history.
- **Alt-screen marks ignored**: full-screen programs that emit 133 sequences
  while on the alt screen are not tracked.
