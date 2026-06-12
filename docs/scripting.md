# Scripting from the CLI

The `cmd`, `send`, `capture`, and `run` verbs let scripts and agents drive a
running session over the daemon socket without attaching a terminal. They act
on the session's focused window and pane, the same target an attached client
would affect.

```sh
# Structural setup, run a test, read the result
plexy-glass cmd -n work "split v" "layout main-vertical"
plexy-glass send -n work --enter "cargo test"
plexy-glass capture -n work | grep "test result: ok"
```

## Verbs

| Verb | Flags | What it does |
|---|---|---|
| `plexy-glass cmd [-n NAME] <LINE>...` | one or more prompt lines | Run each line through the command-prompt grammar in order; stop at the first failure (exit 1). Confirmation messages print to stdout; errors print to stderr. |
| `plexy-glass send [-n NAME] [--enter] <TEXT>...` | `--enter` appends `\r` | Join the TEXT fragments with single spaces and write them to the session's input path. |
| `plexy-glass capture [-n NAME]` | (none) | Print the focused pane's visible screen text to stdout (per-line trailing whitespace trimmed, trailing blank lines dropped). |
| `plexy-glass capture --last-command [-n NAME]` | `--last-command` | Print the last completed OSC 133 command block's output (scrollback-inclusive) to stdout. Exits 1 with `"no command blocks…"` when the pane has no completed block (shell integration not active). |
| `plexy-glass run [-n NAME] [--timeout SECS] <COMMAND>...` | `--timeout SECS` | Type COMMAND + Enter into the input-target pane, wait for the OSC 133 completion mark, print the block output to stdout, and exit with the command's exit code. Requires OSC 133 shell integration. |

## Session resolution

All three verbs share the same resolution rule: `-n NAME` selects that session,
or exits 1 with `no session "NAME"` if it does not exist. Without `-n` we use
the sole running session if there is exactly one; zero sessions exits 1 with
`no sessions running`, and two or more exits 1 listing them as
`multiple sessions running: a, b — use -n`.

## Command grammar

