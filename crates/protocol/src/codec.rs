use std::io::ErrorKind;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::errors::CodecError;

/// Maximum permitted frame payload size. Frames larger than this are rejected
/// before allocation, so a bug or a corrupt peer can't make us allocate
/// gigabytes. This is a sanity bound over a local same-user unix socket, not a
/// security boundary.
///
/// It has to be large enough to hold ONE render frame, and a render frame that
/// first transmits an inline image re-emits that image's whole base64 payload
/// in a single `ServerMsg::Output`. A single image is bounded by the per-screen
/// `ImageStore` budget (64 MiB in the emulator), so 128 MiB gives comfortable
/// headroom for that plus a multi-pane repaint that transmits several images at
/// once. At 1 MiB (the original value) any real inline image — `timg`, `chafa`,
/// a screenshot — overran a single frame and `write_frame` returned
/// `FrameTooLarge`, which tore the client down: the whole point of a
/// "first-class images" mux crashing on the first real image. The renderer now
/// also drops (rather than dies on) a frame over this cap, so an image too big
/// even for 128 MiB degrades to "not painted" instead of a crash.
pub const MAX_FRAME_BYTES: u32 = 128 << 20; // 128 MiB

/// Stateless framing helpers. Wire format: little-endian u32 length prefix,
/// followed by exactly that many bytes of payload.
pub struct Codec;

impl Codec {
    /// Read exactly one frame. Returns `Ok(None)` on clean EOF before any
    /// bytes are read; once length bytes have been consumed, an EOF is an
    /// error (`UnexpectedEof`).
    pub async fn read_frame<R>(reader: &mut R) -> Result<Option<Bytes>, CodecError>
    where
        R: AsyncRead + Unpin,
    {
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(CodecError::Io(e)),
        }
        let len = u32::from_le_bytes(len_buf);
        if len > MAX_FRAME_BYTES {
            return Err(CodecError::FrameTooLarge {
                max: MAX_FRAME_BYTES,
                got: len,
            });
        }
        let mut buf = BytesMut::with_capacity(len as usize);
        buf.resize(len as usize, 0);
        reader.read_exact(&mut buf).await.map_err(|e| {
            if e.kind() == ErrorKind::UnexpectedEof {
                CodecError::UnexpectedEof
            } else {
                CodecError::Io(e)
            }
        })?;
        Ok(Some(buf.freeze()))
    }

    /// Write one frame.
    pub async fn write_frame<W>(writer: &mut W, payload: &[u8]) -> Result<(), CodecError>
    where
        W: AsyncWrite + Unpin,
    {
        let len: u32 = payload
            .len()
            .try_into()
            .map_err(|_| CodecError::FrameTooLarge {
                max: MAX_FRAME_BYTES,
                got: u32::MAX,
            })?;
        if len > MAX_FRAME_BYTES {
            return Err(CodecError::FrameTooLarge {
                max: MAX_FRAME_BYTES,
                got: len,
            });
        }
        writer
            .write_all(&len.to_le_bytes())
            .await
            .map_err(CodecError::Io)?;
        writer.write_all(payload).await.map_err(CodecError::Io)?;
        writer.flush().await.map_err(CodecError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tokio::io;
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    #[tokio::test]
    async fn round_trip_one_frame() {
        let (mut a, mut b) = duplex(64);
        let payload = b"hello world";
        Codec::write_frame(&mut a, payload).await.unwrap();
        let got = Codec::read_frame(&mut b).await.unwrap().expect("frame");
        assert_eq!(&got[..], payload);
    }

    #[tokio::test]
    async fn returns_none_on_clean_eof() {
        let (a, mut b) = duplex(64);
        drop(a);
        let got = Codec::read_frame(&mut b).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn errors_on_eof_mid_frame() {
        let (mut a, mut b) = duplex(64);
        // Write a length prefix but no payload, then drop the writer.
        a.write_all(&5u32.to_le_bytes()).await.unwrap();
        drop(a);
        let err = Codec::read_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, CodecError::UnexpectedEof), "got: {err:?}");
    }

    #[tokio::test]
    async fn rejects_oversized_frames_before_allocating() {
        let (mut a, mut b) = duplex(64);
        let bogus = MAX_FRAME_BYTES + 1;
        a.write_all(&bogus.to_le_bytes()).await.unwrap();
        let err = Codec::read_frame(&mut b).await.unwrap_err();
        match err {
            CodecError::FrameTooLarge { max, got } => {
                assert_eq!(max, MAX_FRAME_BYTES);
                assert_eq!(got, bogus);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_frame_rejects_oversized_payload() {
        // Symmetric with the read guard: an over-cap payload must be refused
        // before any bytes are emitted, with the exact FrameTooLarge shape.
        let mut sink = io::sink();
        let payload = vec![0u8; (MAX_FRAME_BYTES + 1) as usize];
        let err = Codec::write_frame(&mut sink, &payload).await.unwrap_err();
        match err {
            CodecError::FrameTooLarge { max, got } => {
                assert_eq!(max, MAX_FRAME_BYTES);
                assert_eq!(got, MAX_FRAME_BYTES + 1);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handles_split_reads() {
        // Use a tiny duplex buffer so writes get split across the read calls.
        let (mut a, mut b) = duplex(2);
        let writer = tokio::spawn(async move {
            Codec::write_frame(&mut a, &[0xab; 100]).await.unwrap();
        });
        let got = Codec::read_frame(&mut b).await.unwrap().expect("frame");
        writer.await.unwrap();
        assert_eq!(got.len(), 100);
        assert!(got.iter().all(|&x| x == 0xab));
    }
}
