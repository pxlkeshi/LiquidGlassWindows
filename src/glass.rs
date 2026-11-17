//! "Liquid glass", following AmirHossein Aghajari's breakdown of Apple's effect
//! (medium.com/@aghajari/liquid-glass-ios-effect-explanation-dabadd6414ae).
//!
//! The widgets sit on the desktop, so what's behind the card is the wallpaper. We take
//! the wallpaper pixels behind the card and render them through a lens, redrawing only
//! when the wallpaper (or the content) changes:
//!
//! - **Lens:** each pixel samples the background from closer to the card's centre, by up
//!   to half the card's size at the edge, following a circular lens curve
//!   `1 - sqrt(1 - x^2)`. The centre stays clean; the outer band warps strongly.
//! - **Chromatic aberration:** red and blue are sampled a few pixels apart, radially.
//! - **Blur, tint, saturation:** a light blur, a slight darkening, a small colour boost.
//! - **Specular rim light:** brightest where the edge faces the light.
//!
//! All the light maths runs in linear light; we convert back to sRGB at the end.

use crate::render::{self, Bitmap};
use crate::wallpaper;

/// How far in from the edge the lens reaches, as a fraction of the card's shorter side
/// (0.5 would reach the centre). The article uses 0.3.
pub const LENS_BAND: f32 = 0.3;
/// On elongated cards, the lens pulls from at most this many times the short half-side.
pub const MAX_REACH_RATIO: f32 = 4.0;
/// Lens strength: 1.0 = at the very edge, sample from the card's centre (the article's value).
pub const LENS_STRENGTH: f32 = 1.0;
/// Red/blue split for chromatic aberration, as a fraction of the screen height (~3 px at 1600).
pub const CHROMA_FRACTION: f32 = 0.0019;
/// Blur radius, as a fraction of the screen height.
pub const BLUR_FRACTION: f32 = 0.0012;
/// Darkening of the view through the glass (1.0 = none; the article uses 0.9).
pub const TINT: f32 = 0.9;
/// Colour saturation of what's seen through the glass (1.0 = unchanged).
pub const SATURATION: f32 = 1.2;
/// Strength of the specular rim light (0..1).
pub const SPECULAR: f32 = 0.62;
/// Direction the light comes from, in degrees (screen space; -135 = straight from the upper
/// left, so the brightest points are the top-left and bottom-right corners, as on macOS).
pub const LIGHT_ANGLE: f32 = -135.0;
/// How bright the far side's reflected highlight is, relative to the lit side (macOS: nearly equal).
pub const REFLECTED: f32 = 0.8;
/// How sharply the highlight gathers at the corners facing the light (higher = tighter).
pub const HIGHLIGHT_FOCUS: f32 = 3.0;
/// The faint hairline that runs all the way round, even where no light hits (0..1 of the rim).
pub const HAIRLINE: f32 = 0.22;
/// Thickness of the bright rim line, as a fraction of the screen height (~1 px at 1600).
pub const RIM_FRACTION: f32 = 0.00062;
/// The soft glow spreading inward from the lit corners, relative to the rim line.
pub const RIM_GLOW: f32 = 0.14;

/// Precomputed lens for one card size, plus the last rendered glass layer (without content).
pub struct Glass {
    width: i32,
    height: i32,
    border: i32,
    blur: i32,
    /// Per pixel: offset from the pixel to where the background is sampled.
    displacement: Vec<(f32, f32)>,
    /// Per pixel: red/blue sampling offset (blue = +, red = -).
    chroma: Vec<(f32, f32)>,
    coverage: Vec<f32>,
    specular: Vec<f32>,
    /// The glass layer for a given wallpaper and screen position, premultiplied sRGB (r, g, b, a).
    layer: Option<(wallpaper::Source, (i32, i32), Vec<u32>)>,
}

