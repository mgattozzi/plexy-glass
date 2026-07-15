use std::future::pending;
use std::time::Duration;
use std::{io, str};

use bytes::BytesMut;
use plexy_glass_protocol::errors::CodecError;
use plexy_glass_protocol::{
    ClientMsg, Codec, ColorScheme, CreatePolicy, ExitStatus, PtySize, ServerMsg, SessionEntry,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::error::ClientError;
use crate::picker::{PickerOutcome, PickerRow, PickerState, PickerTheme, RowKind, RowStatus};
use crate::query::{self, HostStatus};
use crate::roster::{self, RosterSource};
use crate::transport::{Connect, Host, InstallPolicy, RemoteName, Target};
use crate::tty;

const STDIN_CHUNK: usize = 4096;

/// A control event the outer terminal sent on the input stream (focus / theme).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OuterEvent {
    FocusIn,
    FocusOut,
    ColorScheme(ColorScheme),
}

/// Strip outer-terminal focus (`\e[I`/`\e[O`) and color-scheme
/// (`\e[?997;1n`/`\e[?997;2n`) sequences from `buf` *in place*, returning them in
/// order. Remaining bytes are ordinary input forwarded to the daemon.
///
/// Scans a single chunk in isolation with no cross-call state: a sequence split
/// across two `stdin.read`s (the `\e[` at the tail of one chunk, the rest at the
/// head of the next) is NOT recognized, so its bytes pass through as input.
/// Terminals emit these notifications atomically, so a split is rare; the pin
/// test below locks this so adding carry-over later is a deliberate change.
pub fn scan_outer_events(buf: &mut Vec<u8>) -> Vec<OuterEvent> {
    let mut events = Vec::new();
    let mut kept = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        // `\e[I` / `\e[O`
        if i + 2 < buf.len() && buf[i] == 0x1b && buf[i + 1] == b'[' {
            if buf[i + 2] == b'I' {
                events.push(OuterEvent::FocusIn);
                i += 3;
                continue;
            }
            if buf[i + 2] == b'O' {
                events.push(OuterEvent::FocusOut);
                i += 3;
                continue;
            }
            // `\e[?997;Xn`
            if let Some((scheme, len)) = parse_color_scheme(&buf[i..]) {
                events.push(OuterEvent::ColorScheme(scheme));
                i += len;
                continue;
            }
        }
        kept.push(buf[i]);
        i += 1;
    }
    *buf = kept;
    events
}

/// Parse a leading `\e[?997;1n` / `\e[?997;2n`. Returns `(scheme, bytes_consumed)`.
fn parse_color_scheme(b: &[u8]) -> Option<(ColorScheme, usize)> {
    const PREFIX: &[u8] = b"\x1b[?997;";
    if !b.starts_with(PREFIX) {
        return None;
    }
    let rest = &b[PREFIX.len()..];
    let mut j = 0;
    while j < rest.len() && rest[j].is_ascii_digit() {
        j += 1;
    }
    if j == 0 || j >= rest.len() || rest[j] != b'n' {
        return None;
    }
    let n: u32 = str::from_utf8(&rest[..j]).ok()?.parse().ok()?;
    let scheme = match n {
        1 => ColorScheme::Dark,
        2 => ColorScheme::Light,
        _ => return None,
    };
    Some((scheme, PREFIX.len() + j + 1))
}

/// Why the pump handed control back to the outer attach loop.
#[derive(Debug)]
pub enum PumpExit {
    /// A bare EOF on the daemon socket (detach, or the daemon itself died) —
    /// the client should quit.
    Ended(ExitStatus),
    /// The daemon's renderer sent `ServerMsg::Exited`: the SESSION itself
    /// ended (killed, or its last shell exited), not a detach or daemon
    /// crash — a detach aborts the renderer before it can write this marker
    /// (see `crates/daemon/src/renderer.rs`). No payload: the follow
    /// decision (which session to jump to) is made by the outer loop after
    /// querying the daemon, not here.
    Follow,
    /// (Milestone B) the picker chose a session on a DIFFERENT daemon (or a new
    /// one on a host); the outer loop re-attaches (`lib.rs::run`'s `next =
    /// (reconnect_target, Some(reconnect_name))`). `target.host: None` reconnects
    /// on the LOCAL daemon; `create_if_missing` is globally `CreateIfMissing`
    /// (`client_attach_smart`), so a fresh `name` creates and an existing one
    /// attaches — no flag needed here.
    ReconnectTo { target: crate::Target, name: String },
}

