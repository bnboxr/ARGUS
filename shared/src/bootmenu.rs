//! The boot manager screen: the layout measured in
//! `shared/design/boot-manager-spec-v1.md`, drawn into the GOP framebuffer.
//!
//! This is a *second* screen, selected by a file on the ESP
//! (`\ARGUS\BOOTMENU.CFG`). The boot log console in [`crate::screen`] is not
//! touched by any of this: if the file is absent, not one line of this module
//! runs, and the loader boots exactly as it did before.
//!
//! Three decisions worth knowing before reading the code:
//!
//! 1. **Text comes from a host-generated atlas** (`bootmenu_atlas.bin`, written
//!    by `tools/gen-bootmenu-atlas.py` and compiled in). There is no font
//!    rasteriser here, because spec §7 costs one at 1-3 ms *per glyph* on a
//!    loader with no GPU. A glyph here is an alpha blit and nothing else.
//!
//! 2. **The bloom is baked, once.** The halo around the emblem is the most
//!    expensive effect on the screen (~200 ms per frame if computed per frame,
//!    spec §7), so it is computed here exactly once — the emblem mask is blurred
//!    at a quarter of the screen scale and added into the background — and it is
//!    never recomputed. `background()` and the foreground functions are separate
//!    for that reason, not for an animation.
//!
//! 3. **Geometry is absolute, at 1920x1080.** The spec is measured in pixels at
//!    that size; scaling it to another mode would move every number away from
//!    the reference. On any other mode the loader says so on serial and keeps
//!    its log console.
//!
//! Colours are the measured tokens from spec §2, one constant each, so a value
//! can be re-checked against the reference PNG by name.

use crate::bootinfo::{pixel_format, FramebufferInfo};

/// The file on the ESP that turns this screen on.
///
/// Its *presence* selects the boot menu; its contents are not read. (There is
/// nothing yet that a file could usefully configure here, and inventing options
/// that do nothing would be worse than having none.)
pub const CFG_PATH: &str = "\\ARGUS\\BOOTMENU.CFG";

/// The one canvas this screen is measured for (spec §1).
const CANVAS_W: u32 = 1920;
const CANVAS_H: u32 = 1080;

// ---- colour tokens (spec §2, each with its sampling coordinate there) -------
const BG_BASE: u32 = 0x02_06_0D; // 20,20
const BG_GLOW: u32 = 0x07_29_47; // 1440,540
const RULE: u32 = 0x0A_2E_3D; // 200,92
const RULE_DIM: u32 = 0x09_2A_3A; // 1720,92
const RULE_DIM_BOTTOM: u32 = 0x09_2A_38; // 1720,999
const DOT_LEFT: u32 = 0x2A_78_88; // 50,92
const DOT_RIGHT: u32 = 0x18_45_52; // 1858,92
const TICK_BRIGHT: u32 = 0x23_AA_DD; // 1510,100
const TICK_RAMP_LO: u32 = 0x08_1E_2C; // 1620,960
const TICK_RAMP_HI: u32 = 0x0B_2D_3E; // 1795,965
const PANEL_BORDER: u32 = 0x0E_2D_3A; // 104,500
const PANEL_FILL: u32 = 0x03_0C_17; // 400,700
const TAB_RULE: u32 = 0x1F_6F_8B; // 170,315
const BRACKET_UPPER: u32 = 0x2C_AA_CB; // 106,345
const BRACKET_LOWER: u32 = 0x21_7F_99; // 106,720
const BAR_RIGHT: u32 = 0x18_5B_73; // 750,750
const HAIRLINE: u32 = 0x0A_24_33; // 400,262
const GRID_V: u32 = 0x05_13_20; // 1060,700
const GRID_H: u32 = 0x05_12_1F; // 1200,270
/// The rings are 1 px on a gradient and too faint to sample to a hex (spec §8
/// states exactly this); this is the background lifted by the +9..+12 lum the
/// local-contrast measurement gives them.
const RING: u32 = 0x1C_5A_72;
const SELECT_FILL: u32 = 0x17_75_9B; // 300,390
const SELECT_EDGE: u32 = 0x41_D5_F8; // 134,400
const ACCENT: u32 = 0x38_BB_DE; // 279,177
const ACCENT_LABEL: u32 = 0x3A_B0_E0; // label ink
const TEXT_WORDMARK: u32 = 0xDA_F6_FF; // wordmark ink
const TEXT_SECTION: u32 = 0x88_C4_DA; // 222..236
const TEXT_ROW: u32 = 0xA2_CC_DE; // row 2 ink
const TEXT_ROW_SELECTED: u32 = 0xEF_FE_FF; // row 1 ink
const TEXT_HINT: u32 = 0x89_B4_C8; // key-hint ink
const TEXT_FOOTER: u32 = 0x56_94_AF; // footer ink
const TEXT_CAPTION: u32 = 0x49_A1_BA; // caption ink
const MARK_FILL: u32 = 0xD8_FF_FF; // emblem ink (range D5FFFF..DAFFFF)

/// Why the boot menu was not drawn. Every variant is logged by the loader; none
/// of them stops a boot.
#[derive(Clone, Copy)]
pub enum Skip {
    NoFramebuffer,
    BitMaskFormat,
    BadStride,
    NotMeasuredMode,
    AtlasUnusable,
}