impl Glass {
    pub fn new(width: i32, height: i32, radius: f32, screen_height: f32) -> Glass {
        let (hw, hh) = (width as f32 / 2.0, height as f32 / 2.0);
        let radius = radius.min(hw).min(hh);
        let size = width.min(height) as f32;
        let chroma_px = screen_height * CHROMA_FRACTION;
        let rim_px = (screen_height * RIM_FRACTION).max(0.6);
        // How far the lens may pull from, each way: half the card, except on very elongated
        // cards (a long pill), where pulling from half its length would smear the view.
        let reach = (hw.min(hh * MAX_REACH_RATIO), hh.min(hw * MAX_REACH_RATIO));
        let light = (LIGHT_ANGLE.to_radians().cos(), LIGHT_ANGLE.to_radians().sin());

        let n = (width * height) as usize;
        let mut displacement = vec![(0.0, 0.0); n];
        let mut chroma = vec![(0.0, 0.0); n];
        let mut coverage = vec![0.0; n];
        let mut specular = vec![0.0; n];

        // Each row is independent: split the rows across CPU cores.
        let w = width as usize;
        let per = rows_per_thread(height);
        std::thread::scope(|scope| {
            let chunks = displacement
                .chunks_mut(w * per)
                .zip(chroma.chunks_mut(w * per))
                .zip(coverage.chunks_mut(w * per))
                .zip(specular.chunks_mut(w * per))
                .enumerate();
            for (c, (((disp, chro), cov), spec)) in chunks {
                scope.spawn(move || {
                    let y0 = c * per;
                    for (k, cov_px) in cov.iter_mut().enumerate() {
                        let (x, y) = ((k % w) as f32, (y0 + k / w) as f32);
                        let p = (x + 0.5 - hw, y + 0.5 - hh);
                        let d = rounded_rect(p, (hw, hh), radius);
                        *cov_px = (0.5 - d).clamp(0.0, 1.0);
                        if *cov_px == 0.0 {
                            continue;
                        }
                        let inward = (-d).max(0.0);
                        // Normalised depth into the glass: 0 at the edge, 0.5 at the centre of the short side.
                        let depth = inward / size;
                        // 1 at the edge, 0 once we're LENS_BAND deep.
                        let from_center = 1.0 - (depth / LENS_BAND).clamp(0.0, 1.0);
                        // Direction away from the card's centre.
                        let len = (p.0 * p.0 + p.1 * p.1).sqrt().max(1e-6);
                        let dir = (p.0 / len, p.1 / len);
                        // Colour fringes, fading out right at the edge as in the article.
                        let edge = smoothstep(0.0, 0.02, depth);
                        chro[k] = (dir.0 * edge * chroma_px, dir.1 * edge * chroma_px);
                        if from_center <= 0.0 {
                            continue; // the flat middle: no bending, no rim light
                        }
                        // Circular lens curve; sample closer to the centre, by up to half the
                        // card's width/height at the edge.
                        let distortion = 1.0 - (1.0 - from_center * from_center).max(0.0).sqrt();
                        disp[k] = (-distortion * dir.0 * reach.0 * LENS_STRENGTH, -distortion * dir.1 * reach.1 * LENS_STRENGTH);
                        // Rim light: a thin bright line at the edge plus a soft glow across the lens band.
                        let n_out = edge_normal(p, (hw, hh), radius);
                        let facing = n_out.0 * light.0 + n_out.1 * light.1;
                        // Highlights gather at the corners facing the light and the opposite
                        // corner; a faint hairline still runs along the sides in between.
                        let lobe = facing.max(0.0).powf(HIGHLIGHT_FOCUS) + REFLECTED * (-facing).max(0.0).powf(HIGHLIGHT_FOCUS);
                        let lit = HAIRLINE + (1.0 - HAIRLINE) * lobe.min(1.0);
                        // A crisp line about a pixel thick, with a very short soft tail inside it.
                        let t = inward / rim_px;
                        let line = if t < 8.0 { (-t * t).exp() * 0.85 + (-t).exp() * 0.15 } else { 0.0 };
                        let rim = line + RIM_GLOW * lobe * from_center * from_center * from_center;
                        spec[k] = (SPECULAR * lit * rim).min(1.0);
                    }
                });
            }
        });

        let blur = (screen_height * BLUR_FRACTION).max(1.0) as i32;
        let border = 4 * blur + chroma_px.ceil() as i32 + 2;
        Glass { width, height, border, blur, displacement, chroma, coverage, specular, layer: None }
    }

