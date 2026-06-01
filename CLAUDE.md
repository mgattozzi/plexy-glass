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
paste; sync-panes; a configurable status bar with live reload; deep OSC handling
(8 hyperlinks, 52 clipboard, 133 prompt marks, 10/11/12 colors, 0/1/2 titles);
keyboard passthrough; interactive overlays (window/pane rename, help); a
`Ctrl+a :` **command prompt** with in-place **session switching**
(`switch_session`); and a `Ctrl+a w` **visual session picker**. Each has a spec
in `docs/superpowers/specs/`.

The overlay subsystem is the substrate for modal UI: add `Overlay` +
`OverlayView` variants (mux), an `OverlayHandler` arm, `WindowManager::open_*`
and an `OverlayKeyResult`, and dispatch at the connection layer (overlays that
need the registry/session list, like the command prompt and picker, are opened
there, not in `WindowManager::handle_command`).

Established feature workflow (it has paid off — keep using it): brainstorm →
write a spec → adversarial self-review of the spec → implement one task per
`jj commit` (each green under the gates above) → adversarial review of the
implementation. Workflows (`Workflow` tool) drive the review fan-outs.

Not yet built (future work): `choose-tree` drill-down + kill/rename from the
picker, declarative session/layout templates, break/join/swap panes, paste
buffers, capture-pane / pipe-pane, and activity/bell monitoring.
