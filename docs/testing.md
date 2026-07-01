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
- `fuzz_compose` (`crates/mux/tests/fuzz_compositor.rs`): `compositor::compose`
  over emulator-generated screens at arbitrary geometry/scroll

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

## VT conformance corpus

`crates/emulator/tests/vt_conformance.rs` is a curated, table-driven corpus
that feeds escape-sequence bytes and asserts the resulting grid/cursor/mode
state against **spec-correct** expected values (DEC VT510 manual, xterm
ctlseqs, esctest), focused on the areas that have historically bitten
emulators: DECSTBM scroll regions, DECOM origin mode, cursor movement at
margins, tab stops, wide-char wrap, the ED/EL/ICH/DCH/IL/DL/ECH
erase/insert/delete ops. It drives the public `Parser` + `Screen` directly
and flushes byte-exactly. Run it with the suite, or alone:

    cargo nextest run -p plexy-glass-emulator --test vt_conformance

A failing case is a real conformance bug, so fix the emulator, or (if the
case is mis-specified against the VT spec) fix the case and cite the spec.
Never weaken a case to make it pass. The corpus is expandable: add `Case`
rows to the relevant `#[test]`.

## Miri (undefined-behavior check)

We run the pure-logic crates under **Miri** on demand to detect undefined
behavior. Note that those crates contain **no hand-written `unsafe`**, so
Miri here is a *soundness sanity check* (the safe code's std/library usage is
UB-free, and the `unsafe` inside dependencies stays sound on our inputs), not
an unsafe audit. Nightly-only, on demand, and **not a gate** (the stable
`cargo nextest run --workspace` remains the gate).

One-time setup:

    rustup +nightly component add miri
    cargo +nightly miri setup

Run a pure crate under Miri (nextest auto-selects its `default-miri` profile):

    # emulator: exclude prop_/fuzz_ binaries + 5 intractable large-buffer tests
    cargo +nightly miri nextest run -p plexy-glass-emulator \
      -E 'not (binary(/^(prop_|fuzz_)/) | test(combining_mark_cap_exact_boundary) | test(combining_mark_flood_is_bounded) | test(dcs_payload_is_capped_at_dcs_cap) | test(graphics_apc_payload_survives_1mb_size) | test(osc_52_oversized_payload_dropped))'

    # keys: exclude prop_/fuzz_ binaries
    cargo +nightly miri nextest run -p plexy-glass-keys \
      -E 'not binary(/^(prop_|fuzz_)/)'

    # mux: snapshot_ tests use fork() (unsupported on Miri/macOS);
    #      hint regex NFA state-machine intractable under Miri
    MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p plexy-glass-mux \
      -E 'not (binary(/^(prop_|fuzz_)/) | test(snapshot_) | test(hint::tests::scans_) | test(hint::tests::url_))'

    # config: reads KDL files and env vars at test time
    MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p plexy-glass-config \
      -E 'not binary(/^(prop_|fuzz_)/)'

    # protocol: run only the sync serialization tests;
    #           async/tokio tests use kqueue, unsupported on Miri/macOS
    cargo +nightly miri nextest run -p plexy-glass-protocol -E 'test(messages::)'

The highest-value pass is arbitrary bytes through the parsers under Miri
(`BOLERO_RANDOM_ITERATIONS=50` caps each run to 50 deterministic inputs):

    MIRIFLAGS=-Zmiri-disable-isolation BOLERO_RANDOM_ITERATIONS=50 \
      cargo +nightly miri nextest run -p plexy-glass-emulator --test fuzz_emulator
    MIRIFLAGS=-Zmiri-disable-isolation BOLERO_RANDOM_ITERATIONS=50 \
      cargo +nightly miri nextest run -p plexy-glass-mux --test fuzz_mouse
    MIRIFLAGS=-Zmiri-disable-isolation BOLERO_RANDOM_ITERATIONS=50 \
      cargo +nightly miri nextest run -p plexy-glass-keys --test fuzz_keys

**Excluded from Miri** (unsupported operations, not bugs):

- `async`/`#[tokio::test]` tests (mio kqueue/epoll): the `plexy-glass-protocol`
  codec and handshake tests; only the 15 sync `messages::` serialization tests
  are in scope.
- `plexy-glass-daemon` + `e2e` PTY/subprocess tests: Miri can't emulate PTY
  allocation or process spawning.
