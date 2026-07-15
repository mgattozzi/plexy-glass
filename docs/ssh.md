# Remote attach over SSH

`-H`/`--host <ssh-target>` runs any connection verb against a daemon on a
remote host over SSH, instead of the local socket:

```sh
plexy-glass -H wsl2 attach
plexy-glass -H prod list
plexy-glass -H work.example.com cmd -n build "split v"
```

`<ssh-target>` is passed straight to `ssh`, so everything in `~/.ssh/config`
works as normal: host aliases, `User@host`, `ProxyJump`, identity files,
`ControlMaster`/agent connection reuse. plexy-glass doesn't reimplement any of
that; it shells out to your own `ssh`.

## Which verbs take `-H`

Every verb that talks to a daemon: `attach`, `list`, `kill`, `reload`, `cmd`,
`send`, `capture`, and `run`. It's a **global** flag, so it can go before or
after the subcommand. `shell-integration`, `daemon`, and `bridge` don't take
it — they aren't client→daemon connections.

`kill` is a little different from the rest. `kill -n <session>` is a normal
daemon request and rides the bridge like everything else. But `kill` with no
`-n` stops the daemon *process* by signalling it, which the client can only do
locally — so for `-H` it runs `<remote-bin> kill` **on the remote host** over
SSH (same binary search as the bridge, see `--remote-bin`) and prints the
remote's outcome. `plexy-glass -H host kill` therefore stops the *remote*
daemon, never your local one.

The local terminal negotiation (Kitty keyboard protocol, graphics, focus,
color scheme) still runs against **your** terminal, not the remote's — the
result travels to the remote daemon inside `ClientHello`. A remote daemon
needs no awareness that it's remote; it just receives frames over a socket
instead of a local one.

## The `ssh` status marker

When you attach with `-H`, the status bar's left cluster leads with a small `ssh`
badge so you never forget the session lives on another host. The marker is
session-scoped: it shows whenever any client attached to the session is remote,
so a purely local session shows nothing, while a local view co-attached to a
session that also has a remote client sees it too. Style it via the `ssh` status
widget (see [docs/configuration.md](configuration.md)).

Note that the marker rides protocol v11: after you upgrade past it, re-run
`--install` once per remote host so the remote daemon matches (a v11 client
against a v10 daemon reports the skew until you do).

That advice has one important limit. `--install` fetches the rolling `nightly`
release, so it can only move a remote to **whatever last passed CI**. If you are
running a locally-built client that is ahead of the nightly — or CI is red, which
freezes the nightly where it stands — then re-running `--install` provisions the
same too-old binary and reports success, and the mismatch stays exactly where it
was. plexy-glass says which case you are in rather than repeating the advice:

```
the remote daemon speaks protocol v12, this client speaks v13. --install already
ran, so the nightly release is BEHIND this client and cannot fix it: either point
--remote-bin at a matching binary, or use a client built from the same nightly
```

