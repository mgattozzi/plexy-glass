# Testing

How to run plexy-glass's tests and coverage. Note that the bug-finding tools
(fuzzing, snapshots, mutation, conformance, Miri) land in later phases of the
testing-hardening initiative; see
`docs/superpowers/specs/2026-06-28-testing-hardening-initiative-design.md`.

## Running the tests

The runner is **cargo-nextest**, not `cargo test`:

    cargo nextest run --workspace     # the full suite, the completion gate
    cargo nextest run -p <crate>      # one crate, for fast iteration

nextest doesn't run doc-tests, so those go separately:
`cargo test --workspace --doc`.

## Code coverage (cargo-llvm-cov)

One-time setup:

    rustup component add llvm-tools-preview
    cargo install cargo-llvm-cov

Generate coverage over the full suite:

    cargo llvm-cov nextest --workspace --summary-only   # per-crate table
    cargo llvm-cov nextest --workspace --html           # browsable report at target/llvm-cov/html/

Coverage is **measured, not gated**. There is no minimum threshold yet
(threshold enforcement is deferred to CI).

### e2e subprocess coverage

`tests/e2e.rs` spawns real `plexy-glass` client + daemon processes (`assert_cmd`
runs the instrumented binary; the client self-execs the daemon via
`current_exe`). cargo-llvm-cov sets a per-process `LLVM_PROFILE_FILE` inherited
by those subprocesses, so each writes its own profile **on clean exit**. Note
that a process killed with SIGKILL flushes nothing.

Captured: the client/daemon integration paths exercised by e2e show up in the report.

## Fuzzing (bolero)

The byte-stream parsers are fuzzed with **bolero**. The targets are normal
`#[test]`s, so they run in the suite (`cargo nextest run --workspace`) in
bolero's DefaultEngine mode: it replays the committed corpus + crash inputs and
does bounded random generation. Found crashes are committed, so they stay
guarded forever.

Targets:
- `parser_advance` (`crates/emulator/tests/fuzz_emulator.rs`): `Emulator::advance`
- `mouse_consume` (`crates/mux/tests/fuzz_mouse.rs`): `MouseParser::consume`
- `key_consume` (`crates/keys/tests/fuzz_keys.rs`): `KeyParser::consume`

Deep, coverage-guided runs use **nightly** + `cargo-bolero`:

    rustup toolchain install nightly --component rust-src llvm-tools-preview
    cargo install cargo-bolero
    cargo +nightly bolero list
    cargo +nightly bolero test parser_advance -p plexy-glass-emulator -e libfuzzer -T 60sec

The generated `corpus/` is gitignored, and crash inputs are committed as
regression seeds.

## Snapshot testing (insta)

The compositor is snapshot-tested with **insta**. Tests compose a scenario
through `compose(...)` and assert a deterministic text dump of the resulting
`VirtualScreen` (`dump_frame` in the compositor test module, a plain grapheme
grid, or, with attributes, a second grid marking
reverse/highlight/dim/bold/underline so attribute-only renders like copy-mode
selection are captured). Goldens live under `crates/mux/src/snapshots/` and are
validated in compare mode by the normal suite:

    cargo nextest run --workspace        # fails if a composed frame drifts

Regenerating / reviewing goldens needs `cargo-insta` (a dev CLI, not required to
run the suite):

    cargo install cargo-insta
    cargo insta test -p plexy-glass-mux       # runs tests, writes *.snap.new on drift
    cargo insta review                        # interactively accept/reject pending
    cargo insta accept                        # accept all pending without review

Pending `*.snap.new` files are gitignored; accepted `.snap` goldens are
committed. When a snapshot legitimately changes, review the diff before
accepting, because an accepted wrong golden locks in a bug.

## Mutation testing (cargo-mutants)

`cargo-mutants` measures **test quality**: it changes the code one mutation at
a time and checks whether a test catches it. A *missed* mutant is a coverage
gap, behavior a test should have pinned but didn't. It is **measured on
demand, not a gate** (it is slow), and scoped to the pure-logic crates
`emulator` and `mux`.

Setup (one-time):

    cargo install --locked cargo-mutants

Run (one cargo invocation at a time, since it takes the project build):

    cargo mutants -p plexy-glass-emulator -f reflow.rs   # one crate + file
    cargo mutants -p plexy-glass-mux                      # whole crate (slow)
    cargo mutants -p plexy-glass-emulator --list          # preview mutants, don't run

It uses nextest (`.cargo/mutants.toml`), runs an unmutated **baseline first**
(the suite must be green), then mutates a **scratch copy** of the tree, so
your checkout is untouched. Results land in `mutants.out/` (gitignored):
`missed.txt`, `caught.txt`, `timeout.txt`, `unviable.txt`, and `outcomes.json`
(summary counts). There is no built-in score; compute kill-rate as
`caught / (caught + missed)`.

Triaging a surviving (missed) mutant:
- **Real gap**: add the smallest test that fails on the mutant and passes on
  the real code (a unit test, or a `hegel` property test for invariant-rich
  modules), then re-run that file to confirm the mutant is now caught.
