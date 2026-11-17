//! Battery: a wide glass pill under the right-hand column, from the volume pill's left edge
//! to the music card's right edge.
//!
//! Left to right: a macOS-style battery glyph (green while charging, yellow in battery saver,
//! red when low), the percentage, a level bar filling the middle, and the status on the right
//! ("Charging", "Fully Charged", "3 hr 12 min left"…).
//!
//! Nothing animates: Windows is asked for the battery state every couple of seconds (a cheap
//! call) and the pill is only redrawn when something it shows changes.

use crate::glass;
use crate::render::{self, Bitmap};
use crate::tiles::{fill, polygon};
use crate::window;
use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, SetTimer};

/// Timer for re-reading the battery and noticing wallpaper changes.
pub const TIMER_ID: usize = 9;
const TICK_MS: u32 = 2000;

/// Gap above the pill (same as between the other cards), as a fraction of the screen height.
const GAP_FRACTION: f32 = 0.012;
/// Pill height, as a fraction of the screen height.
const HEIGHT_FRACTION: f32 = 0.05;
/// Space at each rounded end, as a share of the pill's height.
const END_PAD: f32 = 0.42;
/// Battery glyph body height, as a fraction of the screen height (its width is 2.1x this).
const GLYPH_FRACTION: f32 = 0.0128;
/// Percentage and status text sizes (digit height), as fractions of the screen height.
const PERCENT_FRACTION: f32 = 0.0112;
const STATUS_FRACTION: f32 = 0.0096;
/// Opacity of the status text.
const SECONDARY: f32 = 0.62;
/// Level bar thickness, as a fraction of the screen height.
const BAR_FRACTION: f32 = 0.0052;
/// Opacity of the empty part of the level bar and of the glyph's outline.
const TRACK: f32 = 0.16;
const OUTLINE: f32 = 0.5;
/// At or below this percentage (on battery), the level turns red.
const LOW: u8 = 20;

/// Apple's system colours (dark appearance).
const WHITE: (f32, f32, f32) = (255.0, 255.0, 255.0);
const GREEN: (f32, f32, f32) = (48.0, 209.0, 88.0);
const YELLOW: (f32, f32, f32) = (255.0, 214.0, 10.0);
const RED: (f32, f32, f32) = (255.0, 69.0, 58.0);

/// What the pill shows.
#[derive(Clone, Copy, PartialEq, Default)]
struct Power {
    /// None = no battery (a desktop PC) or Windows doesn't know.
    percent: Option<u8>,
    plugged_in: bool,
    charging: bool,
    saver: bool,
    /// Estimated minutes left on battery, if Windows has an estimate.
    minutes_left: Option<u32>,
}

fn read() -> Power {
    let mut s = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut s) }.is_err() {
        return Power::default();
    }
    let no_battery = s.BatteryFlag & 128 != 0;
    let plugged_in = s.ACLineStatus == 1;
    Power {
        percent: (!no_battery && s.BatteryLifePercent <= 100).then_some(s.BatteryLifePercent),
        plugged_in,
        charging: s.BatteryFlag & 8 != 0,
        saver: s.SystemStatusFlag == 1,
        minutes_left: (!plugged_in && s.BatteryLifeTime != u32::MAX).then(|| (s.BatteryLifeTime + 30) / 60),
    }
}

struct State {
    glass: Option<glass::Glass>,
    shown: Option<(Power, (i32, i32, i32, i32))>,
}

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State { glass: None, shown: None }) };
}

/// The music card (its right edge and bottom); the left edge follows the volume pill.
static ABOVE: AtomicIsize = AtomicIsize::new(0);
static HWND_SELF: AtomicIsize = AtomicIsize::new(0);

pub fn set_anchor(above: HWND) {
    ABOVE.store(above.0 as isize, Ordering::Relaxed);
}

pub fn register(hwnd: HWND) {
    HWND_SELF.store(hwnd.0 as isize, Ordering::Relaxed);
}

/// Redraws after the cards above moved or changed size.
pub fn follow_anchor() {
    let raw = HWND_SELF.load(Ordering::Relaxed);
    if raw != 0 {
        redraw(HWND(raw as *mut std::ffi::c_void));
    }
}

