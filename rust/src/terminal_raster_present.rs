//! CPU raster present for the Flutter hot path (MVP).
//!
//! True Flutter external [`Texture`] registration (Linux `FlPixelBufferTexture`,
//! macOS/Windows `FlutterTextureRegistry`) is not wired: this crate is an
//! `ffiPlugin` with cargokit-only native glue and no texture registrar. Until
//! that exists, Rust owns a retained RGBA pixmap; Dart uploads it to a
//! `ui.Image` and draws it in a thin CustomPaint (not the MirrorGrid cell loop).
//!
//! Cursor stays on Dart [`CursorPainter`] above the image for MVP.

use crate::engine::LineUpdate;

/// One presentable RGBA frame plus chrome needed for input routing.
#[derive(Clone, Debug)]
pub struct RasterPresentFrame {
    pub width: u32,
    pub height: u32,
    /// Packed RGBA8888, row-major, length `width * height * 4`.
    pub rgba: Vec<u8>,
    pub cursor_line: u32,
    pub cursor_col: u32,
    pub cursor_visible: bool,
    pub cursor_shape: u8,
    pub cursor_blinking: bool,
    pub mode_flags: u32,
    pub display_offset: u32,
    pub history_size: u32,
    pub scroll_fraction: f64,
    pub default_fg: u32,
    pub default_bg: u32,
    pub cursor_color: u32,
    /// True when the whole buffer was rebuilt (resize / full damage).
    pub full: bool,
}

/// Retained RGBA pixmap updated from columnar line damage.
#[derive(Clone, Debug)]
pub struct TerminalRasterPresent {
    width: u32,
    height: u32,
    cell_w: u16,
    cell_h: u16,
    cols: u16,
    rows: u16,
    rgba: Vec<u8>,
}

impl TerminalRasterPresent {
    pub fn new(cols: u16, rows: u16, cell_w: u16, cell_h: u16) -> Self {
        let cell_w = cell_w.max(1);
        let cell_h = cell_h.max(1);
        let cols = cols.max(1);
        let rows = rows.max(1);
        let width = cols as u32 * cell_w as u32;
        let height = rows as u32 * cell_h as u32;
        let mut s = Self {
            width,
            height,
            cell_w,
            cell_h,
            cols,
            rows,
            rgba: vec![0; (width * height * 4) as usize],
        };
        s.clear(0x00_00_00);
        s
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_w: u16, cell_h: u16, default_bg: u32) {
        let next = Self::new(cols, rows, cell_w, cell_h);
        *self = next;
        self.clear(default_bg);
    }

    /// Fill the entire buffer with packed `0x00RRGGBB` (alpha forced opaque).
    pub fn clear(&mut self, bg: u32) {
        let (r, g, b) = unpack_rgb(bg);
        for px in self.rgba.chunks_exact_mut(4) {
            px[0] = r;
            px[1] = g;
            px[2] = b;
            px[3] = 255;
        }
    }

    /// Fill one viewport row with a solid color (no glyphs) — pipe proof.
    pub fn fill_row_solid(&mut self, row: u32, color: u32) {
        if row >= self.rows as u32 {
            return;
        }
        let (r, g, b) = unpack_rgb(color);
        let y0 = (row * self.cell_h as u32) as usize;
        let y1 = y0 + self.cell_h as usize;
        for y in y0..y1 {
            let row_off = y * self.width as usize * 4;
            for x in 0..self.width as usize {
                let i = row_off + x * 4;
                self.rgba[i] = r;
                self.rgba[i + 1] = g;
                self.rgba[i + 2] = b;
                self.rgba[i + 3] = 255;
            }
        }
    }

