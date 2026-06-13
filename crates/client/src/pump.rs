use crate::error::ClientError;
use bytes::BytesMut;
use plexy_glass_protocol::{ClientMsg, Codec, ColorScheme, ExitStatus, PtySize, ServerMsg};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

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
    let n: u32 = std::str::from_utf8(&rest[..j]).ok()?.parse().ok()?;
    let scheme = match n {
        1 => ColorScheme::Dark,
        2 => ColorScheme::Light,
        _ => return None,
    };
    Some((scheme, PREFIX.len() + j + 1))
}

/// Run the three concurrent pumps:
///   stdin  -> ClientMsg::Input(bytes)  -> daemon
///   daemon -> ServerMsg::Output(bytes) -> stdout
///   SIGWINCH (delivered via `resize_rx`) -> ClientMsg::Resize(size) -> daemon
///
/// Returns the child's exit status when the daemon sends `Exited`.
pub async fn pump<R, W, In, Out>(
    mut daemon_read: R,
    mut daemon_write: W,
    mut stdin: In,
    mut stdout: Out,
    mut resize_rx: mpsc::Receiver<PtySize>,
) -> Result<ExitStatus, ClientError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    In: AsyncRead + Unpin + Send + 'static,
    Out: AsyncWrite + Unpin + Send + 'static,
{
    let mut stdin_buf = BytesMut::with_capacity(STDIN_CHUNK);
    let exit_status: ExitStatus;
    // Cancel-safety: `Codec::read_frame` is `read_exact`-based and NOT
    // cancel-safe. If `select!` drops it mid-frame (because a stdin byte or a
    // resize won the race while a daemon Output frame was still arriving) the
    // bytes already consumed from the socket are lost and the stream desyncs.
    // So the read future is pinned across iterations and recreated only after
    // it completes, mirroring the daemon's `serve_attach` (see connection.rs).
    let mut read_fut = Box::pin(Codec::read_frame(&mut daemon_read));
    loop {
        stdin_buf.clear();
        stdin_buf.resize(STDIN_CHUNK, 0);
        tokio::select! {
            // Daemon -> client
            frame = &mut read_fut => {
                let frame = match frame? {
                    Some(f) => f,
                    None => {
                        exit_status = ExitStatus::Unknown;
                        break;
                    }
                };
                // Recreate the pinned read future for the next iteration, and only ever
                // after it completed, so no buffered frame bytes are lost. Drop the old
                // (completed) future first to release its borrow of `daemon_read` before
                // the new one reborrows it.
                read_fut = {
                    drop(read_fut);
                    Box::pin(Codec::read_frame(&mut daemon_read))
                };
                let msg: ServerMsg = postcard::from_bytes(&frame)
                    .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
                match msg {
                    ServerMsg::Output(b) => {
                        stdout.write_all(&b).await.map_err(ClientError::Io)?;
                        stdout.flush().await.map_err(ClientError::Io)?;
                    }
                    ServerMsg::Exited { status } => {
                        exit_status = status;
                        break;
                    }
                    ServerMsg::Error(e) => {
                        return Err(ClientError::DaemonError(e));
                    }
                    ServerMsg::Attached { .. } => {} // already saw it in the caller
                    // `ServerMsg` is `#[non_exhaustive]`; future variants are ignored here.
                    #[allow(unreachable_patterns)]
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
                // Extract outer-terminal focus/theme events; relay them as
                // dedicated `ClientMsg`s, forward the remaining bytes as Input.
                let events = scan_outer_events(&mut chunk);
                for ev in events {
                    let msg = match ev {
                        OuterEvent::FocusIn => ClientMsg::FocusIn,
                        OuterEvent::FocusOut => ClientMsg::FocusOut,
                        OuterEvent::ColorScheme(s) => ClientMsg::ColorScheme(s),
                    };
                    send_client_msg(&mut daemon_write, &msg).await?;
                }
                if !chunk.is_empty() {
                    let msg = ClientMsg::Input(bytes::Bytes::from(chunk));
                    send_client_msg(&mut daemon_write, &msg).await?;
                }
            }
            // Client -> daemon (resize)
            Some(size) = resize_rx.recv() => {
                send_client_msg(&mut daemon_write, &ClientMsg::Resize(size)).await?;
            }
        }
    }
    Ok(exit_status)
}

pub async fn send_client_msg<W>(writer: &mut W, msg: &ClientMsg) -> Result<(), ClientError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = postcard::to_allocvec(msg)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Encode(e.to_string()))?;
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
        .ok_or_else(|| ClientError::Io(std::io::Error::other("daemon closed before Attached")))?;
    let msg: ServerMsg = postcard::from_bytes(&frame)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
    match msg {
        ServerMsg::Attached { .. } => Ok(()),
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        other => Err(ClientError::Io(std::io::Error::other(format!(
            "expected Attached, got {other:?}"
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use plexy_glass_protocol::{ExitStatus, ServerMsg};
    use tokio::io::duplex;

    #[tokio::test]
    async fn pump_writes_output_to_stdout_and_exits_on_exited() {
        let (mut server_w, client_r) = duplex(64 * 1024);
        let (server_r, client_w) = duplex(64 * 1024);
        drop(server_r); // we don't read from the client in this test
        let (stdin_w, stdin_r) = duplex(64);
        drop(stdin_w);
        let (mut stdout_r, stdout_w) = duplex(64 * 1024);
        let (_tx, resize_rx) = mpsc::channel(4);

        // Server-side: emit one `Output` and then `Exited`.
        let server = tokio::spawn(async move {
            let out = ServerMsg::Output(Bytes::from_static(b"abc"));
            let bytes = postcard::to_allocvec(&out).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
            let done = ServerMsg::Exited { status: ExitStatus::Code(0) };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(client_r, client_w, stdin_r, stdout_w, resize_rx)
            .await
            .unwrap();
        assert!(matches!(status, ExitStatus::Code(0)), "got: {status:?}");

        let mut out = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            stdout_r.read_to_end(&mut out),
        )
        .await;
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
        let (mut server_w, client_r) = duplex(16);
        let (server_r, client_w) = duplex(64 * 1024);
        let (mut stdin_w, stdin_r) = duplex(64 * 1024);
        let (mut stdout_r, stdout_w) = duplex(256 * 1024);
        let (_tx, resize_rx) = mpsc::channel(4);

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
                tokio::task::yield_now().await;
            }
        });

        let srv_payload = payload.clone();
        let server = tokio::spawn(async move {
            let out = ServerMsg::Output(Bytes::from(srv_payload));
            let bytes = postcard::to_allocvec(&out).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
            let done = ServerMsg::Exited { status: ExitStatus::Code(0) };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(client_r, client_w, stdin_r, stdout_w, resize_rx)
            .await
            .expect("pump must not error on interleaved stdin");
        assert!(matches!(status, ExitStatus::Code(0)), "got: {status:?}");

        let mut out = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stdout_r.read_to_end(&mut out),
        )
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
}
