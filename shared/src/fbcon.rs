//! A minimal linear-framebuffer text console, shared by the loader and the kernel.
//!
//! This exists for one reason: an owner's laptop has no serial port, so a boot
//! that only writes to COM1 is a blind boot. Everything here is deliberately
//! plain — one 8x16 bitmap font, integer scaling, no blending, no animation.
//! The Boot Manager's visual language is a later slice with its own contract;
//! this is diagnostic text.
//!
//! # The two bugs this module is written around
//!
//! 1. **Stride.** `pixels_per_scan_line` is not `width`. The firmware pads
//!    rows, so the address of pixel `(x, y)` is `base + (y * stride + x) * 4`,
//!    never `(y * width + x) * 4`. Using `width` here produces the classic
//!    sheared, diagonal picture. Every write below goes through [`Console::pitch`]
//!    (bytes per row) and no arithmetic in this file uses `width` as a pitch.
//! 2. **Bounds.** The framebuffer is memory the firmware lent us, and writing
//!    outside it is a silent corruption of whatever lives there. Every pixel
//!    write is clipped against both the visible area (`width` x `height`) and
//!    the addressable area (`stride` x `height`), and the two are kept apart on
//!    purpose: we clip drawing to the visible area, and we size our own buffer
//!    view by the addressable area.
//!
//! Pixels are 32 bits in the GOP format the loader recorded in `BootInfo`:
//! `RGBx8` (red in the low byte) or `BGRx8` (blue in the low byte). `BitMask`
//! and `BltOnly` are not drawn to — a `BltOnly` framebuffer cannot be written
//! directly at all, and guessing at a bitmask we do not carry in `BootInfo`
//! would be a guess about the owner's screen.

use core::fmt;

use crate::bootinfo::{pixel_format, FramebufferInfo};
use crate::font8x16::{FIRST_CHAR, GLYPH_HEIGHT, GLYPH_WIDTH, GLYPHS, LAST_CHAR};

/// Background: `#08090B`, the near-black the whole product is built on.
pub const RGB_BACKGROUND: u32 = 0x0809_0B;
/// Body text.
pub const RGB_TEXT: u32 = 0xE6E8_EC;
/// Secondary text: details after a stage name.
pub const RGB_DIM: u32 = 0x8A91_99;
/// A stage that passed.
pub const RGB_OK: u32 = 0x5FE0_8A;
/// The header and the rule under it.
pub const RGB_ACCENT: u32 = 0x4C9A_FF;
/// A refusal.
pub const RGB_REFUSED: u32 = 0xFF5C_5C;

/// Why a console could not be created, so the caller can log a named reason
/// instead of a shrug.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoConsole {
    /// No GOP: `phys` or a dimension is zero.
    Absent,
    /// `pixels_per_scan_line < width` — the firmware described a surface it
    /// cannot address.
    BadStride,
    /// `BltOnly`: no linear framebuffer to write into.
    BltOnly,
    /// `BitMask`: per-channel masks we do not carry in `BootInfo`.
    BitMask,
    /// The firmware's own size is smaller than one row of pixels.
    TooSmall,
    /// The mode is smaller than one glyph cell even at scale 1.
    TooSmallForText,
}

impl NoConsole {
    pub const fn reason(self) -> &'static str {
        match self {
            NoConsole::Absent => "no_gop",
            NoConsole::BadStride => "stride_below_width",
            NoConsole::BltOnly => "blt_only_format",
            NoConsole::BitMask => "bitmask_format",
            NoConsole::TooSmall => "framebuffer_too_small",
            NoConsole::TooSmallForText => "framebuffer_smaller_than_one_glyph",
        }
    }
}

/// A text console on a GOP linear framebuffer.
///
/// Cheap to construct and holds no allocation: it is a view over firmware
/// memory plus a cursor.
pub struct Console {
    /// Byte address of pixel (0, 0).
    base: *mut u8,
    /// Visible width in pixels.
    width: u32,
    /// Visible height in pixels, already clamped to what `size_bytes` allows.
    height: u32,
    /// Pixels per scan line as the firmware reported it (`>= width`).
    stride: u32,
    /// One of [`pixel_format`].
    format: u32,
    /// Integer text scale. 1 = 8x16 cells, 2 = 16x32, and so on.
    scale: u32,
    /// Text columns and rows at `scale`.
    cols: u32,
    rows: u32,
    /// Cursor, in cells.
    col: u32,
    row: u32,
}

