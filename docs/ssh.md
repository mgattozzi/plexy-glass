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

The local terminal negotiation (Kitty keyboard protocol, graphics, focus,
color scheme) still runs against **your** terminal, not the remote's — the
result travels to the remote daemon inside `ClientHello`. A remote daemon
needs no awareness that it's remote; it just receives frames over a socket
instead of a local one.

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

The command plexy-glass runs on the remote resolves in order:

1. `--remote-bin <path>` — explicit, always wins.
2. the `--install` cache path (`~/.cache/plexy-glass/bin/plexy-glass`), if
   `--install` has provisioned one there.
3. bare `plexy-glass` — found only if it's on the remote's **non-interactive**
   PATH. `ssh host cmd` runs a non-login shell, so `~/.cargo/bin` and similar
   user-local install locations often aren't there even if they'd be on PATH
   in an interactive session — a common miss if `plexy-glass` isn't on the
   remote's system PATH.

If the resolved command isn't found, SSH exits 127 and plexy-glass reports it
directly instead of hanging or printing a bare connection error:

```
remote `plexy-glass` not found on the host; pass --remote-bin <path> or --install
```

## `--install` (provisioning a remote binary)

Not yet implemented as of this writing — the flag is accepted but does
nothing yet. It will provision a compatible `plexy-glass` on the remote by
downloading the matching `nightly` release artifact and pushing it over the
existing SSH connection, so a remote with nothing installed can be reached
with one command. Track its landing in this doc's history; until then use
`--remote-bin` to point at a binary you've placed on the remote yourself.

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
