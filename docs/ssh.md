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
SSH (same PATH-then-cache binary resolution as the bridge) and prints the
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
against a v10 daemon prints `handshake: peer speaks protocol version 10, we
speak 11` until you do).

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

Without `--remote-bin`, plexy-glass tries `plexy-glass` on the remote's PATH
first and falls back to the `--install` cache path, so both a system install
and an `--install`-provisioned binary work with no extra flag:

1. `--remote-bin <path>` — explicit, always wins. Invoked directly, no fallback.
2. bare `plexy-glass` — used if it's on the remote's **non-interactive** PATH.
   `ssh host cmd` runs a non-login shell, so `~/.cargo/bin` and similar
   user-local install locations often aren't there even if they'd be on PATH
   in an interactive session; put `plexy-glass` on the remote's system PATH
   (e.g. `/usr/local/bin`) for this to hit.
3. the `--install` cache path (`~/.cache/plexy-glass/bin/plexy-glass`) — where
   `--install` provisions the binary. This is the fallback when it isn't on
   PATH, so `--install` once and then plain `-H <host> <verb>` both find it.

The fallback (steps 2 and 3) runs as a single `sh -c` conditional on the
remote, so it's correct whatever the remote **login** shell is — `ssh host cmd`
re-parses the command through the login shell, which may be nushell or fish,
not POSIX `sh`.

If neither PATH nor the cache path has a binary, SSH exits 127 and plexy-glass
reports it directly instead of hanging or printing a bare connection error:

```
remote `plexy-glass` not found on the host (neither on PATH nor at ~/.cache/plexy-glass/bin); either run with --install, or install it on the remote and add it to your PATH (or pass --remote-bin <path>)
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

The picker's `i` key is the inline alternative to `--install`: with an empty
filter and the cursor on a host row (an existing anchor or the `＋` row), `i`
toggles connect-with-install for the *next* host you connect to, shown in the
footer as `i install: on`/`off`. It runs the same provision-or-update-then-
attach flow described above, so a host with nothing installed, or one that's
behind on protocol version, can be reached without dropping to a shell for a
separate `--install` run first.

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