impl Skip {
    pub const fn reason(self) -> &'static str {
        match self {
            Skip::NoFramebuffer => "no_framebuffer",
            Skip::BitMaskFormat => "bit_mask_format",
            Skip::BadStride => "stride_below_width",
            Skip::NotMeasuredMode => "mode_is_not_1920x1080",
            Skip::AtlasUnusable => "atlas_unusable",
        }
    }
}

// ---------------------------------------------------------------------------
// The glyph atlas
// ---------------------------------------------------------------------------

const STYLE_COUNT: usize = 7;
const WORDMARK: usize = 0;
const SUBTITLE: usize = 1;
const SECTION: usize = 2;
const LABEL: usize = 3;
const ROW: usize = 4;
const HINT: usize = 5;
const FOOTER: usize = 6;
const GLYPH_RECORD: usize = 16;

/// The generated atlas, compiled in. `tools/gen-bootmenu-atlas.py` writes it.
static ATLAS_BYTES: &[u8] = include_bytes!("bootmenu_atlas.bin");
const ATLAS_MAGIC: &[u8; 6] = b"ARGBA1";

fn read_u16(bytes: &[u8], at: usize) -> u32 {
    (bytes[at] as u32) | ((bytes[at + 1] as u32) << 8)
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    (bytes[at] as u32)
        | ((bytes[at + 1] as u32) << 8)
        | ((bytes[at + 2] as u32) << 16)
        | ((bytes[at + 3] as u32) << 24)
}

/// One glyph, as the atlas stores it.
#[derive(Clone, Copy)]
struct Glyph {
    /// Ink offset from the pen origin, in pixels.
    dx: i32,
    /// Ink offset from the baseline (up is negative).
    dy: i32,
    width: u32,
    height: u32,
    advance: i32,
    /// Byte offset into the atlas blob.
    offset: usize,
}

/// One style: a cap height, the reference's advance ÷ cap, and its glyphs.
#[derive(Clone, Copy)]
struct Style {
    cap: u32,
    /// advance ÷ cap × 1000, from spec §3. Tracking is derived from it, so a
    /// rendered string keeps the reference's width-to-cap ratio whatever face
    /// the atlas was built from.
    advance_ratio: u32,
    first: usize,
    count: usize,
}

struct Atlas {
    styles: [Style; STYLE_COUNT],
    blob: &'static [u8],
}

impl Atlas {
    fn parse() -> Option<Atlas> {
        let bytes = ATLAS_BYTES;
        if bytes.len() < 16 || &bytes[..6] != ATLAS_MAGIC {
            return None;
        }
        if read_u16(bytes, 8) as usize != STYLE_COUNT {
            return None;
        }
        let mut styles = [Style {
            cap: 0,
            advance_ratio: 0,
            first: 0,
            count: 0,
        }; STYLE_COUNT];
        let mut at = 12;
        for slot in styles.iter_mut() {
            if at + 8 > bytes.len() {
                return None;
            }
            slot.cap = read_u16(bytes, at);
            slot.count = read_u16(bytes, at + 4) as usize;
            slot.advance_ratio = read_u16(bytes, at + 6);
            slot.first = at + 8;
            at += 8 + slot.count * GLYPH_RECORD;
            if at > bytes.len() {
                return None;
            }
        }
        if at + 4 > bytes.len() {
            return None;
        }
        let blob_len = read_u32(bytes, at) as usize;
        let blob_start = at + 4;
        if blob_start + blob_len > bytes.len() {
            return None;
        }
        Some(Atlas {
            styles,
            blob: &bytes[blob_start..blob_start + blob_len],
        })
    }

    /// The glyph for `ch`, or `None` when the atlas does not carry it.
    fn glyph(&self, style: usize, ch: char) -> Option<Glyph> {
        let style = &self.styles[style];
        // The generator writes printable ASCII in order, then the two arrows.
        let index = match ch as u32 {
            cp @ 0x20..=0x7E => (cp - 0x20) as usize,
            0x2191 => 95,
            0x2193 => 96,
            _ => return None,
        };
        if index >= style.count {
            return None;
        }
        let at = style.first + index * GLYPH_RECORD;
        let bytes = ATLAS_BYTES;
        Some(Glyph {
            dx: read_u16(bytes, at + 6) as i16 as i32,
            dy: read_u16(bytes, at + 8) as i16 as i32,
            width: read_u16(bytes, at + 2),
            height: read_u16(bytes, at + 4),
            advance: read_u16(bytes, at + 10) as i16 as i32,
            offset: read_u32(bytes, at + 12) as usize,
        })
    }