    /// Rotate viewport pixel rows by `delta` cells (positive = content moves down,
    /// matching MirrorGrid when scrolled up into history).
    pub fn rotate_rows(&mut self, delta: i32) {
        if delta == 0 || self.rows == 0 {
            return;
        }
        let row_bytes = (self.width * self.cell_h as u32 * 4) as usize;
        let n = self.rows as i32;
        let mut next = self.rgba.clone();
        if delta > 0 {
            let d = delta.min(n);
            for dst in (d..n).rev() {
                let src = dst - d;
                let s = (src as usize) * row_bytes;
                let t = (dst as usize) * row_bytes;
                next[t..t + row_bytes].copy_from_slice(&self.rgba[s..s + row_bytes]);
            }
        } else {
            let d = (-delta).min(n);
            for dst in 0..(n - d) {
                let src = dst + d;
                let s = (src as usize) * row_bytes;
                let t = (dst as usize) * row_bytes;
                next[t..t + row_bytes].copy_from_slice(&self.rgba[s..s + row_bytes]);
            }
        }
        self.rgba = next;
    }

    /// Rasterize a damaged line: cell backgrounds + embedded ASCII bitmaps.
    pub fn blit_line(&mut self, line: &LineUpdate) {
        let row = line.line;
        if row >= self.rows as u32 {
            return;
        }
        let n = line.codepoints.len().min(self.cols as usize);
        for col in 0..n {
            self.blit_cell(
                row,
                col as u32,
                line.codepoints[col],
                line.fg[col],
                line.bg[col],
            );
        }
    }

    fn blit_cell(&mut self, row: u32, col: u32, codepoint: u32, fg: u32, bg: u32) {
        let x0 = (col * self.cell_w as u32) as i32;
        let y0 = (row * self.cell_h as u32) as i32;
        let cw = self.cell_w as i32;
        let ch = self.cell_h as i32;
        fill_rect(&mut self.rgba, self.width, x0, y0, cw, ch, bg);
        if codepoint > 32 {
            draw_glyph(
                &mut self.rgba,
                self.width,
                x0,
                y0,
                cw,
                ch,
                codepoint,
                fg,
            );
        }
    }

    pub fn into_frame(
        &self,
        chrome: RasterChrome,
        full: bool,
    ) -> RasterPresentFrame {
        RasterPresentFrame {
            width: self.width,
            height: self.height,
            rgba: self.rgba.clone(),
            cursor_line: chrome.cursor_line,
            cursor_col: chrome.cursor_col,
            cursor_visible: chrome.cursor_visible,
            cursor_shape: chrome.cursor_shape,
            cursor_blinking: chrome.cursor_blinking,
            mode_flags: chrome.mode_flags,
            display_offset: chrome.display_offset,
            history_size: chrome.history_size,
            scroll_fraction: chrome.scroll_fraction,
            default_fg: chrome.default_fg,
            default_bg: chrome.default_bg,
            cursor_color: chrome.cursor_color,
            full,
        }
    }
}

/// Chrome fields copied from a [`crate::engine::RenderUpdate`] without line cells.
#[derive(Clone, Debug)]
pub struct RasterChrome {
    pub cursor_line: u32,
    pub cursor_col: u32,
    pub cursor_visible: bool,
    pub cursor_shape: u8,
    pub cursor_blinking: bool,
    pub mode_flags: u32,
    pub display_offset: u32,
    pub history_size: u32,
    pub scroll_fraction: f64,
    pub default_fg: u32,
    pub default_bg: u32,
    pub cursor_color: u32,
}

fn unpack_rgb(c: u32) -> (u8, u8, u8) {
    let r = ((c >> 16) & 0xFF) as u8;
    let g = ((c >> 8) & 0xFF) as u8;
    let b = (c & 0xFF) as u8;
    (r, g, b)
}

