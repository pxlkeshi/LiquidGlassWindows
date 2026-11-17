//! Shared CPU text/shape drawing into premultiplied-alpha bitmaps.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};

/// Off-white used by the clock (R, G, B).
pub const OFF_WHITE: (u8, u8, u8) = (0xF2, 0xEE, 0xE6);
/// Pure white used by the weather card.
pub const WHITE: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);

/// The tall display font used for the clock digits.
pub fn pillar() -> FontRef<'static> {
    FontRef::try_from_slice(include_bytes!("../assets/Pillar.ttf")).expect("bundled font is valid")
}

/// The clean text font used for the date and weather.
pub fn sf() -> FontRef<'static> {
    FontRef::try_from_slice(include_bytes!("../assets/SF-Pro-Display-Regular.otf")).expect("bundled font is valid")
}

/// Light weight of the text font (large numbers).
pub fn sf_light() -> FontRef<'static> {
    FontRef::try_from_slice(include_bytes!("../assets/SF-Pro-Display-Light.otf")).expect("bundled font is valid")
}

/// Medium weight of the text font.
pub fn sf_medium() -> FontRef<'static> {
    FontRef::try_from_slice(include_bytes!("../assets/SF-Pro-Display-Medium.otf")).expect("bundled font is valid")
}

/// Bold weight of the text font.
pub fn sf_bold() -> FontRef<'static> {
    FontRef::try_from_slice(include_bytes!("../assets/SF-Pro-Display-Bold.otf")).expect("bundled font is valid")
}

/// Lays premultiplied pixel `src` over premultiplied pixel `dst` (both 0xAARRGGBB), in integers.
#[inline]
pub fn over(dst: u32, src: u32) -> u32 {
    let sa = src >> 24;
    if sa == 0 {
        return dst;
    }
    if sa == 255 {
        return src;
    }
    let k = 255 - sa;
    // (d * k) / 255, rounded, for all four channels.
    let mul = |d: u32| {
        let t = d * k + 128;
        (t + (t >> 8)) >> 8
    };
    let ch = |shift: u32| ((src >> shift) & 0xFF) + mul((dst >> shift) & 0xFF);
    ch(24).min(255) << 24 | ch(16).min(255) << 16 | ch(8).min(255) << 8 | ch(0).min(255)
}

/// Scales a premultiplied pixel's opacity by `a` (0..=256, where 256 = unchanged).
#[inline]
pub fn scale(p: u32, a: u32) -> u32 {
    if a >= 256 {
        return p;
    }
    let ch = |shift: u32| ((((p >> shift) & 0xFF) * a) >> 8) << shift;
    ch(24) | ch(16) | ch(8) | ch(0)
}

pub struct Bitmap {
    pub width: i32,
    pub height: i32,
    /// Row of the text baseline within the bitmap (the bottom edge for non-text bitmaps).
    pub baseline: i32,
    /// Premultiplied 0xAARRGGBB, top row first — the layout UpdateLayeredWindow expects.
    pub pixels: Vec<u32>,
    /// Colour that `put` paints with.
    pub ink: (u8, u8, u8),
}

impl Bitmap {
    pub fn new(width: i32, height: i32) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        Self { width, height, baseline: height, pixels: vec![0; (width * height) as usize], ink: OFF_WHITE }
    }

    /// Paints `ink` at (x, y) with the given coverage (0..1), keeping the stronger of old and new.
    pub fn put(&mut self, x: i32, y: i32, coverage: f32) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let a = (coverage.clamp(0.0, 1.0) * 255.0).round() as u32;
        let slot = &mut self.pixels[(y * self.width + x) as usize];
        if a > *slot >> 24 {
            let (r, g, b) = self.ink;
            let pm = |c: u8| c as u32 * a / 255;
            *slot = a << 24 | pm(r) << 16 | pm(g) << 8 | pm(b);
        }
    }

    /// Makes the whole bitmap more transparent (0 = invisible, 1 = unchanged).
    pub fn fade(&mut self, opacity: f32) {
        let k = opacity.clamp(0.0, 1.0);
        for p in &mut self.pixels {
            let scale = |shift: u32| ((((*p >> shift) & 0xFF) as f32 * k).round() as u32) << shift;
            *p = scale(24) | scale(16) | scale(8) | scale(0);
        }
    }

    /// Copies `other` into this bitmap with its top-left at (x, y).
    pub fn blit(&mut self, other: &Bitmap, x: i32, y: i32) {
        for row in 0..other.height {
            for col in 0..other.width {
                let (px, py) = (x + col, y + row);
                if px >= 0 && py >= 0 && px < self.width && py < self.height {
                    let src = other.pixels[(row * other.width + col) as usize];
                    let dst = &mut self.pixels[(py * self.width + px) as usize];
                    if src >> 24 > *dst >> 24 {
                        *dst = src;
                    }
                }
            }
        }
    }

    /// Draws `other` on top of this bitmap (normal "over" blending), with its top-left at (x, y):
    /// for text on a solid shape, which `blit` would leave hidden.
    pub fn blit_over(&mut self, other: &Bitmap, x: i32, y: i32) {
        for row in 0..other.height {
            for col in 0..other.width {
                let (px, py) = (x + col, y + row);
                if px >= 0 && py >= 0 && px < self.width && py < self.height {
                    let src = other.pixels[(row * other.width + col) as usize];
                    let a = src >> 24;
                    if a == 0 {
                        continue;
                    }
                    let dst = &mut self.pixels[(py * self.width + px) as usize];
                    let keep = 255 - a;
                    let ch = |shift: u32| {
                        let s = (src >> shift) & 0xFF;
                        let d = (*dst >> shift) & 0xFF;
                        (s + (d * keep + 127) / 255).min(255) << shift
                    };
                    *dst = ch(24) | ch(16) | ch(8) | ch(0);
                }
            }
        }
    }
}

