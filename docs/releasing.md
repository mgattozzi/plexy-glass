# Releasing

plexy-glass ships **nightly** binaries built from the tip of `main` by
`.github/workflows/ci.yml`. There are no tagged stable releases yet.

## Targets

Four static/self-contained binaries, named by their exact Rust target triple:

| Triple | Host | Notes |
|---|---|---|
| `x86_64-unknown-linux-musl` | Linux x86_64 | static musl, runs on any Linux |
| `aarch64-unknown-linux-musl` | Linux ARM64 | static musl, runs on any Linux |
| `x86_64-apple-darwin` | Intel macOS | |
| `aarch64-apple-darwin` | Apple Silicon macOS | |

No Windows (plexy-glass is a unix-socket/PTY tool).

## Nightly pipeline

On every push to `main`, once the `test` job group is green, the `build` group
cross-compiles all four targets and the `release` group replaces the assets on a
single rolling **`nightly`** GitHub pre-release (the tag is moved to the pushed
commit). Each run publishes the four `plexy-glass-<triple>` binaries plus a
`SHA256SUMS`. The download URLs are stable per platform and always resolve to
tip-of-`main`:

    https://github.com/mgattozzi/plexy-glass/releases/download/nightly/plexy-glass-<triple>

## Building a Linux binary locally

The Linux targets are built with [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild)
(zig as the cross-linker → static musl, glibc-version-proof, both arches from any
host):

    brew install zig                       # or your platform's zig
    cargo install cargo-zigbuild
    rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
    cargo zigbuild --release --target x86_64-unknown-linux-musl --bin plexy-glass

The result at `target/x86_64-unknown-linux-musl/release/plexy-glass` is
statically linked and runs on any Linux (verified under Alpine).

## macOS Gatekeeper

The macOS binaries are unsigned. A `curl` download (how `plexy-glass -H host
--install` fetches them) is NOT quarantined and runs directly. A *browser*
download is quarantined — clear it with `xattr -d com.apple.quarantine
./plexy-glass` or right-click → Open once.
