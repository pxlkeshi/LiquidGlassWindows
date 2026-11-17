//! Network and Bluetooth cards, macOS Control Center style, stacked to the left of the
//! weather card: a round icon button (blue when on), the name, and a status line.
//! Clicking the button turns Wi-Fi / Bluetooth on or off (Ethernet just shows its status).

use crate::anim::{self, Spring};
use crate::connectivity::{self, Status};
use crate::glass;
use crate::render::{self, Bitmap};
use crate::window;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, SetTimer};

/// Timer for noticing wallpaper changes (the glass shows the wallpaper behind it).
pub const TIMER_ID: usize = 6;
const TICK_MS: u32 = 2000;

/// Apple's system blue (dark appearance).
const BLUE: (f32, f32, f32) = (10.0, 132.0, 255.0);
/// Opacity of the round button when off.
const OFF_OPACITY: f32 = 0.18;
/// Gap between cards, as a fraction of the screen height (same as between weather and music).
const GAP_FRACTION: f32 = 0.012;
/// Button diameter, as a fraction of the screen height.
const BUTTON_FRACTION: f32 = 0.034;
/// Corner radius as a share of half the card's height (1.0 = a full pill).
const CORNER_ROUNDNESS: f32 = 0.9;
/// How much the button shrinks while pressed.
const PRESS_SHRINK: f32 = 0.1;
/// Opacity of the status line.
const SECONDARY: f32 = 0.7;
/// How long to show a click's result before trusting what Windows reports again.
const OPTIMISTIC_SECS: f32 = 4.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Network,
    Bluetooth,
}

struct Tile {
    hwnd: HWND,
    glass: Option<glass::Glass>,
    /// Where the button is, in window coordinates.
    button: (i32, i32, i32, i32),
    /// 0 = off (grey), 1 = on (blue).
    on: Spring,
    press: Spring,
    pressed: bool,
    /// State shown right after a click, until Windows confirms it.
    optimistic: Option<(bool, Instant)>,
    last_frame: Instant,
    was_animating: bool,
    started: bool,
}

thread_local! {
    static TILES: RefCell<HashMap<Kind, Tile>> = RefCell::new(HashMap::new());
}

/// The weather window: the cards line up to its left.
static ANCHOR: AtomicIsize = AtomicIsize::new(0);

pub fn set_anchor(hwnd: HWND) {
    ANCHOR.store(hwnd.0 as isize, Ordering::Relaxed);
}

pub fn register(kind: Kind, hwnd: HWND) {
    TILES.with(|t| {
        t.borrow_mut().insert(
            kind,
            Tile {
                hwnd,
                glass: None,
                button: (0, 0, 0, 0),
                on: Spring::new(0.0, 0.35, 0.9),
                press: Spring::new(0.0, 0.25, 0.7),
                pressed: false,
                optimistic: None,
                last_frame: Instant::now(),
                was_animating: false,
                started: false,
            },
        );
    });
}

/// Redraws both cards (e.g. after the weather card moved or changed size).
pub fn follow_anchor() {
    for kind in [Kind::Network, Kind::Bluetooth] {
        if let Some(h) = TILES.with(|t| t.borrow().get(&kind).map(|t| t.hwnd)) {
            redraw(kind, h);
        }
    }
}

/// What a card shows for the current status.
struct View {
    title: &'static str,
    status: String,
    on: bool,
    icon: Icon,
}

#[derive(Clone, Copy)]
enum Icon {
    /// Wi-Fi with this many of its three arcs lit (None = all lit, no signal shown).
    Wifi(Option<u8>),
    Ethernet,
    Bluetooth,
}