/// Run the three concurrent pumps:
///   stdin  -> ClientMsg::Input(bytes)  -> daemon
///   daemon -> ServerMsg::Output(bytes) -> stdout
///   SIGWINCH (delivered via `resize_rx`) -> ClientMsg::Resize(size) -> daemon
///
/// Borrows all of its IO from the caller's outer attach loop (it never spawns,
/// so the `Send + 'static` bounds are unnecessary). Returns `PumpExit::Ended`
/// on a bare EOF (detach or daemon death); `PumpExit::Follow` when the daemon
/// signals the session itself ended; `ReconnectTo` is reserved for the picker
/// (Milestone B) and never produced here.
///
/// `current_target` is the daemon this pump is attached to (`&next.0` in
/// `run`); its `host` tags the current daemon's rows in the session picker
/// (`None` when truly local) and is the daemon EXCLUDED from the roster query.
///
/// `open_picker_after_attach` is set by a `FollowDecision::SwitchThenPick`
/// (`run`'s `Follow` handling in `lib.rs`): when true, the picker opens
/// immediately, before this pump reads a single daemon frame, seeded from a
/// fresh `ListSessions` query rather than waiting for the daemon to push
/// `OpenSessionPicker` (an ordinary attach never gets that message).
/// `attached_name` is the session this pump just attached to (from the
/// `Attached` reply, not the name requested — the daemon may have picked),
/// used to seed the picker's `current` row.
// ponytail: IO handles + a resize channel + the follow-pick bool/name; a
// wrapper struct would just rename the same transient call-site args (cf.
// `draw_box`'s allow in compositor.rs).
#[expect(
    clippy::too_many_arguments,
    reason = "IO handles, a resize channel, the target, and the follow-pick flag/name; no natural param grouping"
)]
pub async fn pump<R, W, In, Out>(
    daemon_read: &mut R,
    daemon_write: &mut W,
    stdin: &mut In,
    stdout: &mut Out,
    initial_size: PtySize,
    resize_rx: &mut mpsc::Receiver<PtySize>,
    current_target: &Target,
    open_picker_after_attach: bool,
    attached_name: &str,
) -> Result<PumpExit, ClientError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    In: AsyncRead + Unpin,
    Out: AsyncWrite + Unpin,
{
    let mut stdin_buf = BytesMut::with_capacity(STDIN_CHUNK);
    // The client's live terminal size: seeded at build, updated on SIGWINCH, and
    // handed to the picker so it can center + size its box.
    let mut size = initial_size;
    // Cancel-safety: `Codec::read_frame` is `read_exact`-based and NOT
    // cancel-safe. If `select!` drops it mid-frame (because a stdin byte or a
    // resize won the race while a daemon Output frame was still arriving) the
    // bytes already consumed from the socket are lost and the stream desyncs.
    // So the read future is pinned across iterations and recreated only after
    // it completes, mirroring the daemon's `serve_attach` (see connection.rs).
    let mut read_fut = Box::pin(Codec::read_frame(&mut *daemon_read));
    // The in-pump session-picker sub-mode (`ServerMsg::OpenSessionPicker`).
    // `Some` pairs the picker's own state with the session name it was
    // opened against, so `PickerOutcome::Switch` can no-op a reselect of the
    // already-attached session. While `Some`, stdin routes to the picker
    // (never the daemon) and `ServerMsg::Output` frames are dropped.
    let mut picker: Option<(PickerState, String)> = None;
    // The streaming per-daemon query receiver, live only while the picker is up
    // (`Some` iff `picker` is `Some`). Each `(host, status)` fills the matching
    // host's rows in incrementally; `None` on the channel means every queried
    // daemon has resolved.
    let mut picker_rx: Option<mpsc::UnboundedReceiver<(Host, HostStatus)>> = None;

    if open_picker_after_attach {
        let entries = fresh_session_list(current_target).await?;
        let (state, rx) = build_picker_state(entries, current_target, attached_name, size);
        stdout
            .write_all(PICKER_ENTER)
            .await
            .map_err(ClientError::Io)?;
        tty::set_alt_active(true);
        stdout
            .write_all(&state.render())
            .await
            .map_err(ClientError::Io)?;
        stdout.flush().await.map_err(ClientError::Io)?;
        picker_rx = Some(rx);
        picker = Some((state, attached_name.to_string()));
    }

    // Once stdin hits EOF, stop polling it. A closed stdin read returns 0
    // immediately and forever, so a bare `continue` on it busy-spins the select
    // loop and starves the daemon-frame arm (on a current-thread runtime it
    // never yields, so a buffered `Exited`/frame is never read). Gating the arm
    // keeps the session alive, driven by daemon frames, with no spin.
    let mut stdin_open = true;

    loop {
        stdin_buf.clear();
        stdin_buf.resize(STDIN_CHUNK, 0);
        tokio::select! {
            // Daemon -> client
            frame = &mut read_fut => {
                let Some(frame) = frame? else {
                    return Ok(PumpExit::Ended(ExitStatus::Unknown));
                };
                // Recreate the pinned read future for the next iteration, and only ever
                // after it completed, so no buffered frame bytes are lost. Drop the old
                // (completed) future first to release its borrow of `daemon_read` before
                // the new one reborrows it.
                read_fut = {
                    drop(read_fut);
                    Box::pin(Codec::read_frame(&mut *daemon_read))
                };
                let msg: ServerMsg = postcard::from_bytes(&frame)
                    .map_err(|e| CodecError::Decode(e.to_string()))?;
                match msg {
                    ServerMsg::Output(b) => {
                        // While the picker is up, daemon output is dropped rather
                        // than written underneath the picker's own drawing.
                        if picker.is_none() {
                            stdout.write_all(&b).await.map_err(ClientError::Io)?;
                            stdout.flush().await.map_err(ClientError::Io)?;
                        }
                    }
                    ServerMsg::Exited { .. } => {
                        // The renderer writes this marker only when the
                        // session itself ended (killed, or its last shell
                        // exited) — a detach aborts the renderer before it
                        // ever gets here (see `renderer.rs`), so this is
                        // never a detach. Follow to another session instead
                        // of exiting; the status is vestigial (the pane's
                        // real exit status, if any, already came through
                        // separately) and unused here.
                        return Ok(PumpExit::Follow);
                    }
                    ServerMsg::Error(e) => {
                        return Err(ClientError::DaemonError(e));
                    }
                    ServerMsg::OpenSessionPicker { sessions, current } => {
                        // Note: stdin typed in the round-trip window before this
                        // arrives is still forwarded to the pane (inherent to the
                        // client-rendered picker: the daemon sends the session
                        // list and keeps serving the pane until we open the box).
                        //
                        // Assemble the daemon set: the current daemon (a Live
                        // Host anchor + its `sessions`, tagged by the real
                        // current host) plus every OTHER daemon in
                        // `{local} ∪ roster` MINUS current (a Pending anchor
                        // each). The picker opens IMMEDIATELY; the streaming
                        // query below fills the other daemons' rows in as each
                        // resolves.
                        let (state, rx) =
                            build_picker_state(sessions, current_target, &current, size);
                        stdout.write_all(PICKER_ENTER).await.map_err(ClientError::Io)?;
                        tty::set_alt_active(true);
                        stdout.write_all(&state.render()).await.map_err(ClientError::Io)?;
                        stdout.flush().await.map_err(ClientError::Io)?;
                        picker_rx = Some(rx);
                        picker = Some((state, current));
                    }
                    // `Attached` was already handled by the caller; `ServerMsg`
                    // is `#[non_exhaustive]`, so ignore it and any future variants.
                    #[allow(
                        unreachable_patterns,
                        reason = "ServerMsg is #[non_exhaustive]; the wildcard handles the caller-consumed Attached and any variants added later"
                    )]
                    _ => {}
                }
            }
            // Client -> daemon (stdin). Disabled once stdin closes (see
            // `stdin_open` above) so a persistent EOF can't spin the loop.
            n = stdin.read(&mut stdin_buf), if stdin_open => {
                let n = n.map_err(ClientError::Io)?;
                if n == 0 {
                    // stdin closed; stop polling it and keep the session alive
                    // (driven by daemon frames) until the child exits.
                    stdin_open = false;
                    continue;
                }
                let mut chunk = stdin_buf.split_to(n).to_vec();
                if picker.is_some() {
                    // Picker sub-mode: stdin goes only to the picker, never the
                    // daemon. `take()` sidesteps borrowing `picker` across the
                    // `picker = Some(..)` reassignment in the re-render arm below.
                    // invariant: just checked is_some(), so take() cannot yield None.
                    let (mut state, current) = picker.take().expect("picker.is_some() checked above");
                    match feed_picker_bytes(&mut state, &chunk) {
                        Some(PickerOutcome::Switch(name)) => {
                            // Leaving the picker: pop back to the main screen.
                            stdout.write_all(PICKER_LEAVE).await.map_err(ClientError::Io)?;
                            tty::set_alt_active(false);
                            stdout.flush().await.map_err(ClientError::Io)?;
                            if name == current {
                                // Reselecting the already-attached session: no
                                // SwitchSession needed, but the picker already
                                // painted a `\x1b[2J\x1b[H` clear over the screen and
                                // nothing else will repaint it on an idle session.
                                // Redraw exactly like the Cancel arm below so the
                                // daemon re-emits a full frame over the cleared
                                // screen.
                                send_client_msg(&mut *daemon_write, &ClientMsg::Redraw).await?;
                            } else {
                                send_client_msg(
                                    &mut *daemon_write,
                                    &ClientMsg::SwitchSession { name },
                                )
                                .await?;
                            }
                            // Same-session reselect or a real switch: either way the
                            // picker is done; leave `picker` at None (already taken)
                            // and drop the query receiver so its task stops.
                            picker_rx = None;
                        }
                        Some(PickerOutcome::Cancel) => {
                            // Leaving the picker: pop back to the main screen.
                            stdout.write_all(PICKER_LEAVE).await.map_err(ClientError::Io)?;
                            tty::set_alt_active(false);
                            stdout.flush().await.map_err(ClientError::Io)?;
                            send_client_msg(&mut *daemon_write, &ClientMsg::Redraw).await?;
                            picker_rx = None;
                        }
                        Some(
                            PickerOutcome::Reconnect { host, name, install }
                            | PickerOutcome::New { host, name, install },
                        ) => {
                            // Cross-daemon jump (Reconnect) or a brand-new
                            // session on a host (New): hand it back to the outer
                            // attach loop rather than acting on it here. `host:
                            // None` re-attaches on the LOCAL daemon; the global
                            // `create_if_missing` (CreateIfMissing) means an existing
                            // `name` attaches and a fresh one creates, so New needs no
                            // separate flag. `picker`/`picker_rx` are dropped
                            // along with the rest of `pump`'s state on return.
                            //
                            // A host-anchor Enter reconnects to that daemon's
                            // DEFAULT session, which `accept()` encodes as an
                            // empty `name`; normalize it to the same `"main"`
                            // default `client_attach_smart` uses so an empty name
                            // never reaches the wire (the daemon rejects `""`
                            // with `EmptyName`, which would eject the client).
                            let name = if name.is_empty() {
                                "main".to_string()
                            } else {
                                name
                            };
                            // Leaving the picker to re-attach: pop back to the
                            // main screen. Load-bearing on the reconnect path —
                            // the re-attach runs SSH auth in cooked mode reading
                            // /dev/tty before any daemon frame, so a still-hidden
                            // cursor means typing an SSH password blind, and a
                            // still-pushed alt buffer means the prompt (and any
                            // error) lands on a screen that is about to vanish.
                            stdout.write_all(PICKER_LEAVE).await.map_err(ClientError::Io)?;
                            tty::set_alt_active(false);
                            stdout.flush().await.map_err(ClientError::Io)?;
                            return Ok(PumpExit::ReconnectTo {
                                target: resolve_target(&host, current_target, install),
                                name,
                            });
                        }
                        Some(PickerOutcome::Forget { host }) => {
                            // Forget an ad-hoc host: rewrite the roster file,
                            // then rebuild just the OTHER-daemon rows (the
                            // current daemon's own anchor + sessions are
                            // untouched) and restart their query, exactly like
                            // the initial `OpenSessionPicker` assembly — the
                            // forgotten host's row (and any already-resolved
                            // session rows under it) disappears. Stay in the
                            // picker; do NOT return.
                            roster::forget_adhoc(&host);
                            let fresh = build_roster_rows(Some(&current_target.host));
                            state.replace_other_rows(
                                &Some(current_target.host.clone()),
                                fresh.rows,
                                fresh.adhoc,
                            );
                            stdout
                                .write_all(&state.render())
                                .await
                                .map_err(ClientError::Io)?;
                            stdout.flush().await.map_err(ClientError::Io)?;
                            picker_rx =
                                Some(spawn_picker_query(fresh.remote_hosts, fresh.query_local));
                            picker = Some((state, current));
                        }
                        Some(PickerOutcome::Kill { host, name }) => {
                            // Resolve the row's host to a Target the same way
                            // `accept`/`Reconnect` do: the row's own daemon
                            // when it differs from ours (a fresh Target, no
                            // known remote_bin/install for it), or
                            // `current_target` itself (keeping its
                            // remote_bin/install) when the row IS our own
                            // daemon. Either way this is a FRESH one-off
                            // connection (`Connect::Only`) — `KillSession` is
                            // only accepted as a connection's first message —
                            // mirroring `client_kill_session` (`lib.rs:407`).
                            let target =
                                resolve_target(&host, current_target, InstallPolicy::UseExisting);
                            let is_current_session =
                                host == current_target.host && name == current;
                            let reply = crate::request_reply(
                                &target,
                                Connect::Only,
                                ClientMsg::KillSession { name: name.clone() },
                            )
                            .await;
                            match reply {
                                Ok(ServerMsg::SessionKilled { .. }) if is_current_session => {
                                    // Do nothing here: the daemon is tearing
                                    // down OUR main connection right now, its
                                    // renderer will write the Exited marker
                                    // (the arm above), and that already turns
                                    // into PumpExit::Follow.
                                }
                                Ok(ServerMsg::SessionKilled { .. }) => {
                                    // A different session: drop its row
                                    // locally and stay in the picker, mirroring
                                    // the Forget arm's local edit + repaint.
                                    state.remove_row(&host, &name);
                                    stdout
                                        .write_all(&state.render())
                                        .await
                                        .map_err(ClientError::Io)?;
                                    stdout.flush().await.map_err(ClientError::Io)?;
                                }
                                Ok(ServerMsg::Error(e)) => {
                                    stdout
                                        .write_all(
                                            format!(
                                                "\r\nplexy-glass: {}\r\n",
                                                ClientError::DaemonError(e)
                                            )
                                            .as_bytes(),
                                        )
                                        .await
                                        .map_err(ClientError::Io)?;
                                    stdout.flush().await.map_err(ClientError::Io)?;
                                }
                                // Any other reply (rare, `ServerMsg` is
                                // `#[non_exhaustive]`) or a transport/connect
                                // failure: flash it and stay in the picker.
                                Ok(_) => {}
                                Err(e) => {
                                    stdout
                                        .write_all(format!("\r\nplexy-glass: {e}\r\n").as_bytes())
                                        .await
                                        .map_err(ClientError::Io)?;
                                    stdout.flush().await.map_err(ClientError::Io)?;
                                }
                            }
                            picker = Some((state, current));
                        }
                        None => {
                            stdout.write_all(&state.render()).await.map_err(ClientError::Io)?;
                            stdout.flush().await.map_err(ClientError::Io)?;
                            picker = Some((state, current));
                        }
                    }
                    continue;
                }
                // Extract outer-terminal focus/theme events; relay them as
                // dedicated `ClientMsg`s, forward the remaining bytes as Input.
                let events = scan_outer_events(&mut chunk);
                for ev in events {
                    let msg = match ev {
                        OuterEvent::FocusIn => ClientMsg::FocusIn,
                        OuterEvent::FocusOut => ClientMsg::FocusOut,
                        OuterEvent::ColorScheme(s) => ClientMsg::ColorScheme(s),
                    };
                    send_client_msg(&mut *daemon_write, &msg).await?;
                }
                if !chunk.is_empty() {
                    let msg = ClientMsg::Input(bytes::Bytes::from(chunk));
                    send_client_msg(&mut *daemon_write, &msg).await?;
                }
            }
            // Client -> daemon (resize)
            Some(new_size) = resize_rx.recv() => {
                size = new_size;
                send_client_msg(&mut *daemon_write, &ClientMsg::Resize(size)).await?;
                if let Some((state, _current)) = picker.as_mut() {
                    state.set_size(size);
                    stdout.write_all(&state.render()).await.map_err(ClientError::Io)?;
                    stdout.flush().await.map_err(ClientError::Io)?;
                }
            }
            // Streaming per-daemon query results (only while the picker is up).
            // `next_status` pends forever when `picker_rx` is None, so this arm
            // is inert outside the picker.
            status = next_status(&mut picker_rx) => {
                match status {
                    Some((host, hs)) => {
                        // A straggler after the picker closed has no rows to
                        // update; ignore it. Otherwise fold the result in and
                        // repaint under the same picker-owns-the-screen rule.
                        if let Some((state, _current)) = picker.as_mut() {
                            let (row_status, rows) = resolve_status(&host, hs);
                            state.resolve_host(&host, row_status, rows);
                            stdout.write_all(&state.render()).await.map_err(ClientError::Io)?;
                            stdout.flush().await.map_err(ClientError::Io)?;
                        }
                    }
                    // Channel closed: every queried daemon resolved. Stop polling
                    // (else `recv` returns `None` in a hot loop).
                    None => picker_rx = None,
                }
            }
        }
    }
}

