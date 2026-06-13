# plexy-glass — agent conventions

Read this before touching the repo.

## Project

plexy-glass is a Rust terminal multiplexer (tmux/zellij-like) with first-class
OSC handling and Ghostty-style terminal integration. The original five-phase
plan is complete and the project has grown well beyond it (see **Project
status** below). `docs/superpowers/specs/` and `docs/superpowers/plans/` hold
the authoritative design and implementation docs — one spec (and usually one
plan) per feature, newest by date.

## Version control: use jj, not git

This repo uses [Jujutsu (jj)](https://github.com/martinvonz/jj) colocated with
git. **Do not use `git add` / `git commit` / `git checkout`** unless asked
explicitly. Use the jj equivalents:

| Need                                         | Command                              |
|----------------------------------------------|--------------------------------------|
| Inspect working copy                         | `jj st`                              |
| See history                                  | `jj log`                             |
| Describe the current (unfinalized) change    | `jj describe -m "msg"`               |
| Finalize current change, start a new one     | `jj commit -m "msg"`                 |
| Start a fresh empty change                   | `jj new`                             |
| Diff the current change                      | `jj diff`                            |
| Diff a specific revision                     | `jj diff -r <id>`                    |

Notes:
- jj auto-tracks every file in the working copy, so there is **no `add` step**.
- When an implementation plan step says `git add X && git commit -m "..."`,
  translate to `jj commit -m "..."`. The `git add` part is unnecessary.
- The `.gitignore` is respected by jj.
- `jj git push` / `jj git fetch` interoperate with the git remote when one
  exists (this repo currently has none).

## Branching

Implementation runs **directly on `main`**. Feature branches are not required
for this personal greenfield project. Each task in a plan should produce one
commit via `jj commit -m "..."`; advance the `main` bookmark to the tip when a
feature is done (`jj bookmark set main -r @-`).

## Implementation plans

Plans live at `docs/superpowers/plans/YYYY-MM-DD-<topic>.md`. They are
task-by-task with full code per step. Follow the plan; do not invent
scope. If a step is wrong, fix the plan first, then proceed.

## Code conventions

- Rust 2024 edition.
- `cargo clippy --workspace --all-targets -- -D warnings` must pass before
  any task is considered done.
- The test runner is **cargo-nextest**, not `cargo test`. The **full workspace
  suite — `cargo nextest run --workspace` — must pass before any task or feature
  is considered complete.** Per-crate or name-filtered runs (`-p <crate>`,
  `nextest run <name>`) are fine for fast iteration, but they are **not** the
  completion gate; always finish with the full run. nextest does **not** run
  doc-tests; if you add any, also run `cargo test --workspace --doc`.
- No `unwrap`/`expect` in non-test code except for invariants that cannot
  fail (each documented with a one-line `// invariant:` comment).
- No `#[allow]` annotations without a one-line justification comment.

## User documentation

User-facing docs are **per-topic files under `docs/`**, with `README.md` as
the index (intro, quick start, keybindings, links). Current topics:

- `docs/configuration.md` — the `config.kdl` reference (palette, status bar,
  keymap + verb vocabulary, declarative sessions, the command prompt).
- `docs/scripting.md` — the `cmd` / `send` / `capture` / `run` CLI surface.

Any change to the user-visible surface — commands, command-prompt verbs,
keybinding verbs, default bindings, the config schema, CLI subcommands, or
notable behavior — must update the relevant topic doc (or add a new one when a
feature opens a genuinely new topic; link it from README) **in the same
change**. Treat this as a completion gate alongside clippy and the full test
suite: a feature is not done while the docs describe the world before it.

## Unicode and text width

Terminal layout is measured in **display columns**, never bytes or `char`s. Every
width / alignment / truncation / centering computation goes through the
`emulator::width` module — the single source of truth — re-exported from
`plexy-glass-emulator` (every crate depends on it): `display_width`,
`char_width`, `grapheme_advance`, `graphemes_with_width`, `truncate_to_width`.
**Do not** use `s.len()` or `s.chars().count()` for layout — they are correct
only for ASCII.

- A wide grapheme (CJK, most emoji) occupies two grid cells: the grapheme cell
  plus a `Cell::wide_spacer()` (empty grapheme) in the next column. The diff
  renderer skips spacers and advances the cursor by display width.
- In the mux compositor, `put_char` / `put_str` are the width-aware grid
  painters — they write the spacer and return the end display column. Paint text
  through them, and size/center boxes with `display_width`; don't hand-roll
  `chars().enumerate()` column loops.
- The emulator core (screen/parser/reflow) is already grapheme- and
  wide-char-correct; don't reimplement width there.

## Testing notes

- The emulator buffers the trailing grapheme until the next byte arrives (for
  cluster/combining handling). In tests, feed a trailing byte (or use the
  `screen_with_lines` helper) so the last grapheme lands in the grid before
  asserting on it.
- `tokio::io::duplex` gives two endpoints; bytes written to one are read from
  the **other**. Drive the client from one endpoint and read daemon output from
  it — do not split a single endpoint into read+write halves expecting loopback.
- `Session::register_client` / `deregister_client` take a `blocking_lock`; call
  them via `spawn_blocking` from async code (see `serve_attach` /
  `switch_session`).
- Don't launch multiple `cargo` / `nextest` invocations at once — they serialize
  on the target-dir lock and look like a hang. Run one at a time (the suite is
  ~1 minute).
- **Every in-process daemon test that builds a `Session`/registry must start
  with `let _g = crate::test_env::isolate();`** (`crates/daemon/src/lib.rs`) —
  it points `XDG_STATE_HOME` at a per-test tempdir (held for the test's whole
  body, across `.await`s) so the debounced persist loop and `attach_or_create`
  restores never touch the user's real state dir. The single guard replaces the
  old per-module copies and the unique-name + `delete_session` workaround.
- **`Command::NewWindow` / splits spawn `$SHELL`, not the test's `SpawnSpec`**
  (`default_spec` deliberately runs the default shell). A unit test whose child
  must produce specific OUTPUT (e.g. echo a BEL byte back) cannot rely on the
  user's interactive login shell — its startup sources real rc files (slow and
  load-sensitive) and its behavior is config-dependent. Use
  `new_window_with_spec(spec(), ...)` to get a deterministic `cat` child. (This
  was the root cause of the historical `…_from_a_real_bel` full-suite flake:
  the BEL only existed because zsh's line editor beeps on ^G.)
- Each e2e test spawns a client that auto-spawns a *daemon*; the `TestEnv` guard
  returned by `isolate_dirs` kills that daemon on drop (`plexy-glass kill` in the
  test's isolated env). Don't bypass it, or daemons orphan and hold PTYs open.
  `kill` (no `-n`) is scoped to the **current runtime dir's** daemon (via its
  pidfile) — `kill --all` is the UID-wide sweep. This scoping is what lets the
  e2e tests run concurrently without one test's teardown killing another's
  daemon; don't make the default `kill` UID-wide again.
- The e2e tests use a `TestSession` persistent-reader harness: one reader thread
  per PTY accumulates all output into a shared buffer, and every step polls it
  (`wait_for` / `wait_ready` / `snapshot`) instead of sleeping a fixed delay.
  This removed the fixed-`sleep` timing-flake class and made the e2e binary ~2x
  faster. Use it for any new e2e test; don't reintroduce `sleep`-then-`read`.
- The `e2e` nextest group runs at **`num-cpus`** (full parallelism); `isolate_dirs`
  sets `TOKIO_WORKER_THREADS=2` in each spawned process so a test's client+daemon
  use ~4 threads instead of ~ncpu each (production is unaffected). Measured clean
  over 9 consecutive full-`--workspace` runs at caps 4/8/12/18 on an 18-core host
  (~6.5s/run). Historical note: the suite used to be capped to 1 because
  `plexy-glass kill` swept *every* daemon for the user and killed sibling tests'
  daemons mid-run — that was a real `kill`-scoping bug (now fixed), not a timing
  or resource limit. If the suite ever flakes wide again, suspect a teardown that
  kills daemons it shouldn't, not the cap. See `.config/nextest.toml`.

## Dependencies — always pin to the current latest

Before adding or modifying any `[dependencies]`, `[dev-dependencies]`, or
`[workspace.dependencies]` entry, check the **current latest stable**
version on crates.io and pin to it. Do not rely on training-data versions
or on what an implementation plan said months ago — both drift.

Quick checks:

```bash
# latest stable version
cargo info <crate> | head -5
curl -s https://crates.io/api/v1/crates/<crate> \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['crate']['max_stable_version'])"

# available features for a specific version
curl -s https://crates.io/api/v1/crates/<crate>/<version> \
  | python3 -c "import json,sys; print(sorted(json.load(sys.stdin)['version']['features'].keys()))"
```

If the plan pins to an older version (or names a feature the latest no
longer has), **fix the plan first**, then the manifest. Don't paper over
plan/reality drift in the Cargo.toml only — the next task gets it wrong
the same way.

## Project status

Implemented and on `main`: the daemon + client foundation; a full VT emulator
(grid, scrollback, reflow, wide-char/grapheme correctness); windows, panes,
H/V splits, zoom, resize; detach/reattach with on-disk session
persistence/restore; multi-client; copy mode with search; full mouse; bracketed
paste; sync-panes; a configurable status bar with live reload; **KDL v2 config**
(`config.kdl` — a hard cutover from the old TOML; the decoder is
`crates/config/src/kdl_config.rs`, the in-memory `Config` model is unchanged);
**declarative default sessions** (recursive `session → window → split/pane`
layouts with per-pane shell commands, built fresh at daemon boot; config wins for
declared names over saved on-disk state — `crates/daemon/src/declared.rs` +
`Session::build_from_template`; windows accept a `cwd`
(`window "api" cwd="~/p/api" { … }`) — a window's cwd is its permanent
**home base**: every pane and split created in the window spawns there
(precedence `pane.cwd → window.cwd → session.cwd → daemon cwd`), and splits /
interactive new panes always use the home base, not the active pane's live
OSC-7 location; a `Ctrl+a c` window anchors to the session cwd; **declarative
v2** adds split ratios (`ratio=<weight>` per direct split-child, default 1 →
even `1/N`; `ratio=0` rejected; a nested split's weight is its own `ratio`, not
its leaf count; applied via `set_ratios_preorder`), an active window/pane
(`active=#true`, at-most-one-each decode-enforced; mapped to a built PaneId via
the DFS index), and per-pane `env { KEY "v" }` overlays inherited
session→window→pane (pane wins per key) — set ON TOP of the inherited daemon
env: the spawn path **overlays, never `env_clear`s**, so `PATH`/`TERM`/etc.
survive; reload re-reads the templates via `SessionRegistry::build_declared`
(builds newly-declared names, never rebuilds live ones, 24×80 default size like
boot), and `switch_session` auto-creates a declared-but-not-running target from
`config_snapshot()`); deep OSC handling
(8 hyperlinks, 52 clipboard, 133 prompt marks, 10/11/12 colors, 0/1/2 titles);
keyboard passthrough; interactive overlays (window/pane rename, help); a
`Ctrl+a :` **command prompt** with in-place **session switching**
(`switch_session`); a `Ctrl+a w` **visual session picker**; a `Ctrl+a W`
**choose-tree** (session→window→pane drill-down with switch/kill/rename across
sessions, incremental filter `/`, collapse/expand `h`/`l`, and session rename
`r` — registry re-key, `Mutex<String>` name accessor, commit-on-success
re-stamp of open tree, deferred old-file sweep); **pane mobility** — break (`Ctrl+a !`), a marked pane (`Ctrl+a m`),
join (`:join-pane`), swap (`Ctrl+a {`/`}`, `:swap-pane`; `:swap-pane` with no
argument also works cross-window: the marked pane's slot and the active pane's
slot exchange occupants via `replace_leaf`/`swap_occupant`, focus and zoom
follow the slot, mark is preserved); **paste buffers** —
copy-mode yanks push a bounded named-buffer stack, `Ctrl+a ]` pastes the newest,
`:paste bufferN` pastes by name, `Ctrl+a =` opens a choose-buffer overlay, and
`:set-buffer` / `:save-buffer` / `:load-buffer` bridge text and files
(prompt-only verbs at the connection layer; daemon-side paths, `~`-expanded,
relative refused; load gated to regular files ≤ 10 MiB); **popup panes** — `Ctrl+a P` /
`:popup [cmd]` / `bind "…" "popup:lazygit"` opens a transient PTY-backed
floating pane (centered 80%×80% box) running `$SHELL -c <cmd>` at the active
pane's live OSC-7 cwd (home-base fallback); modal (all keys to the child, every
other chord swallowed), auto-closes on child exit, `Ctrl+a q` / `:close-popup`
closes, and it is transient across detach (any client's teardown closes it) —
`crates/daemon/src/popup.rs` + the `popup` field on `WindowManager`; and
per-window **monitoring** — activity/bell
(`Ctrl+a M` / `:monitor-activity` / `:monitor-bell`, `#`/`!` flags),
**silence** (`:monitor-silence <secs>`, `~` flag, a dedicated armed-only
session-scope 1s tick task with a per-window episode latch), and
**command-completion** (`:monitor-command`, OSC-133;D blocks vs a per-pane
`blocks_completed` baseline, `✓`/`✗` flags) — each surfaced as a status
window-list flag plus an **edge-triggered status-line alert message**
(`activity in window 2 (api)` / `done in window 3 (logs): exit 1` / …; emitted
under the held WM lock with the TTL wake scheduled after release via
`Session::schedule_status_expiry_wake`); flags + toggles are runtime-only and
clear when the window is viewed; and **keyboard-protocol negotiation** —
the emulator is a correct negotiating terminal (guarded CSI-`m` dispatch so
`CSI > 4 ; 2 m` is XTMODKEYS not SGR; per-pane modifyOtherKeys level + Kitty
keyboard flags stacks; XTVERSION `CSI >q`, DECRQM `$p`, XTGETTCAP `DCS +q`
with an honest capability table; `?1004` focus and `?2031` color-scheme modes),
a canonical lossless `KeyEvent` model (press/repeat/release, associated text,
shifted/base-layout alternates, super/hyper/meta/lock modifiers aligned to the
wire `1+bitset`), a per-pane key **re-encode** stage (legacy / modifyOtherKeys
27-form / Kitty CSI-u with down-conversion), client probe→negotiate→graceful-
fallback→precise-teardown of the outer terminal, focus/color-scheme routed
end-to-end, a symmetric **decode** of the modifyOtherKeys 27-form
(`CSI 27 ; mods ; code ~` → the same `KeyEvent` the re-encode emits), a
**~30ms Esc idle-flush** in the connection input loop (a bare `\x1b` parks in
the paste→mouse→key parser chain; the flush turns it into `Key(Escape)` so Esc
cancels overlays on legacy / modifyOtherKeys clients, not only Kitty — the
read_frame future is pinned/recreated for cancel-safety, the timer gated by an
`armed` flag so it never busy-wakes when idle), and **overlay input isolation**
(while an overlay is open `InputEvent::Bytes`/`Paste` are discarded — the modal
owns input, nothing leaks to the pane's child); and **colored underlines** (SGR
`58`/`59`, per-cell `underline_color`, advertised as `Setulc`); and **preset
layouts** — five
presets (`even-horizontal`/`even-vertical`/`main-horizontal`/`main-vertical`/
`tiled`), `Ctrl+a Space` cycling with per-window memory, `:layout <name>` /
`layout:<name>` verbs, the active pane takes the main slot in main-*, evenness
via a balanced ratio tree (`crates/mux/src/preset.rs`), and ratio-faithful
restore (saved split ratios are re-applied on restore — fixing the old 50/50
limitation); and **CLI scripting** — `plexy-glass cmd [-n NAME] <LINE>...` runs
command-prompt lines headlessly reusing the prompt grammar verbatim
(`command_prompt::parse`), `plexy-glass send [-n NAME] [--enter] <TEXT>...` injects
bytes into the input path (popup- and sync-panes-aware),
`plexy-glass capture [-n NAME]` reads the focused pane's visible grid as plain
text (`screen_text` in `crates/mux/src/selection.rs`), and `plexy-glass run
[-n NAME] [--timeout SECS] <COMMAND>...` injects a command into the
input-target pane, waits for the OSC 133 `D` completion mark (fenced by
`Screen::blocks_completed`, a monotonic counter incremented per block in the
emulator's D branch), prints the block output, and exits with the command's
exit code — using `pane_at_prompt` (`crates/mux/src/blocks.rs`) to detect the
at-prompt precondition; protocol v8 (`ExecCommand`/`ExecDone` appended to
their enums, `serve_exec` in `crates/daemon/src/connection.rs`,
`client_exec` in `crates/client/src/lib.rs`); sole-or-explicit session
resolution with exact error texts; interactive-only
verbs (detach/switch/help/sessions/tree/buffers) refused with
`"<verb>: requires an attached client"`; `reload`/`paste` work headless;
honest exit codes (0 all-ok, 1 any-failure, stop-at-first for multi-line cmd,
exit-code passthrough for `run`, 124 for `run` timeout);
`send`/`capture`/`run` are popup-aware by design (same input-target-pane path); `send` fans out to all sync-panes-synchronized panes; `run` deliberately bypasses sync-panes (writes only to the input target pane — a synchronized multi-pane run has no single answer);
no auto-spawn (distinct from list/reload); and a **configurable prefix** —
`keymap.prefix` is consumed for real: binding strings accept a `prefix` chord
token (case-insensitive, any position; `parse_chord_seq_with_prefix` in
`crates/keys/src/spec.rs`), `build_keymap` resolves it once per build with a
warn-and-fall-back-to-`Ctrl+a` policy for invalid/empty/multi-chord values,
every built-in default is declared prefix-relative (`binding("prefix c", …)`
in `crates/config/src/default.rs`), and the help overlay substitutes the
configured prefix string back into the keys column; and **command-block
awareness** — OSC 133 marks live as per-row annotations (`Row::mark`,
`crates/emulator/src/grid.rs`) that ride rows through scrollback, eviction,
and reflow; block helpers in `crates/mux/src/blocks.rs`; copy-mode `[`/`]`
(jump prompt) and `o` (select output) keys; viewport `prev_prompt`/`next_prompt`
verbs with default chords `prefix <`/`prefix >`; `:copy-output` / `copy_output`
binding verb yanks the last completed block's output; `plexy-glass capture
--last-command` (protocol v7, `CaptureLastCommand` message) prints the
scrollback-inclusive block output from a script; **block exit-status border** —
each pane's left border is color-coded per visible row by the block's exit
status: ok-color `│` for exit 0, fail-color `▌` for nonzero, plain for
unmarked rows / running blocks; the whole block (prompt row through the row
before the next prompt) takes the status; coloring is viewport-tracked (live,
wheel scrollback, copy mode); alt-screen panes revert to plain while active;
marked-ring beats block status, block status beats the active ring on status
rows; colors and `enabled` flag from the `blocks` config node (live-reloads)
via `viewport_block_status` in `crates/mux/src/blocks.rs` and the border
painter in `crates/mux/src/borders.rs`. Each has a spec in
`docs/superpowers/specs/`.

The overlay subsystem is the substrate for modal UI: add `Overlay` +
`OverlayView` variants (mux), an `OverlayHandler` arm, `WindowManager::open_*`
and an `OverlayKeyResult`, and dispatch at the connection layer (overlays that
need the registry/session list, like the command prompt and picker, are opened
there, not in `WindowManager::handle_command`). Overlays whose actions are
cross-session or need the registry (choose-tree, choose-buffer) carry their own
pure handler (`tree.rs`/`buffer.rs`) returning a crate-local outcome enum that
the daemon adapts to `OverlayKeyResult`, instead of routing through
`OverlayHandler::handle`.

Established feature workflow (it has paid off — keep using it): brainstorm →
write a spec → adversarial self-review of the spec → implement one task per
`jj commit` (each green under the gates above) → adversarial review of the
implementation; user-facing docs (README / the configuration reference) are
updated as part of each feature, per **User documentation**. Workflows
(`Workflow` tool) drive the review fan-outs.

Not yet built (future work): none currently tracked.
(Silence monitoring + bell/activity alert messages shipped with the 2026-06-12
alerts feature; "push notifications on run completion" is cleared by
monitor-command + the `run` CLI's synchronous exit code — a detached `run`
completes in its session's active window with nobody to see a flag, and the
exit code IS the notification for detached scripting, while monitor-command
serves the attached-but-looking-elsewhere case.)
(choose-tree,
break/join/swap + marked pane, paste buffers, and activity/bell monitoring shipped
— 2026-05-31 specs/plans; the KDL config migration + declarative sessions shipped
— 2026-06-01 specs/plans; keyboard-protocol negotiation + colored underlines
shipped — 2026-06-01 specs/plans; popup panes shipped — 2026-06-09 spec/plan;
preset layouts + the user-facing config docs shipped — 2026-06-09 spec/plan;
cleanup bundle — C1–C12 bug/test/structure fixes — shipped 2026-06-09 spec/plan;
CLI scripting surface — `plexy-glass cmd / send / capture`, prompt-grammar reuse,
protocol v6, popup-aware, sole-or-explicit session resolution — shipped
2026-06-10 spec/plan; configurable prefix — the `prefix` chord token,
prefix-relative defaults, resolved-chord help — shipped 2026-06-10 spec/plan;
command-block awareness — OSC 133 row marks, copy-mode block navigation,
viewport prompt verbs, copy-output, capture --last-command, protocol v7 —
shipped 2026-06-11 spec/plan; block exit-status border — left-border coloring
per block exit status, viewport-tracked, blocks config node — shipped
2026-06-12 spec/plan; `run` verb — synchronous command execution,
blocks_completed counter, pane_at_prompt, protocol v8, ExecCommand/ExecDone,
serve_exec/client_exec, exit-code passthrough, timeout 124 — shipped
2026-06-12 spec/plan; blocks completion bundle — PROMPT_END row marks + side-list
deletion, `block_command_line` / `closing_exit` helpers, `--json` for
`capture --last-command` and `run` (protocol v9, `{"output","exit_code",
"command_line"}`), scrolled prompt-click-to-jump (while scrolled back, plain
left-press on a prompt row scrolls that command to viewport top), popup border
exit-status coloring (left border of popup boxes takes the same per-block
status as regular panes) — shipped 2026-06-12 spec/plan; cross-window
swap-with-marked — `:swap-pane` with no argument works when the marked pane is
in another window of the same session, via `replace_leaf`/`swap_occupant`,
focus/zoom follow the slot, mark preserved — shipped 2026-06-12 spec/plan;
choose-tree v2 — incremental filter `/`, collapse/expand `h`/`l`, session
rename `r` via registry re-key + `Mutex<String>` name accessor +
commit-on-success re-stamp + deferred old-file sweep — shipped 2026-06-12
spec/plan; paste buffers v2 — `set-buffer`/`save-buffer`/`load-buffer` +
paste-by-name, shape-based save split, refuse-relative path policy, load
gates (regular file, 10 MiB), preview 4 KiB scan cap — shipped 2026-06-12
spec; pipe-pane — session-level `:pipe-pane [cmd…]` verb
(`PromptCommand::PipePane(Option<String>)`) that tees the input-target pane's
raw output to `$SHELL -c <cmd>`; the pipe rides the existing pane output
broadcast (`Pane::subscribe_output`), one drain task per pipe in `crate::pipe`;
one pipe per pane (start replaces), too-slow consumers close (broadcast
`Lagged` → kill+reap), every close path funnels through one kill→reap→clear-slot
exit; cwd via the shared `WindowManager::pane_cwd(target)`; runtime-only (not
persisted), popup pipes die on detach — shipped 2026-06-12 spec/plan; alerts —
edge-triggered activity/bell/silence/command-completion alert messages +
`~`/`✓`/`✗` window-list flags, `:monitor-command` / `:monitor-silence <secs>`
(parse arity 0|1, pinned error text), per-window `blocks_completed` baselines
(advance unconditionally, RIS decrease re-baselines silently), a dedicated
armed-only session-scope 1s silence tick with a per-window episode latch,
deadlock-aware message emission under the held WM lock +
`Session::schedule_status_expiry_wake` — shipped 2026-06-12 spec/plan;
declarative sessions v2 — split ratios (`ratio=` weights → preorder
`set_ratios_preorder`, even-by-default, `ratio=0` rejected, nested-split
weight = its own ratio), active window/pane (`active=#true`, at-most-one-each,
DFS-index → built PaneId), per-pane `env` overlays (session∪window∪pane, pane
wins) set ON TOP of the inherited daemon env (the spawn path overlays, the
`env_clear` was removed — `PATH`/`TERM` survive), `SessionRegistry::build_declared`
reused by boot + reload (newly-declared names built, live never rebuilt, 24×80),
and `switch_session` auto-create from `config_snapshot()` — shipped 2026-06-12
spec/plan; scrollback + mark persistence — per-pane scrollback (text +
attrs + OSC 133 marks) persisted to the session file and restored as the pane's
scrollback on daemon restart (the fresh shell draws below it). Persist DTOs in
`crates/daemon/src/persist.rs` (`PaneScrollbackV1`/`RowV1`/`CellV1` with per-cell
default-field elision + compact `serde_json::to_vec`, `ColorV1`/`UnderlineStyleV1`/
`WrapV1`/`RowMarkV1`) with explicit live↔DTO mappers (emulator types stay
serde-free; `hyperlink_id` dropped, links not persisted); capture via
`capture_scrollback(screen, N=1000)` over `scrollback ++ main_grid.rows`
(`main_grid = screen.alt.unwrap_or(&screen.active)` — MAIN grid even on alt),
trailing-default-cell trim + blank-trailing-row drop; `Screen::preseed_scrollback`
threaded THROUGH `Pane::spawn` (applied before the reader thread starts so no
child byte races the seed) → `Window::spawn_first`/`split`/`split_at` →
`WindowManager::new_with_preseed`/`new_window_with_spec_preseed`/
`split_window_at_dfs_preseed`; `restore_from` forwards each saved pane's
scrollback into the spawn path (first pane via `Session::new_with_preseed`);
block counters left at 0/None (NOT recomputed — block nav reads `Row.mark`
directly, recompute would misfire the monitor-command alert); width-mismatch
seeds rows as-is (first resize normalizes); save moved onto `spawn_blocking`
guarded by a `persist_in_flight` async mutex (`stop_persist` acquires it before
aborting the loop so an in-flight save completes before `kill`'s
`delete_session`) — shipped 2026-06-13 spec/plan; keyboard follow-ups —
modifyOtherKeys 27-form decode (symmetric with the re-encode emitter), the
~30ms Esc idle-flush in the connection loop (a bare `\x1b` parks in the
paste→mouse→key parser chain — `InputRouter::has_pending`/`flush_keys` drain it,
`MouseParser`/`PasteParser` gained mid-sequence + flush helpers — and becomes
`Key(Escape)` so Esc cancels overlays on legacy/MOK clients; read_frame pinned +
recreated for cancel-safety, timer gated by `armed`), and overlay input
isolation (`InputEvent::Bytes`/`Paste` discarded while an overlay is open; the
per-event dispatch extracted to `dispatch_input_event`) — shipped 2026-06-13
spec/plan.)