There is no `--install-version` or local-binary push yet (the latter needs
cross-compiling to the remote's triple), so for a dev client ahead of the
nightly, `--remote-bin` pointing at a binary you built for that host is the way.

`--install` stops the remote daemon whenever it actually pushes a new binary,
because writing the binary is not an upgrade on its own: the daemon already
listening on the remote socket goes on running the *old* one, and `bridge`
connects to it rather than starting the new one. So the restart is what makes
the upgrade take effect, and it means **a successful `--install` ends the
sessions on that host** — the daemon is memory-only, so there is no way to run
new bytes without restarting it. An `--install` that finds the remote already
current changes nothing and leaves your sessions alone.

One caveat if you used `--install` before this behavior existed: it may have
left a host with the new binary on disk and the old daemon still running. A
matching checksum makes `--install` a no-op, so it can't heal that on its own.
Run `plexy-glass -H <host> kill` once and reattach.

## How it works: the `bridge`

`-H` spawns `ssh -T <target> <remote-bin> bridge` and treats the child's
stdin/stdout as the daemon connection (`-T` disables remote PTY allocation,
so the byte stream stays 8-bit clean — a PTY would mangle the binary framing).
`bridge` is a small subcommand that runs **on the remote host**: it resolves
the remote's own daemon socket, connects (or spawns, unless `--no-spawn`), and
relays bytes both ways between its stdio and that socket. It never parses a
frame — it's a protocol-opaque pipe. You don't run it yourself; plexy-glass
invokes it for you as the SSH command.

## `--remote-bin`

Without `--remote-bin`, plexy-glass searches the remote for a working binary, in
this order:

1. `--remote-bin <path>` — explicit, always wins. Invoked directly, no search.
2. bare `plexy-glass`, on the remote's **non-interactive** PATH.
3. `~/.cargo/bin/plexy-glass`
4. `~/.local/bin/plexy-glass`
5. `~/.cache/plexy-glass/bin/plexy-glass` — where `--install` provisions it.

Steps 3 and 4 exist because of a genuinely confusing failure: `ssh host cmd`
runs your login shell **non-interactively**, so none of the rc files that build
your interactive PATH are read. A `plexy-glass` you installed with `cargo
install`, that you can run, that `which` finds — is simply not on the PATH we
get. If your remote login shell is **nushell** it's worse still: nushell never
reads the POSIX profile chain in any mode, so the `~/.cargo/env` line rustup
writes into `~/.profile` is dead code there.

One command tells you what we actually see:

```
ssh <host> "sh -c 'command -v plexy-glass; echo \$PATH'"
```

Each candidate is checked by **running** it (`plexy-glass --version`), not by
testing that the file is there. A wrong-architecture binary exists and is
executable, so a file test says yes and only running it says no. Steps 2–5 run
as a single `sh -c` loop on the remote, so it's correct whatever the remote
**login** shell is — `ssh host cmd` re-parses the command through that shell,
which may be nushell or fish, not POSIX `sh`.

`--remote-bin` follows you through the session picker, but only back to the
**same** host: it names a path on one machine, so reconnecting to `wsl2` keeps
it, and jumping to `prod` or to the local daemon drops it rather than pointing
them at a path that means nothing there. For a path that should apply to a host
you reach *from* the picker, there is nowhere to put one yet — that wants a
per-host config entry, which doesn't exist today.

If nothing works, the script says so itself and plexy-glass reports it directly
rather than printing a bare connection error:

```
no working remote `plexy-glass` on the host: tried PATH, ~/.cargo/bin, ~/.local/bin and ~/.cache/plexy-glass/bin. Note ssh runs your login shell NON-interactively, so a PATH set in an interactive rc (or in ~/.profile, which nushell never reads) is not visible here — pass --remote-bin <path>, or run with --install
```

## `--install` (provisioning a remote binary)

```sh
plexy-glass -H wsl2 --install attach
```

`--install` provisions a compatible `plexy-glass` on the remote before
`open_transport` spawns the `bridge`, so a remote with nothing installed can
be reached with one command. It's **local-download-then-push**: the binary is
fetched on your machine and streamed to the remote over the existing SSH
connection, rather than asking the remote to reach out to GitHub itself — this
works even when the remote host has no outbound internet access (a common
case for boxes behind a jump host or firewall).

The flow, each SSH round trip kept to a minimum:

1. One `ssh` call runs `uname -sm` on the remote and, if a binary is already
   cached, hashes it (`sha256sum`, falling back to `shasum -a 256` on macOS
   remotes) — both in a single command.
2. The `uname` output maps to a Rust target triple. Supported today:
   `x86_64`/`aarch64` Linux (static musl) and `x86_64`/`arm64` macOS
   (`apple-darwin`). Anything else is a clear error telling you to use
   `--remote-bin` instead.
3. `curl` fetches `SHA256SUMS` from the `nightly` GitHub release, **on your
   local machine**, and picks out the line for the matching triple's asset.
4. If the remote's cached binary's checksum already matches, `--install` is a
   no-op — safe to pass on every invocation.
5. Otherwise `curl` downloads the matching `plexy-glass-<triple>` asset
   locally and re-hashes it. If the downloaded bytes don't match
   `SHA256SUMS`, `--install` aborts with an error and never touches the
   remote — a corrupt or tampered download is never pushed.
6. The verified bytes are streamed over `ssh` to
   `~/.cache/plexy-glass/bin/plexy-glass` on the remote (`mkdir -p` +
   `chmod +x`). Later connections fall back to that cache path when
   `plexy-glass` isn't on the remote PATH (see `--remote-bin` above), so a
   plain `-H <host> <verb>` finds it with no repeated `--install`.

Requirements: `curl` and a SHA-256 hasher (`sha256sum` or `shasum`) on your
**local** machine; `sh`, `uname`, and one of those same hashers on the
**remote**. No new dependency on plexy-glass's side — it shells out to tools
that are already on macOS and Linux dev hosts. `--install` benefits from the
same SSH conveniences as everything else in this doc (keys, agent forwarding,
`ControlMaster` connection reuse), since steps 1 and 6 are just more `ssh`
invocations against `<ssh-target>`.

Only the `nightly` release channel is supported for now; there's no
`--install-version` or stable-channel pin yet.

## Connecting from the picker

You don't need the CLI to reach a new host at all. From an attached client,
`Ctrl+a w` opens the [session picker](configuration.md#session-picker-ctrla-w),
and its last row is always `＋ Connect to a host…`: `Enter` on it opens a
one-line prompt, type any ssh target (same syntax as `-H`) and `Enter`
connects. Once the attach lands the host joins your ad-hoc roster, exactly
like a `-H` attach does, so it's its own anchor the next time you open the
picker; a host that fails to connect isn't remembered.

The picker's `i` key is the inline alternative to `--install`: in the picker's
Navigate mode `i` toggles connect-with-install for the *next* host you connect
to, shown in the footer as `i install: on`/`off`. The toggle is unconditional —
it fires regardless of which row the cursor is on or whether a filter is
applied. It runs the same provision-or-update-then-attach flow described above,
so a host with nothing installed, or one that's behind on protocol version, can
be reached without dropping to a shell for a separate `--install` run first.

## Auth prompts

SSH may need to prompt for a password, a key passphrase, or host-key
confirmation. Those prompts happen in normal cooked terminal mode, before
plexy-glass takes the terminal raw for the interactive session — you'll see
them exactly as you would running `ssh` by hand. The common case (key or
agent auth) has no prompt at all and attaching is seamless. Once the
connection is established, plexy-glass completes its own handshake over the
tunnel and only then switches the terminal to raw mode for the session.

## Detach model

A dropped SSH connection (network blip, closing your laptop, killing the
local `ssh` process) is indistinguishable from a normal detach: the pump sees
EOF, the local terminal is restored, and the client exits cleanly. The remote
daemon is unaffected — it's memory-only and outlives any particular
connection, so the session is still there the next time you `-H <target>
attach`. There's no reconnect-in-place (no mosh-style roaming); you just run
the attach command again.

## Scripting verbs

`cmd`, `send`, `capture`, and `run` all accept `-H` the same way `attach`
does — see [docs/scripting.md](scripting.md) for the verbs themselves.