/// The `Target` for a picker row's daemon, given the one we're attached to.
///
/// Same daemon: reuse `current` wholesale, keeping its `--remote-bin` and
/// `--install`. A different daemon: a fresh `Target`, because those two are
/// per-host and nothing we know about ours transfers — `--remote-bin` is a path
/// on THIS host, and pointing host B at host A's binary is worse than not
/// knowing. (Which is also why the roster fan-out in `query.rs` builds bare
/// Targets and must keep doing so: it queries every host at once, and there is
/// no one path that could be right for all of them. A per-host config `bin=` is
/// the only honest source for that.)
///
/// The Kill arm worked this out first and got it right; the Reconnect/New arm
/// hardcoded `remote_bin: None`, so `--remote-bin` silently stopped applying the
/// moment you went through the picker — even reconnecting to the SAME host you
/// had just passed it for. One rule, one place, both arms call it.
fn resolve_target(host: &Host, current: &Target, install: InstallPolicy) -> Target {
    if *host == current.host {
        Target {
            install,
            ..current.clone()
        }
    } else {
        Target {
            host: host.clone(),
            remote_bin: None,
            install,
        }
    }
}

/// Enter the picker's screen: push the alternate screen buffer.
///
/// The picker is a full-screen modal, and until now it painted straight onto the
/// MAIN screen — its `render` opens with `\x1b[2J\x1b[H`, wiping whatever the
/// daemon had composed there. That is why `Switch`/`Cancel` have to ask the
/// daemon to `Redraw`, and why anything that went wrong between closing the
/// picker and the next frame (a failed reconnect, say) printed its error on top
/// of a still-resident box. On the alt buffer the main screen is left exactly as
/// the daemon last painted it, and popping restores it for free.
///
/// The `2J` in `render` is still right — it clears the ALT buffer on entry, which
/// may hold stale content from a previous push.
const PICKER_ENTER: &[u8] = b"\x1b[?1049h";

/// Leave the picker's screen: pop back to the main buffer, then unhide the cursor
/// `render` hid. Pop first, so the cursor we unhide is the main screen's.
///
/// Note this does NOT remove the need for the `Redraw` on the `Switch`/`Cancel`
/// paths: `ServerMsg::Output` frames are dropped while the picker is open (see
/// the drain arm's `picker.is_none()` guard), so the main screen the terminal
/// restores is intact but STALE. Popping fixes the paint; only the daemon can fix
/// the content.
const PICKER_LEAVE: &[u8] = b"\x1b[?1049l\x1b[?25h";

/// Drive the session picker with NO attached session, to an outcome.
///
/// The picker used to exist only inside `pump`, i.e. only once you were already
/// attached to something. That is exactly backwards for the case that needs it
/// most: when an attach FAILS there is no session to open a picker in, so the
/// error had nowhere to go but out of `run` and into the process exit. Detached,
/// the picker becomes the answer to "that didn't work, where do you want to go
/// instead" rather than a thing you can only reach once you're already somewhere.
///
/// Returns the daemon + session to attach next, or `None` to fall back to the
/// command line (the user pressed Esc, or there is nothing anywhere to offer).
///
/// This is only representable because `Host` names `Local`: `current_host: None`
/// now means "attached to nothing" rather than "attached to local", so `accept()`
/// matches no row, and every Enter is a `Reconnect`. See `PickerState::accept`.
///
/// The caller owns raw mode (`HostTty`); this owns the alt screen for its
/// lifetime.
pub(crate) async fn run_standalone_picker<In, Out>(
    stdin: &mut In,
    stdout: &mut Out,
    size: PtySize,
    notice: &str,
) -> Result<Option<(Target, String)>, ClientError>
where
    In: AsyncRead + Unpin,
    Out: AsyncWrite + Unpin,
{
    // Nothing anywhere to offer -> the command line, rather than an empty box.
    //
    // Deliberately keyed on REACHABILITY, not on session count: a reachable
    // daemon with zero sessions has plenty to offer, since `n` makes one on it.
    // Likewise an empty roster is not the end of it while a daemon is up. Only
    // "no hosts known AND no local daemon" is genuinely nowhere to go.
    //
    // The local probe is cheap enough to do inline: it is a unix socket, so a
    // missing daemon fails in microseconds and never pays the per-host ssh
    // budget the roster query does.
    if roster::assemble(&roster::config_remotes(), &roster::load_adhoc()).is_empty()
        && !local_daemon_is_live().await
    {
        return Ok(None);
    }

    let roster_rows = build_roster_rows(None);
    let mut state = PickerState::new_with_current(roster_rows.rows, &None, "");
    state.set_adhoc_hosts(roster_rows.adhoc);
    state.set_size(size);
    state.set_theme(PickerTheme::resolve(&roster::config_palette()));
    state.set_notice(notice.to_string());
    let mut picker_rx = Some(spawn_picker_query(
        roster_rows.remote_hosts,
        roster_rows.query_local,
    ));

    stdout
        .write_all(PICKER_ENTER)
        .await
        .map_err(ClientError::Io)?;
    tty::set_alt_active(true);
    let outcome = drive_standalone_picker(stdin, stdout, &mut state, &mut picker_rx).await;
    stdout
        .write_all(PICKER_LEAVE)
        .await
        .map_err(ClientError::Io)?;
    tty::set_alt_active(false);
    stdout.flush().await.map_err(ClientError::Io)?;
    outcome
}

/// Whether a daemon is listening on the local socket. `Connect::Only`, so asking
/// can never start one — the point is to find out if there is anywhere to go, not
/// to make somewhere to go.
async fn local_daemon_is_live() -> bool {
    matches!(
        crate::request_reply(&Target::default(), Connect::Only, ClientMsg::ListSessions).await,
        Ok(ServerMsg::SessionList { .. })
    )
}

/// The standalone picker's event loop, split out so `run_standalone_picker` can
/// pop the alt screen on every exit path including `?`.
async fn drive_standalone_picker<In, Out>(
    stdin: &mut In,
    stdout: &mut Out,
    state: &mut PickerState,
    picker_rx: &mut Option<mpsc::UnboundedReceiver<(Host, HostStatus)>>,
) -> Result<Option<(Target, String)>, ClientError>
where
    In: AsyncRead + Unpin,
    Out: AsyncWrite + Unpin,
{
    let mut buf = BytesMut::with_capacity(STDIN_CHUNK);
    stdout
        .write_all(&state.render())
        .await
        .map_err(ClientError::Io)?;
    stdout.flush().await.map_err(ClientError::Io)?;

    loop {
        tokio::select! {
            n = stdin.read_buf(&mut buf) => {
                let n = n.map_err(ClientError::Io)?;
                // stdin closed with nothing chosen: there is no session to keep
                // alive here (that is the whole point of this picker), so the
                // only honest answer is the command line.
                if n == 0 {
                    return Ok(None);
                }
                let chunk = buf.split().to_vec();
                match feed_picker_bytes(state, &chunk) {
                    Some(PickerOutcome::Cancel) => return Ok(None),
                    Some(
                        PickerOutcome::Reconnect { host, name, install }
                        | PickerOutcome::New { host, name, install },
                    ) => {
                        // A host anchor commits an empty name meaning "that
                        // daemon's default session"; normalize it exactly as the
                        // attached picker does, so an empty name never reaches
                        // the wire (the daemon rejects it with `EmptyName`).
                        let name = if name.is_empty() { "main".to_string() } else { name };
                        return Ok(Some((Target { host, remote_bin: None, install }, name)));
                    }
                    Some(PickerOutcome::Switch(name)) => {
                        // Unreachable: `accept` only emits `Switch` for a row on
                        // the daemon we are attached to, and we are attached to
                        // none. Treat it as what it would have to mean anyway
                        // rather than panic on a state the types still permit.
                        return Ok(Some((Target::default(), name)));
                    }
                    Some(PickerOutcome::Forget { host }) => {
                        roster::forget_adhoc(&host);
                        let fresh = build_roster_rows(None);
                        state.replace_other_rows(&None, fresh.rows, fresh.adhoc);
                        *picker_rx = Some(spawn_picker_query(fresh.remote_hosts, fresh.query_local));
                    }
                    Some(PickerOutcome::Kill { host, name }) => {
                        // No session of ours to lose here, so unlike the attached
                        // picker's Kill there is no current-session case: every
                        // kill is someone else's, drop the row and stay.
                        let target = Target { host: host.clone(), remote_bin: None, install: InstallPolicy::UseExisting };
                        match crate::request_reply(&target, Connect::Only, ClientMsg::KillSession { name: name.clone() }).await {
                            Ok(ServerMsg::SessionKilled { .. }) => state.remove_row(&host, &name),
                            Ok(ServerMsg::Error(e)) => {
                                state.set_notice(ClientError::DaemonError(e).to_string());
                            }
                            Ok(_) => {}
                            Err(e) => state.set_notice(e.to_string()),
                        }
                    }
                    None => {}
                }
            }
            status = next_status(picker_rx) => {
                match status {
                    Some((host, hs)) => {
                        let (row_status, rows) = resolve_status(&host, hs);
                        state.resolve_host(&host, row_status, rows);
                    }
                    // Channel closed: every queried daemon resolved. Stop polling
                    // (else `recv` returns `None` in a hot loop). `next_status`
                    // pends forever once this is `None`, so the arm goes inert.
                    None => *picker_rx = None,
                }
            }
        }
        stdout
            .write_all(&state.render())
            .await
            .map_err(ClientError::Io)?;
        stdout.flush().await.map_err(ClientError::Io)?;
    }
}

