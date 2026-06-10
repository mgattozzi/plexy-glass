# Scripting from the CLI

The `cmd`, `send`, and `capture` verbs let scripts and agents drive a running
session over the daemon socket without attaching a terminal. They act on the
session's focused window and pane, the same target that an attached client
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
| `plexy-glass capture [-n NAME]` | none | Print the focused pane's visible screen text to stdout (per-line trailing whitespace trimmed, trailing blank lines dropped). |

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

## Popup-aware write and read

`send` and `capture` both target the *input target pane*: the popup's child
when a popup is open, otherwise the focused pane. So a script's write→read
pair (`send` then `capture`) always addresses the same pane, even when a
popup is active.

## No auto-spawn

These verbs need a daemon that's already running. If none is reachable they
exit 1 immediately, and unlike `list` and `reload` they won't auto-spawn
one.
