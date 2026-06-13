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
| `plexy-glass capture --last-command [-n NAME]` | `--last-command`, `--json` | Print the last completed OSC 133 command block's output (scrollback-inclusive) to stdout. Add `--json` for `{"output", "exit_code", "command_line"}`. Exits 1 when no block exists. |
| `plexy-glass run [-n NAME] [--timeout SECS] <COMMAND>...` | `--timeout SECS`, `--json` | Type COMMAND + Enter into the input-target pane, wait for the OSC 133 completion mark, print the block output to stdout (or JSON with `--json`), and exit with the command's exit code. Requires OSC 133 shell integration. |

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

## Paste buffers from scripts

The buffer verbs make `cmd` a text/file bridge into the daemon's
paste-buffer stack (the same stack copy-mode yanks push onto):

```sh
plexy-glass cmd "load-buffer ~/snippets/deploy.sh"   # file → newest buffer
plexy-glass cmd "set-buffer some literal   text"     # text → newest buffer (verbatim)
plexy-glass cmd "save-buffer /tmp/yanked.txt"        # newest buffer → file
plexy-glass cmd "save-buffer buffer3 /tmp/old.txt"   # named buffer → file
plexy-glass cmd "paste buffer2"                      # paste a named buffer
```

Success messages print to stdout and name what happened:
`buffer set (N bytes)`, `saved bufferN → /abs/path (N bytes)`,
`loaded /abs/path (N bytes)`.

### Path policy

`save-buffer` and `load-buffer` paths are resolved **daemon-side**. A leading
`~` / `~/…` expands against the daemon's `$HOME` (there is no `~user` form).
After expansion, **relative paths are refused** with
`<verb>: relative paths are not supported — the daemon's working directory is
not yours; use an absolute or ~ path`. We refuse them because the daemon's
working directory is whichever directory the first auto-spawning client
happened to be in, and you can't discover it from any plexy-glass surface,
so relative resolution would be a silent footgun.

### Limits

- `set-buffer` text is one prompt line: leading/trailing whitespace is
  trimmed (internal whitespace is preserved verbatim) and a line cannot carry
  newlines, so use `load-buffer` for multi-line content. No clipboard
  interaction (OSC 52 remains a copy-mode-yank behavior).
- `load-buffer` accepts regular files only (FIFOs, devices, and directories
  are refused with `load-buffer: <path>: not a regular file`; symlinks to
  regular files are followed) and caps the size at **10 MiB**
  (`load-buffer: <path> is N bytes (limit 10 MiB)`), since buffers are
  memory-resident and cloned per paste. Empty files load as an empty buffer.
- `save-buffer` writes the buffer's bytes verbatim as a truncating overwrite
  (non-atomic, and deliberately so: these are user export files, not state
  files). A first token shaped like `bufferN` names the source buffer;
  otherwise the whole tail is the path and the **newest** buffer is written.
  Note that a path whose first word is literally `bufferN ` is pathological
  and not supported.

## pipe-pane — stream a pane's output to a command

`pipe-pane` tees the target pane's raw output stream into an external
command, same idea as tmux's `pipe-pane`. It is a command-prompt verb, so it
works interactively (`Ctrl+a :`) and headlessly through `cmd`:

```sh
plexy-glass cmd "pipe-pane tee -a session.log"   # start: append the pane's output to a file
plexy-glass cmd "pipe-pane"                       # stop the pipe
```

Synopsis:

| Form | Effect |
|---|---|
| `:pipe-pane <cmd>` (or `cmd "pipe-pane <cmd>"`) | Start (or replace) the pipe |
| `:pipe-pane` (or `cmd "pipe-pane"`) | Stop the running pipe |

### Semantics

- **What flows**: the pane's *raw output bytes*, exactly what the emulator
  receives from the PTY (escape sequences and control bytes included), from
  the moment the pipe starts onward. Note that there is **no scrollback
  backfill**: output produced before the pipe started is not replayed.
- **The consumer**: the command runs as `$SHELL -c <cmd>` with the pane's
  output piped to its stdin; stdout and stderr go to `/dev/null` (the consumer
  is a sink, not a pane). It spawns at the **target pane's** live OSC-7 cwd
  (home-base fallback), the same `$SHELL` new windows and popups use.
