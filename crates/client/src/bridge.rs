//! The `bridge` subcommand: a protocol-opaque stdio↔daemon-socket relay, run on
//! the remote host by a `-H` client over SSH. It `connect_or_spawn`s (or, with
//! `--no-spawn`, `connect_only`s) the *remote* daemon — reusing the same code a
//! local client uses, now running on the remote — and copies bytes both ways.

use std::process;

use tokio::io::{AsyncRead, AsyncWrite, copy, split, stdin, stdout};

use crate::error::ClientError;
use crate::transport::{Connect, connect_only, connect_or_spawn, default_socket_path};

/// Copy bytes both directions between a client pipe (`client_in`/`client_out`)
/// and the daemon socket (`daemon_read`/`daemon_write`), finishing as soon as
/// EITHER direction ends (client detach → stdin EOF; session end → socket EOF).
pub async fn relay<CI, CO, DR, DW>(
    mut client_in: CI,
    mut client_out: CO,
    mut daemon_read: DR,
    mut daemon_write: DW,
) -> Result<(), ClientError>
where
    CI: AsyncRead + Unpin,
    CO: AsyncWrite + Unpin,
    DR: AsyncRead + Unpin,
    DW: AsyncWrite + Unpin,
{
    tokio::select! {
        r = copy(&mut client_in, &mut daemon_write) => { r.map_err(ClientError::Io)?; }
        r = copy(&mut daemon_read, &mut client_out) => { r.map_err(ClientError::Io)?; }
    }
    Ok(())
}

/// Entry point for `plexy-glass bridge [--no-spawn]`: relay this process's
/// stdin/stdout to the remote daemon's socket. `Connect::Only` (`--no-spawn`)
/// mirrors the client's connect-only verbs so a remote scripting call doesn't
/// start a daemon; `Connect::Spawn` starts one if none is running.
pub async fn run_bridge(connect: Connect) -> Result<(), ClientError> {
    let socket = default_socket_path()?;
    let stream = match connect {
        Connect::Only => connect_only(&socket).await?,
        Connect::Spawn => connect_or_spawn(&socket).await?,
    };
    let (daemon_read, daemon_write) = split(stream);
    let client_stdin = stdin();
    let client_stdout = stdout();
    relay(client_stdin, client_stdout, daemon_read, daemon_write).await?;
    // `tokio::io::stdin()` has spawned a blocking read thread that never ends;
    // exit hard (like the interactive client) so runtime teardown can't hang.
    process::exit(0);
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;

    // Bytes written to the "client in" endpoint reach the "daemon write"
    // endpoint (client → daemon direction), proving the relay copies through.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_copies_client_to_daemon() {
        let (client_far, client_near) = duplex(64); // client pipe
        let (daemon_near, daemon_far) = duplex(64); // daemon socket
        let (ci, co) = split(client_near);
        let (dr, dw) = split(daemon_near);
        let task = tokio::spawn(async move { relay(ci, co, dr, dw).await });

        let (mut cf_r, mut cf_w) = split(client_far);
        let (mut df_r, mut df_w) = split(daemon_far);
        cf_w.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        df_r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        // And daemon → client.
        df_w.write_all(b"pong").await.unwrap();
        let mut buf2 = [0u8; 4];
        cf_r.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"pong");

        // `drop(cf_w)` alone would NOT signal EOF here: `tokio::io::split`'s
        // `WriteHalf` shares the underlying `DuplexStream` with `ReadHalf` via
        // an `Arc`, and only the `DuplexStream`'s own `Drop` (which fires once
        // every handle to it is gone) notifies the peer. `cf_r` is still in
        // scope, so the `Arc` never hits zero. `shutdown()` is the real
        // signal (mirrors a socket half-close) and wakes the pending reader.
        cf_w.shutdown().await.unwrap(); // client detach → relay finishes
        let _ = task.await;
    }
}