    fn alpha(&self, glyph: &Glyph, row: u32, col: u32) -> u8 {
        let at = glyph.offset + (row * glyph.width + col) as usize;
        self.blob.get(at).copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Surface: pixels only. No colours above, no layout below.
// ---------------------------------------------------------------------------

struct Surface {
    base: *mut u8,
    width: u32,
    height: u32,
    stride: u32,
    bgr: bool,
}

impl Surface {
    fn new(fb: FramebufferInfo) -> Result<Surface, Skip> {
        if fb.phys == 0 || fb.width == 0 || fb.height == 0 {
            return Err(Skip::NoFramebuffer);
        }
        if fb.stride < fb.width {
            return Err(Skip::BadStride);
        }
        let bgr = match fb.pixel_format {
            pixel_format::RED_GREEN_BLUE_RESERVED_8BIT => false,
            pixel_format::BLUE_GREEN_RED_RESERVED_8BIT => true,
            _ => return Err(Skip::BitMaskFormat),
        };
        let mut height = fb.height;
        if fb.size_bytes != 0 {
            let row_bytes = fb.stride as u64 * 4;
            let rows_that_fit = (fb.size_bytes / row_bytes) as u32;
            if rows_that_fit < height {
                height = rows_that_fit;
            }
        }
        if height == 0 {
            return Err(Skip::NoFramebuffer);
        }
        Ok(Surface {
            base: fb.phys as *mut u8,
            width: fb.width,
            height,
            stride: fb.stride,
            bgr,
        })
    }

    #[inline]
    fn encode(&self, rgb: u32) -> u32 {
        let r = (rgb >> 16) & 0xFF;
        let g = (rgb >> 8) & 0xFF;
        let b = rgb & 0xFF;
        if self.bgr {
            b | (g << 8) | (r << 16)
        } else {
            r | (g << 8) | (b << 16)
        }
    }

    #[inline]
    fn offset(&self, x: u32, y: u32) -> usize {
        (y as usize * self.stride as usize + x as usize) * 4
    }

    #[inline]
    fn put(&self, x: u32, y: u32, rgb: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let value = self.encode(rgb);
        // SAFETY: `offset` is inside the surface for any in-range (x, y), and the
        // firmware handed us this range as a writable linear framebuffer.
        unsafe {
            core::ptr::write_unaligned(self.base.add(self.offset(x, y)) as *mut u32, value);
        }
    }

    #[inline]
    fn get(&self, x: u32, y: u32) -> u32 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        // SAFETY: as `put`.
        let value =
            unsafe { core::ptr::read_unaligned(self.base.add(self.offset(x, y)) as *const u32) };
        // Undo `encode` exactly: in RGBx8 the red byte is the low one, in BGRx8
        // it is the high one, so one of the two has to be swapped back and the
        // other is already 0x00RRGGBB. Getting this backwards blends against a
        // colour with red and blue exchanged — visible only where alpha is used.
        if self.bgr {
            value & 0x00FF_FFFF
        } else {
            ((value & 0xFF) << 16) | (value & 0xFF00) | ((value >> 16) & 0xFF)
        }
    }

    /// Blend `rgb` over the pixel with 8-bit coverage `a`.
    #[inline]
    fn blend(&self, x: u32, y: u32, rgb: u32, a: u32) {
        if a == 0 {
            return;
        }
        if a >= 255 {
            self.put(x, y, rgb);
            return;
        }
        let dst = self.get(x, y);
        let mix = |shift: u32| -> u32 {
            let d = (dst >> shift) & 0xFF;
            let s = (rgb >> shift) & 0xFF;
            (d * (255 - a) + s * a) / 255
        };
        self.put(x, y, (mix(16) << 16) | (mix(8) << 8) | mix(0));
    }

    /// Filled rectangle, `x0..x1` and `y0..y1` inclusive (spec §1 convention).
    fn rect(&self, x0: i32, y0: i32, x1: i32, y1: i32, rgb: u32) {
        if x1 < x0 || y1 < y0 {
            return;
        }
        for y in y0.max(0)..=y1.min(self.height as i32 - 1) {
            for x in x0.max(0)..=x1.min(self.width as i32 - 1) {
                self.put(x as u32, y as u32, rgb);
            }
        }
    }

    /// Filled circle with a one-pixel analytic edge, centre in pixel units.
    ///
    /// Coverage comes from the first-order expansion of the circle equation,
    /// `d - r ≈ (d² - r²) / 2r`, which needs no square root — `core` has none.
    fn disc(&self, cx: f64, cy: f64, r: f64, rgb: u32) {
        let x0 = floor_i(cx - r - 1.0);
        let x1 = ceil_i(cx + r + 1.0);
        let y0 = floor_i(cy - r - 1.0);
        let y1 = ceil_i(cy + r + 1.0);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f64 + 0.5 - cx;
                let dy = y as f64 + 0.5 - cy;
                let d2 = dx * dx + dy * dy;
                let coverage = 0.5 - (d2 - r * r) / (2.0 * r);
                if coverage <= 0.0 {
                    continue;
                }
                let a = (coverage.min(1.0) * 255.0) as u32;
                self.blend(x as u32, y as u32, rgb, a);
            }
        }
    }

    /// One-pixel ellipse outline, anti-aliased, over an arc range in degrees
    /// (0 = +x, 90 = down; a full 0..360 draws the closed ellipse).
    fn ellipse_arc(
        &self,
        cx: f64,
        cy: f64,
        rx: f64,
        ry: f64,
        rgb: u32,
        alpha: u32,
        from_deg: f64,
        to_deg: f64,
    ) {
        let x0 = floor_i(cx - rx - 1.0);
        let x1 = ceil_i(cx + rx + 1.0);
        let y0 = floor_i(cy - ry - 1.0);
        let y1 = ceil_i(cy + ry + 1.0);
        let r_eff = (rx + ry) * 0.5;
        let full = (to_deg - from_deg).abs() >= 359.0;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = (x as f64 + 0.5 - cx) / rx;
                let dy = (y as f64 + 0.5 - cy) / ry;
                let q = dx * dx + dy * dy;
                let coverage = 0.5 - ((q - 1.0) * r_eff * 0.5).abs();
                if coverage <= 0.0 {
                    continue;
                }
                if !full {
                    let mut deg = atan2_deg(dy, dx);
                    if deg < 0.0 {
                        deg += 360.0;
                    }
                    let from = wrap360(from_deg);
                    let to = wrap360(to_deg);
                    let inside = if from <= to {
                        deg >= from && deg <= to
                    } else {
                        deg >= from || deg <= to
                    };
                    if !inside {
                        continue;
                    }
                }
                let a = (coverage.min(1.0) * alpha as f64) as u32;
                self.blend(x as u32, y as u32, rgb, a);
            }
        }
    }

    /// Signed distance from a pixel centre to a rounded rectangle's edge.
    fn rounded_rect_sdf(
        &self,
        x: i32,
        y: i32,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        radius: f64,
    ) -> f64 {
        let cx = (x0 as f64 + x1 as f64 + 1.0) * 0.5;
        let cy = (y0 as f64 + y1 as f64 + 1.0) * 0.5;
        let hx = (x1 - x0 + 1) as f64 * 0.5 - radius;
        let hy = (y1 - y0 + 1) as f64 * 0.5 - radius;
        let qx = (x as f64 + 0.5 - cx).abs() - hx;
        let qy = (y as f64 + 0.5 - cy).abs() - hy;
        let ax = qx.max(0.0);
        let ay = qy.max(0.0);
        approx_sqrt(ax * ax + ay * ay) + qx.max(qy).min(0.0) - radius
    }

    /// Rounded rectangle, filled.
    fn rounded_rect(&self, x0: i32, y0: i32, x1: i32, y1: i32, radius: f64, rgb: u32) {
        for y in y0.max(0)..=y1.min(self.height as i32 - 1) {
            for x in x0.max(0)..=x1.min(self.width as i32 - 1) {
                let sdf = self.rounded_rect_sdf(x, y, x0, y0, x1, y1, radius);
                let coverage = 0.5 - sdf;
                if coverage <= 0.0 {
                    continue;
                }
                self.blend(x as u32, y as u32, rgb, (coverage.min(1.0) * 255.0) as u32);
            }
        }
    }

    /// Rounded rectangle outline of `width` pixels, following the same edge.
    fn rounded_rect_border(
        &self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        radius: f64,
        width: f64,
        rgb: u32,
    ) {
        for y in y0.max(0)..=y1.min(self.height as i32 - 1) {
            for x in x0.max(0)..=x1.min(self.width as i32 - 1) {
                let outer = self.rounded_rect_sdf(x, y, x0, y0, x1, y1, radius);
                let inner =
                    self.rounded_rect_sdf(x, y, x0, y0, x1, y1, radius) + width;
                let coverage = (0.5 - outer).clamp(0.0, 1.0)
                    - (0.5 - inner).clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                self.blend(x as u32, y as u32, rgb, (coverage.min(1.0) * 255.0) as u32);
            }
        }
    }

    /// Draw a string with its ink starting at `x_ink` and its baseline at
    /// `baseline`. Returns the ink width.
    fn text(
        &self,
        atlas: &Atlas,
        style: usize,
        x_ink: i32,
        baseline: i32,
        text: &str,
        rgb: u32,
    ) -> i32 {
        // Tracking: the reference's advance ÷ cap is the one number that
        // survives a face change (spec §3), so the space between glyphs is
        // adjusted until this string's mean advance matches it.
        let mut count = 0i32;
        let mut advance_sum = 0i32;
        for ch in text.chars() {
            if let Some(g) = atlas.glyph(style, ch) {
                advance_sum += g.advance;
                count += 1;
            }
        }
        if count == 0 {
            return 0;
        }
        let cap = atlas.styles[style].cap as i32;
        let target = (atlas.styles[style].advance_ratio as i32 * cap) / 1000;
        let tracking = target - advance_sum / count;

        let Some(first) = text.chars().next().and_then(|ch| atlas.glyph(style, ch)) else {
            return 0;
        };
        let mut pen = x_ink - first.dx;
        let start = pen;
        for ch in text.chars() {
            let Some(glyph) = atlas.glyph(style, ch) else {
                continue;
            };
            for row in 0..glyph.height {
                for col in 0..glyph.width {
                    let a = atlas.alpha(&glyph, row, col) as u32;
                    if a == 0 {
                        continue;
                    }
                    let x = pen + glyph.dx + col as i32;
                    let y = baseline + glyph.dy + row as i32;
                    if x < 0 || y < 0 {
                        continue;
                    }
                    self.blend(x as u32, y as u32, rgb, a);
                }
            }
            pen += glyph.advance + tracking;
        }
        pen - tracking - start
    }
}