- **One pipe per pane**: starting a new pipe **replaces** any running one (the
  old consumer is killed), and `pipe-pane` with no command stops it.
- **Targets the input target pane**: the popup's child when a popup is open,
  otherwise the focused pane, the same pane `send` and `capture` address.
- **Too-slow consumers are closed**: if the consumer can't keep up and the
  pipe falls a full channel behind, data has been irrecoverably lost, so we
  **close** the pipe (and kill the consumer) rather than write a
  silently-gapped stream. Honest failure over a corrupt log. The pane itself
  is never stalled by a slow consumer.
- **Popup caveat**: a pipe attached to the **popup** pane dies when any client
  detaches. Popups are transient by design, so the pipe goes down with the
  pane. A pipe on an ordinary window pane survives detach/reattach.
- **Not persisted**: pipes are runtime-only state. They do **not** survive a
  daemon restart or session restore.

Status messages: `pipe-pane → <cmd>` (started), `pipe-pane stopped` (stopped),
`pipe-pane: no pipe` (stop with nothing running), and, surfaced asynchronously
when the consumer goes away, `pipe-pane: consumer exited` and
`pipe-pane: consumer too slow — pipe closed`.

```sh
# Append the active pane's output to a rolling log
plexy-glass cmd "pipe-pane tee -a session.log"
# … work in the session …
plexy-glass cmd "pipe-pane"          # stop logging
```

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

### `--json` — structured capture output

Add `--json` to get a compact JSON object instead of plain text:

```sh
plexy-glass capture --last-command --json [-n NAME]
```

Output (one object + newline; `serde_json` sorts keys alphabetically):

```json
{"command_line":"cargo test 2>&1","exit_code":0,"output":"running 42 tests\n..."}
```

| Key | Type | Notes |
|---|---|---|
| `output` | string | Block output text (same as plain `capture --last-command`) |
| `exit_code` | number or null | `133;D` exit code; null when D carried no code |
| `command_line` | string or null | Typed command, from the `133;B` mark; null when the shell omitted B/C |

Popup-aware: targets the popup's child pane when a popup is open (same
input-target path as plain capture).

```sh
# Assert the last command succeeded
plexy-glass capture --last-command --json -n work \
  | jq -e '.exit_code == 0'
```

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

### `--json` — structured run output

Pass `--json` and you get a compact JSON object on stdout instead of the
plain command output:

```sh
plexy-glass run --json [-n NAME] [--timeout SECS] <COMMAND>...
```

Output (one object + newline; `serde_json` sorts keys alphabetically):

```json
{"command_line":"cargo test","exit_code":0,"output":"running 42 tests\n...","timed_out":false}
```

| Key | Type | Notes |
|---|---|---|
| `output` | string | Block output text (same as a non-JSON run) |
| `exit_code` | number or null | The `133;D` exit code; null when the D carried no payload |
| `timed_out` | bool | `true` when `--timeout` expired (exit code 124) |
| `command_line` | string | The command text passed to `run` (client-side, so always present) |

The JSON carries data, not diagnostics, so `--json` doesn't change exit-code
semantics or stderr notes. Refusals (`CommandResult` errors) stay plain
stderr + exit 1.

**`duration` deliberately omitted**: `ExecDone` doesn't carry timing, so if
you need it, wall-clock the invocation yourself.

```sh
# Gate a commit on a passing test suite, check via JSON
result=$(plexy-glass run --json -n work "cargo test")
echo "$result" | jq -e '.exit_code == 0 and .timed_out == false'
```

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

`send`, `capture` (including `--last-command` and `--last-command --json`),
and `run` all target the *input target pane*: the popup's child when a popup
is open, otherwise the focused pane. So a script's write→read sequence always
addresses the same pane, even when a popup is active.

`run` deliberately bypasses the sync-panes fan-out and writes only to the
input target pane, not to every pane in a synchronized group. `send` fans out
to all synchronized panes; `run` does not, because a synchronized multi-pane
run has no single answer (each pane has its own block counter and output).

## No auto-spawn

These verbs need a daemon that's already running. If none is reachable they
exit 1 immediately, and unlike `list` and `reload` they won't auto-spawn
one.