fn fill_rect(rgba: &mut [u8], width: u32, x0: i32, y0: i32, w: i32, h: i32, color: u32) {
    let (r, g, b) = unpack_rgb(color);
    let width = width as i32;
    let height = (rgba.len() / 4 / width.max(1) as usize) as i32;
    for dy in 0..h {
        let y = y0 + dy;
        if y < 0 || y >= height {
            continue;
        }
        for dx in 0..w {
            let x = x0 + dx;
            if x < 0 || x >= width {
                continue;
            }
            let i = ((y as u32 * width as u32 + x as u32) * 4) as usize;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    }
}

/// Tiny 5×7 glyphs for printable ASCII — MVP text without a font stack.
fn draw_glyph(
    rgba: &mut [u8],
    width: u32,
    x0: i32,
    y0: i32,
    cell_w: i32,
    cell_h: i32,
    codepoint: u32,
    fg: u32,
) {
    let Some(bitmap) = ascii_glyph(codepoint) else {
        return;
    };
    let pad_x = ((cell_w - 5).max(0)) / 2;
    let pad_y = ((cell_h - 7).max(0)) / 2;
    for row in 0..7i32 {
        let bits = bitmap[row as usize];
        for col in 0..5i32 {
            if bits & (1 << (4 - col)) != 0 {
                fill_rect(
                    rgba,
                    width,
                    x0 + pad_x + col,
                    y0 + pad_y + row,
                    1,
                    1,
                    fg,
                );
            }
        }
    }
}

fn ascii_glyph(codepoint: u32) -> Option<[u8; 7]> {
    // Minimal subset for MVP proofs; other printable ASCII → hollow block.
    match codepoint {
        0x41 => Some([0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11]), // A
        0x42 => Some([0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E]), // B
        0x43 => Some([0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E]), // C
        0x48 => Some([0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11]), // H
        0x49 => Some([0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x1F]), // I
        0x30 => Some([0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E]), // 0
        0x31 => Some([0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E]), // 1
        0x23 => Some([0x0A, 0x0A, 0x1F, 0x0A, 0x1F, 0x0A, 0x0A]), // #
        c if (32..=126).contains(&c) => Some([0x1F, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1F]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::LineUpdate;

    fn px(buf: &TerminalRasterPresent, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * buf.width() + x) * 4) as usize;
        let r = buf.rgba();
        [r[i], r[i + 1], r[i + 2], r[i + 3]]
    }

    #[test]
    fn clear_fills_default_bg() {
        let mut buf = TerminalRasterPresent::new(2, 2, 4, 4);
        buf.clear(0x00_12_34_56);
        assert_eq!(px(&buf, 0, 0), [0x12, 0x34, 0x56, 255]);
        assert_eq!(px(&buf, 7, 7), [0x12, 0x34, 0x56, 255]);
    }

    #[test]
    fn fill_row_solid_only_touches_that_row() {
        let mut buf = TerminalRasterPresent::new(2, 2, 4, 4);
        buf.clear(0x00_00_00_00);
        buf.fill_row_solid(1, 0x00_FF_00_00);
        assert_eq!(px(&buf, 0, 0), [0, 0, 0, 255]);
        assert_eq!(px(&buf, 0, 4), [255, 0, 0, 255]);
        assert_eq!(px(&buf, 7, 7), [255, 0, 0, 255]);
    }

    #[test]
    fn blit_line_draws_bg_and_glyph_pixels() {
        let mut buf = TerminalRasterPresent::new(2, 1, 8, 10);
        buf.clear(0x00_00_00_00);
        let line = LineUpdate {
            line: 0,
            codepoints: vec![b'A' as u32, b' ' as u32],
            fg: vec![0x00_FF_FF_FF, 0x00_FF_FF_FF],
            bg: vec![0x00_00_00_80, 0x00_00_00_40],
            flags: vec![0, 0],
            hyperlink_id: vec![0, 0],
        };
        buf.blit_line(&line);
        // Cell 0 bg
        assert_eq!(px(&buf, 0, 0), [0, 0, 0x80, 255]);
        // Cell 1 bg (space — no glyph)
        assert_eq!(px(&buf, 8, 0), [0, 0, 0x40, 255]);
        // Somewhere in the A glyph should be white
        let mut saw_fg = false;
        for y in 0..10u32 {
            for x in 0..8u32 {
                if px(&buf, x, y) == [255, 255, 255, 255] {
                    saw_fg = true;
                }
            }
        }
        assert!(saw_fg, "expected glyph foreground pixels for 'A'");
    }
}