/// `atan2` in degrees without `core`'s missing float intrinsics.
///
/// Only used to decide whether a pixel belongs to a ring's arc, so a polynomial
/// approximation good to a fraction of a degree is far more than enough.
fn atan2_deg(y: f64, x: f64) -> f64 {
    if x == 0.0 && y == 0.0 {
        return 0.0;
    }
    let ax = x.abs();
    let ay = y.abs();
    let (num, den) = if ax >= ay { (ay, ax) } else { (ax, ay) };
    let angle = if den == 0.0 { 0.0 } else { num / den };
    let a2 = angle * angle;
    // atan on [0,1] — Abramowitz & Stegun 4.4.49, error < 0.02 rad.
    let mut result = angle
        * (0.999_866_3
            + a2 * (-0.332_623_5
                + a2 * (0.193_778_6 + a2 * (-0.107_386_3 + a2 * 0.038_250_6))));
    if ay > ax {
        result = core::f64::consts::FRAC_PI_2 - result;
    }
    if x < 0.0 {
        result = core::f64::consts::PI - result;
    }
    let degrees = result * (180.0 / core::f64::consts::PI);
    if y < 0.0 {
        -degrees
    } else {
        degrees
    }
}

/// `floor` for the box bounds: casting truncates toward zero, so a negative
/// value needs one step down. `core` has no `f64::floor`.
fn floor_i(v: f64) -> i32 {
    let t = v as i32;
    if (t as f64) > v {
        t - 1
    } else {
        t
    }
}

