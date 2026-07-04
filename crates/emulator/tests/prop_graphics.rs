//! Property tests for the Kitty animation-frame decoder and `ImageStore`'s
//! frame bookkeeping: round-trip/bounds invariants, not implementation
//! restatement. See CLAUDE.md's property-testing conventions.

use std::sync::Arc;

use hegel::{TestCase, generators as gs};
use plexy_glass_emulator::graphics::{
    Frame, Image, ImageFormat, ImageProtocol, ImageStore, parse_command,
};

// ponytail: `ImageStore::CAP_FRAMES_PER_IMAGE` is a private associated const
// (graphics.rs), so this integration test (a separate crate) can't reach it
// without widening its visibility. Hardcoded here instead of adding a `pub`
// just for a test; must track `crates/emulator/src/graphics.rs`'s
// `CAP_FRAMES_PER_IMAGE`.
const CAP_FRAMES_PER_IMAGE: usize = 512;

fn sample_image(id: u32) -> Image {
    Image {
        id,
        protocol: ImageProtocol::Kitty,
        format: ImageFormat::Rgba,
        pixel_w: 1,
        pixel_h: 1,
        data_b64: Arc::from(b"x".as_slice()),
        iterm_args: None,
        generation: 1,
        frames: Arc::new(Vec::new()),
        anim_control: None,
    }
}

fn sample_frame(n: u8) -> Frame {
    Frame {
        frame_number: None,
        canvas_source: None,
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        overwrite: false,
        bg_color: 0,
        gap_ms: 0,
        format: ImageFormat::Rgba,
        data_b64: Arc::from(vec![n]),
    }
}

/// Any well-formed `a=f` frame with random key values parses without
/// panicking and every key that was set is recoverable from the parsed
/// command (round-trip, not a restatement of the parser's own logic — this
/// asserts the OUTPUT matches the INPUT, checked against independently
/// generated values, not against whatever the parser happens to compute).
#[hegel::test(test_cases = 200)]
fn prop_frame_command_round_trips_its_keys(tc: TestCase) {
    let id = tc.draw(gs::integers::<u32>().min_value(1).max_value(1000));
    let r = tc.draw(gs::integers::<u16>().min_value(1).max_value(500));
    let x = tc.draw(gs::integers::<u32>().min_value(0).max_value(10_000));
    let y = tc.draw(gs::integers::<u32>().min_value(0).max_value(10_000));
    let z = tc.draw(gs::integers::<i32>().min_value(-5000).max_value(5000));
    let overwrite = tc.draw(gs::booleans());
    let x_key = if overwrite { ",X=1" } else { "" };
    let framed =
        format!("\x1b_Ga=f,i={id},r={r},x={x},y={y},z={z}{x_key},f=24,s=1,v=1;QUJD\x1b\\");
    let cmd = parse_command(framed.as_bytes());
    let Some(cmd) = cmd else {
        panic!("valid a=f command failed to parse");
    };
    tc.note(&format!("id={id} r={r} x={x} y={y} z={z} overwrite={overwrite}"));
    assert_eq!(cmd.action, b'f');
    assert_eq!(cmd.id, Some(id));
    assert_eq!(cmd.rows, Some(r));
    assert_eq!(cmd.frame_x, Some(x));
    assert_eq!(cmd.frame_y, Some(y));
    assert_eq!(cmd.z, Some(z));
    assert_eq!(cmd.compose_overwrite, overwrite);
}

/// `push_frame` never grows an image's frame log past the documented cap,
/// no matter how many frames arrive.
#[hegel::test(test_cases = 50)]
fn prop_push_frame_never_exceeds_cap(tc: TestCase) {
    let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(1000));
    let mut store = ImageStore::default();
    store.insert(sample_image(1));
    for i in 0..n {
        store.push_frame(1, sample_frame((i % 256) as u8));
    }
    tc.note(&format!("pushed {n} frames"));
    let len = store
        .get(1)
        .expect("image 1 must still be present (byte budget is far above what this test pushes)")
        .frames
        .len();
    assert!(
        len <= CAP_FRAMES_PER_IMAGE,
        "frame log grew past the documented cap: {len}"
    );
    assert_eq!(len, n.min(CAP_FRAMES_PER_IMAGE));
}

/// Frame bytes always count toward the store's total (a cheap proxy for "the
/// byte-budget accounting stays consistent" — this doesn't re-derive the
/// eviction threshold, just confirms bytes are neither double-counted nor
/// dropped as frames arrive and the image is re-read).
#[hegel::test(test_cases = 50)]
fn prop_frame_bytes_are_reflected_in_total_bytes(tc: TestCase) {
    let sizes = tc.draw(gs::vecs(gs::integers::<u8>().min_value(1).max_value(200)).max_size(20));
    let mut store = ImageStore::default();
    store.insert(sample_image(1));
    let mut expected: usize = store
        .get(1)
        .expect("image 1 must be present right after insert")
        .total_bytes();
    for &sz in &sizes {
        let data = vec![0u8; sz as usize];
        let frame = Frame {
            frame_number: None,
            canvas_source: None,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            overwrite: false,
            bg_color: 0,
            gap_ms: 0,
            format: ImageFormat::Rgba,
            data_b64: Arc::from(data),
        };
        store.push_frame(1, frame);
        expected += sz as usize;
    }
    tc.note(&format!("sizes={sizes:?}"));
    let total = store
        .get(1)
        .expect("image 1 must still be present (byte budget is far above what this test pushes)")
        .total_bytes();
    assert_eq!(total, expected);
}
