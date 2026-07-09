//! The bridge relay is protocol-transparent: a framed request written on the
//! "client" side arrives byte-identical on the "daemon" side and a reply comes
//! back, with no framing awareness in the relay.

use plexy_glass_client::bridge::relay;
use plexy_glass_protocol::{ClientMsg, Codec};
use tokio::io::{AsyncWriteExt, duplex, split};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_is_transparent_to_framed_traffic() {
    let (client_far, client_near) = duplex(4096);
    let (daemon_near, daemon_far) = duplex(4096);
    let (ci, co) = split(client_near);
    let (dr, dw) = split(daemon_near);
    let task = tokio::spawn(async move { relay(ci, co, dr, dw).await });

    let (mut cf_r, mut cf_w) = split(client_far);
    let (mut df_r, mut df_w) = split(daemon_far);

    // Client writes a framed message; it must arrive intact on the daemon side.
    let payload = postcard::to_allocvec(&ClientMsg::ListSessions).unwrap();
    Codec::write_frame(&mut cf_w, &payload).await.unwrap();
    let got = Codec::read_frame(&mut df_r).await.unwrap().unwrap();
    assert_eq!(&got[..], &payload[..]);

    // Daemon writes a frame back; it must arrive intact on the client side.
    Codec::write_frame(&mut df_w, b"reply-bytes").await.unwrap();
    let back = Codec::read_frame(&mut cf_r).await.unwrap().unwrap();
    assert_eq!(&back[..], b"reply-bytes");

    // See the analogous comment in `bridge::tests::relay_copies_client_to_daemon`:
    // a bare `drop(cf_w)` would not signal EOF while `cf_r` is still alive
    // (both halves share the underlying `DuplexStream` via an `Arc`), so shut
    // the write half down explicitly to unblock `relay`'s read of `ci`.
    cf_w.shutdown().await.unwrap();
    let _ = task.await;
}
