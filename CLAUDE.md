# plexy-glass — agent conventions

Read this before touching the repo.

## Project

plexy-glass is a Rust terminal multiplexer (tmux/zellij-like) with first-class
OSC handling and Ghostty-style terminal integration. It is decomposed into five
phases; see `docs/superpowers/specs/` and `docs/superpowers/plans/` for the
authoritative design and implementation plans.

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

Phase-1 implementation runs **directly on `main`**. Feature branches are not
required for this personal greenfield project. Each task in the plan should
produce one commit on main via `jj commit -m "..."`.

## Implementation plans

Plans live at `docs/superpowers/plans/YYYY-MM-DD-<topic>.md`. They are
task-by-task with full code per step. Follow the plan; do not invent
scope. If a step is wrong, fix the plan first, then proceed.

## Code conventions

- Rust 2024 edition.
- `cargo clippy --workspace --all-targets -- -D warnings` must pass before
  any task is considered done.
- `cargo test --workspace` must pass before any task is considered done.
- No `unwrap`/`expect` in non-test code except for invariants that cannot
  fail (each documented with a one-line `// invariant:` comment).
- No `#[allow]` annotations without a one-line justification comment.

## Phase 1 scope reminders

Phase 1 is the daemon + client + one PTY-backed session foundation. It
**does not** include: ANSI/VT emulation, detach/reattach, panes/splits,
multi-client per session, a config file, or OSC interception. Those are
later phases — do not let scope creep happen.