/// `ceil`, for the same reason as [`floor_i`].
fn ceil_i(v: f64) -> i32 {
    let t = v as i32;
    if (t as f64) < v {
        t + 1
    } else {
        t
    }
}

/// Bring an angle in degrees into `0..360` without `f64::rem_euclid`.
fn wrap360(v: f64) -> f64 {
    let mut r = v;
    while r < 0.0 {
        r += 360.0;
    }
    while r >= 360.0 {
        r -= 360.0;
    }
    r
}

/// Square root by Newton iteration.
fn approx_sqrt(v: f64) -> f64 {
    if v <= 0.0 {
        return 0.0;
    }
    let mut guess = v * 0.5 + 0.5;
    for _ in 0..16 {
        guess = 0.5 * (guess + v / guess);
    }
    guess
}

fn lerp(a: u32, b: u32, t: f64) -> u32 {
    let ch = |shift: u32| -> u32 {
        let av = (a >> shift) & 0xFF;
        let bv = (b >> shift) & 0xFF;
        let mixed = av as f64 + (bv as f64 - av as f64) * t;
        mixed.clamp(0.0, 255.0) as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

// ---------------------------------------------------------------------------
// The emblem: a flat placeholder inside the measured 383x500 box.
// ---------------------------------------------------------------------------

const EMBLEM_X0: i32 = 1207;
const EMBLEM_Y0: i32 = 226;
const EMBLEM_W: u32 = 383;
const EMBLEM_H: u32 = 500;

/// A flat letter `A`, drawn from three stroked segments.
///
/// The owner has not chosen the final mark, so this is deliberately a
/// placeholder: one colour, no gradient, no bevel, no iris. It fills the box the
/// measured mark occupies, and nothing about it claims to be the ARGUS mark.
fn emblem_coverage(x: i32, y: i32) -> f64 {
    // The stroke's half-width is 31 px, so every centre line stays 31 px inside
    // the box: the mark must not touch the box edge, or the bloom's mask would
    // have ink on its border and the halo would be clipped into a rectangle.
    let apex_x = 1398.0;
    let apex_y = 262.0;
    let foot_y = 690.0;
    let left_x = 1250.0;
    let right_x = 1546.0;
    let stroke = 62.0;
    let cross_y = 545.0;

    let px = x as f64 + 0.5;
    let py = y as f64 + 0.5;
    let mut best = distance_to_segment(px, py, apex_x, apex_y, left_x, foot_y);
    let right = distance_to_segment(px, py, apex_x, apex_y, right_x, foot_y);
    if right < best {
        best = right;
    }
    // The crossbar spans between the two legs at its own height, so it meets
    // them instead of floating past them.
    let t = (cross_y - apex_y) / (foot_y - apex_y);
    let lx = apex_x + (left_x - apex_x) * t;
    let rx = apex_x + (right_x - apex_x) * t;
    let bar = distance_to_segment(px, py, lx, cross_y, rx, cross_y);
    if bar < best {
        best = bar;
    }

    (stroke * 0.5 + 0.5 - best).clamp(0.0, 1.0)
}

fn distance_to_segment(px: f64, py: f64, x0: f64, y0: f64, x1: f64, y1: f64) -> f64 {
    let vx = x1 - x0;
    let vy = y1 - y0;
    let wx = px - x0;
    let wy = py - y0;
    let len2 = vx * vx + vy * vy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0)
    };
    let dx = wx - vx * t;
    let dy = wy - vy * t;
    approx_sqrt(dx * dx + dy * dy)
}

/// Scratch for the emblem mask and its quarter-scale blur, in `.bss`.
///
/// The blur is the one expensive thing on this screen (spec §7: ~200 ms per
/// frame if done per frame), so it is done once into these buffers.
static mut MASK: [u8; (EMBLEM_W * EMBLEM_H) as usize] = [0; (EMBLEM_W * EMBLEM_H) as usize];
/// The blur grid is padded by a quarter of the halo's own reach, so the glow
/// has somewhere to decay to nothing. Without the padding the grid's edge cells
/// hold the emblem's ink and the clamped sampling turns the halo into a
/// rectangle with hard edges exactly at the sample box.
const BLUR_PAD: usize = 24;
const BLUR_W: usize = 96 + 2 * BLUR_PAD;
const BLUR_H: usize = 125 + 2 * BLUR_PAD;
static mut BLUR_A: [u8; BLUR_W * BLUR_H] = [0; BLUR_W * BLUR_H];
static mut BLUR_B: [u8; BLUR_W * BLUR_H] = [0; BLUR_W * BLUR_H];