fn rect_of(raw: isize) -> Option<RECT> {
    if raw == 0 {
        return None;
    }
    let mut r = RECT::default();
    unsafe { GetWindowRect(HWND(raw as *mut std::ffi::c_void), &mut r).ok()? };
    Some(r)
}

/// Where the pill sits: x, y, width, height (for the stocks card under it).
pub fn rect() -> Option<(i32, i32, i32, i32)> {
    geometry()
}

/// Where the pill sits: x, y, width, height.
fn geometry() -> Option<(i32, i32, i32, i32)> {
    let shf = window::screen_size().1 as f32;
    let left = crate::sliders::volume_left();
    let above = rect_of(ABOVE.load(Ordering::Relaxed))?;
    let h = (shf * HEIGHT_FRACTION).round() as i32;
    let y = above.bottom + (shf * GAP_FRACTION) as i32;
    Some((left, y, (above.right - left).max(h * 2), h))
}

pub fn redraw(hwnd: HWND) {
    let shf = window::screen_size().1 as f32;
    let Some(geom) = geometry() else { return };
    let (x, y, w, h) = geom;
    let power = read();
    STATE.with(|st| {
        let mut st = st.borrow_mut();
        let content = draw(&power, w, h, shf);
        let mut glass = match st.glass.take() {
            Some(g) if g.size() == (w, h) => g,
            _ => glass::Glass::new(w, h, h as f32 / 2.0, shf),
        };
        if let Some(bmp) = glass.render(&content, 0, (x, y), true) {
            if let Err(e) = window::present(hwnd, &bmp, POINT { x, y }) {
                eprintln!("could not draw battery: {e}");
            }
        }
        st.glass = Some(glass);
        st.shown = Some((power, geom));
    });
    unsafe {
        SetTimer(Some(hwnd), TIMER_ID, TICK_MS, None);
    }
}

/// Redraws if the battery, the cards above, or the wallpaper behind the glass changed.
pub fn tick(hwnd: HWND) {
    let changed = STATE.with(|st| {
        let mut st = st.borrow_mut();
        let Some((power, geom)) = st.shown else { return true };
        if power != read() || Some(geom) != geometry() {
            return true;
        }
        st.glass.as_mut().is_some_and(|g| g.render(&Bitmap::new(1, 1), 0, (geom.0, geom.1), false).is_some())
    });
    if changed {
        redraw(hwnd);
    }
}

// ---------- drawing ----------

fn status_text(p: &Power) -> String {
    let Some(percent) = p.percent else { return "No Battery".into() };
    let text = if p.charging {
        "Charging".to_string()
    } else if p.plugged_in {
        if percent >= 95 { "Fully Charged".into() } else { "Plugged In, Not Charging".into() }
    } else {
        match p.minutes_left {
            Some(m) if m >= 60 => format!("{} hr {} min left", m / 60, m % 60),
            Some(m) => format!("{m} min left"),
            None => "On Battery".into(),
        }
    };
    if p.saver && !p.plugged_in { format!("{text} · Battery Saver") } else { text }
}

fn level_color(p: &Power) -> (f32, f32, f32) {
    match p.percent {
        _ if p.charging => GREEN,
        _ if p.saver && !p.plugged_in => YELLOW,
        Some(v) if v <= LOW && !p.plugged_in => RED,
        _ => WHITE,
    }
}