- `snapshot_*` compositor tests in `plexy-glass-mux`: insta calls `fork()`
  internally to capture test output, and Miri cannot emulate `fork()` on macOS.
- `hint::tests::scans_*` and `hint::tests::url_*`: the regex NFA has too many
  Miri-tracked transitions per character; runs exceeded 2 min each.
- Large-buffer emulator tests: 5 tests feed multi-MB byte streams through the
  VTE parser, and at Miri's ~40x slowdown they are intractable. They are
  covered by the normal `cargo nextest run --workspace` gate and the fuzz scan
  above.
- `prop_*` binaries: `hegeltest-c-0.23.1` (hegel's C FFI layer) triggers a
  Stacked Borrows violation under Miri. The violation originates entirely in
  the C library's pointer aliasing, a known C-FFI / Miri limitation, **not a
  bug in our code**. The property tests remain in the normal test gate.

A Miri "Undefined Behavior" report is a real soundness bug to fix. An
"unsupported operation" is a syscall Miri cannot emulate, so exclude that test
rather than treating it as a bug.

**Baseline:** 2026-06-29. Miri reports **no UB** across the pure crates and
the parser scan (UB-clean), with `prop_*`/async/PTY/large-buffer tests
excluded as noted.

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
Kill-rate = caught / (caught + missed). Counts are as-measured **after** this
pass's tests were added: the real gaps were killed with targeted tests, and
every remaining missed mutant is documented in source with
`// Equivalent note:`, mostly genuine equivalents plus a few acknowledged
coverage gaps left for on-demand follow-up (still counted as missed, never
hidden). No `#[mutants::skip]` is used anywhere, so the skipped count is 0 for
every module.

| Module | caught | missed | kill-rate |
|---|---|---|---|
| `layout.rs` | 125 | 10 | 93% |
| `mouse.rs` | 60 | 1 | 98% |
| `selection.rs` | 92 | 23 | 80% |
| `borders.rs` | 81 | 15 | 84% |
| `copy_mode.rs` | 163 | 4 | 98% |
| `preset.rs` | 28 | 2 | 93% |
| `hint.rs` | 72 | 22 | 77% |
| `command_prompt.rs` | 52 | 6 | 90% |
| `block_mode.rs` | 80 | 4 | 95% |
| `diff.rs` | 109 | 33 | 77% |
| `compositor.rs` | 99 | 16 | 86% |
| `blocks.rs` | 189 | 32 | 86% |

Note that the `copy_mode.rs` and `block_mode.rs` numbers are post-fix:
modifier-guard mutants previously mislabeled "equivalent" were killed with
targeted tests (the mislabeling rested on a coverage argument, not a genuine
equivalence argument). `copy_mode.rs` rose from 59% to 98% and `block_mode.rs`
rose from 65% to 95%. All remaining missed mutants in both files are genuine
equivalents documented in source with `// Equivalent note:`.

`copy_mode.rs` remaining 4 equivalents: `Release`-arm deletion at `96:13` (same
value as fallthrough), `> → >=` at `100:26` (wheel delta==0 is a no-op either
way), `> → >=` at `131:21` (u16: `start >= 0` is always true, but the extra
iteration wraps to index 65535, finds `None`, and breaks, same as `> 0`),
`< → <=` at `144:34` (the extra iteration at `cols` also finds `None` and
breaks).

`block_mode.rs` remaining 4 equivalents: `!f.query.is_empty() → true` in
`active_set` (when the filter is `Some`, an empty-query commit clears the
filter to `None` via `handle_filter_prompt`, so `Some(f)` with an empty query
never reaches `active_set`); two `|| → &&` in `snap_after_filter` (empty query:
`recompute_matches` seeds matches as all prompts, so `contains(selected)` is
true and fires the return anyway; empty matches: `.find()` on empty returns
`None`, same as the early `return`); `< → <=` in the reverse-search (selected
is guaranteed absent from matches by the `contains` guard, so `<=` finds the
same element).

`diff.rs` survivors are a mix: most are arithmetic-offset equivalents (viewport
geometry clamped or overwritten), but a few are real coverage gaps (e.g. the
Kitty image-reset / virtual-placement paths that no current test exercises).
Those are labeled as gaps, not equivalents, and stay counted as missed. All
survivors are documented in source with `// Equivalent note:`.

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