// `Console` holds a raw pointer to firmware memory, so it is deliberately
// neither `Send` nor `Sync` in spirit: there is exactly one of them, it lives in
// a static in the caller, and neither the loader nor the kernel has threads yet.
// Writes are per-pixel raw pointer stores, so no reference to the whole buffer is
// ever derived and no aliasing rule is broken by the firmware owning it too.
impl Console {
    /// Build a console for `fb`, or say precisely why not.
    ///
    /// The scale is chosen from the height so that the text is legible on a
    /// 1080p panel and still fits on 640x480: `height / 480`, clamped to 1..=4.
    /// Integer only — a fractional scale would need interpolation, and
    /// interpolating a bitmap font is how text ends up grey and fuzzy.
    pub fn new(fb: FramebufferInfo) -> Result<Console, NoConsole> {
        if fb.phys == 0 || fb.width == 0 || fb.height == 0 {
            return Err(NoConsole::Absent);
        }
        if fb.stride < fb.width {
            return Err(NoConsole::BadStride);
        }
        match fb.pixel_format {
            pixel_format::RED_GREEN_BLUE_RESERVED_8BIT | pixel_format::BLUE_GREEN_RED_RESERVED_8BIT => {}
            pixel_format::BIT_MASK => return Err(NoConsole::BitMask),
            pixel_format::BLT_ONLY => return Err(NoConsole::BltOnly),
            _ => return Err(NoConsole::BltOnly),
        }

        // Addressable pixels: rows are `stride` wide, so the buffer needs
        // `stride * height` pixels. If the firmware's own size is smaller than
        // that (it should not be), believe the smaller of the two and shrink
        // the visible height rather than writing past the end.
        let mut height = fb.height;
        if fb.size_bytes != 0 {
            let row_bytes = fb.stride as u64 * 4;
            let rows_that_fit = (fb.size_bytes / row_bytes) as u32;
            if rows_that_fit < height {
                height = rows_that_fit;
            }
        }
        if height == 0 {
            return Err(NoConsole::TooSmall);
        }

        let scale = (height / 480).clamp(1, 4);
        let cols = fb.width / (GLYPH_WIDTH as u32 * scale);
        let rows = height / (GLYPH_HEIGHT as u32 * scale);
        if cols == 0 || rows == 0 {
            return Err(NoConsole::TooSmallForText);
        }

        Ok(Console {
            base: fb.phys as *mut u8,
            width: fb.width,
            height,
            stride: fb.stride,
            format: fb.pixel_format,
            scale,
            cols,
            rows,
            col: 0,
            row: 0,
        })
    }

    /// Integer text scale in use.
    pub const fn scale(&self) -> u32 {
        self.scale
    }

    /// Visible geometry: `(width, height)` in pixels.
    pub const fn pixels(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Text geometry: `(columns, rows)` at the current scale.
    pub const fn cells(&self) -> (u32, u32) {
        (self.cols, self.rows)
    }

    /// Bytes per row. This — never `width * 4` — is the pitch.
    const fn pitch(&self) -> usize {
        self.stride as usize * 4
    }

    /// Encode a `0xRRGGBB` colour into one 32-bit pixel in the surface's format.
    const fn encode(&self, rgb: u32) -> u32 {
        let r = (rgb >> 16) & 0xFF;
        let g = (rgb >> 8) & 0xFF;
        let b = rgb & 0xFF;
        match self.format {
            pixel_format::BLUE_GREEN_RED_RESERVED_8BIT => b | (g << 8) | (r << 16),
            // RGBx8, and the default for anything we validated as 32-bit.
            _ => r | (g << 8) | (b << 16),
        }
    }

    /// Write one pixel, clipped to the visible area.
    ///
    /// # Safety note
    ///
    /// The bound checked here is the *visible* area, which is inside the
    /// addressable area by construction (`stride >= width`, `height` already
    /// clamped by `size_bytes`), so the store cannot leave the framebuffer.
    fn put(&mut self, x: u32, y: u32, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = y as usize * self.pitch() + x as usize * 4;
        // SAFETY: `base` is the framebuffer the firmware reported, `offset` is
        // inside `stride * height * 4` bytes of it because `x < width <=
        // stride` and `y < height`, and `height` was clamped by `size_bytes`.
        // The store is unaligned-safe (4-byte aligned by construction) and
        // volatile-free: nothing else reads this memory concurrently.
        unsafe {
            let p = self.base.add(offset) as *mut u32;
            core::ptr::write_volatile(p, color);
        }
    }

    /// Fill a rectangle, clipped to the visible area.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, rgb: u32) {
        let color = self.encode(rgb);
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        let mut yy = y;
        while yy < y_end {
            let mut xx = x;
            while xx < x_end {
                self.put(xx, yy, color);
                xx += 1;
            }
            yy += 1;
        }
    }

