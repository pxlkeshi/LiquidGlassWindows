//! Volume and brightness: two tall glass sliders side by side, iOS Control Center style,
//! filling the space under the Bluetooth card down to the bottom of the music card.
//!
//! - A white fill rises from the bottom, clipped to the pill shape.
//! - The icon at the base (speaker / sun) changes with the level and turns grey where the
//!   white fill covers it.
//! - Drag anywhere to change the level relative to where you started (no jumping), or use the
//!   scroll wheel.
//! - Clicking the speaker opens the volume mixer: the pill stretches out to the left like
//!   liquid glass (a spring with a little overshoot and squash) into a panel listing every
//!   app that's playing sound, each with its own slider. Clicking the speaker again, or
//!   anywhere outside, springs it closed.
//! - Outside changes (keyboard keys, Windows' flyout) glide in on a spring; while dragging,
//!   the fill follows the mouse exactly.

use crate::anim::{self, Spring};
use crate::glass;
use crate::levels::{self, Levels};
use crate::mixer::{self, App};
use crate::render::{self, Bitmap};
use crate::window;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetWindowRect, HHOOK, KillTimer, MSLLHOOKSTRUCT, PostMessageW, SetTimer, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_MOUSE_LL, WM_APP, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_RBUTTONDOWN,
};

/// Timer for noticing wallpaper changes (the glass shows the wallpaper behind it).
pub const TIMER_ID: usize = 7;
const TICK_MS: u32 = 2000;

/// Gap between the sliders and the cards around them, as a fraction of the screen height.
const GAP_FRACTION: f32 = 0.012;
/// Slider width as a share of half the column (1.0 = the two fill the column's width).
const SLIM: f32 = 0.65;
/// Corner radius as a share of the slider's width (0.5 would be fully round ends).
const ROUNDNESS: f32 = 0.5;
/// Opacity of the white fill.
/// The fill is tinted liquid glass: a translucent white over the glass (the wallpaper still
/// shows through), slightly stronger towards its top, with a soft bright edge on top.
const FILL_TINT_TOP: f32 = 0.42;
const FILL_TINT_BOTTOM: f32 = 0.26;
/// Opacity of the bright edge along the top of the fill.
const FILL_EDGE: f32 = 0.55;
/// Icon colour on top of the white fill (grey) and on the glass (white).
const ICON: (f32, f32, f32) = (255.0, 255.0, 255.0);
/// Scroll-wheel step per notch.
const WHEEL_STEP: f32 = 0.05;
/// Width of the open volume mixer, as a fraction of the screen height.
const MIXER_WIDTH_FRACTION: f32 = 0.25;
/// Spring for opening/closing the mixer: a little overshoot, like liquid.
const EXPAND_SPRING: (f32, f32) = (0.55, 0.74);
/// How much the panel squashes vertically while it's moving fast (the "liquid" feel).
const SQUASH: f32 = 0.012;
const MAX_SQUASH: f32 = 0.05;
/// Timer that prepares the open mixer's glass in advance (so the first opening is smooth).
pub const PREWARM_TIMER_ID: usize = 8;
/// Sent by the outside-click watcher: close the mixer.
pub const WM_MIXER_CLOSE: u32 = WM_APP + 7;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Volume,
    Brightness,
}

struct Drag {
    start_level: f32,
    start_y: i32,
    moved: bool,
}

/// What a press inside the open mixer is doing.
#[derive(Clone)]
enum MixerPress {
    /// Dragging an app's slider (its key, and where its bar is in window coordinates).
    Bar { key: String, x0: i32, width: i32 },
    /// Pressed an app's icon (mutes it on release).
    Icon(String),
    /// Pressed the header's speaker (mutes everything on release).
    MuteAll,
}

/// Clickable parts of the open mixer, in window coordinates.
#[derive(Clone, Default)]
struct MixerHits {
    /// Where the master slider (the original pill) starts.
    master_x0: i32,
    rows: Vec<(String, (i32, i32, i32, i32), (i32, i32, i32, i32))>, // key, icon rect, bar rect
    mute_all: (i32, i32, i32, i32),
}

struct Slider {
    hwnd: HWND,
    /// Volume only: 0 = closed pill, 1 = mixer fully open.
    expand: Spring,
    open: bool,
    /// The wallpaper behind everywhere the volume pill / mixer can reach, prepared once: the
    /// closed pill and every animation frame are drawn over it with the same maths, so
    /// opening and closing can't jump or flicker.
    backdrop: Option<glass::Backdrop>,
    hits: MixerHits,
    mixer_press: Option<MixerPress>,
    /// App volumes the user just set, shown until Windows reports them.
    app_pending: HashMap<String, (f32, Instant)>,
    /// The drawn app list, reused while only the panel's size animates.
    list_cache: Option<(u64, Bitmap)>,
    glass: Option<glass::Glass>,
    /// Shown level (0..1).
    level: Spring,
    drag: Option<Drag>,
    /// The level the user set, shown until the system reports it (avoids flicking back).
    pending: Option<(f32, Instant)>,
    height: i32,
    icon_centre: (i32, i32),
    last_frame: Instant,
    was_animating: bool,
    started: bool,
}

thread_local! {
    static SLIDERS: RefCell<HashMap<Kind, Slider>> = RefCell::new(HashMap::new());
}

/// The Bluetooth card (the sliders start below it) and the music card (they end level with it).
static ABOVE: AtomicIsize = AtomicIsize::new(0);
static BOTTOM: AtomicIsize = AtomicIsize::new(0);

pub fn set_anchors(above: HWND, bottom: HWND) {
    ABOVE.store(above.0 as isize, Ordering::Relaxed);
    BOTTOM.store(bottom.0 as isize, Ordering::Relaxed);
}

