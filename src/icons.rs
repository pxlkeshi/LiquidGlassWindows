//! Multicolour weather icons in the style of Apple Weather, drawn crisply at any size:
//! a golden sun, a pale crescent moon, smooth organic clouds shaded from
//! white to a cool grey, blue rain, white snow and a yellow lightning bolt.
//!
//! Every shape is a signed distance function in "unit" coordinates (0..1 across the icon,
//! y pointing down), so edges are anti-aliased exactly and shapes can blend smoothly.

use crate::render::{self, Bitmap};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum WeatherIcon {
    Sun,
    Moon,
    PartlySunny,
    PartlyMoon,
    Cloudy,
    Fog,
    Drizzle,
    Rain,
    Snow,
    Thunder,
}

type P = (f32, f32);
type Rgb = (f32, f32, f32);

// Palette (sRGB).
const SUN_TOP: Rgb = (255.0, 222.0, 89.0);
const SUN_BOTTOM: Rgb = (255.0, 166.0, 28.0);
const RAY: Rgb = (255.0, 196.0, 52.0);
const MOON_TOP: Rgb = (255.0, 249.0, 225.0);
const MOON_BOTTOM: Rgb = (226.0, 216.0, 178.0);
const CLOUD_TOP: Rgb = (255.0, 255.0, 255.0);
const CLOUD_BOTTOM: Rgb = (205.0, 214.0, 228.0);
const CLOUD_BACK_TOP: Rgb = (190.0, 199.0, 212.0);
const CLOUD_BACK_BOTTOM: Rgb = (150.0, 161.0, 178.0);
const STORM_TOP: Rgb = (168.0, 176.0, 190.0);
const STORM_BOTTOM: Rgb = (112.0, 121.0, 137.0);
const RAIN: Rgb = (64.0, 156.0, 255.0);
const DRIZZLE: Rgb = (110.0, 182.0, 255.0);
const SNOW: Rgb = (240.0, 247.0, 255.0);
const FOG: Rgb = (214.0, 221.0, 232.0);
const BOLT_TOP: Rgb = (255.0, 230.0, 60.0);
const BOLT_BOTTOM: Rgb = (255.0, 186.0, 20.0);

/// Draws `icon` into a `size`×`size` bitmap.
pub fn draw(icon: WeatherIcon, size: i32) -> Bitmap {
    let mut c = Canvas { out: Bitmap::new(size, size), s: size as f32 };
    match icon {
        WeatherIcon::Sun => sun(&mut c, (0.5, 0.5), 1.0),
        WeatherIcon::Moon => moon(&mut c, (0.5, 0.5), 1.0, true),
        WeatherIcon::PartlySunny => {
            sun(&mut c, (0.38, 0.36), 0.72);
            cloud_with_shadow(&mut c, (0.57, 0.62), 0.98, CLOUD_TOP, CLOUD_BOTTOM);
        }
        WeatherIcon::PartlyMoon => {
            moon(&mut c, (0.4, 0.38), 0.72, false);
            cloud_with_shadow(&mut c, (0.57, 0.62), 0.98, CLOUD_TOP, CLOUD_BOTTOM);
        }
        WeatherIcon::Cloudy => {
            cloud(&mut c, (0.62, 0.4), 0.86, CLOUD_BACK_TOP, CLOUD_BACK_BOTTOM);
            cloud_with_shadow(&mut c, (0.46, 0.6), 1.08, CLOUD_TOP, CLOUD_BOTTOM);
        }
        WeatherIcon::Fog => {
            cloud(&mut c, (0.5, 0.4), 1.05, CLOUD_TOP, CLOUD_BOTTOM);
            for (y, x0, x1) in [(0.74, 0.18, 0.76), (0.86, 0.28, 0.84)] {
                c.fill(|p| capsule(p, (x0, y), (x1, y)) - 0.035, |_| FOG, 0.95);
            }
        }
        WeatherIcon::Drizzle => {
            cloud(&mut c, (0.5, 0.38), 1.05, CLOUD_TOP, CLOUD_BOTTOM);
            for x in [0.32, 0.5, 0.68] {
                c.fill(|p| capsule(p, (x + 0.015, 0.73), (x - 0.015, 0.83)) - 0.03, |_| DRIZZLE, 1.0);
            }
        }
        WeatherIcon::Rain => {
            cloud(&mut c, (0.5, 0.38), 1.05, CLOUD_TOP, CLOUD_BOTTOM);
            for (x, y0) in [(0.34, 0.72), (0.52, 0.76), (0.7, 0.72)] {
                c.fill(|p| capsule(p, (x + 0.04, y0), (x - 0.04, y0 + 0.17)) - 0.03, |p| shade(RAIN, p.1, 0.7, 0.92), 1.0);
            }
        }
        WeatherIcon::Snow => {
            cloud(&mut c, (0.5, 0.38), 1.05, CLOUD_TOP, CLOUD_BOTTOM);
            for (x, y) in [(0.32, 0.78), (0.5, 0.86), (0.68, 0.78)] {
                flake(&mut c, (x, y), 0.062);
            }
        }
        WeatherIcon::Thunder => {
            cloud(&mut c, (0.5, 0.36), 1.05, STORM_TOP, STORM_BOTTOM);
            let bolt = [(0.55, 0.52), (0.4, 0.74), (0.5, 0.74), (0.44, 0.94), (0.63, 0.67), (0.53, 0.67), (0.6, 0.52)];
            c.fill(|p| polygon(p, &bolt) - 0.004, |p| gradient(BOLT_TOP, BOLT_BOTTOM, (p.1 - 0.52) / 0.42), 1.0);
        }
    }
    c.out
}

