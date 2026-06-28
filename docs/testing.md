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
