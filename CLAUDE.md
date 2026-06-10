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

`README.md` and `docs/configuration.md` are the user-facing docs. Any change to
the user-visible surface — commands, command-prompt verbs, keybinding verbs,
default bindings, the config schema, CLI subcommands, or notable behavior —
must update them **in the same change**. Treat this as a completion gate
alongside clippy and the full test suite: a feature is not done while the docs
describe the world before it.

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
OSC-7 location; a `Ctrl+a c` window anchors to the session cwd); deep OSC handling
(8 hyperlinks, 52 clipboard, 133 prompt marks, 10/11/12 colors, 0/1/2 titles);
keyboard passthrough; interactive overlays (window/pane rename, help); a
`Ctrl+a :` **command prompt** with in-place **session switching**
(`switch_session`); a `Ctrl+a w` **visual session picker**; a `Ctrl+a W`
**choose-tree** (session→window→pane drill-down with switch/kill/rename across
sessions); **pane mobility** — break (`Ctrl+a !`), a marked pane (`Ctrl+a m`),
join (`:join-pane`), and swap (`Ctrl+a {`/`}`, `:swap-pane`); **paste buffers** —
copy-mode yanks push a bounded named-buffer stack, `Ctrl+a ]` pastes the newest,
`Ctrl+a =` opens a choose-buffer overlay; **popup panes** — `Ctrl+a P` /
`:popup [cmd]` / `bind "…" "popup:lazygit"` opens a transient PTY-backed
floating pane (centered 80%×80% box) running `$SHELL -c <cmd>` at the active
pane's live OSC-7 cwd (home-base fallback); modal (all keys to the child, every
other chord swallowed), auto-closes on child exit, `Ctrl+a q` / `:close-popup`
closes, and it is transient across detach (any client's teardown closes it) —
`crates/daemon/src/popup.rs` + the `popup` field on `WindowManager`; and
per-window **activity/bell
monitoring** (`Ctrl+a M` / `:monitor-activity` / `:monitor-bell`) surfaced as
`#`/`!` flags in the status window-list; and **keyboard-protocol negotiation** —
the emulator is a correct negotiating terminal (guarded CSI-`m` dispatch so
`CSI > 4 ; 2 m` is XTMODKEYS not SGR; per-pane modifyOtherKeys level + Kitty
keyboard flags stacks; XTVERSION `CSI >q`, DECRQM `$p`, XTGETTCAP `DCS +q`
with an honest capability table; `?1004` focus and `?2031` color-scheme modes),
a canonical lossless `KeyEvent` model (press/repeat/release, associated text,
shifted/base-layout alternates, super/hyper/meta/lock modifiers aligned to the
wire `1+bitset`), a per-pane key **re-encode** stage (legacy / modifyOtherKeys
27-form / Kitty CSI-u with down-conversion), client probe→negotiate→graceful-
fallback→precise-teardown of the outer terminal, focus/color-scheme routed
end-to-end, and **colored underlines** (SGR `58`/`59`, per-cell
`underline_color`, advertised as `Setulc`); and **preset layouts** — five
presets (`even-horizontal`/`even-vertical`/`main-horizontal`/`main-vertical`/
`tiled`), `Ctrl+a Space` cycling with per-window memory, `:layout <name>` /
`layout:<name>` verbs, the active pane takes the main slot in main-*, evenness
via a balanced ratio tree (`crates/mux/src/preset.rs`), and ratio-faithful
restore (saved split ratios are re-applied on restore — fixing the old 50/50
limitation); and **CLI scripting** — `plexy-glass cmd [-n NAME] <LINE>...` runs
command-prompt lines headlessly reusing the prompt grammar verbatim
(`command_prompt::parse`), `plexy-glass send [-n NAME] [--enter] <TEXT>...` injects
bytes into the input path (popup- and sync-panes-aware), and
`plexy-glass capture [-n NAME]` reads the focused pane's visible grid as plain
text (`screen_text` in `crates/mux/src/selection.rs`); protocol v6
(`RunCommand`/`SendInput`/`CapturePane` + `CommandResult`/`PaneCapture`);
sole-or-explicit session resolution with exact error texts; interactive-only
verbs (detach/switch/help/sessions/tree/buffers) refused with
`"<verb>: requires an attached client"`; `reload`/`paste` work headless;
honest exit codes (0 all-ok, 1 any-failure, stop-at-first for multi-line cmd);
`send`/`capture` are popup-aware by design (same input-target-pane path);
no auto-spawn (distinct from list/reload). Each has a spec in `docs/superpowers/specs/`.

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

Not yet built (future work): pipe-pane; cross-window **swap**-pane
and the choose-tree filter/collapse + session rename (deferred in their specs);
silence monitoring + bell/activity alert messages; set/save/load paste buffers;
**`keymap.prefix` is decoded but never consumed** — the prefix is hard-coded
`Ctrl+a` (known gap surfaced while writing the configuration reference).
Declarative-session v1 boundaries left for later: split ratios + active
window/pane selection in the template, per-pane env maps, re-reading templates on
`Ctrl+a R` reload, and `switch_session` auto-creating a not-yet-running declared
session (see the 2026-06-01 declarative-sessions spec's non-goals). (choose-tree,
break/join/swap + marked pane, paste buffers, and activity/bell monitoring shipped
— 2026-05-31 specs/plans; the KDL config migration + declarative sessions shipped
— 2026-06-01 specs/plans; keyboard-protocol negotiation + colored underlines
shipped — 2026-06-01 specs/plans; popup panes shipped — 2026-06-09 spec/plan;
preset layouts + the user-facing config docs shipped — 2026-06-09 spec/plan;
cleanup bundle — C1–C12 bug/test/structure fixes — shipped 2026-06-09 spec/plan;
CLI scripting surface — `plexy-glass cmd / send / capture`, prompt-grammar reuse,
protocol v6, popup-aware, sole-or-explicit session resolution — shipped
2026-06-10 spec/plan.)