pub fn register(kind: Kind, hwnd: HWND) {
    SLIDERS.with(|s| {
        s.borrow_mut().insert(
            kind,
            Slider {
                hwnd,
                expand: Spring::new(0.0, EXPAND_SPRING.0, EXPAND_SPRING.1),
                open: false,
                backdrop: None,
                hits: MixerHits::default(),
                mixer_press: None,
                app_pending: HashMap::new(),
                list_cache: None,
                glass: None,
                level: Spring::new(0.0, 0.3, 1.0),
                drag: None,
                pending: None,
                height: 1,
                icon_centre: (0, 0),
                last_frame: Instant::now(),
                was_animating: false,
                started: false,
            },
        );
    });
    if kind == Kind::Volume {
        *VOLUME.lock().unwrap() = hwnd.0 as isize;
        unsafe {
            SetTimer(Some(hwnd), PREWARM_TIMER_ID, 1500, None);
        }
    }
}

/// The volume window (for the outside-click watcher).
static VOLUME: std::sync::Mutex<isize> = std::sync::Mutex::new(0);
/// The outside-click watcher, while the mixer is open.
static HOOK: std::sync::Mutex<isize> = std::sync::Mutex::new(0);

/// Redraws both sliders (after the cards around them moved or resized).
pub fn follow_anchor() {
    for kind in [Kind::Volume, Kind::Brightness] {
        if let Some(h) = SLIDERS.with(|s| s.borrow().get(&kind).map(|s| s.hwnd)) {
            redraw(kind, h);
        }
    }
}