fn view(kind: Kind, s: &Status, optimistic: Option<bool>) -> View {
    match kind {
        // No Wi-Fi at all: an Ethernet card instead.
        Kind::Network if s.wired && s.wifi_on.is_none() => {
            View { title: "Ethernet", status: "Connected".into(), on: true, icon: Icon::Ethernet }
        }
        Kind::Network => {
            let actual = s.wifi_on.unwrap_or(false);
            let on = optimistic.unwrap_or(actual);
            let status = match (s.wifi_on, on) {
                (None, _) => "Unavailable".to_string(),
                (_, false) => "Off".to_string(),
                (_, true) if !actual => "Turning On…".to_string(),
                _ if s.connected && !s.ssid.is_empty() => s.ssid.clone(),
                _ if s.connected => "Connected".to_string(),
                _ => "Not Connected".to_string(),
            };
            // A cable plugged in as well: say so on the same line.
            let status = if s.wired { format!("{status} · Ethernet") } else { status };
            let lit = (on && s.signal > 0).then(|| match s.signal {
                67.. => 3,
                34..=66 => 2,
                _ => 1,
            });
            View { title: "Wi-Fi", status, on, icon: Icon::Wifi(lit) }
        }
        Kind::Bluetooth => {
            let actual = s.bluetooth_on.unwrap_or(false);
            let on = optimistic.unwrap_or(actual);
            let status = match (s.bluetooth_on, on) {
                (None, _) => "Unavailable".to_string(),
                (_, false) => "Off".to_string(),
                (_, true) if !actual => "Turning On…".to_string(),
                _ if !s.devices.is_empty() => s.devices.join(", "),
                _ => "On".to_string(),
            };
            View { title: "Bluetooth", status, on, icon: Icon::Bluetooth }
        }
    }
}

pub fn redraw(kind: Kind, hwnd: HWND) {
    let (sw, sh) = window::screen_size();
    let shf = sh as f32;
    let status = connectivity::current();
    let pad = (shf * 0.016) as i32;
    let gap = (shf * GAP_FRACTION) as i32;

    // Same width as the weather card; the network card's top lines up with it, Bluetooth below.
    let anchor = anchor_rect().unwrap_or(RECT {
        left: sw - (shf * 0.015) as i32 - (shf * 0.2) as i32,
        top: (shf * 0.015) as i32,
        right: sw - (shf * 0.015) as i32,
        bottom: (shf * 0.09) as i32,
    });
    let width = (anchor.right - anchor.left - 2 * pad).max((shf * 0.1) as i32);
    let button = (shf * BUTTON_FRACTION).round() as i32;
    let (cw, ch) = (width + 2 * pad, button + 2 * pad);
    let x = anchor.left - gap - cw;
    let y = match kind {
        Kind::Network => anchor.top,
        Kind::Bluetooth => anchor.top + ch + gap,
    };
    let pos = POINT { x, y };

    let animating = TILES.with(|tiles| {
        let mut tiles = tiles.borrow_mut();
        let Some(tile) = tiles.get_mut(&kind) else { return false };

        // Advance the animations.
        let now = Instant::now();
        let dt = if tile.was_animating { now.duration_since(tile.last_frame).as_secs_f32().min(0.05) } else { 0.0 };
        tile.last_frame = now;
        let actual = match kind {
            Kind::Network => status.wifi_on.unwrap_or(status.wired),
            Kind::Bluetooth => status.bluetooth_on.unwrap_or(false),
        };
        if let Some((wanted, at)) = tile.optimistic {
            if wanted == actual || at.elapsed().as_secs_f32() > OPTIMISTIC_SECS {
                tile.optimistic = None;
            }
        }
        let v = view(kind, &status, tile.optimistic.map(|o| o.0));
        tile.on.target = if v.on { 1.0 } else { 0.0 };
        tile.press.target = if tile.pressed { 1.0 } else { 0.0 };
        if !tile.started {
            tile.on.snap();
            tile.started = true;
        }
        tile.on.step(dt);
        tile.press.step(dt);

        let content = draw(&v, tile.on.value, tile.press.value, shf, width, button);
        tile.button = (pad, pad, pad + button, pad + button);

        let glass = match tile.glass.take() {
            Some(g) if g.size() == (cw, ch) => g,
            // Nearly pill-shaped corners.
            _ => glass::Glass::new(cw, ch, ch as f32 / 2.0 * CORNER_ROUNDNESS, shf),
        };
        let mut glass = glass;
        if let Some(bmp) = glass.render(&content, pad, (pos.x, pos.y), true) {
            if let Err(e) = window::present(hwnd, &bmp, pos) {
                eprintln!("could not draw tile: {e}");
            }
        }
        tile.glass = Some(glass);
        let animating = !tile.on.settled() || !tile.press.settled() || tile.pressed;
        tile.was_animating = animating;
        animating
    });
    anim::set_animating(hwnd, animating);
    unsafe {
        SetTimer(Some(hwnd), TIMER_ID, TICK_MS, None);
    }
}