    pub fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    /// Renders the glass for a card at `screen_pos` with `content` on top (placed `pad` pixels
    /// in from the edge). The glass itself is only recomputed when the wallpaper or position
    /// changes; otherwise only the content is laid on top again. Returns None if nothing about
    /// the glass changed and `force` isn't set.
    pub fn render(&mut self, content: &Bitmap, pad: i32, screen_pos: (i32, i32), force: bool) -> Option<Bitmap> {
        let source = wallpaper::Source::current();
        let fresh = matches!(&self.layer, Some((s, p, _)) if *s == source && *p == screen_pos);
        if !fresh {
            let (bx, by) = (screen_pos.0 - self.border, screen_pos.1 - self.border);
            let (bw, bh) = (self.width + 2 * self.border, self.height + 2 * self.border);
            let bg = Rgb { width: bw, height: bh, pixels: source.patch(bx, by, bw, bh) };
            self.layer = Some((source, screen_pos, self.glass_layer(bg)));
        } else if !force {
            return None;
        }
        let (_, _, layer) = self.layer.as_ref()?;
        Some(self.overlay(layer, content, pad))
    }

    /// The glass on its own over `bg` (the area behind the card plus `border` pixels on each side).
    fn glass_layer(&self, mut bg: Rgb) -> Vec<u32> {
        // All light maths (blur, refraction, colour, highlights) happens in linear light,
        // like a real renderer; we only go back to sRGB at the very end.
        for p in &mut bg.pixels {
            for v in p.iter_mut() {
                *v = srgb_to_linear(*v / 255.0);
            }
        }
        bg.blur(self.blur);
        let b = self.border as f32;
        self.shade(&bg, b, b)
    }

    /// The glass over a prepared backdrop (see `Backdrop`), for a card at `screen_pos`, with
    /// `content` on top. Any size works over the same backdrop, so a card can change size
    /// every frame and still look exactly like a fully rendered card of that size.
    pub fn compose_on(&self, backdrop: &Backdrop, screen_pos: (i32, i32), content: &Bitmap, pad: i32) -> Bitmap {
        let ox = (screen_pos.0 - backdrop.x) as f32;
        let oy = (screen_pos.1 - backdrop.y) as f32;
        let layer = self.shade(&backdrop.rgb, ox, oy);
        self.overlay(&layer, content, pad)
    }