/// The rows + query inputs assembled for one `OpenSessionPicker`.
struct PickerAssembly {
    rows: Vec<PickerRow>,
    /// Ad-hoc host names, for `PickerState::set_adhoc_hosts` (drives `x`→Forget
    /// and the render-time divider).
    adhoc: Vec<String>,
    /// The OTHER daemons' remote hosts to stream-query (roster minus current).
    /// Remotes only — the local daemon rides `query_local`, not this list, since
    /// it is queried on the socket rather than over ssh.
    remote_hosts: Vec<RemoteName>,
    /// Whether the local daemon is in the OTHER set (true only when we're
    /// attached to a REMOTE), so it gets queried on the local socket.
    query_local: bool,
}

/// Format one `SessionEntry` into a picker Session row tagged by `host`. The
/// label matches the daemon's old overlay verbatim.
fn session_row(e: SessionEntry, host: Host) -> PickerRow {
    PickerRow {
        label: format!(
            "{} \u{2014} {} win, {} panes, {} clients",
            e.name, e.windows, e.panes, e.clients
        ),
        name: e.name,
        host,
        kind: RowKind::Session,
        status: RowStatus::Live,
    }
}

/// Build the picker's rows from the current daemon's `sessions` + the roster.
///
/// The current daemon is a SELECTABLE Live `Host` anchor (so `n` on it creates
/// a session on THIS daemon — the Task-4-review fix) with its sessions as child
/// rows tagged by `current_host`. Every OTHER daemon in `{local} ∪ roster`
/// MINUS current (compared by host) gets a `Pending` anchor. The
/// configured-then-ad-hoc order from `roster::assemble` is preserved so the
/// render-time divider lands correctly.
fn build_picker_rows(sessions: Vec<SessionEntry>, current_host: &Host) -> PickerAssembly {
    let mut rows = Vec::new();
    // `Host: Display` is `local` for the local daemon, else the ssh target — the
    // anchor label and the row identity are the same string.
    let anchor_name = current_host.to_string();
    rows.push(PickerRow {
        name: anchor_name.clone(),
        label: anchor_name,
        host: current_host.clone(),
        kind: RowKind::Host,
        status: RowStatus::Live,
    });
    for e in sessions {
        rows.push(session_row(e, current_host.clone()));
    }

    let mut roster_rows = build_roster_rows(Some(current_host));
    rows.append(&mut roster_rows.rows);

    PickerAssembly {
        rows,
        adhoc: roster_rows.adhoc,
        remote_hosts: roster_rows.remote_hosts,
        query_local: roster_rows.query_local,
    }
}

/// Build a ready-to-render `PickerState` from a session list plus the daemon
/// we're attached to, and kick off the streaming per-host roster query.
/// Shared by the `ServerMsg::OpenSessionPicker` handler (the daemon-initiated
/// picker, `Ctrl+a w`) and the follow-then-pick path (`pump`'s
/// `open_picker_after_attach`): both need the exact same rows assembled from a
/// session list, `current_target`, and the current session's name.
fn build_picker_state(
    sessions: Vec<SessionEntry>,
    current_target: &Target,
    current: &str,
    size: PtySize,
) -> (PickerState, mpsc::UnboundedReceiver<(Host, HostStatus)>) {
    let assembly = build_picker_rows(sessions, &current_target.host);
    let mut state =
        PickerState::new_with_current(assembly.rows, &Some(current_target.host.clone()), current);
    state.set_adhoc_hosts(assembly.adhoc);
    state.set_size(size);
    state.set_theme(PickerTheme::resolve(&roster::config_palette()));
    let rx = spawn_picker_query(assembly.remote_hosts, assembly.query_local);
    (state, rx)
}

/// A one-off `ListSessions` round trip on a fresh connection (`Connect::Only`,
/// no spawn), for seeding the follow-then-pick picker. `request_reply` opens
/// its own connection to `target`, separate from this pump's attached one —
/// the daemon dispatches `ListSessions` only as a connection's first message
/// (see `crates/daemon/src/connection.rs`), so it cannot ride the existing
/// attach connection.
async fn fresh_session_list(target: &Target) -> Result<Vec<SessionEntry>, ClientError> {
    match crate::request_reply(target, Connect::Only, ClientMsg::ListSessions).await? {
        ServerMsg::SessionList { entries } => Ok(entries),
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        _ => Err(ClientError::UnexpectedReply),
    }
}

/// The OTHER-daemon host rows, assembled fresh from the roster: a `local`
/// Pending anchor when we're attached to a REMOTE (so local is an OTHER
/// daemon), plus one Pending anchor per roster host in `{config} ∪ ad-hoc`
/// MINUS `current_host`. This is exactly the second half of
/// `build_picker_rows` — the current daemon's own anchor + session rows are
/// never part of this set — factored out so the `x`/Forget rebuild (Task 6)
/// can re-run it after the roster file changes, without re-fetching or
/// disturbing the current daemon's rows.
struct RosterRows {
    rows: Vec<PickerRow>,
    adhoc: Vec<String>,
    remote_hosts: Vec<RemoteName>,
    query_local: bool,
}

/// `attached` is the daemon we are attached to, or `None` when we are attached to
/// NONE — the standalone picker. Both fall out of the same rule: a daemon is an
/// OTHER daemon unless it is the one we are attached to, so with `None` every
/// daemon (local included) is queried and nothing is excluded.
fn build_roster_rows(attached: Option<&Host>) -> RosterRows {
    let roster = roster::assemble(&roster::config_remotes(), &roster::load_adhoc());
    let query_local = attached != Some(&Host::Local);
    let mut rows = Vec::new();
    if query_local {
        rows.push(PickerRow {
            name: Host::Local.to_string(),
            label: Host::Local.to_string(),
            host: Host::Local,
            kind: RowKind::Host,
            status: RowStatus::Pending,
        });
    }
    let mut remote_hosts = Vec::new();
    let mut adhoc = Vec::new();
    for h in roster {
        if attached.and_then(Host::remote) == Some(&h.host) {
            continue; // this IS the current daemon
        }
        if h.source == RosterSource::AdHoc {
            adhoc.push(h.host.to_string());
        }
        rows.push(PickerRow {
            name: h.host.to_string(),
            label: h.host.to_string(),
            host: Host::Remote(h.host.clone()),
            kind: RowKind::Host,
            status: RowStatus::Pending,
        });
        remote_hosts.push(h.host);
    }

    // The `＋ Connect to a host…` affordance is NOT a row: the picker synthesizes
    // it as an always-present trailing slot at render time (like the section
    // headers/divider), so it can't be duplicated or land mid-list.

    RosterRows {
        rows,
        adhoc,
        remote_hosts,
        query_local,
    }
}

/// Map a resolved [`HostStatus`] to its picker row status + the Session rows to
/// splice under the host (empty unless `Live`).
fn resolve_status(host: &Host, hs: HostStatus) -> (RowStatus, Vec<PickerRow>) {
    match hs {
        HostStatus::Live(entries) => (
            RowStatus::Live,
            entries
                .into_iter()
                .map(|e| session_row(e, host.clone()))
                .collect(),
        ),
        HostStatus::Empty => (RowStatus::Empty, Vec::new()),
        HostStatus::Unreachable => (RowStatus::Unreachable, Vec::new()),
        HostStatus::VersionMismatch(v) => (RowStatus::VersionMismatch(v), Vec::new()),
    }
}

/// Kick off the streaming per-daemon query for the picker's OTHER daemons and
/// return the receiver the drain arm polls. Remote hosts stream via
/// [`query::spawn_query`] (keyed by their host string); the local daemon — in
/// the set only when we're attached to a REMOTE — is queried once on the local
/// socket (`Connect::Only`, no spawn). Both feed ONE
/// `(Option<String>, HostStatus)` channel keyed exactly like `PickerRow::host`,
/// so the drain updates rows uniformly. The original sender is dropped here so
/// the channel closes once every producer finishes.
fn spawn_picker_query(
    remote_hosts: Vec<RemoteName>,
    query_local: bool,
) -> mpsc::UnboundedReceiver<(Host, HostStatus)> {
    const PER_HOST: Duration = Duration::from_millis(2500);
    let (tx, rx) = mpsc::unbounded_channel();
    if !remote_hosts.is_empty() {
        // `spawn_query` streams `(String, HostStatus)`; re-key each result to
        // `Some(host)` onto the unified channel.
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
        query::spawn_query(remote_hosts, PER_HOST, raw_tx);
        let fwd = tx.clone();
        tokio::spawn(async move {
            while let Some((host, status)) = raw_rx.recv().await {
                if fwd.send((Host::Remote(host), status)).is_err() {
                    break; // picker closed
                }
            }
        });
    }
    if query_local {
        // The last (or only) sender; move it so the channel closes when the
        // local query finishes. Any earlier remote forwarder holds its own clone.
        let ltx = tx;
        tokio::spawn(async move {
            let target = Target::default();
            let res = match timeout(
                PER_HOST,
                crate::request_reply(&target, Connect::Only, ClientMsg::ListSessions),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(ClientError::Io(io::Error::other("timeout"))),
            };
            let _ = ltx.send((Host::Local, query::classify(res)));
        });
    }
    rx
}