// ---------- pieces ----------

/// A golden sun: rounded rays and a top-lit disc.
fn sun(c: &mut Canvas, centre: P, scale: f32) {
    let r = 0.2 * scale;
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let (s, co) = (a.sin(), a.cos());
        let inner = (centre.0 + co * r * 1.42, centre.1 + s * r * 1.42);
        let outer = (centre.0 + co * r * 1.92, centre.1 + s * r * 1.92);
        c.fill(|p| capsule(p, inner, outer) - 0.03 * scale, |_| RAY, 1.0);
    }
    c.fill(|p| dist(p, centre) - r, |p| gradient(SUN_TOP, SUN_BOTTOM, (p.1 - (centre.1 - r)) / (2.0 * r)), 1.0);
    // A gentle highlight on the upper left of the disc.
    c.fill_alpha(|p| {
        let inside = dist(p, centre) < r;
        let d = dist(p, (centre.0 - r * 0.35, centre.1 - r * 0.4));
        if inside { 0.35 * (1.0 - smoothstep(0.0, r * 0.75, d)) } else { 0.0 }
    }, (255.0, 250.0, 220.0));
}

/// A pale crescent moon (optionally with two small sparkles).
fn moon(c: &mut Canvas, centre: P, scale: f32, stars: bool) {
    let r = 0.26 * scale;
    let cut = (centre.0 + r * 0.55, centre.1 - r * 0.38);
    c.fill(
        |p| (dist(p, centre) - r).max(-(dist(p, cut) - r * 0.86)),
        |p| gradient(MOON_TOP, MOON_BOTTOM, (p.1 - (centre.1 - r)) / (2.0 * r)),
        1.0,
    );
    if stars {
        sparkle(c, (0.76, 0.3), 0.06);
        sparkle(c, (0.68, 0.52), 0.035);
    }
}

/// A smooth, organic cloud: overlapping puffs blended together, top-lit.
fn cloud(c: &mut Canvas, centre: P, scale: f32, top: Rgb, bottom: Rgb) {
    let (y0, y1) = (centre.1 - 0.26 * scale, centre.1 + 0.2 * scale);
    c.fill(|p| cloud_sdf(p, centre, scale), |p| gradient(top, bottom, (p.1 - y0) / (y1 - y0)), 1.0);
}

/// A cloud with a soft shadow under it, to separate it from what's behind (sun, moon, cloud).
fn cloud_with_shadow(c: &mut Canvas, centre: P, scale: f32, top: Rgb, bottom: Rgb) {
    let shadow = (centre.0 + 0.0, centre.1 + 0.035 * scale);
    c.fill_alpha(|p| {
        let d = cloud_sdf(p, shadow, scale);
        0.22 * (1.0 - smoothstep(-0.01, 0.07 * scale, d))
    }, (40.0, 50.0, 70.0));
    cloud(c, centre, scale, top, bottom);
}

fn cloud_sdf(p: P, c: P, s: f32) -> f32 {
    let q = ((p.0 - c.0) / s, (p.1 - c.1) / s);
    let k = 0.06;
    let d = smin(dist(q, (-0.18, 0.02)) - 0.15, dist(q, (0.0, -0.08)) - 0.2, k);
    let d = smin(d, dist(q, (0.19, 0.03)) - 0.14, k);
    let d = smin(d, capsule(q, (-0.2, 0.08), (0.21, 0.08)) - 0.1, k);
    d * s
}