/// Rasterise the emblem into `MASK`, then blur a quarter-scale copy of it.
///
/// Three box passes are a Gaussian to within a few percent, and radius 5 at
/// quarter scale is 20 screen pixels, which puts the halo's visible edge at the
/// ~56 px the reference measures (spec §1.12). Called once per paint.
fn build_bloom() {
    // SAFETY: single-threaded, called once from `paint` before anything reads
    // these buffers; no reference to them outlives this function except the raw
    // pointers used inside it.
    let mask = core::ptr::addr_of_mut!(MASK) as *mut u8;
    for y in 0..EMBLEM_H {
        for x in 0..EMBLEM_W {
            let coverage = emblem_coverage(EMBLEM_X0 + x as i32, EMBLEM_Y0 + y as i32);
            // SAFETY: (x, y) are inside the mask.
            unsafe {
                *mask.add((y * EMBLEM_W + x) as usize) = (coverage * 255.0) as u8;
            }
        }
    }

    let a = core::ptr::addr_of_mut!(BLUR_A) as *mut u8;
    let b = core::ptr::addr_of_mut!(BLUR_B) as *mut u8;

    // Downscale 4x by averaging, so the halo is cheap to blur and stays smooth.
    // The emblem lands in the padded middle of the grid; the padding stays zero.
    for by in 0..BLUR_H {
        for bx in 0..BLUR_W {
            let mut sum = 0u32;
            let mut n = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let gx = bx as i32 - BLUR_PAD as i32;
                    let gy = by as i32 - BLUR_PAD as i32;
                    if gx < 0 || gy < 0 {
                        continue;
                    }
                    let x = gx as usize * 4 + sx;
                    let y = gy as usize * 4 + sy;
                    if x < EMBLEM_W as usize && y < EMBLEM_H as usize {
                        // SAFETY: inside the mask, as above.
                        sum += unsafe { *mask.add(y * EMBLEM_W as usize + x) } as u32;
                        n += 1;
                    }
                }
            }
            // SAFETY: inside the blur grid.
            unsafe {
                *a.add(by * BLUR_W + bx) = (sum / n.max(1)) as u8;
            }
        }
    }

    let radius = 5i32;
    let mut source = a;
    let mut target = b;
    for _ in 0..3 {
        // Horizontal pass.
        for y in 0..BLUR_H {
            for x in 0..BLUR_W {
                let mut sum = 0u32;
                let mut n = 0u32;
                let mut dx = -radius;
                while dx <= radius {
                    let sx = x as i32 + dx;
                    if sx >= 0 && sx < BLUR_W as i32 {
                        // SAFETY: inside the blur grid.
                        sum += unsafe { *source.add(y * BLUR_W + sx as usize) } as u32;
                        n += 1;
                    }
                    dx += 1;
                }
                // SAFETY: inside the blur grid.
                unsafe {
                    *target.add(y * BLUR_W + x) = (sum / n.max(1)) as u8;
                }
            }
        }
        core::mem::swap(&mut source, &mut target);
        // Vertical pass.
        for y in 0..BLUR_H {
            for x in 0..BLUR_W {
                let mut sum = 0u32;
                let mut n = 0u32;
                let mut dy = -radius;
                while dy <= radius {
                    let sy = y as i32 + dy;
                    if sy >= 0 && sy < BLUR_H as i32 {
                        // SAFETY: inside the blur grid.
                        sum += unsafe { *source.add(sy as usize * BLUR_W + x) } as u32;
                        n += 1;
                    }
                    dy += 1;
                }
                // SAFETY: inside the blur grid.
                unsafe {
                    *target.add(y * BLUR_W + x) = (sum / n.max(1)) as u8;
                }
            }
        }
        core::mem::swap(&mut source, &mut target);
    }
    // An even number of swaps leaves the result in `a`; make that explicit so
    // the rest of the module only ever reads BLUR_A.
    if source != a {
        for i in 0..BLUR_W * BLUR_H {
            // SAFETY: both buffers are the same size and inside the statics.
            unsafe {
                *a.add(i) = *source.add(i);
            }
        }
    }
}

/// Sample the baked blur at a point given in emblem-box pixels.
fn blur_value(x: f64, y: f64) -> f64 {
    // SAFETY: read-only, after `build_bloom` filled the buffer.
    let blur = unsafe { &*core::ptr::addr_of!(BLUR_A) };
    let fx = (x / 4.0 + BLUR_PAD as f64).clamp(0.0, (BLUR_W - 1) as f64);
    let fy = (y / 4.0 + BLUR_PAD as f64).clamp(0.0, (BLUR_H - 1) as f64);
    let x0 = fx as usize;
    let y0 = fy as usize;
    let x1 = (x0 + 1).min(BLUR_W - 1);
    let y1 = (y0 + 1).min(BLUR_H - 1);
    let tx = fx - x0 as f64;
    let ty = fy - y0 as f64;
    let top = blur[y0 * BLUR_W + x0] as f64 * (1.0 - tx) + blur[y0 * BLUR_W + x1] as f64 * tx;
    let bottom =
        blur[y1 * BLUR_W + x0] as f64 * (1.0 - tx) + blur[y1 * BLUR_W + x1] as f64 * tx;
    top * (1.0 - ty) + bottom * ty
}

// ---------------------------------------------------------------------------
// Background: painted once, then never recomputed.
// ---------------------------------------------------------------------------

