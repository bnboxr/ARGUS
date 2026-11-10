//! The on-screen boot console, shared by the loader and the kernel.
//!
//! Serial is the machine-readable log and stays exactly as it was; this is the
//! same story told to the owner's eyes, because a laptop has no COM1. Both
//! outputs are always written — neither ever replaces the other.
//!
//! One screen, one static, no threads: both binaries run single-threaded, so a
//! global console is the honest model. Every function here is a no-op if the
//! firmware offered no usable framebuffer, and says nothing about it — the
//! serial log already carries the `stage=gop` verdict, and a screen cannot
//! report its own absence.

use crate::bootinfo::FramebufferInfo;
use crate::fbcon::{
    Console, NoConsole, RGB_ACCENT, RGB_BACKGROUND, RGB_DIM, RGB_OK, RGB_REFUSED, RGB_TEXT,
};

/// The screen, once we have one.
static mut CONSOLE: Option<Console> = None;

/// Access the console, if the firmware gave us one.
///
/// # Safety
///
/// Single-threaded by construction, and each borrow is confined to one call, so
/// no two `&mut` to the console are ever live at the same time.
fn with<R>(f: impl FnOnce(&mut Console) -> R) -> Option<R> {
    // SAFETY: see above — one thread, borrows do not overlap.
    let slot = unsafe { &mut *core::ptr::addr_of_mut!(CONSOLE) };
    slot.as_mut().map(f)
}

/// Attach the console to a framebuffer, or return why there is none.
///
/// Nothing is painted here: the loader paints the header and the kernel must not
/// erase what the loader left behind.
pub fn attach(fb: FramebufferInfo) -> Result<(), NoConsole> {
    let console = Console::new(fb)?;
    // SAFETY: single thread; the borrow ends with this statement.
    unsafe {
        *core::ptr::addr_of_mut!(CONSOLE) = Some(console);
    }
    Ok(())
}

/// Paint the screen from scratch: background, header, mode line, rule.
///
/// The header is the product name and the phase, exactly as the owner asked to
/// see it. The mode line on the right says which resolution this is, so a
/// screenshot identifies itself without the serial log next to it.
pub fn header() {
    with(|c| {
        c.clear(RGB_BACKGROUND);
        let scale = c.scale();
        let margin = 8 * scale;
        let top = 12 * scale;
        let (width, _) = c.pixels();
        let (cols, rows) = c.cells();

        c.draw_text(margin, top, "ARGUS v0.1 phase-I", RGB_ACCENT, scale);

        let mut mode = crate::fbcon::Line::new();
        use core::fmt::Write;
        let _ = write!(mode, "{}x{}  {}x{} cells  scale {}", width, c.pixels().1, cols, rows, scale);
        let mode_w = mode.as_str().len() as u32 * 8 * scale;
        let right = width.saturating_sub(mode_w + margin);
        c.draw_text(right, top, mode.as_str(), RGB_DIM, scale);

        c.rule(top + 20 * scale, RGB_ACCENT);
        c.set_cursor(0, 3);
    });
}

/// Print one completed stage: `[ ok ] <label>   <detail>`.
pub fn stage(label: &str, detail: &str) {
    with(|c| {
        let scale = c.scale();
        let margin = 8 * scale;
        let y = c.cursor_pixels().1;
        c.draw_text(margin, y, "[ ok ]", RGB_OK, scale);
        c.draw_text(margin + 7 * 8 * scale, y, label, RGB_TEXT, scale);
        if !detail.is_empty() {
            let label_w = label.len() as u32 * 8 * scale;
            c.draw_text(
                margin + 7 * 8 * scale + label_w + 3 * 8 * scale,
                y,
                detail,
                RGB_DIM,
                scale,
            );
        }
        c.advance_line();
    });
}

/// Print a line that is not a pass/fail pair: a note, a heading, a warning.
pub fn note(text: &str, rgb: u32) {
    with(|c| {
        let scale = c.scale();
        let margin = 8 * scale;
        let y = c.cursor_pixels().1;
        c.draw_text(margin, y, text, rgb, scale);
        c.advance_line();
    });
}

/// Print a line in the "this really is the kernel" colour, large, at the bottom
/// of the screen — below whatever the loader wrote, never over it.
pub fn alive(text: &str) {
    with(|c| {
        let scale = c.scale();
        let (_, rows) = c.cells();
        c.set_cursor(0, rows.saturating_sub(2));
        c.advance_line();
        let y = c.cursor_pixels().1;
        c.draw_text(8 * scale, y, text, RGB_OK, scale * 2);
        c.advance_line();
    });
}

/// Render a refusal code large and centred, and leave it there.
///
/// The stage log above stays visible on purpose: the code says *what* was
/// refused, the lines above say *where*. This is the last thing drawn, so it is
/// what the screen shows until the machine is power-cycled.
pub fn refusal(code: &str) {
    with(|c| {
        let scale = c.scale();
        let (width, height) = c.pixels();
        let band_top = height * 45 / 100;
        let band_height = height - band_top;
        // Clear the band so the code does not fight the log for legibility.
        c.fill_rect(0, band_top, width, band_height, RGB_BACKGROUND);
        c.rule(band_top, RGB_REFUSED);
        c.draw_centred(band_top + 10 * scale, "BOOT REFUSED", RGB_REFUSED, scale * 2);
        c.draw_large_centred(
            band_top + 30 * scale,
            band_height - 30 * scale,
            code,
            RGB_REFUSED,
        );
        c.draw_centred(
            band_top + band_height - 20 * scale,
            "the boot stopped here; the serial log has the full trace",
            RGB_DIM,
            scale,
        );
    });
}
