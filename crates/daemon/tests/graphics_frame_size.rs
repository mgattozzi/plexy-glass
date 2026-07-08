//! Regression: a real inline-image render must fit in one transport frame.
//!
//! The daemon renders a client's whole frame into a single `ServerMsg::Output`,
//! and a frame that first transmits an inline image re-emits that image's whole
//! base64 payload. With `MAX_FRAME_BYTES` at its original 1 MiB, any real image
//! (`timg`, `chafa`, a screenshot) overran a single frame; `Codec::write_frame`
//! returned `FrameTooLarge`, and the renderer's discarded error tore the client
//! down — the whole session "crashed" on the first real image. This pins that a
//! multi-MB image render now serialises to a frame the codec accepts.

use plexy_glass_emulator::{Color, Emulator};
use plexy_glass_mux::compositor::compose;
use plexy_glass_mux::{
    ChromeColors, DiffRenderer, GraphicsCaps, PaneDragRole, PaneId, PaneView, Rect, StatusPlacement,
};
use plexy_glass_protocol::{Codec, MAX_FRAME_BYTES, ServerMsg};

#[tokio::test]
async fn multi_megabyte_image_render_fits_in_one_frame() {
    let rows: u16 = 40;
    let cols: u16 = 120;

    // A ~1.5 MiB image: `a=T,f=24` (raw RGB, dims from s=/v= so no PNG decode)
    // with a big base64 payload. The emulator stores the payload verbatim and
    // the Kitty render re-emits it, so the render output is ~payload-sized —
    // the exact shape `timg -pk` produces, just synthetic so the test is
    // self-contained. 'A' repeated is valid base64 (decodes to zero bytes).
    let payload = "A".repeat(1_500_000);
    let apc = format!("\x1b_Ga=T,i=1,f=24,s=64,v=64;{payload}\x1b\\");

    let mut emu = Emulator::new(rows, cols);
    emu.advance(apc.as_bytes());
    let screen = emu.screen();
    assert_eq!(
        screen.placements.len(),
        1,
        "the image must have been captured and placed"
    );

    let view = PaneView {
        id: PaneId(0),
        rect: Rect::new(0, 0, rows, cols),
        screen,
        is_active: true,
        scroll_offset: 0,
        copy_mode: None,
        block_mode: None,
        title: None,
        marked: false,
        drag_role: PaneDragRole::None,
    };
    let vs = compose(
        &[view],
        (rows, cols),
        None,
        StatusPlacement::Bottom,
        None,
        None,
        None,
        None,
        None,
        Color::Rgb(0xdc, 0xa5, 0x61),
        ChromeColors::ansi_default(),
    );

    let mut diff = DiffRenderer::new();
    diff.set_graphics_caps(GraphicsCaps {
        kitty: true,
        sixel: false,
        iterm2: false,
    });
    let render_bytes = diff.render(&vs);

    // Wrap exactly as the renderer does, then frame it to a sink.
    let msg = ServerMsg::Output(bytes::Bytes::from(render_bytes));
    let framed = postcard::to_allocvec(&msg).expect("encode");

    // The render frame is genuinely multi-MB (so this test is meaningful: it
    // would have tripped the old 1 MiB cap), and the codec now accepts it.
    assert!(
        framed.len() > 1 << 20,
        "render frame is {} bytes; must exceed the old 1 MiB cap to be a real regression",
        framed.len()
    );
    assert!(
        (framed.len() as u32) <= MAX_FRAME_BYTES,
        "render frame is {} bytes, over MAX_FRAME_BYTES ({MAX_FRAME_BYTES})",
        framed.len()
    );
    let mut sink: Vec<u8> = Vec::new();
    Codec::write_frame(&mut sink, &framed)
        .await
        .expect("a real inline-image frame must be sendable, not FrameTooLarge");
}