    /// The lens, colour and highlights over a blurred, linear-light background; (ox, oy) is
    /// where the card's top-left sits in `bg`.
    fn shade(&self, bg: &Rgb, ox: f32, oy: f32) -> Vec<u32> {
        let mut layer = vec![0u32; (self.width * self.height) as usize];
        let tint = TINT.powf(2.2); // the article's 0.9 is in sRGB terms
        let to_srgb = srgb_table();
        let w = self.width as usize;
        let per = rows_per_thread(self.height);
        // Each row is independent: split the rows across CPU cores.
        std::thread::scope(|scope| {
            for (c, out) in layer.chunks_mut(w * per).enumerate() {
                scope.spawn(move || {
                    let base = c * per * w;
                    for (k, px) in out.iter_mut().enumerate() {
                        let i = base + k;
                        let cov = self.coverage[i];
                        if cov == 0.0 {
                            continue;
                        }
                        let (x, y) = ((i % w) as f32, (i / w) as f32);
                        // Lens sample, with red and blue taken a little apart (chromatic aberration).
                        let (dx, dy) = self.displacement[i];
                        let (cx, cy) = self.chroma[i];
                        let (sx, sy) = (x + 0.5 + ox + dx, y + 0.5 + oy + dy);
                        let mut c = [bg.sample(sx - cx, sy - cy)[0], bg.sample(sx, sy)[1], bg.sample(sx + cx, sy + cy)[2]];
                        // Saturation (Rec. 709 luminance, the correct weights for linear light) and tint.
                        let luma = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
                        for v in &mut c {
                            *v = ((luma + (*v - luma) * SATURATION) * tint).clamp(0.0, 1.0);
                        }
                        // Specular rim light, added as light.
                        let s = self.specular[i];
                        for v in &mut c {
                            *v += (1.0 - *v) * s;
                        }
                        // Back to sRGB, premultiplied by edge coverage, packed as a ready-to-use pixel.
                        let to8 = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u32;
                        let a8 = to8(cov);
                        let ch = |v: f32| to8(to_srgb[(v.clamp(0.0, 1.0) * 4095.0) as usize] * cov).min(a8);
                        *px = a8 << 24 | ch(c[0]) << 16 | ch(c[1]) << 8 | ch(c[2]);
                    }
                });
            }
        });
        layer
    }

    /// Lays premultiplied `content` over the glass layer, the way Windows blends. The glass is
    /// copied as-is; only pixels with content on them are blended.
    fn overlay(&self, layer: &[u32], content: &Bitmap, pad: i32) -> Bitmap {
        let mut out = Bitmap::new(self.width, self.height);
        out.pixels.copy_from_slice(layer);
        for cy in 0..content.height {
            let y = cy + pad;
            if y < 0 || y >= self.height {
                continue;
            }
            let (src_row, dst_row) = ((cy * content.width) as usize, (y * self.width) as usize);
            for cx in 0..content.width {
                let x = cx + pad;
                if x < 0 || x >= self.width {
                    continue;
                }
                let s = content.pixels[src_row + cx as usize];
                if s >> 24 != 0 {
                    let d = &mut out.pixels[dst_row + x as usize];
                    *d = render::over(*d, s);
                }
            }
        }
        out
    }
}

// ---------- colour space ----------

/// sRGB value (0..1) → linear light (0..1).
fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

/// Linear light → sRGB as a lookup table (4096 steps): much faster than the exact formula,
/// which matters when a card is re-rendered every animation frame.
fn srgb_table() -> &'static [f32] {
    static TABLE: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| (0..4096).map(|i| linear_to_srgb(i as f32 / 4095.0)).collect())
}

/// The wallpaper behind an area, read, converted to linear light and blurred once, so glass
/// of any size inside that area can be drawn quickly (e.g. every frame of an animation).
pub struct Backdrop {
    x: i32,
    y: i32,
    rgb: Rgb,
    source: wallpaper::Source,
    area: (i32, i32, i32, i32),
}

impl Backdrop {
    /// Prepares the backdrop for cards anywhere inside the screen rectangle (x, y, w, h).
    pub fn capture(area: (i32, i32, i32, i32), screen_height: f32) -> Backdrop {
        let blur = (screen_height * BLUR_FRACTION).max(1.0) as i32;
        let margin = 4 * blur + (screen_height * CHROMA_FRACTION).ceil() as i32 + 4;
        let (x, y) = (area.0 - margin, area.1 - margin);
        let (w, h) = (area.2 + 2 * margin, area.3 + 2 * margin);
        let source = wallpaper::Source::current();
        let mut rgb = Rgb { width: w, height: h, pixels: source.patch(x, y, w, h) };
        for p in &mut rgb.pixels {
            for v in p.iter_mut() {
                *v = srgb_to_linear(*v / 255.0);
            }
        }
        rgb.blur(blur);
        Backdrop { x, y, rgb, source, area }
    }