`cmd` lines go through the same parser as the interactive `Ctrl+a :` prompt,
so the vocabulary is identical (see
[The command prompt](configuration.md#the-command-prompt) for the full verb
table) and every future prompt verb is scriptable for free.

Some verbs require an attached client and are refused headlessly:

| Refused verb | Reason |
|---|---|
| `detach` | Acts on the calling client, and a one-shot connection has none |
| `switch <session>` | Switches the calling client's session |
| `help` | Opens a modal overlay that would hijack whoever is attached |
| `sessions` | Opens a modal overlay |
| `tree` | Opens a modal overlay |
| `buffers` | Opens a modal overlay |

These return exit 1 with `<verb>: requires an attached client`. All other
verbs, including `reload` and `paste`, work headlessly.

## Exit-code semantics

Exit 0 means all operations succeeded. Exit 1 means at least one failed. For
`cmd` with multiple lines, the run stops at the first failure (subsequent lines
are not sent). Errors are printed to stderr.

## `capture --last-command`

`--last-command` captures the output of the last completed command block in the
pane (requires OSC 133 shell integration). The output is scrollback-inclusive,
so the full command output comes back even if it has scrolled off screen.

```sh
# Capture the last build output into a file
plexy-glass send -n work --enter "cargo build 2>&1"
plexy-glass capture --last-command -n work > /tmp/build.log

# Fail fast if shell integration is not active
plexy-glass capture --last-command -n work || echo "no blocks" >&2
```

Exit 0 means a completed block was found and printed; exit 1 with
`"no command blocks — shell integration not active? see docs/command-blocks.md"`
means no block exists (integration not configured, or the pane was just
restored). See [docs/command-blocks.md](command-blocks.md) for shell-integration
setup.

## `run` — synchronous command execution

```sh
plexy-glass run [-n NAME] [--timeout SECS] <COMMAND>...
```

Types `COMMAND` (fragments joined with single spaces) followed by Enter into
the session's input-target pane (popup-aware), waits for the shell to emit an
OSC 133 `D` completion mark, prints the command's output to stdout, and exits
with the command's exit code. Unlike `send` + `capture`, the whole
send→wait→print sequence is atomic from the script's point of view.

**Requires OSC 133 shell integration.** See [docs/command-blocks.md](command-blocks.md)
for setup.

### Preconditions

`run` checks three conditions before injecting anything, so a busy pane never
gets bytes typed into it blind. If any fails it exits 1 with the matching
message on stderr:

| Condition | Stderr message |
|---|---|
| Shell integration active (any OSC 133 mark in the pane) | `no command blocks — shell integration not active? see docs/command-blocks.md` |
| No command already running (pane is at a prompt) | `pane is busy: a command is running` |
| Not in a full-screen application | `pane is busy: alternate screen is active` |

All three are checked atomically in a single screen snapshot, and that single
closure also closes the window between the precondition check and the
completion-counter baseline read. Note that a residual PTY-backlog window
remains (the spec's fencing-honesty note explains it), which `run`'s contract
already disclaims.

### Exit codes

| Code | Meaning |
|---|---|
| 0–255 | The command's own exit code, passed through directly. |
| 124 | `--timeout SECS` expired before the command finished. The command is **not** killed, it keeps running in the pane. |
| 1 | A plexy-glass failure (no session, precondition rejected, pane child exited mid-run, pane was reset mid-command, or any other daemon refusal). |

**Disambiguating 1 vs. 124**: exit code 1 could be the command's own exit
status or a plexy-glass failure, and the stderr output is what tells them
apart. A refusal (no session, precondition rejected, etc.) prints a message
prefixed with `run:` (e.g. `run: pane is busy: a command is running`), a
transport failure before the daemon is reached prints `plexy-glass: <err>`,
and a command that exits 1 cleanly prints nothing on stderr from
`plexy-glass run`.

**D without exit payload**: if the shell emits `OSC 133;D` with no exit-code
field, `run` prints the output, emits `run: shell integration reported no exit
code` on stderr, and exits 0.

**No default timeout.** Without `--timeout`, `run` waits indefinitely for the
completion mark. Press `Ctrl-C` to abandon the wait (the command keeps running
in the pane).

### Accepted limitations

These are constraints we've accepted, not bugs:

- **Don't type in the pane while `run` is waiting.** Anything you type goes
  to the running command, and that can make the next `D` mark land
  unexpectedly early or late.
- **Nested shells and SSH sessions.** If the pane is running a shell inside
  another shell (e.g. `ssh`, `docker exec -it`), and that inner shell also
  emits OSC 133 marks, a `D` from the inner shell satisfies the wait early.
- **Backgrounded work.** `run "make &"` returns as soon as the shell emits
  its own `D` for the job submission, not when the background job finishes.
- **A-without-C shells.** Some shell integrations emit `A` (prompt start) but
  omit `C` (output start). The busy-check can't tell "mid-command" apart for
  these shells and fails open, so it may inject a second command while one is
  running.
- **Output after `clear`.** If the command clears the screen, the captured
  output is best-effort (whatever survives in the grid); the exit code is
  always exact.

### Examples

```sh
# Gate a commit on the test suite — in your real session
plexy-glass run -n work "cargo test" && plexy-glass run -n work "jj commit -m wip"

# Capture a value
rev=$(plexy-glass run -n api "git rev-parse HEAD")

# Bound a long build
plexy-glass run --timeout 600 "cargo build --release" || echo "build broke or stalled"
```

## Popup-aware write and read

`send`, `capture`, and `run` all target the *input target pane*: the popup's
child when a popup is open, otherwise the focused pane, so a script's
write→read sequence always addresses the same pane even when a popup is
active.

`run` deliberately bypasses the sync-panes fan-out and writes only to the
input target pane, not to every pane in a synchronized group. `send` fans out
to all synchronized panes; `run` does not, because a synchronized multi-pane
run has no single answer (each pane has its own block counter and output).

## No auto-spawn

These verbs need a daemon that's already running. If none is reachable they
exit 1 immediately, and unlike `list` and `reload` they won't auto-spawn
one.
