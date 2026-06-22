//! In-memory composite output grid. The compositor builds one; the
//! diff-renderer compares two to produce ANSI bytes.

use plexy_glass_emulator::ImageFormat;
use plexy_glass_emulator::Cell;
use std::sync::Arc;

/// An image placement resolved to host (terminal) coordinates, ready for the
/// per-client renderer to transmit (once) and place. Built by the compositor
/// from each pane's `Screen::placements` + `images`, clipped to the visible
/// viewport. Carries the image data so a client seeing it for the first time
/// can transmit it.
#[derive(Debug, Clone)]
pub struct VisiblePlacement {
    /// Stable per-frame key (pane id folded with the placement seq) for the
    /// renderer's cross-frame placement diff.
    pub key: u64,
    /// Host-global image id (raw per-pane id folded with the pane id) so two
    /// panes that both use, say, Kitty image id 5 don't collide on the wire.
    pub image_id: u32,
    pub placement_id: u32,
    /// Content version of the source image; the renderer re-transmits when this
    /// changes for an already-transmitted id.
    pub generation: u64,
    pub format: ImageFormat,
    /// Full image dimensions, used by the `a=t` transmit (always the whole image).
    pub pixel_w: u32,
    pub pixel_h: u32,
    /// Source pixel sub-rectangle to display (the visible part after clipping).
    /// Equals the full image when uncropped; the renderer emits Kitty `x/y/w/h`
    /// crop keys only when it's a strict sub-rect.
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub data_b64: Arc<[u8]>,
    pub host_row: u16,
    pub host_col: u16,
    /// Displayed cell box (already clipped to the visible region).
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone)]
pub struct VirtualScreen {
    pub cells: Vec<Cell>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_visible: bool,
    pub rows: u16,
    pub cols: u16,
    /// Inline-image placements to transmit/place after the cell diff.
    pub placements: Vec<VisiblePlacement>,
}

impl VirtualScreen {
    pub fn blank(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            cells: vec![Cell::default(); rows as usize * cols as usize],
            cursor: None,
            cursor_visible: false,
            rows,
            cols,
            placements: Vec::new(),
        }
    }

    pub fn cell(&self, r: u16, c: u16) -> Option<&Cell> {
        if r >= self.rows || c >= self.cols {
            return None;
        }
        self.cells.get(r as usize * self.cols as usize + c as usize)
    }

    pub fn cell_mut(&mut self, r: u16, c: u16) -> Option<&mut Cell> {
        if r >= self.rows || c >= self.cols {
            return None;
        }
        let cols = self.cols as usize;
        self.cells.get_mut(r as usize * cols + c as usize)
    }

    pub fn put(&mut self, r: u16, c: u16, cell: Cell) {
        if let Some(slot) = self.cell_mut(r, c) {
            *slot = cell;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol_str::SmolStr;

    #[test]
    fn blank_dimensions() {
        let v = VirtualScreen::blank(4, 6);
        assert_eq!(v.rows, 4);
        assert_eq!(v.cols, 6);
        assert_eq!(v.cells.len(), 24);
        assert!(v.cells.iter().all(|c| c.is_blank()));
    }

    #[test]
    fn put_then_get_roundtrips() {
        let mut v = VirtualScreen::blank(2, 2);
        let c = Cell {
            grapheme: SmolStr::new("Z"),
            ..Cell::default()
        };
        v.put(0, 1, c.clone());
        assert_eq!(v.cell(0, 1), Some(&c));
        assert!(v.cell(0, 0).unwrap().is_blank());
    }

    #[test]
    fn put_oob_is_noop() {
        let mut v = VirtualScreen::blank(2, 2);
        let c = Cell {
            grapheme: SmolStr::new("X"),
            ..Cell::default()
        };
        v.put(99, 99, c);
        assert!(v.cells.iter().all(|c| c.is_blank()));
    }
}