- **Equivalent / untestable**: two options, depending on how tightly killable
  and equivalent mutants are mixed in the function:
  - **(a) Whole-item skip**: if the *entire* function or item has only
    equivalent mutants (or the function is pure glue with no distinguishable
    observable behavior), annotate it with
    `#[cfg_attr(test, mutants::skip)] // reason: …`. The `cfg_attr(test, …)`
    keeps `mutants` a dev-dependency and compiles out of release builds.
  - **(b) In-source note**: if the function mixes killable and equivalent
    mutants (the common case in the emulator), leave the equivalent survivor
    counted as missed and add an `// Equivalent note: <reason>` comment at the
    mutation site explaining *why* the surviving mutation cannot change
    observable behavior. This is more honest than suppressing the whole
    function's measurement with `mutants::skip`: the kill-rate stays accurate
    and the comment is auditable.
  Never skip a mutant just to raise the kill-rate number.

Large modules (`emulator/src/screen.rs`, `mux/src/compositor.rs`) are slow to
mutate whole, so scope them by function with `--re '<fn-name-regex>'`.

## Baseline

### Mutation baseline — emulator

Measured 2026-06-28, `cargo mutants -p plexy-glass-emulator -f <file> --test-tool nextest`.
Kill-rate = caught / (caught + missed). Every remaining missed mutant is
documented as equivalent in a source comment (`// Equivalent note:`). We added
no `mutants::skip` annotations, so the skipped column is 0 for all modules.
`screen.rs` (≈379 mutants) and full-crate sweeps are deferred on-demand.

| Module | caught | missed (all equiv) | skipped | kill-rate |
|---|---|---|---|---|
| `width.rs` | 22 | 0 | 0 | 100% |
| `cursor.rs` | 5 | 0 | 0 | 100% |
| `tabs.rs` | 13 | 1 | 0 | 93% |
| `modes.rs` | 17 | 0 | 0 | 100% |
| `keyboard.rs` | 25 | 0 | 0 | 100% |
| `parser.rs` | 45 | 3 | 0 | 94% |
| `reflow.rs` | 71 | 3 | 0 | 96% |
| `grid.rs` | 90 | 2 | 0 | 98% |
| `graphics.rs` | 172 | 7 | 0 | 96% |

Note that `reflow.rs` and `graphics.rs` each have additional timeout/unviable
mutants (caught by test-timeout) that aren't reflected in the caught or missed
columns.

Measured 2026-06-28 with `cargo llvm-cov nextest --workspace`. The workspace
total is **93.2% lines**.

| Crate | Lines % |
|---|---|
| plexy-glass-emulator | 94.7 |
| plexy-glass-mux | 96.4 |
| plexy-glass-keys | 94.2 |
| plexy-glass-config | 93.8 |
| plexy-glass-protocol | 96.8 |
| plexy-glass-status | 93.0 |
| plexy-glass-daemon | 90.5 |
| plexy-glass-client | 83.8 |
| plexy-glass (binary) | 88.3 |

### Mutation baseline — mux

Measured 2026-06-29, `cargo mutants --timeout 20 -p plexy-glass-mux --file crates/mux/src/<file>`.
Kill-rate = caught / (caught + missed). After this pass we added targeted
tests for the real gaps; the remaining missed mutants are documented as
equivalent in source (`// Equivalent note:`). Note that the counts are
as-measured before the new tests were added.

| Module | caught | missed (all equiv after triage) | kill-rate |
|---|---|---|---|
| `layout.rs` | 125 | 10 | 93% |
| `mouse.rs` | 60 | 1 | 98% |
| `selection.rs` | 92 | 23 | 80% |
| `borders.rs` | 81 | 15 | 84% |
| `copy_mode.rs` | 99 | 70 | 59% |
| `preset.rs` | 28 | 2 | 93% |
| `hint.rs` | 72 | 22 | 77% |
| `command_prompt.rs` | 52 | 6 | 90% |
| `block_mode.rs` | 55 | 29 | 65% |
| `diff.rs` | 109 | 33 | 77% |
| `compositor.rs` | 99 | 16 | 86% |
| `blocks.rs` | 189 | 32 | 86% |

Notes: `copy_mode.rs` has a large proportion of modifier-guard equivalents (no
test sends modified motion keys, so the guards are never the distinguishing
condition); `block_mode.rs` and `diff.rs` have many arithmetic-offset
equivalents (viewport geometry that is clamped or overwritten by subsequent
passes). All survivors are documented in source.

### Lowest-covered modules (later-phase targets)

1. `crates/mux/src/status.rs`: 0.0% (3 lines, a trivial stub)
2. `crates/protocol/src/errors.rs`: 0.0% (3 lines, a trivial stub)
3. `crates/daemon/src/lib.rs`: 42.4%
4. `crates/client/src/tty.rs`: 74.8%
5. `crates/config/src/types.rs`: 75.0%
6. `crates/client/src/lib.rs`: 75.9%
7. `crates/status/src/widget.rs`: 78.1%
8. `crates/status/src/engine.rs`: 80.0%

Re-run the command above to refresh these numbers.