fn background(surface: &Surface) {
    // 1. Base colour, with the right half's radial lift toward the mark (§2: the
    //    same nominal background is #030C17 at 480,540 and #072947 at 1440,540).
    let cx = 1400.0;
    let cy = 520.0;
    let radius = 620.0;
    let radius2 = radius * radius;
    for y in 0..CANVAS_H.min(surface.height) {
        for x in 0..CANVAS_W.min(surface.width) {
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            let d2 = dx * dx + dy * dy;
            let colour = if d2 < radius2 {
                let t = 1.0 - d2 / radius2;
                lerp(BG_BASE, BG_GLOW, t * t)
            } else {
                BG_BASE
            };
            surface.put(x, y, colour);
        }
    }

    // 2. The 90 px grid. There is none on the left half — it starts at x = 790.
    for k in 0..=12 {
        let x = 790 + 90 * k;
        surface.rect(x, 6, x, 994, GRID_V);
    }
    for k in 1..=11 {
        let y = 90 + 90 * k;
        // The horizontal lines fade in from the first vertical line; the exact
        // curve is [UNMEASURED] (spec §8), so it is a linear ramp over 90 px.
        for x in 790..CANVAS_W {
            let a = if x < 880 {
                ((x - 790) * 255 / 90) as u32
            } else {
                255
            };
            surface.blend(x, y as u32, GRID_H, a);
        }
    }

    // 3. The rings: an ellipse (rx 414, ry 423) around a 380 px middle ring that
    //    is broken in the reference, and a closed 360 px inner circle.
    surface.ellipse_arc(cx, 503.0, 414.0, 423.0, RING, 70, 0.0, 360.0);
    surface.ellipse_arc(cx, 503.0, 380.0, 380.0, RING, 45, 20.0, 80.0);
    surface.ellipse_arc(cx, 503.0, 380.0, 380.0, RING, 45, 110.0, 170.0);
    surface.ellipse_arc(cx, 503.0, 380.0, 380.0, RING, 45, 200.0, 250.0);
    surface.ellipse_arc(cx, 503.0, 380.0, 380.0, RING, 45, 290.0, 340.0);
    surface.ellipse_arc(cx, 503.0, 360.0, 360.0, RING, 60, 0.0, 360.0);

    // 4. The bloom, added into the background exactly once (§7).
    build_bloom();
    let halo = 220i32;
    for y in (EMBLEM_Y0 - halo)..(EMBLEM_Y0 + EMBLEM_H as i32 + halo) {
        if y < 0 || y as u32 >= surface.height {
            continue;
        }
        for x in (EMBLEM_X0 - halo)..(EMBLEM_X0 + EMBLEM_W as i32 + halo) {
            if x < 0 || x as u32 >= surface.width {
                continue;
            }
            let value = blur_value((x - EMBLEM_X0) as f64, (y - EMBLEM_Y0) as f64);
            if value <= 1.0 {
                continue;
            }
            // +17 lum at the mark's edge, decaying to nothing over ~56 px: a
            // bluish white, so the halo reads as light coming off the emblem.
            let a = (value * 0.16).min(48.0) as u32;
            surface.blend(x as u32, y as u32, 0xBF_E8_FF, a);
        }
    }

    // 5. The emblem itself: flat, one colour, over its own halo. The coverage
    //    comes from the same mask the bloom was built from.
    // SAFETY: read-only, after `build_bloom` filled the mask.
    let mask = unsafe { &*core::ptr::addr_of!(MASK) };
    for y in 0..EMBLEM_H {
        for x in 0..EMBLEM_W {
            let a = mask[(y * EMBLEM_W + x) as usize] as u32;
            if a == 0 {
                continue;
            }
            surface.blend(
                (EMBLEM_X0 + x as i32) as u32,
                (EMBLEM_Y0 + y as i32) as u32,
                MARK_FILL,
                a,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Foreground: the measured chrome and the text.
// ---------------------------------------------------------------------------

fn corner_rules(surface: &Surface) {
    // Left rules: 376 px, ending in a 10 px dot at x 50.5.
    surface.rect(60, 92, 435, 93, RULE);
    surface.disc(50.5, 92.0, 5.0, DOT_LEFT);
    surface.rect(60, 999, 435, 1000, RULE);
    surface.disc(50.5, 999.0, 5.0, DOT_LEFT);
    // Right rules: 142 px, ending in a 7 px dot at x 1858.
    surface.rect(1720, 92, 1861, 93, RULE_DIM);
    surface.disc(1858.0, 92.0, 3.5, DOT_RIGHT);
    surface.rect(1720, 999, 1861, 1000, RULE_DIM_BOTTOM);
    surface.disc(1858.0, 999.0, 3.5, DOT_RIGHT);
}

fn ticks(surface: &Surface) {
    // Top-right: 17 ticks, pitch 18, 2 px wide, hanging below the rule. Every
    // fourth is 21 px tall, the rest 8 px.
    for k in 0..17 {
        let x = 1510 + 18 * k;
        let height = if k % 4 == 0 { 21 } else { 8 };
        surface.rect(x, 96, x + 1, 96 + height - 1, TICK_BRIGHT);
    }
    // Bottom-right: 8 ticks, pitch 25, 6 px wide, sitting above the rule, with a
    // left-to-right ramp measured at four points in the reference.
    for k in 0..8 {
        let x = 1620 + 25 * k;
        let height = if k % 3 == 0 { 13 } else { 21 };
        let t = ((x - 1620) as f64 / 175.0).clamp(0.0, 1.0);
        let colour = lerp(TICK_RAMP_LO, TICK_RAMP_HI, t);
        surface.rect(x, 955, x + 5, 955 + height - 1, colour);
    }
}

fn headline(surface: &Surface, atlas: &Atlas) {
    // Wordmark: cap 41, ink from x 112, baseline 194 (last ink row 193).
    surface.text(atlas, WORDMARK, 112, 194, "ARGUS", TEXT_WORDMARK);
    // Subtitle: cap 16, ink at x 364 — spec §6 option (a): the wordmark keeps
    // cap 41, so the subtitle moves right to 112 + 210 + 42 = 364.
    surface.text(atlas, SUBTITLE, 364, 186, "// BOOT INTERFACE", ACCENT);
    // Section head and its hairline (696 px from x 110, 26 px below the text).
    surface.text(atlas, SECTION, 113, 237, "SYSTEM SELECTION", TEXT_SECTION);
    surface.rect(110, 262, 805, 262, HAIRLINE);
}

fn panel(surface: &Surface, atlas: &Atlas) {
    // Panel: 721 x 465 at (104,300), 2 px border, 16 px corners.
    surface.rounded_rect(104, 300, 824, 764, 16.0, PANEL_FILL);
    surface.rounded_rect_border(104, 300, 824, 764, 16.0, 2.0, PANEL_BORDER);

    // Label tab: a 91 px rule 15 px below the panel top, then the label 16 px
    // below the rule and 29 px in from the panel edge.
    surface.rect(130, 315, 220, 316, TAB_RULE);
    surface.text(
        atlas,
        LABEL,
        133,
        342,
        "CHOOSE AN OPERATING SYSTEM",
        ACCENT_LABEL,
    );

    // Bracket marks, attached to the panel border.
    surface.rect(104, 332, 107, 378, BRACKET_UPPER);
    surface.rect(104, 700, 107, 744, BRACKET_LOWER);
    surface.rect(700, 750, 794, 751, BAR_RIGHT);
}

fn entries(surface: &Surface, atlas: &Atlas) {
    // Selected row: 655 x 56 at (134,373), radius 6, 2 px edge, 7 px left bar.
    surface.rounded_rect(134, 373, 788, 428, 6.0, SELECT_FILL);
    surface.rounded_rect_border(134, 373, 788, 428, 6.0, 2.0, SELECT_EDGE);
    surface.rounded_rect(134, 373, 140, 428, 3.0, SELECT_EDGE);
    // Text ink 29 px in from the row box, 14 px down from its top, cap 18.
    surface.text(atlas, ROW, 163, 405, "ARGUS Linux", TEXT_ROW_SELECTED);

    // Rows 2-4: 56 px tall, pitch 62, text inset 28 px.
    let rest = [
        (435, "Advanced options for Linux"),
        (497, "Windows Boot Manager"),
        (559, "UEFI Firmware Settings"),
    ];
    for (top, label) in rest {
        surface.text(atlas, ROW, 162, top + 32, label, TEXT_ROW);
    }
}

fn hints(surface: &Surface, atlas: &Atlas) {
    // Every token is placed at the x the reference measures its ink at (§1.10):
    // bracket, arrows and slash do not sit on a uniform grid.
    let row_one = [
        (117, "["),
        (149, "\u{2191}"),
        (168, "/"),
        (192, "\u{2193}"),
        (224, "]"),
        (288, "NAVIGATE"),
    ];
    let row_two = [(117, "["), (148, "ENTER"), (224, "]"), (289, "LAUNCH")];
    for (x, token) in row_one {
        surface.text(atlas, HINT, x, 859, token, TEXT_HINT);
    }
    for (x, token) in row_two {
        surface.text(atlas, HINT, x, 899, token, TEXT_HINT);
    }
}

fn footer(surface: &Surface, atlas: &Atlas) {
    surface.text(
        atlas,
        FOOTER,
        111,
        977,
        "ARGUS / CORE SYSTEM INTERFACE",
        TEXT_FOOTER,
    );
    // The caption names what this screen is. It is never "GRUB 2": our boot
    // manager is not GRUB and is not derived from it (spec §6).
    surface.text(
        atlas,
        FOOTER,
        1604,
        1044,
        "ARGUS // BOOT MANAGER",
        TEXT_CAPTION,
    );
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Draw the boot manager screen into `fb`.
///
/// `Ok(())` means every element of spec §1-§3 was drawn. `Err(Skip)` names the
/// reason nothing was, and the loader logs it and keeps its log console.
pub fn paint(fb: FramebufferInfo) -> Result<(), Skip> {
    if fb.width != CANVAS_W || fb.height != CANVAS_H {
        return Err(Skip::NotMeasuredMode);
    }
    let surface = Surface::new(fb)?;
    let Some(atlas) = Atlas::parse() else {
        return Err(Skip::AtlasUnusable);
    };

    // The background, rings, grid and bloom: computed once, never per frame.
    background(&surface);
    corner_rules(&surface);
    ticks(&surface);
    headline(&surface, &atlas);
    panel(&surface, &atlas);
    entries(&surface, &atlas);
    hints(&surface, &atlas);
    footer(&surface, &atlas);
    Ok(())
}
