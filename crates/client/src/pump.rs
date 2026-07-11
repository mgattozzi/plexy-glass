use std::future::pending;
use std::time::Duration;
use std::{io, str};

use bytes::BytesMut;
use plexy_glass_protocol::errors::CodecError;
use plexy_glass_protocol::{
    ClientMsg, Codec, ColorScheme, ExitStatus, PtySize, ServerMsg, SessionEntry,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::error::ClientError;
use crate::picker::{PickerOutcome, PickerRow, PickerState, RowKind, RowStatus};
use crate::query::{self, HostStatus};
use crate::roster::{self, RosterSource};
use crate::transport::{Connect, Target};

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
    /// The child exited or the daemon closed — the client should quit.
    Ended(ExitStatus),
    /// (Milestone B) the picker chose a session on a DIFFERENT daemon (or a new
    /// one on a host); the outer loop re-attaches (`lib.rs::run`'s `next =
    /// (reconnect_target, Some(reconnect_name))`). `target.host: None` reconnects
    /// on the LOCAL daemon; `create_if_missing` is globally `true`
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
/// when the child exits or the daemon closes; `ReconnectTo` is reserved for the
/// picker (Milestone B) and never produced here.
///
/// `current_target` is the daemon this pump is attached to (`&next.0` in
/// `run`); its `host` tags the current daemon's rows in the session picker
/// (`None` when truly local) and is the daemon EXCLUDED from the roster query.
pub async fn pump<R, W, In, Out>(
    daemon_read: &mut R,
    daemon_write: &mut W,
    stdin: &mut In,
    stdout: &mut Out,
    resize_rx: &mut mpsc::Receiver<PtySize>,
    current_target: &Target,
) -> Result<PumpExit, ClientError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    In: AsyncRead + Unpin,
    Out: AsyncWrite + Unpin,
{
    let mut stdin_buf = BytesMut::with_capacity(STDIN_CHUNK);
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
    let mut picker_rx: Option<mpsc::UnboundedReceiver<(Option<String>, HostStatus)>> = None;
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
                    ServerMsg::Exited { status } => {
                        return Ok(PumpExit::Ended(status));
                    }
                    ServerMsg::Error(e) => {
                        return Err(ClientError::DaemonError(e));
                    }
                    ServerMsg::OpenSessionPicker { sessions, current } => {
                        // Note: stdin typed in the round-trip window before this
                        // arrives is still forwarded to the pane (inherent to the
                        // client-rendered picker; the old daemon overlay switched
                        // synchronously and swallowed those keystrokes).
                        // Session-row labels match `open_session_picker_overlay`
                        // (crates/daemon/src/connection.rs) verbatim, so the
                        // client-rendered picker reads the same as the old
                        // daemon-rendered one.
                        //
                        // Assemble the daemon set: the current daemon (a Live
                        // Host anchor + its `sessions`, tagged by the real
                        // current host) plus every OTHER daemon in
                        // `{local} ∪ roster` MINUS current (a Pending anchor
                        // each). The picker opens IMMEDIATELY; the streaming
                        // query below fills the other daemons' rows in as each
                        // resolves.
                        let assembly =
                            build_picker_rows(sessions, current_target.host.as_deref());
                        let mut state = PickerState::new_with_current(
                            assembly.rows,
                            &current_target.host,
                            &current,
                        );
                        state.set_adhoc_hosts(assembly.adhoc);
                        stdout.write_all(&state.render()).await.map_err(ClientError::Io)?;
                        stdout.flush().await.map_err(ClientError::Io)?;
                        picker_rx =
                            Some(spawn_picker_query(assembly.remote_hosts, assembly.query_local));
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
            // Client -> daemon (stdin)
            n = stdin.read(&mut stdin_buf) => {
                let n = n.map_err(ClientError::Io)?;
                if n == 0 {
                    // stdin closed; we keep the session alive until the child exits.
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
                            send_client_msg(&mut *daemon_write, &ClientMsg::Redraw).await?;
                            picker_rx = None;
                        }
                        Some(
                            PickerOutcome::Reconnect { host, name }
                            | PickerOutcome::New { host, name },
                        ) => {
                            // Cross-daemon jump (Reconnect) or a brand-new
                            // session on a host (New): hand it back to the outer
                            // attach loop rather than acting on it here. `host:
                            // None` re-attaches on the LOCAL daemon; the global
                            // `create_if_missing = true` means an existing `name`
                            // attaches and a fresh one creates, so New needs no
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
                            return Ok(PumpExit::ReconnectTo {
                                target: Target {
                                    host,
                                    remote_bin: None,
                                    install: false,
                                },
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
                            let fresh = build_roster_rows(current_target.host.as_deref());
                            state.replace_other_rows(&current_target.host, fresh.rows, fresh.adhoc);
                            stdout
                                .write_all(&state.render())
                                .await
                                .map_err(ClientError::Io)?;
                            stdout.flush().await.map_err(ClientError::Io)?;
                            picker_rx =
                                Some(spawn_picker_query(fresh.remote_hosts, fresh.query_local));
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
            Some(size) = resize_rx.recv() => {
                send_client_msg(&mut *daemon_write, &ClientMsg::Resize(size)).await?;
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
                            let (row_status, rows) = resolve_status(host.as_deref(), hs);
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

/// The rows + query inputs assembled for one `OpenSessionPicker`.
struct PickerAssembly {
    rows: Vec<PickerRow>,
    /// Ad-hoc host names, for `PickerState::set_adhoc_hosts` (drives `x`→Forget
    /// and the render-time divider).
    adhoc: Vec<String>,
    /// The OTHER daemons' remote hosts to stream-query (roster minus current).
    remote_hosts: Vec<String>,
    /// Whether the local daemon is in the OTHER set (true only when we're
    /// attached to a REMOTE), so it gets queried on the local socket.
    query_local: bool,
}

/// Format one `SessionEntry` into a picker Session row tagged by `host`. The
/// label matches the daemon's old overlay verbatim.
fn session_row(e: SessionEntry, host: Option<String>) -> PickerRow {
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
fn build_picker_rows(sessions: Vec<SessionEntry>, current_host: Option<&str>) -> PickerAssembly {
    let mut rows = Vec::new();
    let anchor_name = current_host.map_or_else(|| "local".to_string(), String::from);
    rows.push(PickerRow {
        name: anchor_name.clone(),
        label: anchor_name,
        host: current_host.map(String::from),
        kind: RowKind::Host,
        status: RowStatus::Live,
    });
    for e in sessions {
        rows.push(session_row(e, current_host.map(String::from)));
    }

    let mut roster_rows = build_roster_rows(current_host);
    rows.append(&mut roster_rows.rows);

    PickerAssembly {
        rows,
        adhoc: roster_rows.adhoc,
        remote_hosts: roster_rows.remote_hosts,
        query_local: roster_rows.query_local,
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
    remote_hosts: Vec<String>,
    query_local: bool,
}

fn build_roster_rows(current_host: Option<&str>) -> RosterRows {
    let roster = roster::assemble(&roster::config_remotes(), &roster::load_adhoc());
    // The local daemon (host `None`) is an OTHER daemon only when we're attached
    // to a remote; attached-local, it IS the current daemon and is excluded.
    let query_local = current_host.is_some();
    let mut rows = Vec::new();
    if query_local {
        rows.push(PickerRow {
            name: "local".to_string(),
            label: "local".to_string(),
            host: None,
            kind: RowKind::Host,
            status: RowStatus::Pending,
        });
    }
    let mut remote_hosts = Vec::new();
    let mut adhoc = Vec::new();
    for h in roster {
        if current_host == Some(h.host.as_str()) {
            continue; // this IS the current daemon
        }
        if h.source == RosterSource::AdHoc {
            adhoc.push(h.host.clone());
        }
        rows.push(PickerRow {
            name: h.host.clone(),
            label: h.host.clone(),
            host: Some(h.host.clone()),
            kind: RowKind::Host,
            status: RowStatus::Pending,
        });
        remote_hosts.push(h.host);
    }

    RosterRows {
        rows,
        adhoc,
        remote_hosts,
        query_local,
    }
}

/// Map a resolved [`HostStatus`] to its picker row status + the Session rows to
/// splice under the host (empty unless `Live`).
fn resolve_status(host: Option<&str>, hs: HostStatus) -> (RowStatus, Vec<PickerRow>) {
    match hs {
        HostStatus::Live(entries) => (
            RowStatus::Live,
            entries
                .into_iter()
                .map(|e| session_row(e, host.map(String::from)))
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
    remote_hosts: Vec<String>,
    query_local: bool,
) -> mpsc::UnboundedReceiver<(Option<String>, HostStatus)> {
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
                if fwd.send((Some(host), status)).is_err() {
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
            let _ = ltx.send((None, query::classify(res)));
        });
    }
    rx
}

/// Await the next streaming query result, or pend forever when no picker query
/// is live (`picker_rx` is `None`) so the drain `select!` arm stays inert.
async fn next_status(
    rx: &mut Option<mpsc::UnboundedReceiver<(Option<String>, HostStatus)>>,
) -> Option<(Option<String>, HostStatus)> {
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
pub async fn handshake_spawn<R, W>(
    reader: &mut R,
    writer: &mut W,
    name: Option<String>,
    create_if_missing: bool,
    spec: Option<plexy_glass_protocol::SpawnSpec>,
    size: PtySize,
) -> Result<(), ClientError>
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
        ServerMsg::Attached { .. } => Ok(()),
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        other => Err(ClientError::Io(io::Error::other(format!(
            "expected Attached, got {other:?}"
        )))),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use bytes::Bytes;
    use plexy_glass_protocol::{ExitStatus, ServerMsg, SessionEntry};
    use tokio::io::duplex;
    use tokio::{task, time};

    use super::*;

    #[tokio::test]
    async fn pump_writes_output_to_stdout_and_exits_on_exited() {
        let (mut server_w, mut client_r) = duplex(64 * 1024);
        let (server_r, mut client_w) = duplex(64 * 1024);
        drop(server_r); // we don't read from the client in this test
        let (stdin_w, mut stdin_r) = duplex(64);
        drop(stdin_w);
        let (mut stdout_r, mut stdout_w) = duplex(64 * 1024);
        let (_tx, mut resize_rx) = mpsc::channel(4);

        // Server-side: emit one `Output` and then `Exited`.
        let server = tokio::spawn(async move {
            let out = ServerMsg::Output(Bytes::from_static(b"abc"));
            let bytes = postcard::to_allocvec(&out).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
            let done = ServerMsg::Exited {
                status: ExitStatus::Code(0),
            };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            &mut resize_rx,
            &Target::default(),
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Code(0))),
            "got: {status:?}"
        );

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
            let done = ServerMsg::Exited {
                status: ExitStatus::Code(0),
            };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            &mut resize_rx,
            &Target::default(),
        )
        .await
        .expect("pump must not error on interleaved stdin");
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Code(0))),
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
                String::from_utf8_lossy(&rendered).contains("switch session"),
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

            // Let pump return.
            let done = ServerMsg::Exited {
                status: ExitStatus::Code(0),
            };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            &mut resize_rx,
            &Target::default(),
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Code(0))),
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

            let done = ServerMsg::Exited {
                status: ExitStatus::Code(0),
            };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            &mut resize_rx,
            &Target::default(),
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Code(0))),
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
            None,
        );

        assert_eq!(a.rows[0].kind, RowKind::Host, "current-daemon anchor first");
        assert_eq!(a.rows[0].host, None, "local anchor");
        assert_eq!(a.rows[0].status, RowStatus::Live);
        assert_eq!(a.rows[1].name, "main");
        assert_eq!(a.rows[1].kind, RowKind::Session);
        assert_eq!(a.rows[1].host, None, "current session tagged local");

        assert!(!a.query_local, "attached-local does not query local again");
        assert_eq!(
            a.remote_hosts,
            vec!["prod".to_string(), "scratch".to_string()]
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
        let a = build_picker_rows(vec![picker_entry("api", 1)], Some("prod"));

        assert_eq!(
            a.rows[0].host.as_deref(),
            Some("prod"),
            "remote current anchor"
        );
        assert_eq!(a.rows[0].kind, RowKind::Host);
        assert!(a.query_local, "attached-remote queries the local daemon");
        assert_eq!(
            a.remote_hosts,
            vec!["dev".to_string()],
            "prod (current) excluded"
        );
        let locals: Vec<_> = a.rows.iter().filter(|r| r.host.is_none()).collect();
        assert_eq!(locals.len(), 1, "one local-other anchor");
        assert_eq!(locals[0].kind, RowKind::Host);
        assert_eq!(locals[0].status, RowStatus::Pending);
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
            assert!(text.contains("switch session"), "picker rendered");
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

            let done = ServerMsg::Exited {
                status: ExitStatus::Code(0),
            };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            &mut resize_rx,
            &Target::default(),
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Code(0))),
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

            let done = ServerMsg::Exited {
                status: ExitStatus::Code(0),
            };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            &mut resize_rx,
            &Target::default(),
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Code(0))),
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
                &mut resize_rx,
                &local,
            ) => status.unwrap(),
            () = driver => unreachable!("driver parks forever after its writes"),
        };
        match status {
            PumpExit::ReconnectTo { target: t, name } => {
                assert_eq!(t.host.as_deref(), Some("nonexistent.invalid"));
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
        // as `Reconnect` — the global `create_if_missing=true` is what actually
        // creates the fresh session once the outer loop re-attaches.
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
                &mut resize_rx,
                &local,
            ) => status.unwrap(),
            () = driver => unreachable!("driver parks forever after its writes"),
        };
        match status {
            PumpExit::ReconnectTo { target: t, name } => {
                assert_eq!(t.host.as_deref(), Some("nonexistent.invalid"));
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
            stdin_w.write_all(b"\x0e").await.unwrap();
            stdin_w.write_all(b"x").await.unwrap();

            let second =
                read_until_contains(&mut stdout_r, "filter:", Duration::from_secs(1)).await;
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

            let done = ServerMsg::Exited {
                status: ExitStatus::Code(0),
            };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(
            &mut client_r,
            &mut client_w,
            &mut stdin_r,
            &mut stdout_w,
            &mut resize_rx,
            &Target::default(),
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Code(0))),
            "got: {status:?}"
        );

        // Confirm the roster hook itself is gone, not just the rendered row.
        assert_eq!(roster::load_adhoc(), Vec::<String>::new());

        driver.await.unwrap();
    }
}