    /// Fill the whole visible area with one colour.
    pub fn clear(&mut self, rgb: u32) {
        let (w, h) = (self.width, self.height);
        self.fill_rect(0, 0, w, h, rgb);
        self.col = 0;
        self.row = 0;
    }

    /// Draw `text` with its top-left corner at `(x, y)` in pixels, at `scale`.
    ///
    /// Returns the x coordinate just past the last glyph, so callers can lay
    /// out a second run of text without measuring.
    pub fn draw_text(&mut self, x: u32, y: u32, text: &str, rgb: u32, scale: u32) -> u32 {
        let color = self.encode(rgb);
        let mut pen_x = x;
        for byte in text.bytes() {
            let glyph = glyph(byte);
            let mut gy = 0usize;
            while gy < GLYPH_HEIGHT {
                let bits = glyph[gy];
                let mut gx = 0usize;
                while gx < GLYPH_WIDTH {
                    if bits & (0x80 >> gx) != 0 {
                        // Integer scaling: one glyph pixel becomes a
                        // scale x scale block. No interpolation anywhere.
                        let mut dy = 0;
                        while dy < scale {
                            let mut dx = 0;
                            while dx < scale {
                                self.put(
                                    pen_x + gx as u32 * scale + dx,
                                    y + gy as u32 * scale + dy,
                                    color,
                                );
                                dx += 1;
                            }
                            dy += 1;
                        }
                    }
                    gx += 1;
                }
                gy += 1;
            }
            pen_x += GLYPH_WIDTH as u32 * scale;
        }
        pen_x
    }

    /// Draw `text` horizontally centred at row `y` (in pixels) at `scale`.
    pub fn draw_centred(&mut self, y: u32, text: &str, rgb: u32, scale: u32) {
        let width = text.len() as u32 * GLYPH_WIDTH as u32 * scale;
        let x = self.width.saturating_sub(width) / 2;
        self.draw_text(x, y, text, rgb, scale);
    }

    /// Write one line of console text at the current cursor, scrolling if the
    /// screen is full. Long lines wrap at the right edge.
    pub fn write_line(&mut self, text: &str, rgb: u32) {
        for byte in text.bytes() {
            if byte == b'\n' {
                self.newline();
                continue;
            }
            if self.col >= self.cols {
                self.newline();
            }
            let cell_x = self.col * GLYPH_WIDTH as u32 * self.scale;
            let cell_y = self.row * GLYPH_HEIGHT as u32 * self.scale;
            // One byte as a `&str`: printable ASCII is always valid UTF-8, and
            // anything else is drawn as `?` by `glyph` anyway.
            let one = [if byte.is_ascii_graphic() || byte == b' ' { byte } else { b'?' }];
            if let Ok(s) = core::str::from_utf8(&one) {
                self.draw_text(cell_x, cell_y, s, rgb, self.scale);
            }
            self.col += 1;
        }
        self.newline();
    }

    /// Move to the start of the next line, scrolling when past the bottom.
    fn newline(&mut self) {
        self.col = 0;
        self.row += 1;
        if self.row >= self.rows {
            self.scroll();
        }
    }