fn draw(p: &Power, w: i32, h: i32, sh: f32) -> Bitmap {
    let mut out = Bitmap::new(w, h);
    let cy = h as f32 / 2.0;
    let pad = h as f32 * END_PAD;
    let level = p.percent.unwrap_or(0) as f32 / 100.0;
    let color = level_color(p);

    // Battery glyph: outlined body, a nub on the right, the level inside, a bolt while charging.
    let gh = (sh * GLYPH_FRACTION).round();
    let gw = (gh * 2.1).round();
    let stroke = (gh * 0.09).max(1.2);
    let (bx0, by0) = (pad, cy - gh / 2.0);
    let body = (bx0 + gw / 2.0, cy);
    let radius = gh * 0.32;
    let bounds = (bx0, by0, bx0 + gw + gh * 0.25, by0 + gh);
    fill(&mut out, bounds, |q| rbox(q, body, (gw / 2.0, gh / 2.0), radius).abs() - stroke / 2.0, WHITE, OUTLINE);
    let nub = (bx0 + gw + gh * 0.1, cy);
    fill(&mut out, bounds, |q| rbox(q, nub, (gh * 0.07, gh * 0.18), gh * 0.07), WHITE, OUTLINE);
    let inset = stroke + (gh * 0.08).max(1.0);
    let inner_w = gw - 2.0 * inset;
    let fill_w = (inner_w * level).max(if p.percent.is_some() { gh * 0.18 } else { 0.0 });
    if fill_w > 0.0 {
        let half = (fill_w / 2.0, gh / 2.0 - inset);
        let centre = (bx0 + inset + half.0, cy);
        fill(&mut out, bounds, |q| rbox(q, centre, half, (radius - inset).max(1.0)), color, 1.0);
    }
    if p.charging {
        // A lightning bolt, with a thin dark edge so it reads on the green.
        let bolt: Vec<(f32, f32)> = [(0.12, -0.5), (-0.3, 0.08), (-0.02, 0.08), (-0.12, 0.5), (0.3, -0.08), (0.02, -0.08)]
            .iter()
            .map(|&(u, v)| (body.0 + u * gh * 0.9, cy + v * gh * 1.05))
            .collect();
        fill(&mut out, bounds, |q| polygon(q, &bolt) - 1.0, (0.0, 0.0, 0.0), 0.35);
        fill(&mut out, bounds, |q| polygon(q, &bolt), WHITE, 1.0);
    }

    // Percentage, right after the glyph, sitting on the same centre line.
    let mut x = (bounds.2 + sh * 0.009).round() as i32;
    let digit_h = sh * PERCENT_FRACTION;
    let baseline = (cy + digit_h / 2.0).round() as i32;
    let percent = match p.percent {
        Some(v) => render::line(&render::sf_medium(), &format!("{v}%"), digit_h, 1.0, render::WHITE),
        None => render::line(&render::sf_medium(), "—", digit_h, 1.0, render::WHITE),
    };
    out.blit(&percent, x, baseline - percent.baseline);
    x += percent.width;

    // Status, right-aligned against the far end.
    let mut status = render::line(&render::sf(), &status_text(p), sh * STATUS_FRACTION, 1.0, render::WHITE);
    status.fade(SECONDARY);
    let status_x = w - pad.round() as i32 - status.width;
    out.blit(&status, status_x, baseline - status.baseline);

    // Level bar in the middle.
    let gap = sh * 0.016;
    let (x0, x1) = (x as f32 + gap, status_x as f32 - gap);
    if x1 - x0 > sh * 0.03 && p.percent.is_some() {
        let t = sh * BAR_FRACTION;
        let bar_bounds = (x0, cy - t, x1, cy + t);
        let track = |q: (f32, f32)| rbox(q, ((x0 + x1) / 2.0, cy), ((x1 - x0) / 2.0, t / 2.0), t / 2.0);
        fill(&mut out, bar_bounds, track, WHITE, TRACK);
        // The filled part keeps a rounded end, like the track.
        let fx = (x0 + (x1 - x0) * level).max(x0 + t);
        let filled = |q: (f32, f32)| rbox(q, ((x0 + fx) / 2.0, cy), ((fx - x0) / 2.0, t / 2.0), t / 2.0);
        fill(&mut out, bar_bounds, filled, color, if color == WHITE { 0.9 } else { 1.0 });
    }
    out
}

/// Signed distance to a rounded box at `c` with half-size `half` and corner radius `r`.
fn rbox(p: (f32, f32), c: (f32, f32), half: (f32, f32), r: f32) -> f32 {
    let r = r.min(half.0).min(half.1);
    let q = ((p.0 - c.0).abs() - half.0 + r, (p.1 - c.1).abs() - half.1 + r);
    (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt() + q.0.max(q.1).min(0.0) - r
}