fn reported(kind: Kind, l: &Levels) -> Option<f32> {
    match kind {
        Kind::Volume => l.volume.map(|v| if l.muted { 0.0 } else { v }),
        Kind::Brightness => l.brightness,
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

/// The volume pill's left edge when closed (the window itself grows while the mixer is open).
pub fn volume_left() -> i32 {
    geometry(Kind::Volume).0
}

/// Where a slider sits (collapsed): x, top, width, height.
fn geometry(kind: Kind) -> (i32, i32, i32, i32) {
    let (sw, sh) = window::screen_size();
    let shf = sh as f32;
    let gap = (shf * GAP_FRACTION) as i32;

    // Under the Bluetooth card, same total width, down to the bottom of the music card.
    let above = rect_of(ABOVE.load(Ordering::Relaxed)).unwrap_or(RECT {
        left: sw - (shf * 0.45) as i32,
        top: 0,
        right: sw - (shf * 0.24) as i32,
        bottom: (shf * 0.16) as i32,
    });
    let bottom = rect_of(BOTTOM.load(Ordering::Relaxed)).map(|r| r.bottom).unwrap_or(above.bottom + (shf * 0.25) as i32);
    // Slim sliders, pushed to the right: the pair's right edge lines up with the cards above
    // (towards the weather and music cards), with the usual gap between the two.
    let total_w = above.right - above.left;
    let w = (((total_w - gap) / 2) as f32 * SLIM).round() as i32;
    let left = above.right - (2 * w + gap);
    let x = match kind {
        Kind::Volume => left,
        Kind::Brightness => left + w + gap,
    };
    let top = above.bottom + gap;
    let h = (bottom - top).max(w);
    (x, top, w, h)
}

pub fn redraw(kind: Kind, hwnd: HWND) {
    let (_, sh) = window::screen_size();
    let shf = sh as f32;
    let levels = levels::current();
    let (x, top, w, h) = geometry(kind);
    let pos = POINT { x, y: top };

    let animating = SLIDERS.with(|sliders| {
        let mut sliders = sliders.borrow_mut();
        let Some(s) = sliders.get_mut(&kind) else { return false };
        s.height = h;

        let now = Instant::now();
        let dt = if s.was_animating { now.duration_since(s.last_frame).as_secs_f32().min(0.05) } else { 0.0 };
        s.last_frame = now;

        // What to show: the mouse while dragging; a just-set value until the system confirms
        // it; otherwise what the system reports.
        let system = reported(kind, &levels);
        if let (Some((wanted, at)), Some(sys)) = (s.pending, system) {
            if (wanted - sys).abs() < 0.015 || at.elapsed().as_secs_f32() > 1.5 {
                s.pending = None;
            }
        }
        let target = s.pending.map(|p| p.0).or(system).unwrap_or(0.0);
        s.level.target = target;
        if s.drag.is_some() || !s.started {
            s.level.snap();
            s.started = true;
        }
        s.level.step(dt);

        let available = system.is_some();

        // The volume mixer: open, opening or closing.
        if kind == Kind::Volume {
            s.expand.target = if s.open { 1.0 } else { 0.0 };
            s.expand.step(dt);
            if s.open || !s.expand.settled() || s.expand.value > 0.001 {
                draw_mixer(s, hwnd, &levels, available, (x, top, w, h), shf);
                let animating = !s.level.settled() || s.drag.is_some() || !s.expand.settled() || s.mixer_press.is_some();
                s.was_animating = animating;
                return animating;
            }
        }

        let (content, icon_centre) = draw(kind, s.level.value, &levels, available, w, h, shf);
        s.icon_centre = icon_centre;

        let mut glass = match s.glass.take() {
            Some(g) if g.size() == (w, h) => g,
            _ => glass::Glass::new(w, h, w as f32 * ROUNDNESS, shf),
        };
        let bmp = if kind == Kind::Volume {
            // Same backdrop and maths as the mixer animation, so its first and last frames
            // match the closed pill exactly.
            let backdrop = volume_backdrop(s, (x, top, w, h), shf);
            Some(glass.compose_on(backdrop, (pos.x, pos.y), &content, 0))
        } else {
            glass.render(&content, 0, (pos.x, pos.y), true)
        };
        if let Some(bmp) = bmp {
            if let Err(e) = window::present(hwnd, &bmp, pos) {
                eprintln!("could not draw slider: {e}");
            }
        }
        s.glass = Some(glass);
        let animating = !s.level.settled() || s.drag.is_some();
        s.was_animating = animating;
        animating
    });
    anim::set_animating(hwnd, animating);
    unsafe {
        SetTimer(Some(hwnd), TIMER_ID, TICK_MS, None);
    }
}

pub fn frame(kind: Kind, hwnd: HWND) {
    anim::frame_done(hwnd);
    redraw(kind, hwnd);
}

/// Redraws if the wallpaper behind the glass changed.
pub fn tick(kind: Kind, hwnd: HWND) {
    let changed = SLIDERS.with(|s| {
        let mut s = s.borrow_mut();
        let Some(slider) = s.get_mut(&kind) else { return false };
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(hwnd, &mut r);
        }
        slider.glass.as_mut().is_some_and(|g| g.render(&Bitmap::new(1, 1), 0, (r.left, r.top), false).is_some())
    });
    if changed {
        redraw(kind, hwnd);
    }
}

fn apply(kind: Kind, value: f32) {
    match kind {
        Kind::Volume => levels::set_volume(value),
        Kind::Brightness => levels::set_brightness(value),
    }
}

pub fn press(kind: Kind, hwnd: HWND, x: i32, y: i32) {
    let available = reported(kind, &levels::current()).is_some();
    if !available {
        return;
    }
    // Inside the open mixer, left of the master slider: an app's slider or icon, or "mute all".
    let mixer_hit = SLIDERS.with(|s| {
        let s = s.borrow();
        let slider = s.get(&kind)?;
        if !slider.open || x >= slider.hits.master_x0 {
            return None;
        }
        let inside = |r: (i32, i32, i32, i32)| x >= r.0 && y >= r.1 && x < r.2 && y < r.3;
        if inside(slider.hits.mute_all) {
            return Some(MixerPress::MuteAll);
        }
        for (key, icon, bar) in &slider.hits.rows {
            if inside(*icon) {
                return Some(MixerPress::Icon(key.clone()));
            }
            if inside((bar.0 - 6, bar.1 - 10, bar.2 + 6, bar.3 + 10)) {
                return Some(MixerPress::Bar { key: key.clone(), x0: bar.0, width: (bar.2 - bar.0).max(1) });
            }
        }
        None
    });
    if let Some(hit) = mixer_hit {
        SLIDERS.with(|s| {
            if let Some(slider) = s.borrow_mut().get_mut(&kind) {
                slider.mixer_press = Some(hit.clone());
            }
        });
        unsafe {
            SetCapture(hwnd);
        }
        if let MixerPress::Bar { .. } = hit {
            mouse_move_x(kind, hwnd, x); // jump to where it was clicked
        }
        return;
    }
    if SLIDERS.with(|s| s.borrow().get(&kind).is_some_and(|sl| sl.open && x < sl.hits.master_x0)) {
        return; // empty space in the mixer
    }
    SLIDERS.with(|s| {
        if let Some(slider) = s.borrow_mut().get_mut(&kind) {
            slider.drag = Some(Drag { start_level: slider.level.value, start_y: y, moved: false });
        }
    });
    unsafe {
        SetCapture(hwnd);
    }
    redraw(kind, hwnd);
}

/// Dragging an app's slider in the mixer (absolute: the bar follows the mouse).
fn mouse_move_x(kind: Kind, hwnd: HWND, x: i32) -> bool {
    let set = SLIDERS.with(|s| {
        let mut s = s.borrow_mut();
        let slider = s.get_mut(&kind)?;
        let Some(MixerPress::Bar { key, x0, width }) = slider.mixer_press.clone() else { return None };
        let v = ((x - x0) as f32 / width as f32).clamp(0.0, 1.0);
        slider.app_pending.insert(key.clone(), (v, Instant::now()));
        slider.list_cache = None;
        Some((key, v))
    });
    match set {
        Some((key, v)) => {
            mixer::set_volume(&key, v);
            redraw(kind, hwnd);
            true
        }
        None => false,
    }
}

pub fn mouse_move(kind: Kind, hwnd: HWND, x: i32, y: i32) {
    if mouse_move_x(kind, hwnd, x) {
        return;
    }
    let value = SLIDERS.with(|s| {
        let mut s = s.borrow_mut();
        let slider = s.get_mut(&kind)?;
        let drag = slider.drag.as_mut()?;
        if (y - drag.start_y).abs() > 2 {
            drag.moved = true;
        }
        // Relative: moving the full height of the slider goes from empty to full.
        let v = (drag.start_level + (drag.start_y - y) as f32 / slider.height.max(1) as f32).clamp(0.0, 1.0);
        slider.pending = Some((v, Instant::now()));
        Some(v)
    });
    if let Some(v) = value {
        apply(kind, v);
        redraw(kind, hwnd);
    }
}

pub fn release(kind: Kind, hwnd: HWND, x: i32, y: i32) {
    let drag = SLIDERS.with(|s| s.borrow_mut().get_mut(&kind).and_then(|slider| slider.drag.take()));
    let mixer_press = SLIDERS.with(|s| s.borrow_mut().get_mut(&kind).and_then(|slider| slider.mixer_press.take()));
    unsafe {
        let _ = ReleaseCapture();
    }
    match mixer_press {
        Some(MixerPress::Icon(key)) => {
            mixer::toggle_mute(&key);
            invalidate_list(kind);
        }
        Some(MixerPress::MuteAll) => levels::toggle_mute(),
        _ => {}
    }
    // A click (no drag) on the speaker opens / closes the volume mixer.
    if let Some(d) = drag {
        if !d.moved && kind == Kind::Volume && on_icon(kind, x, y) {
            let open = SLIDERS.with(|s| s.borrow().get(&kind).is_some_and(|sl| sl.open));
            set_mixer_open(hwnd, !open);
        }
    }
    redraw(kind, hwnd);
}

pub fn cancel(kind: Kind, hwnd: HWND) {
    let was = SLIDERS.with(|s| s.borrow_mut().get_mut(&kind).and_then(|slider| slider.drag.take()).is_some());
    if was {
        redraw(kind, hwnd);
    }
}

/// Scroll wheel over a slider: up raises, down lowers.
pub fn wheel(kind: Kind, hwnd: HWND, delta: i16) {
    // Over an app's row in the open mixer: that app's volume.
    let row = SLIDERS.with(|s| {
        let s = s.borrow();
        let slider = s.get(&kind)?;
        if !slider.open {
            return None;
        }
        let mut pt = POINT::default();
        let mut r = RECT::default();
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
            let _ = GetWindowRect(slider.hwnd, &mut r);
        }
        let (x, y) = (pt.x - r.left, pt.y - r.top);
        slider.hits.rows.iter().find(|(_, icon, bar)| y >= icon.1 - 8 && y < icon.3 + 8 && x >= icon.0 && x < bar.2).map(|(k, ..)| k.clone())
    });
    if let Some(key) = row {
        let current = mixer::apps().into_iter().find(|a| a.key == key).map(|a| a.volume).unwrap_or(0.0);
        let from = SLIDERS.with(|s| s.borrow().get(&kind).and_then(|sl| sl.app_pending.get(&key).map(|p| p.0))).unwrap_or(current);
        let v = (from + WHEEL_STEP * delta as f32 / 120.0).clamp(0.0, 1.0);
        SLIDERS.with(|s| {
            if let Some(sl) = s.borrow_mut().get_mut(&kind) {
                sl.app_pending.insert(key.clone(), (v, Instant::now()));
                sl.list_cache = None;
            }
        });
        mixer::set_volume(&key, v);
        redraw(kind, hwnd);
        return;
    }
    let value = SLIDERS.with(|s| {
        let mut s = s.borrow_mut();
        let slider = s.get_mut(&kind)?;
        let from = slider.pending.map(|p| p.0).unwrap_or(slider.level.target);
        let v = (from + WHEEL_STEP * delta as f32 / 120.0).clamp(0.0, 1.0);
        slider.pending = Some((v, Instant::now()));
        Some(v)
    });
    if let Some(v) = value {
        apply(kind, v);
        redraw(kind, hwnd);
    }
}

