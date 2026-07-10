use std::{io, str};

use bytes::BytesMut;
use plexy_glass_protocol::errors::CodecError;
use plexy_glass_protocol::{ClientMsg, Codec, ColorScheme, ExitStatus, PtySize, ServerMsg};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::ClientError;
use crate::picker::{PickerOutcome, PickerRow, PickerState};

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
    /// (Milestone B) the picker chose a session on a DIFFERENT daemon; the outer
    /// loop should re-attach. Unused in Milestone A but defined so the pump's
    /// return type is stable across milestones.
    #[allow(
        dead_code,
        reason = "wired by the outer attach loop; only produced once the picker lands (Milestone B)"
    )]
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
pub async fn pump<R, W, In, Out>(
    daemon_read: &mut R,
    daemon_write: &mut W,
    stdin: &mut In,
    stdout: &mut Out,
    resize_rx: &mut mpsc::Receiver<PtySize>,
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
                        // Row label matches `open_session_picker_overlay`
                        // (crates/daemon/src/connection.rs) verbatim, so the
                        // client-rendered picker reads the same as the old
                        // daemon-rendered one.
                        let rows = sessions
                            .into_iter()
                            .map(|e| {
                                let is_current = e.name == current;
                                PickerRow {
                                    label: format!(
                                        "{} \u{2014} {} win, {} panes, {} clients",
                                        e.name, e.windows, e.panes, e.clients
                                    ),
                                    name: e.name,
                                    is_current,
                                }
                            })
                            .collect();
                        let state = PickerState::new(rows);
                        stdout.write_all(&state.render()).await.map_err(ClientError::Io)?;
                        stdout.flush().await.map_err(ClientError::Io)?;
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
                            // picker is done; leave `picker` at None (already taken).
                        }
                        Some(PickerOutcome::Cancel) => {
                            send_client_msg(&mut *daemon_write, &ClientMsg::Redraw).await?;
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
        }
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

            let rendered =
                read_until_contains(&mut stdout_r, "main", Duration::from_secs(1)).await;
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
        )
        .await
        .unwrap();
        assert!(
            matches!(status, PumpExit::Ended(ExitStatus::Code(0))),
            "got: {status:?}"
        );

        driver.await.unwrap();
    }
}