/// Await the next streaming query result, or pend forever when no picker query
/// is live (`picker_rx` is `None`) so the drain `select!` arm stays inert.
async fn next_status(
    rx: &mut Option<mpsc::UnboundedReceiver<(Host, HostStatus)>>,
) -> Option<(Host, HostStatus)> {
    match rx {
        Some(rx) => rx.recv().await,
        None => pending().await,
    }
}

/// Feed a raw stdin chunk to the picker one logical key at a time, decoding
/// the two arrow-key escape sequences (`\e[A`/`\e[B`) to the picker's
/// Ctrl-P/Ctrl-N up/down bytes first (`PickerState::handle_key` only sees
/// single bytes; arrows arrive as three). Stops and returns the outcome as
/// soon as one key commits or cancels the picker; any bytes still unread in
/// the chunk are dropped (typing ahead of Enter/Esc in the picker is rare
/// and not worth threading through).
///
/// Per-call scan with no cross-call state, same caveat as
/// `scan_outer_events`: an arrow escape split across two `stdin.read` chunks
/// (the `\e[` at the tail of one chunk, the rest at the head of the next) is
/// NOT reassembled, so the lone `\x1b` misfires as a bare Esc (Cancel).
fn feed_picker_bytes(state: &mut PickerState, bytes: &[u8]) -> Option<PickerOutcome> {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 2 < bytes.len() && bytes[i + 1] == b'[' {
            let arrow = match bytes[i + 2] {
                b'A' => Some(0x10), // up (Ctrl-P equivalent)
                b'B' => Some(0x0e), // down (Ctrl-N equivalent)
                _ => None,
            };
            if let Some(key) = arrow {
                if let Some(outcome) = state.handle_key(key) {
                    return Some(outcome);
                }
                i += 3;
                continue;
            }
        }
        if let Some(outcome) = state.handle_key(bytes[i]) {
            return Some(outcome);
        }
        i += 1;
    }
    None
}

pub async fn send_client_msg<W>(writer: &mut W, msg: &ClientMsg) -> Result<(), ClientError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = postcard::to_allocvec(msg).map_err(|e| CodecError::Encode(e.to_string()))?;
    Codec::write_frame(writer, &bytes).await?;
    Ok(())
}