/// A soft snowflake: a small six-armed star.
fn flake(c: &mut Canvas, centre: P, r: f32) {
    c.fill(
        |p| {
            let mut d = f32::MAX;
            for i in 0..3 {
                let a = i as f32 * std::f32::consts::PI / 3.0 + std::f32::consts::FRAC_PI_2;
                let (s, co) = (a.sin() * r, a.cos() * r);
                d = d.min(capsule(p, (centre.0 - co, centre.1 - s), (centre.0 + co, centre.1 + s)));
            }
            d - 0.013
        },
        |_| SNOW,
        1.0,
    );
}

/// A four-pointed sparkle.
fn sparkle(c: &mut Canvas, centre: P, r: f32) {
    let w = r * 0.28;
    let pts = [
        (centre.0, centre.1 - r),
        (centre.0 + w, centre.1 - w),
        (centre.0 + r, centre.1),
        (centre.0 + w, centre.1 + w),
        (centre.0, centre.1 + r),
        (centre.0 - w, centre.1 + w),
        (centre.0 - r, centre.1),
        (centre.0 - w, centre.1 - w),
    ];
    c.fill(|p| polygon(p, &pts), |_| (255.0, 253.0, 240.0), 0.95);
}

// ---------- canvas ----------

struct Canvas {
    out: Bitmap,
    /// Icon size in pixels (unit coordinates × s = pixels).
    s: f32,
}

impl Canvas {
    /// Fills a shape (distance in unit coordinates, negative inside) with a colour per point.
    fn fill(&mut self, sdf: impl Fn(P) -> f32, color: impl Fn(P) -> Rgb, alpha: f32) {
        let n = self.out.width;
        for y in 0..n {
            for x in 0..n {
                let p = ((x as f32 + 0.5) / self.s, (y as f32 + 0.5) / self.s);
                let d = sdf(p) * self.s;
                if d < 1.0 {
                    let a = (0.5 - d).clamp(0.0, 1.0) * alpha;
                    if a > 0.0 {
                        self.put(x, y, color(p), a);
                    }
                }
            }
        }
    }

    /// Paints `color` with a per-point opacity (for glows and shadows).
    fn fill_alpha(&mut self, alpha: impl Fn(P) -> f32, color: Rgb) {
        let n = self.out.width;
        for y in 0..n {
            for x in 0..n {
                let p = ((x as f32 + 0.5) / self.s, (y as f32 + 0.5) / self.s);
                let a = alpha(p).clamp(0.0, 1.0);
                if a > 0.002 {
                    self.put(x, y, color, a);
                }
            }
        }
    }

    fn put(&mut self, x: i32, y: i32, c: Rgb, a: f32) {
        let to8 = |v: f32| v.round().clamp(0.0, 255.0) as u32;
        let a8 = to8(a * 255.0);
        let px = a8 << 24 | to8(c.0 * a).min(a8) << 16 | to8(c.1 * a).min(a8) << 8 | to8(c.2 * a).min(a8);
        let slot = &mut self.out.pixels[(y * self.out.width + x) as usize];
        *slot = render::over(*slot, px);
    }
}

// ---------- maths ----------

fn dist(a: P, b: P) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn capsule(p: P, a: P, b: P) -> f32 {
    let (pa, ba) = ((p.0 - a.0, p.1 - a.1), (b.0 - a.0, b.1 - a.1));
    let h = ((pa.0 * ba.0 + pa.1 * ba.1) / (ba.0 * ba.0 + ba.1 * ba.1).max(1e-9)).clamp(0.0, 1.0);
    ((pa.0 - ba.0 * h).powi(2) + (pa.1 - ba.1 * h).powi(2)).sqrt()
}

/// Smooth minimum: blends two shapes with a soft fillet of size `k` (no visible seams).
fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = (0.5 + 0.5 * (b - a) / k).clamp(0.0, 1.0);
    b * (1.0 - h) + a * h - k * h * (1.0 - h)
}

/// Signed distance to a simple polygon (works for concave shapes like the bolt).
fn polygon(p: P, v: &[P]) -> f32 {
    let mut d = f32::MAX;
    let mut inside = false;
    let n = v.len();
    let mut j = n - 1;
    for i in 0..n {
        d = d.min(capsule(p, v[i], v[j]));
        let (a, b) = (v[i], v[j]);
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
        j = i;
    }
    if inside { -d } else { d }
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn gradient(top: Rgb, bottom: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    (top.0 + (bottom.0 - top.0) * t, top.1 + (bottom.1 - top.1) * t, top.2 + (bottom.2 - top.2) * t)
}

/// A colour lightened at the top and deepened at the bottom of the span y0..y1.
fn shade(c: Rgb, y: f32, y0: f32, y1: f32) -> Rgb {
    let t = ((y - y0) / (y1 - y0)).clamp(0.0, 1.0);
    let k = 1.15 - 0.3 * t;
    (c.0 * k, c.1 * k, c.2 * k)
}


