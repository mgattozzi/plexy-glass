//! Per-client renderer task: diffs frames against last frame, emits ANSI as ServerMsg::Output.

use crate::error::DaemonError;
use plexy_glass_mux::{DiffRenderer, VirtualScreen};
use plexy_glass_protocol::{Codec, ServerMsg};
use std::sync::Arc;
use tokio::io::AsyncWrite;
use tokio::sync::watch;

pub struct Renderer {
    diff: DiffRenderer,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            diff: DiffRenderer::new(),
        }
    }

    pub async fn run<W>(
        mut self,
        mut frame_rx: watch::Receiver<Arc<VirtualScreen>>,
        mut writer: W,
    ) -> Result<(), DaemonError>
    where
        W: AsyncWrite + Unpin,
    {
        // Send the initial frame immediately so the client sees state on attach.
        {
            let initial = frame_rx.borrow_and_update().clone();
            let bytes = self.diff.render(&initial);
            self.send_output(&mut writer, bytes).await?;
        }
        while frame_rx.changed().await.is_ok() {
            let frame = frame_rx.borrow_and_update().clone();
            let bytes = self.diff.render(&frame);
            self.send_output(&mut writer, bytes).await?;
        }
        Ok(())
    }

    async fn send_output<W>(&self, writer: &mut W, bytes: Vec<u8>) -> Result<(), DaemonError>
    where
        W: AsyncWrite + Unpin,
    {
        let msg = ServerMsg::Output(bytes::Bytes::from(bytes));
        let payload = postcard::to_allocvec(&msg)
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("encode: {e}"))))?;
        Codec::write_frame(writer, &payload).await?;
        Ok(())
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}