/// Handles a frame request from the shared frame clock.
pub fn frame(kind: Kind, hwnd: HWND) {
    anim::frame_done(hwnd);
    redraw(kind, hwnd);
}

/// Redraws if the wallpaper behind the glass changed.
pub fn tick(kind: Kind, hwnd: HWND) {
    let changed = TILES.with(|t| {
        let mut t = t.borrow_mut();
        let Some(tile) = t.get_mut(&kind) else { return false };
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(hwnd, &mut r);
        }
        tile.glass.as_mut().is_some_and(|g| g.render(&Bitmap::new(1, 1), 0, (r.left, r.top), false).is_some())
    });
    if changed {
        redraw(kind, hwnd);
    }
}

fn on_button(kind: Kind, x: i32, y: i32) -> bool {
    TILES.with(|t| {
        t.borrow().get(&kind).is_some_and(|tile| {
            let b = tile.button;
            // A little slack around the circle makes it easier to hit.
            x >= b.0 - 4 && y >= b.1 - 4 && x < b.2 + 4 && y < b.3 + 4
        })
    })
}

/// Whether the mouse (window coordinates) is over the button (for the hand cursor).
pub fn over_control(kind: Kind, x: i32, y: i32) -> bool {
    let s = connectivity::current();
    let toggles = match kind {
        Kind::Network => s.wifi_on.is_some(),
        Kind::Bluetooth => s.bluetooth_on.is_some(),
    };
    toggles && on_button(kind, x, y)
}

pub fn press(kind: Kind, hwnd: HWND, x: i32, y: i32) {
    if !over_control(kind, x, y) {
        return;
    }
    TILES.with(|t| {
        if let Some(tile) = t.borrow_mut().get_mut(&kind) {
            tile.pressed = true;
        }
    });
    unsafe {
        SetCapture(hwnd);
    }
    redraw(kind, hwnd);
}

pub fn release(kind: Kind, hwnd: HWND, x: i32, y: i32) {
    let was_pressed = TILES.with(|t| t.borrow_mut().get_mut(&kind).is_some_and(|tile| std::mem::take(&mut tile.pressed)));
    unsafe {
        let _ = ReleaseCapture();
    }
    if was_pressed && over_control(kind, x, y) {
        let s = connectivity::current();
        TILES.with(|t| {
            if let Some(tile) = t.borrow_mut().get_mut(&kind) {
                let actual = match kind {
                    Kind::Network => s.wifi_on.unwrap_or(false),
                    Kind::Bluetooth => s.bluetooth_on.unwrap_or(false),
                };
                let shown = tile.optimistic.map(|o| o.0).unwrap_or(actual);
                tile.optimistic = Some((!shown, Instant::now()));
                match kind {
                    Kind::Network => connectivity::set_wifi(!shown),
                    Kind::Bluetooth => connectivity::set_bluetooth(!shown),
                }
            }
        });
    }
    redraw(kind, hwnd);
}

/// The mouse was taken away mid-press: un-press without acting.
pub fn cancel_press(kind: Kind, hwnd: HWND) {
    let was = TILES.with(|t| t.borrow_mut().get_mut(&kind).is_some_and(|tile| std::mem::take(&mut tile.pressed)));
    if was {
        redraw(kind, hwnd);
    }
}

fn anchor_rect() -> Option<RECT> {
    let raw = ANCHOR.load(Ordering::Relaxed);
    if raw == 0 {
        return None;
    }
    let mut r = RECT::default();
    unsafe { GetWindowRect(HWND(raw as *mut std::ffi::c_void), &mut r).ok()? };
    Some(r)
}

// ---------- drawing ----------