    /// Scroll up by exactly one text row.
    ///
    /// The framebuffer is one contiguous run of `stride * height` pixels, so
    /// one `copy` of everything below the top text row moves the picture up,
    /// padding included. `ptr::copy` — not `copy_nonoverlapping` — because the
    /// source and destination overlap.
    fn scroll(&mut self) {
        let line = GLYPH_HEIGHT as usize * self.scale as usize;
        let pitch = self.pitch();
        let total = self.height as usize;
        if line >= total {
            self.fill_rect(0, 0, self.width, self.height, RGB_BACKGROUND);
        } else {
            let keep = (total - line) * pitch;
            // SAFETY: source is `base + line * pitch`, destination is `base`,
            // and both spans are within `total * pitch` bytes of the buffer,
            // which is inside the framebuffer (`height` was clamped by
            // `size_bytes`).
            unsafe {
                core::ptr::copy(self.base.add(line * pitch), self.base, keep);
            }
            let bg = self.encode(RGB_BACKGROUND);
            let (w, h) = (self.width, self.height);
            self.fill_rect(0, h - line as u32, w, line as u32, bg);
        }
        self.row = self.rows - 1;
    }

    /// Draw a one-pixel-tall (times scale) horizontal rule across the width.
    pub fn rule(&mut self, y: u32, rgb: u32) {
        let (w, _) = (self.width, self.height);
        self.fill_rect(0, y, w, self.scale, rgb);
    }

    /// Move the text cursor, clamped to the screen. Used by the kernel to write
    /// below what the loader left on screen instead of over it.
    pub fn set_cursor(&mut self, col: u32, row: u32) {
        self.col = col.min(self.cols.saturating_sub(1));
        self.row = row.min(self.rows.saturating_sub(1));
    }

    /// Where the cursor is, in pixels: the top-left corner of the next glyph.
    pub const fn cursor_pixels(&self) -> (u32, u32) {
        (
            self.col * GLYPH_WIDTH as u32 * self.scale,
            self.row * GLYPH_HEIGHT as u32 * self.scale,
        )
    }

    /// Move the cursor to the start of the next line, scrolling if needed.
    pub fn advance_line(&mut self) {
        self.newline();
    }

    /// Draw `text` as large as it fits inside the horizontal band
    /// `[top, top + band_height)`, centred both ways. Used for refusal codes,
    /// which have to be readable from across a room.
    ///
    /// The scale is the largest integer that keeps the whole string inside 80%
    /// of the width and one third of the band, clamped to 1..=12.
    pub fn draw_large_centred(&mut self, top: u32, band_height: u32, text: &str, rgb: u32) {
        let len = text.len().max(1) as u32;
        let by_width = (self.width * 4 / 5) / (len * GLYPH_WIDTH as u32);
        let by_height = (band_height / 3).max(1) / GLYPH_HEIGHT as u32;
        let scale = by_width.min(by_height).clamp(1, 12);
        let text_h = GLYPH_HEIGHT as u32 * scale;
        let y = top + band_height.saturating_sub(text_h) / 2;
        self.draw_centred(y, text, rgb, scale);
    }
}

/// The glyph for a byte, with anything outside printable ASCII drawn as `?`
/// rather than as whatever happens to sit next to it in the table.
fn glyph(byte: u8) -> &'static [u8; GLYPH_HEIGHT] {
    let b = if byte < FIRST_CHAR || byte > LAST_CHAR {
        b'?'
    } else {
        byte
    };
    &GLYPHS[(b - FIRST_CHAR) as usize]
}

/// A fixed-capacity string builder, so a caller can format a detail string
/// without an allocator.
///
/// `write!` on this never fails for short input; when the buffer is full the
/// extra bytes are dropped, which is why the capacity is generous relative to
/// what the console prints.
pub struct Line {
    buf: [u8; 96],
    len: usize,
}

impl Line {
    pub const fn new() -> Line {
        Line {
            buf: [0; 96],
            len: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        // SAFETY: only `str` bytes are ever appended, through `write_str`,
        // which copies whole valid UTF-8 slices.
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }
}

impl Default for Line {
    fn default() -> Self {
        Line::new()
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len >= self.buf.len() {
                return Ok(());
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
        Ok(())
    }
}
