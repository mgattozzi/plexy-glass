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

## Baseline

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