/// The card's content: button on the left, title and status to its right.
fn draw(v: &View, on: f32, press: f32, sh: f32, width: i32, button: i32) -> Bitmap {
    let mut out = Bitmap::new(width, button);
    let d = button as f32;
    let (cx, cy) = (d / 2.0, d / 2.0);

    // Round button: grey glass when off, blue when on (blended while animating).
    let on = on.clamp(0.0, 1.0);
    let r = d / 2.0 * (1.0 - PRESS_SHRINK * press.clamp(0.0, 1.2));
    let color = (
        255.0 + (BLUE.0 - 255.0) * on,
        255.0 + (BLUE.1 - 255.0) * on,
        255.0 + (BLUE.2 - 255.0) * on,
    );
    let alpha = OFF_OPACITY + (1.0 - OFF_OPACITY) * on;
    fill(&mut out, (cx - r, cy - r, cx + r, cy + r), |p| ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt() - r, color, alpha);

    // White icon on top, scaled with the button.
    let s = r * 2.0;
    draw_icon(&mut out, v.icon, cx, cy, s);

    // Title and status.
    let text_x = button + (sh * 0.010) as i32;
    let text_w = (width - text_x).max(1);
    let title = fit(&render::sf_bold(), v.title, sh * 0.0105, text_w);
    let mut status = fit(&render::sf(), &v.status, sh * 0.0088, text_w);
    status.fade(SECONDARY);
    let gap = (sh * 0.006) as i32;
    let block = title.height + gap + status.height;
    let top = (button - block) / 2;
    out.blit(&title, text_x, top);
    out.blit(&status, text_x, top + title.height + gap);
    out
}

fn draw_icon(out: &mut Bitmap, icon: Icon, cx: f32, cy: f32, s: f32) {
    let white = (255.0, 255.0, 255.0);
    let bounds = (cx - s / 2.0, cy - s / 2.0, cx + s / 2.0, cy + s / 2.0);
    match icon {
        Icon::Wifi(lit) => {
            // A dot with three arcs above it; unlit arcs are faint (signal strength).
            let base = (cx, cy + s * 0.2);
            let t = s * 0.07;
            let half = 42f32.to_radians();
            fill(out, bounds, |p| dist(p, base) - s * 0.055, white, 1.0);
            for (i, radius) in [0.15, 0.27, 0.39].into_iter().enumerate() {
                let alpha = match lit {
                    Some(n) if (i as u8) >= n => 0.35,
                    _ => 1.0,
                };
                fill(out, bounds, |p| arc(p, base, radius * s, half) - t / 2.0, white, alpha);
            }
        }
        Icon::Bluetooth => {
            // The Bluetooth rune: a vertical stroke with two crossing chevrons.
            let (h, w, t) = (s * 0.27, s * 0.14, s * 0.06);
            let top = (cx, cy - h);
            let bottom = (cx, cy + h);
            let upper_right = (cx + w, cy - h * 0.5);
            let lower_right = (cx + w, cy + h * 0.5);
            let upper_left = (cx - w, cy - h * 0.5);
            let lower_left = (cx - w, cy + h * 0.5);
            fill(
                out,
                bounds,
                |p| {
                    capsule(p, top, bottom)
                        .min(capsule(p, top, upper_right))
                        .min(capsule(p, upper_right, lower_left))
                        .min(capsule(p, bottom, lower_right))
                        .min(capsule(p, lower_right, upper_left))
                        - t / 2.0
                },
                white,
                1.0,
            );
        }
        Icon::Ethernet => {
            // < · · · > : two chevrons with three dots between them.
            let (w, h, t) = (s * 0.32, s * 0.16, s * 0.065);
            fill(
                out,
                bounds,
                |p| {
                    let left = capsule(p, (cx - w + h, cy - h), (cx - w, cy)).min(capsule(p, (cx - w, cy), (cx - w + h, cy + h)));
                    let right = capsule(p, (cx + w - h, cy - h), (cx + w, cy)).min(capsule(p, (cx + w, cy), (cx + w - h, cy + h)));
                    (left.min(right)) - t / 2.0
                },
                white,
                1.0,
            );
            for dx in [-0.11, 0.0, 0.11] {
                let c = (cx + dx * s, cy);
                fill(out, bounds, |p| dist(p, c) - s * 0.035, white, 1.0);
            }
        }
    }
}