/// Opens or closes the volume mixer (with its animation).
pub fn set_mixer_open(hwnd: HWND, open: bool) {
    SLIDERS.with(|s| {
        if let Some(sl) = s.borrow_mut().get_mut(&Kind::Volume) {
            sl.open = open;
            sl.was_animating = sl.was_animating || true;
        }
    });
    mixer::set_open(open);
    if open {
        watch_outside_clicks(true);
    } else {
        watch_outside_clicks(false);
    }
    redraw(Kind::Volume, hwnd);
}

/// Called for WM_MIXER_CLOSE (a click outside the open mixer).
pub fn close_mixer(hwnd: HWND) {
    if SLIDERS.with(|s| s.borrow().get(&Kind::Volume).is_some_and(|sl| sl.open)) {
        set_mixer_open(hwnd, false);
    }
}

/// The app list changed (new app, new level): redraw it.
pub fn mixer_updated(hwnd: HWND) {
    invalidate_list(Kind::Volume);
    redraw(Kind::Volume, hwnd);
}

fn invalidate_list(kind: Kind) {
    SLIDERS.with(|s| {
        if let Some(sl) = s.borrow_mut().get_mut(&kind) {
            sl.list_cache = None;
        }
    });
}

/// Prepares the open mixer's glass ahead of time (it's the slow part), so opening is smooth.
pub fn prewarm(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), PREWARM_TIMER_ID);
    }
    let (_, sh) = window::screen_size();
    let shf = sh as f32;
    let (x, top, w, h) = geometry(Kind::Volume);
    SLIDERS.with(|s| {
        if let Some(sl) = s.borrow_mut().get_mut(&Kind::Volume) {
            let _ = volume_backdrop(sl, (x, top, w, h), shf);
        }
    });
}

/// The backdrop covering everywhere the volume pill and open mixer can be (rebuilt only if
/// the layout or wallpaper changed).
fn volume_backdrop(s: &mut Slider, (x, top, w, h): (i32, i32, i32, i32), sh: f32) -> &glass::Backdrop {
    let (gx, gw) = mixer_frame(x, w, sh);
    let area = (gx, top, gw, h);
    if s.backdrop.as_ref().is_none_or(|b| !b.fits(area)) {
        s.backdrop = Some(glass::Backdrop::capture(area, sh));
    }
    s.backdrop.as_ref().expect("just built")
}

/// The open mixer's left edge and width: it extends left from the pill's right edge.
fn mixer_frame(x: i32, w: i32, sh: f32) -> (i32, i32) {
    let width = ((sh * MIXER_WIDTH_FRACTION) as i32).max(w + 40);
    (x + w - width, width)
}

/// Watches for clicks anywhere outside the volume window while the mixer is open.
fn watch_outside_clicks(on: bool) {
    let mut hook = HOOK.lock().unwrap();
    unsafe {
        if on && *hook == 0 {
            if let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(outside_click), None, 0) {
                *hook = h.0 as isize;
            }
        } else if !on && *hook != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(*hook as *mut std::ffi::c_void));
            *hook = 0;
        }
    }
}