    /// Still valid for this area and the current wallpaper?
    pub fn fits(&self, area: (i32, i32, i32, i32)) -> bool {
        self.area == area && self.source == wallpaper::Source::current()
    }
}

/// Linear light (0..1) → sRGB value (0..1).
fn linear_to_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

// ---------- shapes ----------

/// Signed distance to a rounded rectangle centred at the origin (negative inside).
fn rounded_rect(p: (f32, f32), half: (f32, f32), r: f32) -> f32 {
    let q = (p.0.abs() - half.0 + r, p.1.abs() - half.1 + r);
    let outside = (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt();
    outside + q.0.max(q.1).min(0.0) - r
}

/// How many rows each worker thread handles (at most 8 threads; small cards use fewer).
fn rows_per_thread(height: i32) -> usize {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    ((height as usize).div_ceil(threads)).max(16)
}

/// Outward direction of a rounded rectangle's edge nearest to `p`, worked out directly.
fn edge_normal(p: (f32, f32), half: (f32, f32), r: f32) -> (f32, f32) {
    let (sx, sy) = (p.0.signum(), p.1.signum());
    let q = (p.0.abs() - half.0 + r, p.1.abs() - half.1 + r);
    if q.0 > 0.0 && q.1 > 0.0 {
        // In a corner: away from the corner's centre.
        let len = (q.0 * q.0 + q.1 * q.1).sqrt().max(1e-6);
        (sx * q.0 / len, sy * q.1 / len)
    } else if q.0 > q.1 {
        (sx, 0.0)
    } else {
        (0.0, sy)
    }
}


fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// ---------- background pixels ----------

struct Rgb {
    width: i32,
    height: i32,
    pixels: Vec<[f32; 3]>,
}

impl Rgb {
    /// Two box-blur passes each way: a cheap, soft blur.
    fn blur(&mut self, radius: i32) {
        for _ in 0..2 {
            self.box_pass(radius, true);
            self.box_pass(radius, false);
        }
    }

    fn box_pass(&mut self, r: i32, horizontal: bool) {
        let (len, lines) = if horizontal { (self.width, self.height) } else { (self.height, self.width) };
        let w = self.width;
        let idx = move |line: i32, i: i32| -> usize {
            let i = i.clamp(0, len - 1);
            (if horizontal { line * w + i } else { i * w + line }) as usize
        };
        let mut row = vec![[0.0f32; 3]; len as usize];
        let scale = 1.0 / (2 * r + 1) as f32;
        for line in 0..lines {
            let mut sum = [0.0f32; 3];
            for i in -r..=r {
                let p = self.pixels[idx(line, i)];
                for c in 0..3 {
                    sum[c] += p[c];
                }
            }
            for i in 0..len {
                row[i as usize] = [sum[0] * scale, sum[1] * scale, sum[2] * scale];
                let (add, sub) = (self.pixels[idx(line, i + r + 1)], self.pixels[idx(line, i - r)]);
                for c in 0..3 {
                    sum[c] += add[c] - sub[c];
                }
            }
            for i in 0..len {
                self.pixels[idx(line, i)] = row[i as usize];
            }
        }
    }

    fn sample(&self, x: f32, y: f32) -> [f32; 3] {
        let x = (x - 0.5).clamp(0.0, (self.width - 1) as f32);
        let y = (y - 0.5).clamp(0.0, (self.height - 1) as f32);
        let (x0, y0) = (x.floor() as i32, y.floor() as i32);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let at = |x: i32, y: i32| self.pixels[(y * self.width + x) as usize];
        let mut out = [0.0; 3];
        for c in 0..3 {
            let top = at(x0, y0)[c] * (1.0 - fx) + at(x1, y0)[c] * fx;
            let bottom = at(x0, y1)[c] * (1.0 - fx) + at(x1, y1)[c] * fx;
            out[c] = top * (1.0 - fy) + bottom * fy;
        }
        out
    }
}