/// Renders one line of text, cropped tightly to the ink, so that a digit is `digit_height` pixels tall.
/// `widen` stretches the letters horizontally (1.0 = as designed); `ink` is the text colour.
pub fn line(font: &FontRef, text: &str, digit_height: f32, widen: f32, ink: (u8, u8, u8)) -> Bitmap {
    line_colored(font, text, digit_height, widen, |_, _| ink)
}

/// Like `line`, but each pixel's colour comes from `color(char_index, v)`, where `v` runs from
/// 0 at the top of the line's ink to 1 at the bottom — for per-character colours and gradients.
pub fn line_colored(
    font: &FontRef,
    text: &str,
    digit_height: f32,
    widen: f32,
    color: impl Fn(usize, f32) -> (u8, u8, u8),
) -> Bitmap {
    line_spaced(font, text, digit_height, widen, color, |_, _, _| 0.0)
}

/// Like `line_colored`, with hand-tuned spacing: `nudge(previous_char, char, font_px)` returns
/// pixels to move each character by (negative = closer to the one before), on top of the font's
/// own spacing — for optical corrections the font doesn't make itself.
pub fn line_spaced(
    font: &FontRef,
    text: &str,
    digit_height: f32,
    widen: f32,
    color: impl Fn(usize, f32) -> (u8, u8, u8),
    nudge: impl Fn(Option<char>, char, f32) -> f32,
) -> Bitmap {
    // How tall a digit is relative to the font size, so we can hit `digit_height` exactly.
    let ratio = font
        .outline_glyph(font.glyph_id('0').with_scale(1000.0))
        .map(|g| g.px_bounds().height() / 1000.0)
        .unwrap_or(0.7);
    let size = digit_height / ratio;
    let scale = PxScale { x: size * widen, y: size };
    let scaled = font.as_scaled(scale);

    let mut x = 0.0;
    let mut prev = None;
    let mut prev_char = None;
    let mut outlines = Vec::new();
    for (index, c) in text.chars().enumerate() {
        let id = font.glyph_id(c);
        if let Some(p) = prev {
            x += scaled.kern(p, id);
        }
        x += nudge(prev_char, c, size * widen);
        prev_char = Some(c);
        if let Some(o) = font.outline_glyph(id.with_scale_and_position(scale, point(x, scaled.ascent()))) {
            outlines.push((index, o));
        }
        x += scaled.h_advance(id);
        prev = Some(id);
    }
    if outlines.is_empty() {
        return Bitmap::new(1, 1);
    }

    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for (_, o) in &outlines {
        let b = o.px_bounds();
        x0 = x0.min(b.min.x);
        y0 = y0.min(b.min.y);
        x1 = x1.max(b.max.x);
        y1 = y1.max(b.max.y);
    }
    let mut bmp = Bitmap::new((x1 - x0).ceil() as i32, (y1 - y0).ceil() as i32);
    bmp.baseline = (scaled.ascent() - y0).round() as i32; // glyphs were laid out with the baseline at y = ascent
    let height = bmp.height.max(1) as f32;
    for (index, o) in &outlines {
        let bounds = o.px_bounds();
        let ox = (bounds.min.x - x0) as i32;
        let oy = (bounds.min.y - y0) as i32;
        o.draw(|gx, gy, coverage| {
            let (px, py) = (ox + gx as i32, oy + gy as i32);
            bmp.ink = color(*index, (py as f32 + 0.5) / height);
            bmp.put(px, py, coverage);
        });
    }
    bmp
}