unsafe extern "system" fn outside_click(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let msg = wparam.0 as u32;
        if code >= 0 && (msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN) {
            let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let volume = *VOLUME.lock().unwrap();
            if volume != 0 {
                let hwnd = HWND(volume as *mut std::ffi::c_void);
                let mut r = RECT::default();
                let _ = GetWindowRect(hwnd, &mut r);
                let p = info.pt;
                if p.x < r.left || p.x >= r.right || p.y < r.top || p.y >= r.bottom {
                    let _ = PostMessageW(Some(hwnd), WM_MIXER_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

fn on_icon(kind: Kind, x: i32, y: i32) -> bool {
    SLIDERS.with(|s| {
        s.borrow().get(&kind).is_some_and(|slider| {
            let (cx, cy) = slider.icon_centre;
            (x - cx).abs() < 28 && (y - cy).abs() < 28
        })
    })
}

/// Hand cursor over the whole slider (it's all draggable).
pub fn over_control(kind: Kind) -> bool {
    reported(kind, &levels::current()).is_some()
}

// ---------- the open mixer ----------

/// Draws the volume pill opening into (or closing from) the mixer panel. The glass is
/// rendered at the exact size of every frame over a backdrop prepared once, so it's exactly
/// what a resting card of that size would look like. Fast movement squashes the panel
/// vertically a little, like liquid.
fn draw_mixer(s: &mut Slider, hwnd: HWND, levels: &Levels, available: bool, (x, top, w, h): (i32, i32, i32, i32), sh: f32) {
    let e = s.expand.value;
    let open_amount = e.clamp(0.0, 1.0);
    let (_, gw) = mixer_frame(x, w, sh);
    let right = x + w;
    let width = (w as f32 + (gw - w) as f32 * open_amount).round() as i32;
    let squash = (s.expand.velocity.abs() * SQUASH).min(MAX_SQUASH);
    let height = ((h as f32) * (1.0 - squash)).round() as i32;
    let pos = POINT { x: right - width, y: top + (h - height) / 2 };

    // The glass at exactly this size, over the shared backdrop: identical to how the closed
    // pill is drawn, so the animation flows from and back into it seamlessly.
    let frame_glass = glass::Glass::new(width, height, w as f32 * ROUNDNESS, sh);
    let backdrop = volume_backdrop(s, (x, top, w, h), sh);
    let mut out = frame_glass.compose_on(backdrop, (pos.x, pos.y), &Bitmap::new(1, 1), 0);

    // The master slider stays where the pill was: at the panel's right end.
    let (master, icon_centre) = draw(Kind::Volume, s.level.value, levels, available, w, height, sh);
    let master_x0 = width - w;
    overlay(&mut out, &master, master_x0, 0, 256);
    s.icon_centre = (master_x0 + icon_centre.0, icon_centre.1);

    // The app list: revealed from the right as the panel grows, fading and sliding in.
    let gap = (sh * 0.012) as i32;
    let list_w = gw - w - gap - list_inset(sh);
    let fade = smoothstep(0.3, 0.92, open_amount);
    let list_x = master_x0 - gap - list_w + ((1.0 - fade) * 24.0) as i32;
    let apps = mixer::apps();
    // Drop pending values Windows has caught up with.
    s.app_pending.retain(|k, (v, at)| {
        let reported = apps.iter().find(|a| &a.key == k).map(|a| a.volume);
        at.elapsed().as_secs_f32() < 1.5 && reported.is_none_or(|r| (r - *v).abs() > 0.01)
    });
    let key = list_key(&apps, &s.app_pending, levels.muted, list_w, height);
    if s.list_cache.as_ref().is_none_or(|(k, _)| *k != key) {
        s.list_cache = Some((key, draw_list(&apps, &s.app_pending, levels.muted, list_w, height, sh)));
    }
    let mut hits = MixerHits { master_x0, rows: Vec::new(), mute_all: (0, 0, 0, 0) };
    if let Some((_, list)) = &s.list_cache {
        if fade > 0.01 {
            overlay_clipped(&mut out, list, list_x, 0, (fade * 256.0) as u32);
        }
        if open_amount > 0.9 {
            hits = list_hits(&apps, list_x, list_w, height, sh, master_x0);
        }
    }
    s.hits = hits;

    if let Err(e) = window::present(hwnd, &out, pos) {
        eprintln!("could not draw mixer: {e}");
    }
}

/// Lays `src` over `out` at (x, y) with opacity `a` (0..=256).
fn overlay(out: &mut Bitmap, src: &Bitmap, x: i32, y: i32, a: u32) {
    for row in 0..src.height {
        let oy = y + row;
        if oy < 0 || oy >= out.height {
            continue;
        }
        for col in 0..src.width {
            let ox = x + col;
            if ox < 0 || ox >= out.width {
                continue;
            }
            let p = render::scale(src.pixels[(row * src.width + col) as usize], a);
            if p >> 24 != 0 {
                let d = &mut out.pixels[(oy * out.width + ox) as usize];
                *d = render::over(*d, p);
            }
        }
    }
}

/// Like `overlay`, but only where the glass is (so nothing pokes out past its rounded corners).
fn overlay_clipped(out: &mut Bitmap, src: &Bitmap, x: i32, y: i32, a: u32) {
    for row in 0..src.height {
        let oy = y + row;
        if oy < 0 || oy >= out.height {
            continue;
        }
        for col in 0..src.width {
            let ox = x + col;
            if ox < 0 || ox >= out.width {
                continue;
            }
            let d = &mut out.pixels[(oy * out.width + ox) as usize];
            let coverage = (*d >> 24) + 1; // 1..=256
            let p = render::scale(render::scale(src.pixels[(row * src.width + col) as usize], a), coverage);
            if p >> 24 != 0 {
                *d = render::over(*d, p);
            }
        }
    }
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A fingerprint of everything the list shows, to know when to redraw it.
fn list_key(apps: &[App], pending: &HashMap<String, (f32, Instant)>, all_muted: bool, w: i32, h: i32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (w, h, all_muted).hash(&mut hasher);
    for a in apps {
        a.key.hash(&mut hasher);
        a.name.hash(&mut hasher);
        a.muted.hash(&mut hasher);
        let v = pending.get(&a.key).map(|p| p.0).unwrap_or(a.volume);
        ((v * 1000.0) as i32).hash(&mut hasher);
    }
    hasher.finish()
}

/// Row layout (shared by drawing and hit testing). All y values are from the panel's top.
struct ListLayout {
    /// Vertical centre of the header (title and mute-all button).
    header_cy: i32,
    /// Mute-all button diameter.
    button: i32,
    /// The hairline under the header.
    rule_y: i32,
    first_row: i32,
    row_h: i32,
    icon: i32,
    /// Within a row: where the name's top and the slider's top sit.
    name_dy: i32,
    bar_dy: i32,
    bar_h: i32,
    /// Space between the icon and the name/slider.
    text_gap: i32,
}

/// Space between the panel's left edge (and top) and the list. Generous enough that the
/// header clears the panel's big rounded corner.
fn list_inset(sh: f32) -> i32 {
    (sh * 0.02) as i32
}

const TITLE_FRACTION: f32 = 0.0102;
const NAME_FRACTION: f32 = 0.0082;

fn list_layout(sh: f32) -> ListLayout {
    let px = |f: f32| (sh * f).round() as i32;
    let button = px(0.021);
    let header_cy = list_inset(sh) + button / 2;
    let rule_y = header_cy + button / 2 + px(0.009);
    let icon = px(0.022);
    let (name_h, spacing, bar_h) = (px(NAME_FRACTION), px(0.0068), px(0.0055));
    let block = name_h + spacing + bar_h;
    let name_dy = (icon - block) / 2;
    ListLayout {
        header_cy,
        button,
        rule_y,
        first_row: rule_y + px(0.013),
        row_h: px(0.037),
        icon,
        name_dy,
        bar_dy: name_dy + name_h + spacing,
        bar_h,
        text_gap: px(0.009),
    }
}

fn visible_rows(apps: &[App], h: i32, sh: f32) -> usize {
    let l = list_layout(sh);
    let bottom = h - list_inset(sh);
    (((bottom - l.first_row + (l.row_h - l.icon)) / l.row_h).max(0) as usize).min(apps.len())
}

fn list_hits(apps: &[App], list_x: i32, list_w: i32, h: i32, sh: f32, master_x0: i32) -> MixerHits {
    let l = list_layout(sh);
    let mut rows = Vec::new();
    for (i, a) in apps.iter().take(visible_rows(apps, h, sh)).enumerate() {
        let y = l.first_row + i as i32 * l.row_h;
        let icon = (list_x, y, list_x + l.icon, y + l.icon);
        let bar_x = list_x + l.icon + l.text_gap;
        let bar = (bar_x, y + l.bar_dy, list_x + list_w, y + l.bar_dy + l.bar_h);
        rows.push((a.key.clone(), icon, bar));
    }
    let r = l.button / 2 + 4;
    let cx = list_x + list_w - l.button / 2;
    let mute_all = (cx - r, l.header_cy - r, cx + r, l.header_cy + r);
    MixerHits { master_x0, rows, mute_all }
}

/// The mixer's list: a header (title and a round mute-all button) over a hairline, then one
/// row per app: icon, with the name above a slim slider beside it.
fn draw_list(apps: &[App], pending: &HashMap<String, (f32, Instant)>, all_muted: bool, w: i32, h: i32, sh: f32) -> Bitmap {
    let mut out = Bitmap::new(w.max(1), h);
    let l = list_layout(sh);
    let white = (255.0, 255.0, 255.0);
    let wf = w as f32;

    // Header: the title, its capitals centred on the header line…
    let cap = sh * TITLE_FRACTION;
    let title = render::line(&render::sf_bold(), "Volume Mixer", cap, 1.0, render::WHITE);
    let baseline = (l.header_cy as f32 + cap / 2.0).round() as i32;
    out.blit(&title, 0, baseline - title.baseline);
    // …and a round glass button at the right that mutes everything.
    let r = l.button as f32 / 2.0;
    let c = (wf - r, l.header_cy as f32);
    let on = !all_muted;
    fill(&mut out, (c.0 - r, c.1 - r, c.0 + r, c.1 + r), |p| dist(p, c) - r, white, if on { 0.14 } else { 0.85 });
    let icon_color = if on { white } else { (30.0, 30.0, 30.0) };
    speaker(&mut out, c, r * 0.95, if all_muted { None } else { Some(1.0) }, icon_color, 0.95);
    // Hairline under the header.
    fill(&mut out, (0.0, l.rule_y as f32 - 1.0, wf, l.rule_y as f32 + 2.0), |p| (p.1 - (l.rule_y as f32 + 0.5)).abs() - 0.5, white, 0.14);

    if apps.is_empty() {
        let mut none = render::line(&render::sf(), "No apps are playing sound", sh * 0.0082, 1.0, render::WHITE);
        none.fade(0.55);
        let y = l.first_row + ((h - list_inset(sh) - l.first_row) - none.height) / 2;
        out.blit(&none, (w - none.width) / 2, y.max(l.first_row));
        return out;
    }
    for (i, a) in apps.iter().take(visible_rows(apps, h, sh)).enumerate() {
        let y = l.first_row + i as i32 * l.row_h;
        let dim = if a.muted { 0.4 } else { 1.0 };
        // Icon (or a soft placeholder with the app's initial).
        match &a.icon {
            Some(img) => draw_image(&mut out, img, 0, y, l.icon, dim),
            None => {
                let r = l.icon as f32 / 2.0;
                let cc = (r, y as f32 + r);
                fill(&mut out, (0.0, y as f32, l.icon as f32, (y + l.icon) as f32), |p| dist(p, cc) - r, white, 0.18 * dim);
                let initial: String = a.name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
                let mut t = render::line(&render::sf_bold(), &initial, sh * 0.009, 1.0, render::WHITE);
                t.fade(dim);
                out.blit(&t, (l.icon - t.width) / 2, y + (l.icon - t.height) / 2);
            }
        }
        let text_x = l.icon + l.text_gap;
        let text_w = (w - text_x).max(1);
        let name_cap = sh * NAME_FRACTION;
        let mut name = fit(&render::sf_medium(), &a.name, name_cap, text_w);
        name.fade(dim * 0.92);
        let name_baseline = y + l.name_dy + name_cap.round() as i32;
        out.blit(&name, text_x, name_baseline - name.baseline);
        // Slider: a slim glass track with a white fill.
        let bar_y = (y + l.bar_dy) as f32;
        let bar_h = l.bar_h as f32;
        let (bx0, bx1) = (text_x as f32, wf);
        let track = |p: (f32, f32)| capsule(p, (bx0 + bar_h / 2.0, bar_y + bar_h / 2.0), (bx1 - bar_h / 2.0, bar_y + bar_h / 2.0)) - bar_h / 2.0;
        fill(&mut out, (bx0, bar_y, bx1, bar_y + bar_h), track, white, 0.18);
        let v = pending.get(&a.key).map(|p| p.0).unwrap_or(a.volume).clamp(0.0, 1.0);
        if v > 0.0 {
            // The filled part keeps a rounded end.
            let fx = (bx0 + (bx1 - bx0) * v).max(bx0 + bar_h);
            let filled = |p: (f32, f32)| capsule(p, (bx0 + bar_h / 2.0, bar_y + bar_h / 2.0), (fx - bar_h / 2.0, bar_y + bar_h / 2.0)) - bar_h / 2.0;
            fill(&mut out, (bx0, bar_y, fx, bar_y + bar_h), filled, white, 0.9 * dim);
        }
    }
    out
}

/// Draws an app icon scaled to `size`, at (x, y), at opacity `alpha`.
fn draw_image(out: &mut Bitmap, img: &image::RgbaImage, x: i32, y: i32, size: i32, alpha: f32) {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    for j in 0..size {
        for i in 0..size {
            let u = ((i as f32 + 0.5) / size as f32 * iw - 0.5).clamp(0.0, iw - 1.0);
            let v = ((j as f32 + 0.5) / size as f32 * ih - 0.5).clamp(0.0, ih - 1.0);
            let (x0, y0) = (u.floor() as u32, v.floor() as u32);
            let (x1, y1) = ((x0 + 1).min(img.width() - 1), (y0 + 1).min(img.height() - 1));
            let (fx, fy) = (u - x0 as f32, v - y0 as f32);
            let mut c = [0.0f32; 4];
            for k in 0..4 {
                let a = img.get_pixel(x0, y0).0[k] as f32 * (1.0 - fx) + img.get_pixel(x1, y0).0[k] as f32 * fx;
                let b = img.get_pixel(x0, y1).0[k] as f32 * (1.0 - fx) + img.get_pixel(x1, y1).0[k] as f32 * fx;
                c[k] = a * (1.0 - fy) + b * fy;
            }
            let a = c[3] / 255.0 * alpha;
            if a <= 0.0 {
                continue;
            }
            let to8 = |v: f32| v.round().clamp(0.0, 255.0) as u32;
            let a8 = to8(a * 255.0);
            let px = a8 << 24 | to8(c[0] * a).min(a8) << 16 | to8(c[1] * a).min(a8) << 8 | to8(c[2] * a).min(a8);
            let (ox, oy) = (x + i, y + j);
            if ox >= 0 && oy >= 0 && ox < out.width && oy < out.height {
                let d = &mut out.pixels[(oy * out.width + ox) as usize];
                *d = render::over(*d, px);
            }
        }
    }
}

/// Renders `text`, shortening it with "…" to fit `max_width`.
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

// ---------- drawing ----------

/// The slider's content: fill and icon, `w`×`h`. Returns it and the icon's centre.
fn draw(kind: Kind, level: f32, levels: &Levels, available: bool, w: i32, h: i32, sh: f32) -> (Bitmap, (i32, i32)) {
    let mut out = Bitmap::new(w, h);
    let (wf, hf) = (w as f32, h as f32);
    let radius = wf * ROUNDNESS;
    let half = (wf / 2.0, hf / 2.0);
    let level = level.clamp(0.0, 1.0);

    // Tinted-glass fill from the bottom, clipped to the slider's rounded shape.
    let fill_top = hf * (1.0 - level);
    if level > 0.0 {
        let span = (hf - fill_top).max(1.0);
        let shape = |p: (f32, f32)| rounded_rect((p.0 - half.0, p.1 - half.1), half, radius).max(fill_top - p.1);
        fill_shaded(&mut out, (0.0, fill_top - 1.0, wf, hf), shape, |p| {
            let t = ((p.1 - fill_top) / span).clamp(0.0, 1.0);
            FILL_TINT_TOP + (FILL_TINT_BOTTOM - FILL_TINT_TOP) * t
        });
        // A soft bright edge where the fill meets the empty glass, like light on a liquid surface.
        if level < 0.999 {
            let card = |p: (f32, f32)| rounded_rect((p.0 - half.0, p.1 - half.1), half, radius);
            fill(&mut out, (0.0, fill_top - 3.0, wf, fill_top + 3.0), |p| card(p).max((p.1 - fill_top).abs() - 0.9), ICON, FILL_EDGE);
        }
    }

    // White icon at the base.
    let s = (wf * 0.3).min(sh * 0.03);
    let centre = (wf / 2.0, hf - wf * 0.32);
    let color = ICON;
    let alpha = if available { 1.0 } else { 0.4 };
    match kind {
        Kind::Volume => speaker(&mut out, centre, s, if levels.muted { None } else { Some(level) }, color, alpha),
        Kind::Brightness => sun(&mut out, centre, s, level, color, alpha),
    }
    (out, (centre.0 as i32, centre.1 as i32))
}

/// A speaker with 0–3 sound waves for the level, or a slash when muted (`level` = None).
fn speaker(out: &mut Bitmap, c: (f32, f32), s: f32, level: Option<f32>, color: (f32, f32, f32), alpha: f32) {
    // Shift so the drawn shape (body + waves, or body + slash) is centred on `c`.
    let c = (c.0 + if level.is_none() { s * 0.21 } else { -s * 0.04 }, c.1);
    let x0 = c.0 - s * 0.42;
    let body = [
        (x0, c.1 - s * 0.14),
        (x0 + s * 0.16, c.1 - s * 0.14),
        (x0 + s * 0.42, c.1 - s * 0.36),
        (x0 + s * 0.42, c.1 + s * 0.36),
        (x0 + s * 0.16, c.1 + s * 0.14),
        (x0, c.1 + s * 0.14),
    ];
    let b = (c.0 - s, c.1 - s, c.0 + s, c.1 + s);
    let wave_centre = (x0 + s * 0.42, c.1);
    match level {
        None => {
            // Muted: a slash straight through the body, with a thin gap cut around it.
            let slash = |p| capsule(p, (x0 - s * 0.08, c.1 - s * 0.42), (x0 + s * 0.5, c.1 + s * 0.42));
            fill(out, b, |p| (polygon(p, &body) - s * 0.02).max(s * 0.1 - slash(p)), color, alpha);
            fill(out, b, |p| slash(p) - s * 0.045, color, alpha);
        }
        Some(l) => {
            fill(out, b, |p| polygon(p, &body) - s * 0.02, color, alpha);
            let waves = if l <= 0.001 { 0 } else if l < 0.34 { 1 } else if l < 0.67 { 2 } else { 3 };
            for (i, r) in [0.18, 0.33, 0.48].into_iter().enumerate() {
                let a = if i < waves { alpha } else { alpha * 0.3 };
                fill(out, b, |p| arc_right(p, wave_centre, r * s, 0.85) - s * 0.045, color, a);
            }
        }
    }
}

/// A sun whose rays grow with the level.
fn sun(out: &mut Bitmap, c: (f32, f32), s: f32, level: f32, color: (f32, f32, f32), alpha: f32) {
    let b = (c.0 - s, c.1 - s, c.0 + s, c.1 + s);
    let core = s * 0.2;
    fill(out, b, |p| dist(p, c) - core, color, alpha);
    let ray_len = s * (0.08 + 0.12 * level);
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let (si, co) = (a.sin(), a.cos());
        let inner = (c.0 + co * s * 0.33, c.1 + si * s * 0.33);
        let outer = (c.0 + co * (s * 0.33 + ray_len), c.1 + si * (s * 0.33 + ray_len));
        fill(out, b, |p| capsule(p, inner, outer) - s * 0.045, color, alpha);
    }
}

// ---------- shape helpers ----------

/// Fills a shape in white with an opacity that varies per point (for the shaded glass tint).
fn fill_shaded(out: &mut Bitmap, bounds: (f32, f32, f32, f32), distance: impl Fn((f32, f32)) -> f32, alpha: impl Fn((f32, f32)) -> f32) {
    let (x0, y0) = ((bounds.0 - 1.0).floor().max(0.0) as i32, (bounds.1 - 1.0).floor().max(0.0) as i32);
    let x1 = ((bounds.2 + 1.0).ceil() as i32).min(out.width);
    let y1 = ((bounds.3 + 1.0).ceil() as i32).min(out.height);
    for y in y0..y1 {
        for x in x0..x1 {
            let p = (x as f32 + 0.5, y as f32 + 0.5);
            let d = distance(p);
            if d < 1.0 {
                let a = (0.5 - d).clamp(0.0, 1.0) * alpha(p);
                if a > 0.0 {
                    let v = (255.0 * a).round() as u32;
                    let a8 = (a * 255.0).round() as u32;
                    let px = a8 << 24 | v.min(a8) << 16 | v.min(a8) << 8 | v.min(a8);
                    let slot = &mut out.pixels[(y * out.width + x) as usize];
                    *slot = render::over(*slot, px);
                }
            }
        }
    }
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn capsule(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (pa, ba) = ((p.0 - a.0, p.1 - a.1), (b.0 - a.0, b.1 - a.1));
    let h = ((pa.0 * ba.0 + pa.1 * ba.1) / (ba.0 * ba.0 + ba.1 * ba.1).max(1e-6)).clamp(0.0, 1.0);
    ((pa.0 - ba.0 * h).powi(2) + (pa.1 - ba.1 * h).powi(2)).sqrt()
}

/// Distance to an arc opening to the right, spanning ±`half` radians around horizontal.
fn arc_right(p: (f32, f32), c: (f32, f32), radius: f32, half: f32) -> f32 {
    let (dx, dy) = (p.0 - c.0, p.1 - c.1);
    let angle = dy.atan2(dx); // 0 = pointing right
    if angle.abs() <= half {
        ((dx * dx + dy * dy).sqrt() - radius).abs()
    } else {
        let a = half * angle.signum();
        dist(p, (c.0 + radius * a.cos(), c.1 + radius * a.sin()))
    }
}

fn rounded_rect(p: (f32, f32), half: (f32, f32), r: f32) -> f32 {
    let q = (p.0.abs() - half.0 + r, p.1.abs() - half.1 + r);
    let outside = (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt();
    outside + q.0.max(q.1).min(0.0) - r
}

/// Signed distance to a polygon (negative inside).
fn polygon(p: (f32, f32), v: &[(f32, f32)]) -> f32 {
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

/// Fills pixels in `bounds` by a distance function (negative inside), anti-aliased.
fn fill(out: &mut Bitmap, bounds: (f32, f32, f32, f32), distance: impl Fn((f32, f32)) -> f32, color: (f32, f32, f32), alpha: f32) {
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
                let px = a8 << 24 | to8(color.0 * a).min(a8) << 16 | to8(color.1 * a).min(a8) << 8 | to8(color.2 * a).min(a8);
                let slot = &mut out.pixels[(y * out.width + x) as usize];
                *slot = render::over(*slot, px);
            }
        }
    }
}