/// Renders `text`, shortening it with "…" to fit `max_width` (binary search on length).
fn fit(font: &ab_glyph::FontRef, text: &str, height: f32, max_width: i32) -> Bitmap {
    let full = render::line(font, text, height, 1.0, render::WHITE);
    if full.width <= max_width {
        return full;
    }
    let chars: Vec<char> = text.chars().collect();
    let shortened = |n: usize| chars[..n].iter().collect::<String>().trim_end().to_string() + "…";
    let (mut lo, mut hi) = (1, chars.len().saturating_sub(1).max(1));
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        if render::line(font, &shortened(mid), height, 1.0, render::WHITE).width <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    render::line(font, &shortened(lo), height, 1.0, render::WHITE)
}

// ---------- shape helpers ----------

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Distance to the segment a–b.
fn capsule(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (pa, ba) = ((p.0 - a.0, p.1 - a.1), (b.0 - a.0, b.1 - a.1));
    let h = ((pa.0 * ba.0 + pa.1 * ba.1) / (ba.0 * ba.0 + ba.1 * ba.1).max(1e-6)).clamp(0.0, 1.0);
    ((pa.0 - ba.0 * h).powi(2) + (pa.1 - ba.1 * h).powi(2)).sqrt()
}

/// Distance to an arc of `radius` around `c`, spanning ±`half` radians from straight up
/// (rounded ends).
fn arc(p: (f32, f32), c: (f32, f32), radius: f32, half: f32) -> f32 {
    let (dx, dy) = (p.0 - c.0, p.1 - c.1);
    let angle = dx.atan2(-dy); // 0 = straight up
    if angle.abs() <= half {
        ((dx * dx + dy * dy).sqrt() - radius).abs()
    } else {
        let a = half * angle.signum();
        dist(p, (c.0 + radius * a.sin(), c.1 - radius * a.cos()))
    }
}

/// Fills pixels in `bounds` by a distance function (negative inside), anti-aliased, with
/// `color` at `alpha`.
pub(crate) fn fill(out: &mut Bitmap, bounds: (f32, f32, f32, f32), distance: impl Fn((f32, f32)) -> f32, color: (f32, f32, f32), alpha: f32) {
    let (x0, y0) = ((bounds.0 - 1.0).floor().max(0.0) as i32, (bounds.1 - 1.0).floor().max(0.0) as i32);
    let x1 = ((bounds.2 + 1.0).ceil() as i32).min(out.width);
    let y1 = ((bounds.3 + 1.0).ceil() as i32).min(out.height);
    for y in y0..y1 {
        for x in x0..x1 {
            let d = distance((x as f32 + 0.5, y as f32 + 0.5));
            if d < 1.0 {
                let a = (0.5 - d).clamp(0.0, 1.0) * alpha;
                if a <= 0.0 {
                    continue;
                }
                let to8 = |v: f32| v.round().clamp(0.0, 255.0) as u32;
                let a8 = to8(a * 255.0);
                let p = a8 << 24 | to8(color.0 * a).min(a8) << 16 | to8(color.1 * a).min(a8) << 8 | to8(color.2 * a).min(a8);
                let slot = &mut out.pixels[(y * out.width + x) as usize];
                *slot = render::over(*slot, p);
            }
        }
    }
}

/// Signed distance to a closed polygon (negative inside).
pub(crate) fn polygon(p: (f32, f32), v: &[(f32, f32)]) -> f32 {
    let dot = |a: (f32, f32), b: (f32, f32)| a.0 * b.0 + a.1 * b.1;
    let mut d = dot((p.0 - v[0].0, p.1 - v[0].1), (p.0 - v[0].0, p.1 - v[0].1));
    let mut sign = 1.0;
    let mut j = v.len() - 1;
    for i in 0..v.len() {
        let e = (v[j].0 - v[i].0, v[j].1 - v[i].1);
        let w = (p.0 - v[i].0, p.1 - v[i].1);
        let h = (dot(w, e) / dot(e, e).max(1e-9)).clamp(0.0, 1.0);
        let b = (w.0 - e.0 * h, w.1 - e.1 * h);
        d = d.min(dot(b, b));
        let c = (p.1 >= v[i].1, p.1 < v[j].1, e.0 * w.1 > e.1 * w.0);
        if (c.0 && c.1 && c.2) || (!c.0 && !c.1 && !c.2) {
            sign = -sign;
        }
        j = i;
    }
    sign * d.sqrt()
}