/// Send the initial `AttachOrCreate` request and wait for `Attached`.
///
/// Returns the ATTACHED session's real name from the `Attached` reply, not
/// the requested `name` — the daemon picks one when `name` is `None`, so the
/// reply is the only reliable source (used by `run`'s follow handling to know
/// what just ended, and by a follow-then-pick's picker to seed `current`).
pub async fn handshake_spawn<R, W>(
    reader: &mut R,
    writer: &mut W,
    name: Option<String>,
    create_if_missing: CreatePolicy,
    spec: Option<plexy_glass_protocol::SpawnSpec>,
    size: PtySize,
) -> Result<String, ClientError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    send_client_msg(
        writer,
        &ClientMsg::AttachOrCreate {
            name,
            create_if_missing,
            cmd: spec,
            size,
        },
    )
    .await?;
    let frame = Codec::read_frame(reader)
        .await?
        .ok_or_else(|| ClientError::Io(io::Error::other("daemon closed before Attached")))?;
    let msg: ServerMsg =
        postcard::from_bytes(&frame).map_err(|e| CodecError::Decode(e.to_string()))?;
    match msg {
        ServerMsg::Attached { session_name, .. } => Ok(session_name),
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        other => Err(ClientError::Io(io::Error::other(format!(
            "expected Attached, got {other:?}"
        )))),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};
    use std::{env, fs};

    use bytes::Bytes;
    use plexy_glass_daemon::RuntimePaths;
    use plexy_glass_protocol::{ExitStatus, ServerMsg, SessionEntry, server_handshake};
    use tokio::io::{duplex, split};
    use tokio::net::UnixListener;
    use tokio::sync::oneshot;
    use tokio::{task, time};

    use super::*;

    fn test_size() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[tokio::test]
    async fn pump_writes_output_to_stdout_and_follows_on_exited() {
        let (mut server_w, mut client_r) = duplex(64 * 1024);
        let (server_r, mut client_w) = duplex(64 * 1024);
        drop(server_r); // we don't read from the client in this test
        let (stdin_w, mut stdin_r) = duplex(64);
        drop(stdin_w);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        // Server-side: emit one `Output` and then the session-ended marker.
        let server = tokio::spawn(async move {
            let out = ServerMsg::Output(Bytes::from_static(b"abc"));
            let bytes = postcard::to_allocvec(&out).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
            let done = ServerMsg::Exited {
                status: ExitStatus::Unknown,
            };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            test_size(),
            &mut resize_rx,
            &Target::default(),
            false,
            "main",
        )
        .await
        .unwrap();
        assert!(matches!(status, PumpExit::Follow), "got: {status:?}");

        // `pump` now borrows `stdout_w`, so it no longer drops on return; close it
        // here so `read_to_end` sees EOF instead of blocking to the timeout.
        drop(stdout_w);
        let mut out = Vec::new();
        let _ = time::timeout(Duration::from_millis(200), stdout_r.read_to_end(&mut out)).await;
        assert_eq!(&out, b"abc");

        server.await.unwrap();
    }

    #[tokio::test]
    async fn pump_output_survives_interleaved_stdin() {
        // Regression: `Codec::read_frame` is read_exact-based and NOT
        // cancel-safe. A daemon Output frame split across many socket reads must
        // not be corrupted by stdin bytes arriving mid-frame and winning the
        // `select!` race. A tiny daemon->client pipe (16 bytes) forces the 8 KiB
        // payload to fragment into hundreds of reads; a feeder hammers stdin
        // throughout. The full payload must still reach stdout intact.
        let (mut server_w, mut client_r) = duplex(16);
        let (server_r, mut client_w) = duplex(64 * 1024);
        let (mut stdin_w, mut stdin_r) = duplex(64 * 1024);
        let (mut stdout_r, mut stdout_w) = duplex(256 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        let payload = vec![b'x'; 8192];

        // Drain client->daemon so stdin-forwarding never back-pressures.
        let drain = tokio::spawn(async move {
            let mut r = server_r;
            let mut buf = vec![0u8; 4096];
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
        });

        // Hammer stdin so its select arm is frequently ready while the daemon
        // Output frame is mid-transit. Ends when pump returns and drops stdin_r.
        let feeder = tokio::spawn(async move {
            loop {
                if stdin_w.write_all(b"k").await.is_err() {
                    break;
                }
                task::yield_now().await;
            }
        });

        let srv_payload = payload.clone();
        let server = tokio::spawn(async move {
            let out = ServerMsg::Output(Bytes::from(srv_payload));
            let bytes = postcard::to_allocvec(&out).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
            // Bare EOF (not the Exited marker): this test's intent is data
            // integrity across the payload, so drive a clean-exit end
            // rather than a follow.
            drop(server_w);
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            test_size(),
            &mut resize_rx,
            &Target::default(),
            false,
            "main",
        )
        .await
        .expect("pump must not error on interleaved stdin");
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Unknown)),
            "got: {status:?}"
        );

        // `pump` borrows its IO now, so it drops nothing on return. Close the
        // ends the helper tasks wait on: the stdout write end so `read_to_end`
        // sees EOF, the stdin read end so the feeder's `write_all` errors and its
        // loop ends, and the client->daemon write end so the `drain` task reading
        // the daemon side sees EOF and returns.
        drop(stdout_w);
        drop(stdin_r);
        drop(client_w);
        let mut out = Vec::new();
        time::timeout(Duration::from_secs(5), stdout_r.read_to_end(&mut out))
            .await
            .expect("stdout read timed out")
            .expect("stdout read failed");
        assert_eq!(
            out, payload,
            "daemon->client stream corrupted by mid-frame stdin"
        );

        let _ = feeder.await;
        let _ = server.await;
        let _ = drain.await;
    }

    /// `--remote-bin` is a path on ONE host, so it must follow you back to that
    /// same host through the picker — and must never be handed to a different
    /// one. The Reconnect arm used to hardcode `remote_bin: None`, so the flag
    /// silently stopped applying the moment you went through the picker at all.
    #[test]
    fn resolve_target_keeps_remote_bin_for_the_same_host_and_drops_it_for_others() {
        let current = Target {
            host: Host::Remote(RemoteName::from("wsl2")),
            remote_bin: Some("/opt/pg".to_string()),
            install: InstallPolicy::UseExisting,
        };

        // Same host: the path still applies, and the picker's `i` toggle wins.
        let same = resolve_target(
            &Host::Remote(RemoteName::from("wsl2")),
            &current,
            InstallPolicy::Provision,
        );
        assert_eq!(same.remote_bin.as_deref(), Some("/opt/pg"));
        assert_eq!(same.install, InstallPolicy::Provision);

        // A different remote: `/opt/pg` is a path on wsl2 and means nothing here.
        let other = resolve_target(
            &Host::Remote(RemoteName::from("prod")),
            &current,
            InstallPolicy::UseExisting,
        );
        assert_eq!(other.host, Host::Remote(RemoteName::from("prod")));
        assert_eq!(other.remote_bin, None);

        // The local daemon runs no remote binary at all.
        let local = resolve_target(&Host::Local, &current, InstallPolicy::UseExisting);
        assert_eq!(local.host, Host::Local);
        assert_eq!(local.remote_bin, None);
    }

    #[test]
    fn scan_outer_events_does_not_reassemble_split_sequences() {
        // Per-chunk scan with no cross-call state: a focus sequence split across
        // two calls is NOT recognized; the bytes pass through as ordinary input.
        // Pins the current behavior (terminals emit these atomically).
        let mut first = b"abc\x1b[".to_vec();
        assert!(super::scan_outer_events(&mut first).is_empty());
        assert_eq!(
            first, b"abc\x1b[",
            "incomplete trailing sequence passes through"
        );
        let mut second = b"Idef".to_vec();
        assert!(super::scan_outer_events(&mut second).is_empty());
        assert_eq!(
            second, b"Idef",
            "the split head is not recognized as an event"
        );
    }

    #[test]
    fn scan_outer_events_decodes_focus_and_color_scheme() {
        // Interleaved with ordinary input; we must extract the three control
        // messages and leave the rest as raw input bytes.
        let mut input = b"a\x1b[Ib\x1b[O\x1b[?997;1nc\x1b[?997;2nd".to_vec();
        let events = super::scan_outer_events(&mut input);
        assert_eq!(
            events,
            vec![
                OuterEvent::FocusIn,
                OuterEvent::FocusOut,
                OuterEvent::ColorScheme(plexy_glass_protocol::ColorScheme::Dark),
                OuterEvent::ColorScheme(plexy_glass_protocol::ColorScheme::Light),
            ]
        );
        // The control sequences are stripped; ordinary bytes survive in order.
        assert_eq!(input, b"abcd");
    }

    fn picker_entry(name: &str, clients: u8) -> SessionEntry {
        SessionEntry {
            name: name.into(),
            windows: 1,
            panes: 1,
            clients,
            created: SystemTime::now(),
            last_active: SystemTime::now(),
        }
    }

    /// Read until `buf` contains `needle` or a single read stalls past
    /// `per_read`. The picker's render is one `write_all` call on the daemon
    /// side, so in practice this returns after the first read; the loop is
    /// just insurance against the duplex pipe splitting it.
    async fn read_until_contains<R>(r: &mut R, needle: &str, per_read: Duration) -> Vec<u8>
    where
        R: AsyncRead + Unpin,
    {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = time::timeout(per_read, r.read(&mut chunk))
                .await
                .expect("read timed out waiting for picker render")
                .expect("stdout read failed");
            assert_ne!(n, 0, "stdout closed before {needle:?} appeared");
            buf.extend_from_slice(&chunk[..n]);
            if String::from_utf8_lossy(&buf).contains(needle) {
                return buf;
            }
        }
    }

    #[tokio::test]
    async fn pump_picker_reselect_current_session_sends_redraw() {
        // Regression for the same-session-reselect bug: the picker clears the
        // screen (`\x1b[2J\x1b[H`) when it opens, and nothing else repaints it
        // on an idle session. Reselecting the already-attached session (the
        // cursor starts there, so a bare Enter does this) must still send
        // `ClientMsg::Redraw` so the daemon re-emits a full frame over the
        // cleared screen. Before the fix, this arm sent nothing at all.
        let (mut server_w, mut client_r) = duplex(64 * 1024);
        let (mut server_r, mut client_w) = duplex(64 * 1024);
        let (mut stdin_w, mut stdin_r) = duplex(64);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        let driver = tokio::spawn(async move {
            let open = ServerMsg::OpenSessionPicker {
                sessions: vec![picker_entry("main", 1)],
                current: "main".into(),
            };
            let bytes = postcard::to_allocvec(&open).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();

            let rendered = read_until_contains(&mut stdout_r, "main", Duration::from_secs(1)).await;
            assert!(
                String::from_utf8_lossy(&rendered).contains("plexy-glass"),
                "picker did not render"
            );

            // Enter with the cursor on the current session -> reselect.
            stdin_w.write_all(b"\r").await.unwrap();

            let frame = time::timeout(Duration::from_secs(2), Codec::read_frame(&mut server_r))
                .await
                .expect("timed out waiting for a ClientMsg")
                .unwrap()
                .expect("daemon channel closed before a ClientMsg arrived");
            let msg: ClientMsg = postcard::from_bytes(&frame).unwrap();
            assert_eq!(msg, ClientMsg::Redraw);

            // Let pump return: bare EOF, not the Exited marker — this test's
            // intent is the Redraw-on-reselect behavior, not follow.
            drop(server_w);
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            test_size(),
            &mut resize_rx,
            &Target::default(),
            false,
            "main",
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Unknown)),
            "got: {status:?}"
        );

        driver.await.unwrap();
    }

    #[tokio::test]
    async fn pump_picker_esc_sends_redraw() {
        // Esc cancels the picker; the pump must send `ClientMsg::Redraw` so
        // the daemon repaints over the picker's own screen clear.
        let (mut server_w, mut client_r) = duplex(64 * 1024);
        let (mut server_r, mut client_w) = duplex(64 * 1024);
        let (mut stdin_w, mut stdin_r) = duplex(64);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        let driver = tokio::spawn(async move {
            let open = ServerMsg::OpenSessionPicker {
                sessions: vec![picker_entry("main", 1), picker_entry("build", 0)],
                current: "main".into(),
            };
            let bytes = postcard::to_allocvec(&open).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();

            read_until_contains(&mut stdout_r, "build", Duration::from_secs(1)).await;

            stdin_w.write_all(b"\x1b").await.unwrap();

            let frame = time::timeout(Duration::from_secs(2), Codec::read_frame(&mut server_r))
                .await
                .expect("timed out waiting for a ClientMsg")
                .unwrap()
                .expect("daemon channel closed before a ClientMsg arrived");
            let msg: ClientMsg = postcard::from_bytes(&frame).unwrap();
            assert_eq!(msg, ClientMsg::Redraw);

            // Bare EOF, not the Exited marker — this test's intent is the
            // Esc-cancel Redraw behavior, not follow.
            drop(server_w);
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            test_size(),
            &mut resize_rx,
            &Target::default(),
            false,
            "main",
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Unknown)),
            "got: {status:?}"
        );

        driver.await.unwrap();
    }

    // --- Task 5: daemon-set assembly + the streaming query drain ---

    #[test]
    fn build_picker_rows_local_current_anchors_and_tags_sessions() {
        // Attached-local: the current daemon is a Live local Host anchor, its
        // sessions ride under it tagged local (host None), and the roster's
        // remotes become Pending anchors to query. Local is NOT queried.
        roster::set_test_roster(vec!["prod".into()], vec!["scratch".into()]);
        let a = build_picker_rows(
            vec![picker_entry("main", 1), picker_entry("build", 0)],
            &Host::Local,
        );

        assert_eq!(a.rows[0].kind, RowKind::Host, "current-daemon anchor first");
        assert_eq!(a.rows[0].host, Host::Local, "local anchor");
        assert_eq!(a.rows[0].status, RowStatus::Live);
        assert_eq!(a.rows[1].name, "main");
        assert_eq!(a.rows[1].kind, RowKind::Session);
        assert_eq!(a.rows[1].host, Host::Local, "current session tagged local");

        assert!(!a.query_local, "attached-local does not query local again");
        assert_eq!(
            a.remote_hosts,
            vec![RemoteName::from("prod"), RemoteName::from("scratch")]
        );
        assert_eq!(a.adhoc, vec!["scratch".to_string()]);
        let prod = a
            .rows
            .iter()
            .find(|r| r.name == "prod")
            .expect("prod anchor");
        assert_eq!(prod.kind, RowKind::Host);
        assert_eq!(prod.status, RowStatus::Pending);
    }

    #[test]
    fn build_picker_rows_remote_current_excludes_it_and_queries_local() {
        // Attached to "prod": prod is the current anchor and is EXCLUDED from the
        // query set; the local daemon becomes an OTHER daemon (queried), and
        // "dev" stays a remote to query.
        roster::set_test_roster(vec!["prod".into(), "dev".into()], vec![]);
        let a = build_picker_rows(
            vec![picker_entry("api", 1)],
            &Host::Remote(RemoteName::from("prod")),
        );

        assert_eq!(
            a.rows[0].host,
            Host::Remote(RemoteName::from("prod")),
            "remote current anchor"
        );
        assert_eq!(a.rows[0].kind, RowKind::Host);
        assert!(a.query_local, "attached-remote queries the local daemon");
        assert_eq!(
            a.remote_hosts,
            vec![RemoteName::from("dev")],
            "prod (current) excluded"
        );
        let locals: Vec<_> = a.rows.iter().filter(|r| r.host.is_local()).collect();
        assert_eq!(locals.len(), 1, "one local-other anchor");
        assert_eq!(locals[0].kind, RowKind::Host);
        assert_eq!(locals[0].status, RowStatus::Pending);
    }

    #[test]
    fn build_roster_rows_synthesizes_no_connect_slot() {
        // The `＋ Connect to a host…` affordance is render-only now: the roster
        // stores only real host/session rows (no empty-labeled sentinel), so a
        // duplicate or mid-list `＋` is unrepresentable.
        roster::set_test_roster(vec!["prod".into()], vec![]);
        let a = build_picker_rows(vec![], &Host::Local);
        assert!(
            a.rows.iter().all(|r| !r.label.is_empty()),
            "no fabricated empty-labeled slot row"
        );
        assert_eq!(
            a.rows.last().map(|r| r.name.clone()),
            Some("prod".to_string()),
            "the last row is a real host, not a synthesized slot"
        );
    }

    #[tokio::test]
    async fn pump_picker_streams_roster_and_marks_unreachable() {
        // Seed one configured remote with no reachable daemon. Attached-local, so
        // the current daemon's sessions come from the payload (tagged local); the
        // roster host is stream-queried and — being unresolvable (`.invalid`) or
        // daemon-less — resolves Unreachable, updating its row in place. Proves
        // both the section assembly and the streaming drain re-render.
        roster::set_test_roster(vec!["nonexistent.invalid".into()], vec![]);

        let (mut server_w, mut client_r) = duplex(64 * 1024);
        let (mut server_r, mut client_w) = duplex(64 * 1024);
        let (mut stdin_w, mut stdin_r) = duplex(64);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        let driver = tokio::spawn(async move {
            let open = ServerMsg::OpenSessionPicker {
                sessions: vec![picker_entry("main", 1)],
                current: "main".into(),
            };
            let bytes = postcard::to_allocvec(&open).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();

            // The unreachable glyph (⚠) appears only AFTER the query resolves, so
            // waiting on it proves the streaming drain re-rendered the row.
            let rendered =
                read_until_contains(&mut stdout_r, "\u{26a0}", Duration::from_secs(8)).await;
            let text = String::from_utf8_lossy(&rendered);
            assert!(text.contains("plexy-glass"), "picker rendered");
            assert!(
                text.contains("main"),
                "current daemon's session, tagged local"
            );
            assert!(
                text.contains("nonexistent.invalid"),
                "the configured host anchor appears"
            );

            // Esc closes the picker → Redraw; then let pump return.
            stdin_w.write_all(b"\x1b").await.unwrap();
            let frame = time::timeout(Duration::from_secs(2), Codec::read_frame(&mut server_r))
                .await
                .expect("timed out waiting for a ClientMsg")
                .unwrap()
                .expect("daemon channel closed before a ClientMsg arrived");
            let msg: ClientMsg = postcard::from_bytes(&frame).unwrap();
            assert_eq!(msg, ClientMsg::Redraw);

            // Bare EOF, not the Exited marker — this test's intent is the
            // streaming-query behavior, not follow.
            drop(server_w);
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            test_size(),
            &mut resize_rx,
            &Target::default(),
            false,
            "main",
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Unknown)),
            "got: {status:?}"
        );

        driver.await.unwrap();
    }

    // --- Task 6: connect (reconnect / new-on-host / forget) ---

    #[tokio::test]
    async fn pump_picker_switch_to_different_session_sends_switch_session() {
        // Baseline (unchanged from Milestone A): Enter on a DIFFERENT local
        // session sends a real `SwitchSession`, not just a `Redraw` (that's only
        // the same-session-reselect case, covered above).
        let (mut server_w, mut client_r) = duplex(64 * 1024);
        let (mut server_r, mut client_w) = duplex(64 * 1024);
        let (mut stdin_w, mut stdin_r) = duplex(64);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        let driver = tokio::spawn(async move {
            let open = ServerMsg::OpenSessionPicker {
                sessions: vec![picker_entry("main", 1), picker_entry("build", 0)],
                current: "main".into(),
            };
            let bytes = postcard::to_allocvec(&open).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();

            read_until_contains(&mut stdout_r, "build", Duration::from_secs(1)).await;

            // Cursor starts on "main" (the current session); move to "build".
            stdin_w.write_all(b"\x0e").await.unwrap();
            stdin_w.write_all(b"\r").await.unwrap();

            let frame = time::timeout(Duration::from_secs(2), Codec::read_frame(&mut server_r))
                .await
                .expect("timed out waiting for a ClientMsg")
                .unwrap()
                .expect("daemon channel closed before a ClientMsg arrived");
            let msg: ClientMsg = postcard::from_bytes(&frame).unwrap();
            assert_eq!(
                msg,
                ClientMsg::SwitchSession {
                    name: "build".into()
                }
            );

            // Bare EOF, not the Exited marker — this test's intent is the
            // SwitchSession-on-select behavior, not follow.
            drop(server_w);
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            test_size(),
            &mut resize_rx,
            &Target::default(),
            false,
            "main",
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Unknown)),
            "got: {status:?}"
        );

        driver.await.unwrap();
    }

    #[tokio::test]
    async fn pump_picker_reconnect_to_other_host_anchor_normalizes_empty_name() {
        // Critical #1 + Finding 2 through the pump: Enter on a DIFFERENT
        // daemon's host anchor reconnects to that daemon's default session.
        // `accept()` emits an empty `name` for a host anchor; the pump must
        // NORMALIZE it to `"main"` (never send `Some("")`, which the daemon
        // rejects with `EmptyName` and ejects the client). Attached-local, so
        // the roster's `nonexistent.invalid` host is an OTHER daemon whose
        // Pending anchor renders on the first paint (no query round-trip needed
        // to select it).
        roster::set_test_roster(vec!["nonexistent.invalid".into()], vec![]);

        let (mut server_w, mut client_r) = duplex(64 * 1024);
        let (server_r, mut client_w) = duplex(64 * 1024);
        drop(server_r); // no ClientMsg is expected on this path
        let (mut stdin_w, mut stdin_r) = duplex(64);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        // Same race note as `pump_picker_new_on_host_returns_reconnect_to`:
        // `ReconnectTo` returns without a daemon round-trip, so keep the driver
        // parked in `pending()` after its writes rather than letting a completing
        // task drop `server_w`/`stdin_w` out from under `pump`.
        let driver = async {
            let open = ServerMsg::OpenSessionPicker {
                sessions: vec![picker_entry("main", 1)],
                current: "main".into(),
            };
            let bytes = postcard::to_allocvec(&open).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();

            read_until_contains(&mut stdout_r, "nonexistent.invalid", Duration::from_secs(1)).await;
            // Cursor parks on "main"; one down-move lands on the host anchor.
            stdin_w.write_all(b"\x0e").await.unwrap();
            stdin_w.write_all(b"\r").await.unwrap();
            pending::<()>().await;
        };

        let local = Target::default();
        let status = tokio::select! {
            status = pump(
                &mut client_r,
                &mut client_w,
                &mut stdin_r,
                &mut stdout_w,
                test_size(),
                &mut resize_rx,
                &local,
                false,
                "main",
            ) => status.unwrap(),
            () = driver => unreachable!("driver parks forever after its writes"),
        };
        match status {
            PumpExit::ReconnectTo { target: t, name } => {
                assert_eq!(t.host.remote().map(|n| &**n), Some("nonexistent.invalid"));
                assert_eq!(
                    name, "main",
                    "empty host-anchor name normalized to the daemon default"
                );
            }
            other => panic!("expected ReconnectTo, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pump_picker_new_on_host_returns_reconnect_to() {
        // `n` on a remote host's anchor row opens the new-session prompt;
        // committing it must hand `New{host,name}` back as `ReconnectTo`, same
        // as `Reconnect` — the global `create_if_missing` (CreateIfMissing) is what
        // actually creates the fresh session once the outer loop re-attaches.
        roster::set_test_roster(vec!["nonexistent.invalid".into()], vec![]);

        let (mut server_w, mut client_r) = duplex(64 * 1024);
        let (server_r, mut client_w) = duplex(64 * 1024);
        drop(server_r);
        let (mut stdin_w, mut stdin_r) = duplex(64);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        // Same race as `pump_picker_reconnect_returns_reconnect_to`: `New`
        // returns without a daemon round-trip, so keep `server_w`/`stdin_w`
        // open until `pump` itself resolves rather than letting a completing
        // driver task drop them out from under it.
        let driver = async {
            let open = ServerMsg::OpenSessionPicker {
                sessions: vec![picker_entry("main", 1)],
                current: "main".into(),
            };
            let bytes = postcard::to_allocvec(&open).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();

            read_until_contains(&mut stdout_r, "nonexistent.invalid", Duration::from_secs(1)).await;

            // Cursor starts on "main"; one down-move lands on the host anchor.
            stdin_w.write_all(b"\x0e").await.unwrap();
            stdin_w.write_all(b"n").await.unwrap();
            stdin_w.write_all(b"fresh").await.unwrap();
            stdin_w.write_all(b"\r").await.unwrap();
            pending::<()>().await;
        };

        let local = Target::default();
        let status = tokio::select! {
            status = pump(
                &mut client_r,
                &mut client_w,
                &mut stdin_r,
                &mut stdout_w,
                test_size(),
                &mut resize_rx,
                &local,
                false,
                "main",
            ) => status.unwrap(),
            () = driver => unreachable!("driver parks forever after its writes"),
        };
        match status {
            PumpExit::ReconnectTo { target: t, name } => {
                assert_eq!(t.host.remote().map(|n| &**n), Some("nonexistent.invalid"));
                assert_eq!(name, "fresh");
            }
            other => panic!("expected ReconnectTo, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pump_picker_forget_removes_host_and_stays_in_picker() {
        // `x` on an ad-hoc host row rewrites the roster file and rebuilds the
        // picker's other-daemon rows in place — WITHOUT returning. Prove both:
        // the forgotten host's row is gone from the next render, and the picker
        // is still live afterward (Esc still cancels it normally).
        roster::set_test_roster(vec![], vec!["nonexistent.invalid".into()]);

        let (mut server_w, mut client_r) = duplex(64 * 1024);
        let (mut server_r, mut client_w) = duplex(64 * 1024);
        let (mut stdin_w, mut stdin_r) = duplex(64);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        let driver = tokio::spawn(async move {
            let open = ServerMsg::OpenSessionPicker {
                sessions: vec![picker_entry("main", 1)],
                current: "main".into(),
            };
            let bytes = postcard::to_allocvec(&open).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();

            let first =
                read_until_contains(&mut stdout_r, "nonexistent.invalid", Duration::from_secs(1))
                    .await;
            assert!(String::from_utf8_lossy(&first).contains("(ad-hoc)"));

            // Cursor starts on "main"; one down-move lands on the host anchor.
            // `x` on the ad-hoc host is a Navigate action now (no filter gate).
            stdin_w.write_all(b"\x0e").await.unwrap();
            stdin_w.write_all(b"x").await.unwrap();

            // Wait for a repaint after the forget. The Navigate footer always
            // carries `install:` (the prompt line no longer says `filter:` when
            // the filter is empty under the explicit-filter model), so it's the
            // stable per-render marker to read up to.
            let second =
                read_until_contains(&mut stdout_r, "install:", Duration::from_secs(1)).await;
            assert!(
                !String::from_utf8_lossy(&second).contains("nonexistent.invalid"),
                "forgotten host's row is gone from the rebuilt rows"
            );

            // Still in the picker: Esc cancels normally and sends Redraw.
            stdin_w.write_all(b"\x1b").await.unwrap();
            let frame = time::timeout(Duration::from_secs(2), Codec::read_frame(&mut server_r))
                .await
                .expect("timed out waiting for a ClientMsg")
                .unwrap()
                .expect("daemon channel closed before a ClientMsg arrived");
            let msg: ClientMsg = postcard::from_bytes(&frame).unwrap();
            assert_eq!(msg, ClientMsg::Redraw);

            // Bare EOF, not the Exited marker — this test's intent is the
            // forget/rebuild behavior, not follow.
            drop(server_w);
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            test_size(),
            &mut resize_rx,
            &Target::default(),
            false,
            "main",
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Unknown)),
            "got: {status:?}"
        );

        // Confirm the roster hook itself is gone, not just the rendered row.
        assert_eq!(roster::load_adhoc(), Vec::<RemoteName>::new());

        driver.await.unwrap();
    }

    // --- Task 7: `k` + `y` kills a session from the picker ---

    #[tokio::test]
    async fn pump_picker_kill_noncurrent_session_sends_kill_session_and_drops_the_row() {
        // `k` + `y` on a NON-current session row sends a one-off `KillSession`
        // on a FRESH connection (`KillSession` is only accepted as a
        // connection's first message) to the resolved daemon — here the same
        // local daemon, so a stub bound at `PLEXY_GLASS_DIR`'s socket answers
        // it, same pattern as `pump_opens_picker_after_attach_when_flag_is_set`.
        // On success the row is dropped locally and the picker stays open.
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process, so there is no
        // cross-test race on this env var.
        unsafe { env::set_var("PLEXY_GLASS_DIR", tmp.path()) };
        let paths = RuntimePaths::for_current_user().unwrap();
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let listener = UnixListener::bind(&paths.socket).unwrap();
        let stub = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = split(stream);
            server_handshake(&mut r, &mut w, 4242).await.unwrap();
            let frame = Codec::read_frame(&mut r).await.unwrap().unwrap();
            let msg: ClientMsg = postcard::from_bytes(&frame).unwrap();
            assert_eq!(
                msg,
                ClientMsg::KillSession {
                    name: "build".into()
                }
            );
            let reply = ServerMsg::SessionKilled {
                name: "build".into(),
            };
            Codec::write_frame(&mut w, &postcard::to_allocvec(&reply).unwrap())
                .await
                .unwrap();
        });

        let (mut server_w, mut client_r) = duplex(64 * 1024);
        let (mut server_r, mut client_w) = duplex(64 * 1024);
        let (mut stdin_w, mut stdin_r) = duplex(64);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        let driver = tokio::spawn(async move {
            let open = ServerMsg::OpenSessionPicker {
                sessions: vec![picker_entry("main", 1), picker_entry("build", 0)],
                current: "main".into(),
            };
            let bytes = postcard::to_allocvec(&open).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();

            read_until_contains(&mut stdout_r, "build", Duration::from_secs(1)).await;

            // Cursor starts on "main" (the current session); move to "build"
            // and kill it.
            stdin_w.write_all(b"\x0e").await.unwrap();
            stdin_w.write_all(b"k").await.unwrap();
            stdin_w.write_all(b"y").await.unwrap();

            // Wait for the repaint after the kill; "build" must be gone but
            // the picker itself is still up (the stable `install:` marker).
            let second =
                read_until_contains(&mut stdout_r, "install:", Duration::from_secs(2)).await;
            assert!(
                !String::from_utf8_lossy(&second).contains("build"),
                "killed row is gone from the rebuilt rows"
            );

            // Still in the picker: Esc cancels normally and sends Redraw.
            stdin_w.write_all(b"\x1b").await.unwrap();
            let frame = time::timeout(Duration::from_secs(2), Codec::read_frame(&mut server_r))
                .await
                .expect("timed out waiting for a ClientMsg")
                .unwrap()
                .expect("daemon channel closed before a ClientMsg arrived");
            let msg: ClientMsg = postcard::from_bytes(&frame).unwrap();
            assert_eq!(msg, ClientMsg::Redraw);

            // Bare EOF, not the Exited marker — this test's intent is the
            // kill/drop-row behavior, not follow (that's covered below).
            drop(server_w);
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            test_size(),
            &mut resize_rx,
            &Target::default(),
            false,
            "main",
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Unknown)),
            "got: {status:?}"
        );

        driver.await.unwrap();
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn pump_picker_kill_current_session_lets_the_exited_arm_follow() {
        // Killing the CURRENT session still round-trips the one-off
        // `KillSession` over its own fresh connection, but the pump does
        // nothing special on success (no special-casing by name) — it's the
        // daemon tearing down the PRIMARY connection separately, whose
        // renderer writes the `Exited` marker (Task 4), that the pump turns
        // into `PumpExit::Follow`. Proves the two features compose: handling
        // `Kill` never short-circuits the follow.
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process, so there is no
        // cross-test race on this env var.
        unsafe { env::set_var("PLEXY_GLASS_DIR", tmp.path()) };
        let paths = RuntimePaths::for_current_user().unwrap();
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let listener = UnixListener::bind(&paths.socket).unwrap();
        // In production the `Exited` marker is a CONSEQUENCE of the daemon
        // killing the session, so it can never precede the kill. Model that
        // ordering: the stub signals once it has processed the `KillSession`,
        // and the driver only emits `Exited` after that signal. Without it the
        // pump can follow on `Exited` before it ever sends the kill, and the
        // stub's `accept()` then blocks forever.
        let (kill_done_tx, kill_done_rx) = oneshot::channel::<()>();
        let stub = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = split(stream);
            server_handshake(&mut r, &mut w, 4242).await.unwrap();
            let frame = Codec::read_frame(&mut r).await.unwrap().unwrap();
            let msg: ClientMsg = postcard::from_bytes(&frame).unwrap();
            assert_eq!(
                msg,
                ClientMsg::KillSession {
                    name: "main".into()
                }
            );
            let reply = ServerMsg::SessionKilled {
                name: "main".into(),
            };
            Codec::write_frame(&mut w, &postcard::to_allocvec(&reply).unwrap())
                .await
                .unwrap();
            let _ = kill_done_tx.send(());
        });

        let (mut server_w, mut client_r) = duplex(64 * 1024);
        let (server_r, mut client_w) = duplex(64 * 1024);
        drop(server_r); // no ClientMsg is expected on the primary connection
        let (mut stdin_w, mut stdin_r) = duplex(64);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        let driver = tokio::spawn(async move {
            let open = ServerMsg::OpenSessionPicker {
                sessions: vec![picker_entry("main", 1)],
                current: "main".into(),
            };
            let bytes = postcard::to_allocvec(&open).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();

            read_until_contains(&mut stdout_r, "main", Duration::from_secs(1)).await;

            // Cursor starts on "main" already (the current session).
            stdin_w.write_all(b"k").await.unwrap();
            stdin_w.write_all(b"y").await.unwrap();

            // Only now that the kill has round-tripped (in production the daemon
            // has torn the session down) does the renderer emit the Exited
            // marker on the primary connection.
            kill_done_rx.await.unwrap();
            let done = ServerMsg::Exited {
                status: ExitStatus::Unknown,
            };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            test_size(),
            &mut resize_rx,
            &Target::default(),
            false,
            "main",
        )
        .await
        .unwrap();
        assert!(matches!(status, PumpExit::Follow), "got: {status:?}");

        driver.await.unwrap();
        stub.await.unwrap();
    }

    // --- Follow-then-pick: open the picker after attach (>=2 remaining
    // sessions) ---

    #[tokio::test]
    async fn pump_opens_picker_after_attach_when_flag_is_set() {
        // `open_picker_after_attach` makes `pump` issue its OWN `ListSessions`
        // round trip on a fresh connection (`current_target`, `Connect::Only`)
        // before it ever reads a frame off the attached connection — the
        // daemon dispatches `ListSessions` only as a connection's first
        // message, so this can't ride the primary duplex pair below. Point
        // `PLEXY_GLASS_DIR` at a private tempdir and bind a minimal stub
        // there to answer it.
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: no other test in this crate reads or writes
        // `PLEXY_GLASS_DIR`, and nextest runs each test in its own process,
        // so there is no cross-test race to guard against.
        unsafe { env::set_var("PLEXY_GLASS_DIR", tmp.path()) };
        let paths = RuntimePaths::for_current_user().unwrap();
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let listener = UnixListener::bind(&paths.socket).unwrap();
        let stub = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = split(stream);
            server_handshake(&mut r, &mut w, 4242).await.unwrap();
            let frame = Codec::read_frame(&mut r).await.unwrap().unwrap();
            let msg: ClientMsg = postcard::from_bytes(&frame).unwrap();
            assert_eq!(msg, ClientMsg::ListSessions);
            let reply = ServerMsg::SessionList {
                entries: vec![picker_entry("main", 1), picker_entry("build", 0)],
            };
            Codec::write_frame(&mut w, &postcard::to_allocvec(&reply).unwrap())
                .await
                .unwrap();
        });

        // The PRIMARY attached connection: a plain in-memory duplex, untouched
        // by the stub above. Nothing is ever written on `server_w` — it's only
        // dropped, below, to end `pump` with a bare EOF.
        let (server_w, mut client_r) = duplex(64 * 1024);
        let (mut server_r, mut client_w) = duplex(64 * 1024);
        let (mut stdin_w, mut stdin_r) = duplex(64);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        let pump_task = tokio::spawn(async move {
            pump(
                &mut client_r,
                &mut client_w,
                &mut stdin_r,
                &mut stdout_w,
                test_size(),
                &mut resize_rx,
                &Target::default(),
                true,
                "main",
            )
            .await
        });

        // The picker renders before `pump` reads anything off the primary
        // connection, seeded with BOTH sessions and cursor parked on "main"
        // (the attached one).
        let rendered = read_until_contains(&mut stdout_r, "build", Duration::from_secs(2)).await;
        let text = String::from_utf8_lossy(&rendered);
        assert!(text.contains("main"), "the attached session is listed");
        assert!(text.contains("build"), "the other session is listed");

        // Esc closes the picker -> Redraw over the PRIMARY connection (the
        // stub above is done after its one reply and plays no further part).
        stdin_w.write_all(b"\x1b").await.unwrap();
        let frame = time::timeout(Duration::from_secs(2), Codec::read_frame(&mut server_r))
            .await
            .expect("timed out waiting for a ClientMsg")
            .unwrap()
            .expect("daemon channel closed before a ClientMsg arrived");
        let msg: ClientMsg = postcard::from_bytes(&frame).unwrap();
        assert_eq!(msg, ClientMsg::Redraw);

        // Bare EOF ends the pump.
        drop(server_w);
        let status = time::timeout(Duration::from_secs(2), pump_task)
            .await
            .expect("pump timed out")
            .unwrap()
            .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Unknown)),
            "got: {status:?}"
        );

        stub.await.unwrap();
    }
}
