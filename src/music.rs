//! Music widget under the weather card, on liquid glass: album art, title, artist, a progress
//! bar with times, and previous / play-pause / next buttons. Follows Spotifast through
//! Windows' media controls (see media.rs). When the app is gone, the last song stays, dimmed.
//!
//! Animations follow Apple's style, driven by damped springs:
//! - clicks respond instantly (play/pause flips before the player confirms);
//! - pressed buttons shrink, glow, and spring back;
//! - the play triangle morphs into the pause bars and back;
//! - the album art shrinks a little when paused and springs back when playing;
//! - on a track change the old art and text slide out as the new ones slide in;
//! - the progress bar moves smoothly while playing.

use crate::anim::{self, Spring};
use crate::glass;
use crate::media::{self, Track};
use crate::render::{self, Bitmap};
use crate::window;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, KillTimer, SetTimer};

/// Timer for checking whether the wallpaper changed (ms).
pub const TIMER_ID: usize = 3;
const TICK_MS: u32 = 1000;
/// Timer that moves the progress bar along while playing (when nothing else is animating).
pub const ANIM_TIMER_ID: usize = 4;
const PROGRESS_FRAME_MS: u32 = 100; // ~10 fps is plenty for a slowly moving bar

/// Gap between the weather card and the music card, as a fraction of the screen height.
pub const GAP_FRACTION: f32 = 0.012;
/// Opacity of everything when the player has closed and we're showing the last song.
pub const DIMMED: f32 = 0.5;
/// Album art size while paused, relative to playing (Apple Music shrinks paused art).
pub const PAUSED_ART_SCALE: f32 = 0.88;
/// Spring for the track-change slide: (response in seconds, damping). Springs (rather than
/// fixed-length slides) let a new skip take over mid-slide from wherever things are.
const SLIDE_SPRING: (f32, f32) = (0.5, 0.92);
/// Spring for fading songs in and out.
const FADE_SPRING: (f32, f32) = (0.38, 1.0);
/// Kick given to the current song the moment "next"/"previous" is clicked (instant feedback,
/// before the player reports the new song), in slide-widths per second.
const SKIP_KICK: f32 = 2.2;
/// How far (in slide-widths) the current card leans while waiting for the skipped-to song,
/// and for how long at most.
const LEAN: f32 = 0.2;
const LEAN_MAX_SECS: f32 = 2.0;
/// How much the play/pause icon swells when its spring overshoots (the "bounce").
pub const ICON_BOUNCE: f32 = 0.9;
/// How far the art and text slide during a track change, as a fraction of the card width.
pub const SLIDE_FRACTION: f32 = 0.22;
/// Radius of the soft glow behind a pressed button, as a fraction of the screen height.
const GLOW_FRACTION: f32 = 0.019;
/// After a seek or skip, the previous play state is shown while the player settles: at least
/// this long (the player's "paused while loading" blip usually starts a moment *after* the click)...
const PLAY_HOLD_MIN_SECS: f32 = 0.3;
/// ...and at most this long, so a genuine change is never hidden for more than a few seconds.
const PLAY_HOLD_MAX_SECS: f32 = 3.0;
/// A "paused" report from elsewhere must last this long before the button and art react.
const PAUSE_DEBOUNCE_SECS: f32 = 0.3;
/// How much a pressed button shrinks (0.15 = to 85%).
pub const PRESS_SHRINK: f32 = 0.18;
/// Extra thickness of the progress bar while hovered or dragged, as a fraction of the screen
/// height (Apple-style: the bar grows instead of showing a knob).
const BAR_GROW_FRACTION: f32 = 0.0045;
/// How far above/below the visible bar the scrubber can still be grabbed (pixels at 1600 px tall).
const BAR_GRAB_FRACTION: f32 = 0.009;
/// Opacity of secondary text (artist, times) and the empty part of the progress bar.
const SECONDARY: f32 = 0.7;
const TRACK_OPACITY: f32 = 0.3;

/// The weather window, so the card can sit right under it; and our own window, so we can
/// follow when the weather card changes size.
static ANCHOR: AtomicIsize = AtomicIsize::new(0);
static SELF: AtomicIsize = AtomicIsize::new(0);

pub fn set_anchor(hwnd: HWND) {
    ANCHOR.store(hwnd.0 as isize, Ordering::Relaxed);
}

/// Re-positions the card after the weather card has changed.
pub fn follow_anchor() {
    let raw = SELF.load(Ordering::Relaxed);
    if raw != 0 {
        redraw(HWND(raw as *mut std::ffi::c_void));
    }
}

/// Handles a frame request from the shared frame clock.
pub fn frame(hwnd: HWND) {
    anim::frame_done(hwnd);
    redraw(hwnd);
}

// ---------- animation state ----------

#[derive(Clone, Copy, PartialEq)]
enum Button {
    Previous,
    Play,
    Next,
}

/// The parts of the card that belong to one song, pre-rendered (they slide together).
struct Block {
    key: String,
    art: Bitmap,
    title: Bitmap,
    artist: Bitmap,
    /// Fixed line slots (height, baseline row) for the title and artist, so the layout below
    /// them doesn't move with each song's letters (descenders, accents).
    title_slot: (i32, i32),
    artist_slot: (i32, i32),
}

/// One song's card in the carousel. The newest is the current song; older ones are on their
/// way out. Each has its own position and opacity springs, so a new skip arriving mid-slide
/// carries every card on from exactly where it is.
struct Slide {
    block: Rc<Block>,
    /// Horizontal position in slide-widths: 0 = in place, -1 = gone left, +1 = coming from the right.
    x: Spring,
    alpha: Spring,
    /// Art size frozen when the song starts leaving (the current song follows play/pause).
    art_scale: Option<f32>,
}

impl Slide {
    fn new(block: Rc<Block>, from_x: f32, from_alpha: f32) -> Slide {
        let mut x = Spring::new(from_x, SLIDE_SPRING.0, SLIDE_SPRING.1);
        x.target = 0.0;
        let mut alpha = Spring::new(from_alpha, FADE_SPRING.0, FADE_SPRING.1);
        alpha.target = 1.0;
        Slide { block, x, alpha, art_scale: None }
    }

    fn leaving(&self) -> bool {
        self.alpha.target == 0.0
    }
}

struct Anim {
    started: bool,
    /// 0 = play triangle, 1 = pause bars.
    morph: Spring,
    art_scale: Spring,
    /// How pressed each button is (0..1): previous, play, next.
    press: [Spring; 3],
    pressed: Option<Button>,
    /// Progress bar thickness: 0 = thin, 1 = grown (hovered or being dragged).
    bar_grow: Spring,
    /// Whether the mouse is over the progress bar.
    bar_hover: bool,
    /// While dragging the scrubber: where it is (0..1 through the song).
    scrub: Option<f32>,
    /// After letting go: the position asked for, shown until the player confirms it.
    seek_pending: Option<(f32, Instant)>,
    /// Song cards, oldest first; the last one is the current song.
    slides: Vec<Slide>,
    /// +1 after "next", -1 after "previous" (decides the slide direction), and when.
    last_skip: Option<(f32, Instant)>,
    /// After a skip click, until the new song arrives: the current card leans that way (the
    /// player can take over half a second to report the new song, longer when paused).
    lean: Option<(f32, Instant)>,
    /// Play state shown right after a click, until the player confirms it.
    optimistic: Option<(bool, Instant)>,
    /// After a seek or skip: the play state from before it, shown while the player settles
    /// (players often report a brief "paused" while loading the new spot).
    play_hold: Option<(bool, Instant)>,
    /// When the player first reported "paused" while we still show playing (debounce).
    paused_since: Option<Instant>,
    /// The play state currently shown.
    shown_playing: bool,
    /// Waiting on any of the above to resolve (needs a follow-up redraw even when still).
    settling: bool,
    /// Overall opacity: 1 normally, DIMMED when the player has closed.
    dim: Spring,
    /// Shown progress through the song (0..1); glides when the position jumps.
    progress: Spring,
    last_frame: Instant,
    /// Whether the previous frame was part of an animation (if not, the next starts from rest).
    was_animating: bool,
}

impl Anim {
    fn new() -> Anim {
        Anim {
            started: false,
            morph: Spring::new(0.0, 0.32, 0.82),
            art_scale: Spring::new(1.0, 0.5, 0.62),
            press: [Spring::new(0.0, 0.25, 0.7), Spring::new(0.0, 0.25, 0.7), Spring::new(0.0, 0.25, 0.7)],
            pressed: None,
            bar_grow: Spring::new(0.0, 0.3, 0.85),
            bar_hover: false,
            scrub: None,
            seek_pending: None,
            slides: Vec::new(),
            last_skip: None,
            lean: None,
            optimistic: None,
            play_hold: None,
            paused_since: None,
            shown_playing: false,
            settling: false,
            dim: Spring::new(1.0, 0.45, 1.0),
            progress: Spring::new(0.0, 0.4, 1.0),
            last_frame: Instant::now(),
            was_animating: false,
        }
    }

    fn animating(&self) -> bool {
        !self.morph.settled()
            || !self.art_scale.settled()
            || self.press.iter().any(|p| !p.settled())
            || self.pressed.is_some()
            || self.slides.len() > 1
            || self.slides.iter().any(|s| !s.x.settled() || !s.alpha.settled())
            || !self.dim.settled()
            || !self.progress.settled()
            || !self.bar_grow.settled()
            || self.scrub.is_some()
    }
}

/// Where the buttons are, in window coordinates, for click handling.
#[derive(Clone, Copy, Default)]
struct Hits {
    previous: (i32, i32, i32, i32),
    play: (i32, i32, i32, i32),
    next: (i32, i32, i32, i32),
    /// The scrubber's grab area (taller than the visible bar).
    bar: (i32, i32, i32, i32),
}

impl Hits {
    fn at(&self, x: i32, y: i32) -> Option<Button> {
        let inside = |r: (i32, i32, i32, i32)| x >= r.0 && y >= r.1 && x < r.2 && y < r.3;
        if inside(self.play) {
            Some(Button::Play)
        } else if inside(self.previous) {
            Some(Button::Previous)
        } else if inside(self.next) {
            Some(Button::Next)
        } else {
            None
        }
    }

    fn on_bar(&self, x: i32, y: i32) -> bool {
        x >= self.bar.0 && y >= self.bar.1 && x < self.bar.2 && y < self.bar.3
    }

    /// Position along the bar (0..1) for window x.
    fn bar_fraction(&self, x: i32) -> f32 {
        let w = (self.bar.2 - self.bar.0).max(1) as f32;
        ((x - self.bar.0) as f32 / w).clamp(0.0, 1.0)
    }
}

struct Live {
    glass: glass::Glass,
    hits: Hits,
}

thread_local! {
    static LIVE: RefCell<Option<Live>> = const { RefCell::new(None) };
    static ANIM: RefCell<Anim> = RefCell::new(Anim::new());
}

// ---------- drawing a frame ----------

pub fn redraw(hwnd: HWND) {
    SELF.store(hwnd.0 as isize, Ordering::Relaxed);
    let (sw, sh) = window::screen_size();
    let shf = sh as f32;
    let track = media::current();

    // Same width as the weather card, right under it.
    let pad = (shf * 0.016) as i32;
    let anchor = anchor_rect().unwrap_or(RECT {
        left: sw - (shf * 0.015) as i32 - (shf * 0.2) as i32,
        top: 0,
        right: sw - (shf * 0.015) as i32,
        bottom: (shf * 0.1) as i32,
    });
    let width = (anchor.right - anchor.left - 2 * pad).max((shf * 0.1) as i32);
    media::set_art_size(width as u32);

    let (content, mut hits, animating, playing) = ANIM.with(|a| {
        let mut a = a.borrow_mut();
        let playing = update(&mut a, track.as_ref(), shf, width);
        let (content, hits) = draw(&a, track.as_ref(), shf, width);
        let animating = a.animating();
        a.was_animating = animating;
        (content, hits, animating, playing)
    });

    let (cw, ch) = (content.width + 2 * pad, content.height + 2 * pad);
    let pos = POINT { x: anchor.right - cw, y: anchor.bottom + (shf * GAP_FRACTION) as i32 };
    for b in [&mut hits.previous, &mut hits.play, &mut hits.next, &mut hits.bar] {
        *b = (b.0 + pad, b.1 + pad, b.2 + pad, b.3 + pad);
    }

    LIVE.with(|live| {
        let mut live = live.borrow_mut();
        let mut glass = match live.take() {
            Some(l) if l.glass.size() == (cw, ch) => l.glass,
            _ => glass::Glass::new(cw, ch, shf * 0.022, shf),
        };
        if let Some(bmp) = glass.render(&content, pad, (pos.x, pos.y), true) {
            if let Err(e) = window::present(hwnd, &bmp, pos) {
                eprintln!("could not draw music: {e}");
            }
        }
        *live = Some(Live { glass, hits });
    });

    // Frames only while something moves: every screen refresh during animations,
    // ~10 fps for the progress bar while playing, nothing at all when still.
    anim::set_animating(hwnd, animating);
    unsafe {
        SetTimer(Some(hwnd), TIMER_ID, TICK_MS, None);
        let settling = ANIM.with(|a| a.borrow().settling);
        if !animating && (playing || settling) {
            SetTimer(Some(hwnd), ANIM_TIMER_ID, PROGRESS_FRAME_MS, None);
        } else {
            let _ = KillTimer(Some(hwnd), ANIM_TIMER_ID);
        }
    }
}

/// Advances the animations to now. Returns whether the song is (shown as) playing.
fn update(a: &mut Anim, track: Option<&Track>, sh: f32, width: i32) -> bool {
    let now = Instant::now();
    // If nothing was animating, this frame starts an animation: begin from rest instead of
    // jumping ahead by however long we were idle.
    let dt = if a.was_animating { now.duration_since(a.last_frame).as_secs_f32().min(0.05) } else { 0.0 };
    a.last_frame = now;

    // Play state: what the player reports, unless a click is still waiting for confirmation.
    let reported = track.map(|t| t.playing).unwrap_or(false);
    if let Some((wanted, at)) = a.optimistic {
        if wanted == reported || at.elapsed().as_secs_f32() > 2.5 {
            a.optimistic = None;
        }
    }
    if let Some((held, at)) = a.play_hold {
        let age = at.elapsed().as_secs_f32();
        // Done once the player is back to the original state *and* has confirmed the new
        // position, but never before the minimum (the blip may not have started yet).
        let settled = reported == held && a.seek_pending.is_none() && age >= PLAY_HOLD_MIN_SECS;
        if settled || age > PLAY_HOLD_MAX_SECS {
            a.play_hold = None;
        }
    }
    let playing = if let Some((p, _)) = a.optimistic {
        p // our own play/pause click: instant
    } else if let Some((held, _)) = a.play_hold {
        held // a seek/skip we started: ignore the player's brief blips
    } else if !reported && a.shown_playing && a.started {
        // "Paused" from elsewhere must last a moment before we show it, so the blip
        // between songs doesn't flick the button and the art.
        let since = *a.paused_since.get_or_insert(now);
        if since.elapsed().as_secs_f32() >= PAUSE_DEBOUNCE_SECS {
            a.paused_since = None;
            false
        } else {
            true
        }
    } else {
        a.paused_since = None;
        reported
    };
    a.shown_playing = playing;
    // The lean gives up if the new song never shows (the skip didn't go through).
    if a.lean.is_some_and(|(_, at)| at.elapsed().as_secs_f32() > LEAN_MAX_SECS) {
        a.lean = None;
        if let Some(s) = a.slides.last_mut().filter(|s| !s.leaving()) {
            s.x.target = 0.0;
        }
    }
    a.settling = a.optimistic.is_some() || a.play_hold.is_some() || a.paused_since.is_some() || a.lean.is_some();

    // Track change: slide the old song out and the new one in.
    let key = track.map(|t| format!("{}\u{1}{}", t.title, t.artist)).unwrap_or_default();
    let art_ptr = track.and_then(|t| t.art.as_ref()).map(|a| std::sync::Arc::as_ptr(a) as usize).unwrap_or(0);
    let full_key = format!("{key}\u{2}{art_ptr}\u{2}{width}");
    let current_key = a.slides.last().map(|s| s.block.key.clone());
    if current_key.as_deref() != Some(full_key.as_str()) {
        let new_block = Rc::new(build_block(track, sh, width, full_key.clone()));
        let old_key = current_key.unwrap_or_default();
        let same_song = old_key.split('\u{2}').next() == Some(key.as_str());
        let same_size = old_key.rsplit('\u{2}').next() == full_key.rsplit('\u{2}').next();
        if !a.started || !same_size {
            // First song, or the card changed size: just show it.
            a.slides = vec![Slide::new(new_block, 0.0, 1.0)];
        } else if same_song {
            // Same song, new cover (it arrived late): crossfade in place.
            for s in &mut a.slides {
                s.alpha.target = 0.0;
                s.art_scale.get_or_insert(a.art_scale.value);
            }
            let x = a.slides.last().map(|s| s.x.value).unwrap_or(0.0);
            a.slides.push(Slide::new(new_block, x, 0.0));
        } else {
            // A new song: everything on screen slides out; the new one slides in from the other
            // side. Cards already moving keep their position and speed, so rapid skips flow.
            let direction = match a.last_skip {
                Some((d, at)) if at.elapsed().as_secs_f32() < 6.0 => d,
                _ => 1.0,
            };
            for s in &mut a.slides {
                s.x.target = -direction;
                s.alpha.target = 0.0;
                s.art_scale.get_or_insert(a.art_scale.value);
            }
            a.slides.push(Slide::new(new_block, direction, 0.0));
            a.lean = None;
            // Skipped from paused: the new song slides in small, then follows the player
            // (Spotify starts playing after a skip, so it grows once it's in place).
            if a.play_hold.is_some_and(|(held, _)| !held) {
                a.play_hold = None;
            }
        }
    }
    if let Some((d, _)) = a.lean {
        if let Some(s) = a.slides.last_mut().filter(|s| !s.leaving()) {
            s.x.target = -d * LEAN;
        }
    }
    // Cards that have finished fading out are dropped; keep at most a few in flight.
    a.slides.retain(|s| !(s.leaving() && s.alpha.value < 0.01));
    while a.slides.len() > 4 {
        a.slides.remove(0);
    }

    // Spring targets.
    a.morph.target = if playing { 1.0 } else { 0.0 };
    a.art_scale.target = if playing { 1.0 } else { PAUSED_ART_SCALE };
    for (i, b) in [Button::Previous, Button::Play, Button::Next].into_iter().enumerate() {
        a.press[i].target = if a.pressed == Some(b) { 1.0 } else { 0.0 };
    }
    a.dim.target = if track.is_some_and(|t| t.present) { 1.0 } else { DIMMED };
    // Progress follows the song exactly, but glides when it jumps (seek, restart, new song).
    let fraction = track
        .filter(|t| t.duration > 0.0)
        .map(|t| (t.position_now() / t.duration).clamp(0.0, 1.0) as f32)
        .unwrap_or(0.0);
    // After a seek, keep showing where the user dropped it until the player reports it (it
    // may briefly still report the old position), instead of flashing back.
    if let Some((wanted, at)) = a.seek_pending {
        if (fraction - wanted).abs() < 0.02 || at.elapsed().as_secs_f32() > 2.5 {
            a.seek_pending = None;
        }
    }
    let fraction = a.scrub.or(a.seek_pending.map(|(f, _)| f)).unwrap_or(fraction);
    a.bar_grow.target = if a.bar_hover || a.scrub.is_some() { 1.0 } else { 0.0 };
    a.progress.target = fraction;
    if a.scrub.is_some() {
        a.progress.snap(); // the bar follows the mouse exactly while dragging
    }
    if (a.progress.value - fraction).abs() < 0.01 && a.progress.velocity.abs() < 0.05 {
        a.progress.snap();
    }
    if !a.started {
        // First frame: start at rest, no animation.
        a.morph.snap();
        a.art_scale.snap();
        a.dim.snap();
        a.progress.snap();
        a.started = true;
    }
    a.morph.step(dt);
    a.art_scale.step(dt);
    for s in &mut a.slides {
        s.x.step(dt);
        s.alpha.step(dt);
    }
    a.dim.step(dt);
    a.progress.step(dt);
    a.bar_grow.step(dt);
    for p in &mut a.press {
        p.step(dt);
    }
    playing
}

/// Redraws only if the wallpaper changed (frames for animation come from the animation timer).
pub fn tick(hwnd: HWND) {
    // render(force = false) only does work (and returns something) when the glass itself changed,
    // e.g. a new wallpaper; the glass layer it rebuilds is then reused by redraw.
    let wallpaper_changed = LIVE.with(|live| {
        live.borrow_mut()
            .as_mut()
            .is_some_and(|l| l.glass.render(&Bitmap::new(1, 1), 0, current_pos(hwnd), false).is_some())
    });
    if wallpaper_changed {
        redraw(hwnd);
    }
}

/// Mouse button went down at window coordinates (x, y): start the press animation.
pub fn press(hwnd: HWND, x: i32, y: i32) {
    let hits = LIVE.with(|l| l.borrow().as_ref().map(|l| l.hits));
    let Some(hits) = hits else { return };
    // Grabbing the scrubber: the bar jumps to the mouse and follows it until release.
    if hits.on_bar(x, y) && media::current().is_some_and(|t| t.present && t.duration > 0.0) {
        ANIM.with(|a| {
            let mut a = a.borrow_mut();
            a.scrub = Some(hits.bar_fraction(x));
            a.seek_pending = None;
        });
        unsafe {
            SetCapture(hwnd);
        }
        redraw(hwnd);
        return;
    }
    let button = hits.at(x, y);
    if button.is_some() {
        ANIM.with(|a| a.borrow_mut().pressed = button);
        // Keep getting mouse messages until the button is released, even off the card.
        unsafe {
            SetCapture(hwnd);
        }
        redraw(hwnd);
    }
}

/// Mouse button came up: act if it's still over the button that was pressed.
pub fn release(hwnd: HWND, x: i32, y: i32) {
    // Letting go of the scrubber: jump the song there.
    let scrubbed = ANIM.with(|a| a.borrow_mut().scrub.take());
    if let Some(fraction) = scrubbed {
        unsafe {
            let _ = ReleaseCapture();
        }
        if let Some(t) = media::current().filter(|t| t.duration > 0.0) {
            media::send(media::Command::Seek(fraction as f64 * t.duration));
            ANIM.with(|a| {
                let mut a = a.borrow_mut();
                a.seek_pending = Some((fraction, Instant::now()));
                a.play_hold = Some((a.shown_playing, Instant::now()));
                a.progress.value = fraction;
                a.progress.target = fraction;
                a.progress.velocity = 0.0;
            });
        }
        let hovering = LIVE.with(|l| l.borrow().as_ref().is_some_and(|l| l.hits.on_bar(x, y)));
        ANIM.with(|a| a.borrow_mut().bar_hover = hovering);
        redraw(hwnd);
        return;
    }
    let over = LIVE.with(|l| l.borrow().as_ref().and_then(|l| l.hits.at(x, y)));
    let pressed = ANIM.with(|a| a.borrow_mut().pressed.take());
    unsafe {
        let _ = ReleaseCapture();
    }
    if let (Some(p), Some(o)) = (pressed, over) {
        if p == o {
            ANIM.with(|a| {
                let mut a = a.borrow_mut();
                match p {
                    Button::Play => {
                        let reported = media::current().map(|t| t.playing).unwrap_or(false);
                        let shown = a.optimistic.map(|(s, _)| s).unwrap_or(reported);
                        a.optimistic = Some((!shown, Instant::now()));
                        media::send(media::Command::PlayPause);
                    }
                    Button::Previous => {
                        a.last_skip = Some((-1.0, Instant::now()));
                        // Keep the current play state (and art size) until the new song arrives.
                        a.play_hold = Some((a.shown_playing, Instant::now()));
                        kick(&mut a, -1.0);
                        media::send(media::Command::Previous);
                    }
                    Button::Next => {
                        a.last_skip = Some((1.0, Instant::now()));
                        a.play_hold = Some((a.shown_playing, Instant::now()));
                        kick(&mut a, 1.0);
                        media::send(media::Command::Next);
                    }
                }
            });
        }
    }
    redraw(hwnd);
}

/// Instant feedback for a skip: nudge the current card in the slide direction. Its spring
/// pulls it back unless the new song arrives first, in which case the slide simply continues.
fn kick(a: &mut Anim, direction: f32) {
    if let Some(s) = a.slides.last_mut() {
        s.x.velocity -= direction * SKIP_KICK;
    }
    a.lean = Some((direction, Instant::now()));
    a.was_animating = true;
}

/// The mouse was taken away mid-press (e.g. another window grabbed it): un-press.
pub fn cancel_press(hwnd: HWND) {
    let was_pressed = ANIM.with(|a| {
        let mut a = a.borrow_mut();
        let dragging = a.scrub.take().is_some();
        a.pressed.take().is_some() || dragging
    });
    if was_pressed {
        redraw(hwnd);
    }
}

/// Mouse moved over the card (window coordinates): drag the scrubber, or grow the bar on hover.
pub fn mouse_move(hwnd: HWND, x: i32, y: i32) {
    let Some(hits) = LIVE.with(|l| l.borrow().as_ref().map(|l| l.hits)) else { return };
    // Ask Windows to tell us when the mouse leaves the card (so the bar can shrink back).
    unsafe {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut tme);
    }
    let changed = ANIM.with(|a| {
        let mut a = a.borrow_mut();
        if a.scrub.is_some() {
            a.scrub = Some(hits.bar_fraction(x));
            return true;
        }
        let hover = hits.on_bar(x, y);
        let changed = hover != a.bar_hover;
        a.bar_hover = hover;
        changed
    });
    if changed {
        redraw(hwnd);
    }
}

/// The mouse left the card.
pub fn mouse_leave(hwnd: HWND) {
    let changed = ANIM.with(|a| std::mem::replace(&mut a.borrow_mut().bar_hover, false));
    if changed {
        redraw(hwnd);
    }
}

/// Whether the mouse (window coordinates) is over something clickable (for the hand cursor).
pub fn over_control(x: i32, y: i32) -> bool {
    LIVE.with(|l| l.borrow().as_ref().is_some_and(|l| l.hits.on_bar(x, y) || l.hits.at(x, y).is_some()))
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

fn current_pos(hwnd: HWND) -> (i32, i32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
    }
    (r.left, r.top)
}

// ---------- layout ----------

/// Pre-renders the song-specific parts: art (full size, rounded), title and artist.
fn build_block(track: Option<&Track>, sh: f32, width: i32, key: String) -> Block {
    let (title, artist) = match track {
        Some(t) => (
            if t.title.is_empty() { "Unknown title".to_string() } else { t.title.clone() },
            t.artist.clone(),
        ),
        None => ("Not playing".to_string(), "Spotifast".to_string()),
    };
    let mut art = Bitmap::new(width, width);
    let radius = sh * 0.012;
    match track.and_then(|t| t.art.as_ref()) {
        Some(img) => draw_art(&mut art, img, width, radius),
        None => {
            fill_rounded(&mut art, (0.0, 0.0, width as f32, width as f32), radius, 0.15);
            draw_note(&mut art, width);
        }
    }
    let title = fit(&render::sf_bold(), &title, sh * 0.0115, width);
    let mut artist = fit(&render::sf(), &artist, sh * 0.0095, width);
    artist.fade(SECONDARY);
    let slot = |font: &ab_glyph::FontRef, size: f32| {
        let r = render::line(font, "Hgy", size, 1.0, render::WHITE);
        (r.height, r.baseline)
    };
    let title_slot = slot(&render::sf_bold(), sh * 0.0115);
    let artist_slot = slot(&render::sf(), sh * 0.0095);
    Block { key, art, title, artist, title_slot, artist_slot }
}

/// Draws one frame of the card's content. Returns it and the button rectangles (content coordinates).
fn draw(a: &Anim, track: Option<&Track>, sh: f32, width: i32) -> (Bitmap, Hits) {
    let regular = render::sf();
    let block = &a.slides.last().expect("a song card is built in update").block;
    let (mut pos, dur) = track.map(|t| (t.position_now(), t.duration)).unwrap_or((0.0, 0.0));
    if let Some(f) = a.scrub.or(a.seek_pending.map(|(f, _)| f)) {
        pos = f as f64 * dur; // the elapsed time follows the scrubber
    }
    let time_h = sh * 0.0072;
    let elapsed = time_label(&regular, &fmt_time(pos), time_h);
    let total = time_label(&regular, &fmt_time(dur), time_h);

    // Vertical layout, top to bottom.
    let art_size = width;
    let title_y = art_size + (sh * 0.012) as i32;
    let artist_y = title_y + block.title_slot.0 + (sh * 0.006) as i32;
    let bar_h = (sh * 0.0025).max(2.0);
    let bar_y = (artist_y + block.artist_slot.0) as f32 + sh * 0.012;
    let times_y = (bar_y + bar_h + sh * 0.005) as i32;
    let icon = sh * 0.014;
    let play_icon = sh * 0.018;
    let buttons_cy = (times_y + elapsed.height) as f32 + sh * 0.010 + play_icon / 2.0;
    // Leave room below the buttons for the whole press glow.
    let height = (buttons_cy + GLOW_FRACTION * sh + 2.0).ceil() as i32;
    let mut out = Bitmap::new(width, height);

    // Song cards: older ones sliding/fading out, the current one sliding in.
    let slide = width as f32 * SLIDE_FRACTION;
    let mut draw_block = |b: &Block, art_scale: f32, dx: f32, alpha: f32| {
        let c = art_size as f32 / 2.0;
        blit_scaled(&mut out, &b.art, c + dx, c, art_scale, alpha);
        // Each line sits on its slot's baseline, whatever its letters.
        let ty = title_y + b.title_slot.1 - b.title.baseline;
        let ay = artist_y + b.artist_slot.1 - b.artist.baseline;
        blit_alpha(&mut out, &b.title, dx.round() as i32, ty, alpha);
        blit_alpha(&mut out, &b.artist, dx.round() as i32, ay, alpha);
    };
    for s in &a.slides {
        let alpha = s.alpha.value.clamp(0.0, 1.0);
        if alpha > 0.003 {
            draw_block(&s.block, s.art_scale.unwrap_or(a.art_scale.value), s.x.value * slide, alpha);
        }
    }

    // Progress bar with elapsed / total times underneath. It grows thicker from its centre
    // while hovered or dragged, so nothing around it moves.
    let grow = a.bar_grow.value.max(0.0) * BAR_GROW_FRACTION * sh;
    let centre = bar_y + bar_h / 2.0;
    let thick = bar_h + grow;
    let bar = (0.0, centre - thick / 2.0, width as f32, centre + thick / 2.0);
    let track_opacity = TRACK_OPACITY + 0.12 * a.bar_grow.value.clamp(0.0, 1.0);
    fill_rounded(&mut out, bar, thick / 2.0, track_opacity);
    if dur > 0.0 {
        let filled = bar.0 + (bar.2 - bar.0) * a.progress.value.clamp(0.0, 1.0);
        fill_rounded(&mut out, (bar.0, bar.1, filled.max(bar.0 + thick), bar.3), thick / 2.0, 1.0);
    }
    let grab = BAR_GRAB_FRACTION * sh;
    let bar_hit = (0, (centre - grab) as i32, width, (centre + grab) as i32);
    out.blit(&elapsed, 0, times_y);
    out.blit(&total, width - total.width, times_y);

    // Buttons, centred along the bottom, each shrinking and glowing while pressed.
    let spacing = sh * 0.035;
    let cx = width as f32 / 2.0;
    let cy = buttons_cy;
    let centers = [cx - spacing, cx, cx + spacing];
    for (i, &x) in centers.iter().enumerate() {
        let p = a.press[i].value.clamp(0.0, 1.2);
        if p > 0.01 {
            let r = sh * GLOW_FRACTION;
            let bounds = (x - r, cy - r, x + r, cy + r);
            fill_shape(&mut out, bounds, |q| ((q.0 - x).powi(2) + (q.1 - cy).powi(2)).sqrt() - r, 0.18 * p.min(1.0));
        }
    }
    let scale = |i: usize| 1.0 - PRESS_SHRINK * a.press[i].value;
    draw_skip(&mut out, centers[0], cy, icon * scale(0), false);
    // The shape morph stays between the two icons; any spring overshoot becomes a gentle swell
    // (overshooting the shape itself would turn the triangle's tip inside out).
    let morph = a.morph.value.clamp(0.0, 1.0);
    let bounce = 1.0 + ICON_BOUNCE * (a.morph.value - morph).abs();
    draw_play_pause(&mut out, centers[1], cy, play_icon * scale(1) * bounce, morph);
    draw_skip(&mut out, centers[2], cy, icon * scale(2), true);
    let hit = |x: f32| {
        let r = sh * 0.016;
        ((x - r) as i32, (cy - r) as i32, (x + r) as i32, (cy + r) as i32)
    };
    let hits = Hits { previous: hit(centers[0]), play: hit(centers[1]), next: hit(centers[2]), bar: bar_hit };

    if a.dim.value < 0.999 {
        out.fade(a.dim.value.clamp(0.0, 1.0));
    }
    (out, hits)
}

thread_local! {
    static TIME_LABELS: RefCell<std::collections::HashMap<String, Rc<Bitmap>>> =
        RefCell::new(std::collections::HashMap::new());
}

/// A dimmed time label like "1:23", rendered once and reused.
fn time_label(font: &ab_glyph::FontRef, text: &str, height: f32) -> Rc<Bitmap> {
    TIME_LABELS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() > 32 {
            cache.clear();
        }
        cache
            .entry(format!("{text}@{height}"))
            .or_insert_with(|| {
                let mut b = render::line(font, text, height, 1.0, render::WHITE);
                b.fade(SECONDARY);
                Rc::new(b)
            })
            .clone()
    })
}


/// Renders `text`, shortening it with "…" until it fits in `max_width`. Finds the longest
/// fitting length by binary search (a handful of renders, not one per character).
fn fit(font: &ab_glyph::FontRef, text: &str, height: f32, max_width: i32) -> Bitmap {
    let full = render::line(font, text, height, 1.0, render::WHITE);
    if full.width <= max_width {
        return full;
    }
    let chars: Vec<char> = text.chars().collect();
    let shortened = |n: usize| chars[..n].iter().collect::<String>().trim_end().to_string() + "…";
    let (mut lo, mut hi) = (1, chars.len() - 1); // lo always fits (or is the minimum)
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

fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

// ---------- compositing helpers ----------

/// Lays a premultiplied colour (r, g, b in 0..255, a in 0..1) over the pixel at (x, y).
fn over(out: &mut Bitmap, x: i32, y: i32, c: [f32; 4]) {
    if x < 0 || y < 0 || x >= out.width || y >= out.height || c[3] <= 0.0 {
        return;
    }
    let to8 = |v: f32| v.round().clamp(0.0, 255.0) as u32;
    let a8 = to8(c[3] * 255.0);
    let p = a8 << 24 | to8(c[0]).min(a8) << 16 | to8(c[1]).min(a8) << 8 | to8(c[2]).min(a8);
    let slot = &mut out.pixels[(y * out.width + x) as usize];
    *slot = render::over(*slot, p);
}

/// Copies `src` with its top-left at (x, y), at `alpha` opacity.
fn blit_alpha(out: &mut Bitmap, src: &Bitmap, x: i32, y: i32, alpha: f32) {
    let a = (alpha.clamp(0.0, 1.0) * 256.0).round() as u32;
    if a == 0 {
        return;
    }
    let (c0, c1) = ((-x).max(0), (out.width - x).min(src.width));
    for row in (-y).max(0)..(out.height - y).min(src.height) {
        let (s_row, d_row) = ((row * src.width) as usize, ((y + row) * out.width + x) as usize);
        for col in c0..c1 {
            let p = render::scale(src.pixels[s_row + col as usize], a);
            if p >> 24 != 0 {
                let d = &mut out.pixels[d_row + col as usize];
                *d = render::over(*d, p);
            }
        }
    }
}

/// Draws `src` scaled by `scale` around the centre point (cx, cy), at `alpha` opacity,
/// with smooth (bilinear) sampling.
fn blit_scaled(out: &mut Bitmap, src: &Bitmap, cx: f32, cy: f32, scale: f32, alpha: f32) {
    if (scale - 1.0).abs() < 0.002 {
        let x = (cx - src.width as f32 / 2.0).round() as i32;
        let y = (cy - src.height as f32 / 2.0).round() as i32;
        return blit_alpha(out, src, x, y, alpha);
    }
    let a = (alpha.clamp(0.0, 1.0) * 256.0).round() as u32;
    if a == 0 {
        return;
    }
    let (w, h) = (src.width as f32 * scale, src.height as f32 * scale);
    let (x0, y0) = (cx - w / 2.0, cy - h / 2.0);
    let dx0 = (x0.floor() as i32).max(0);
    let dx1 = ((x0 + w).ceil() as i32).min(out.width);
    let dy0 = (y0.floor() as i32).max(0);
    let dy1 = ((y0 + h).ceil() as i32).min(out.height);
    if dx1 <= dx0 || dy1 <= dy0 {
        return;
    }

    // Where each output column/row samples from: (left/top index, right/bottom index, weight 0..256).
    // An index of -1 means "outside the source" (transparent).
    let axis = |d: i32, origin: f32, len: i32| -> (i32, i32, u32) {
        let u = (d as f32 + 0.5 - origin) / scale - 0.5;
        let i = u.floor() as i32;
        let f = ((u - i as f32) * 256.0) as u32;
        let ok = |k: i32| if k >= 0 && k < len { k } else { -1 };
        (ok(i), ok(i + 1), f)
    };
    let cols: Vec<(i32, i32, u32)> = (dx0..dx1).map(|d| axis(d, x0, src.width)).collect();
    let get = |x: i32, y: i32| if x < 0 || y < 0 { 0 } else { src.pixels[(y * src.width + x) as usize] };
    for dy in dy0..dy1 {
        let (ya, yb, fy) = axis(dy, y0, src.height);
        let d_row = (dy * out.width) as usize;
        for (k, &(xa, xb, fx)) in cols.iter().enumerate() {
            let (p00, p01, p10, p11) = (get(xa, ya), get(xb, ya), get(xa, yb), get(xb, yb));
            if (p00 | p01 | p10 | p11) == 0 {
                continue;
            }
            let ch = |shift: u32| {
                let c = |p: u32| (p >> shift) & 0xFF;
                let top = c(p00) * (256 - fx) + c(p01) * fx;
                let bottom = c(p10) * (256 - fx) + c(p11) * fx;
                (top * (256 - fy) + bottom * fy) >> 16
            };
            let p = render::scale(ch(24) << 24 | ch(16) << 16 | ch(8) << 8 | ch(0), a);
            if p >> 24 != 0 {
                let d = &mut out.pixels[d_row + (dx0 as usize + k)];
                *d = render::over(*d, p);
            }
        }
    }
}

// ---------- shapes ----------

/// Album art, centre-cropped to a square, scaled smoothly, with rounded corners.
fn draw_art(out: &mut Bitmap, img: &image::RgbaImage, size: i32, radius: f32) {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let side = iw.min(ih);
    let (ox, oy) = ((iw - side) / 2.0, (ih - side) / 2.0);
    let scale = side / size as f32;
    let half = size as f32 / 2.0;
    for y in 0..size {
        for x in 0..size {
            let p = (x as f32 + 0.5 - half, y as f32 + 0.5 - half);
            let cov = (0.5 - rounded_rect(p, (half, half), radius)).clamp(0.0, 1.0);
            if cov == 0.0 {
                continue;
            }
            // Average the source pixels this output pixel covers (smooth downscaling).
            let (sx0, sy0) = (ox + x as f32 * scale, oy + y as f32 * scale);
            let n = scale.ceil().max(1.0) as u32;
            let (mut r, mut g, mut b, mut count) = (0.0, 0.0, 0.0, 0.0);
            for j in 0..n {
                for i in 0..n {
                    let px = ((sx0 + (i as f32 + 0.5) * scale / n as f32) as u32).min(img.width() - 1);
                    let py = ((sy0 + (j as f32 + 0.5) * scale / n as f32) as u32).min(img.height() - 1);
                    let c = img.get_pixel(px, py).0;
                    r += c[0] as f32;
                    g += c[1] as f32;
                    b += c[2] as f32;
                    count += 1.0;
                }
            }
            let to8 = |v: f32| (v / count * cov).round().clamp(0.0, 255.0) as u32;
            out.pixels[(y * out.width + x) as usize] =
                ((cov * 255.0).round() as u32) << 24 | to8(r) << 16 | to8(g) << 8 | to8(b);
        }
    }
}

/// A simple music note for the placeholder art.
fn draw_note(out: &mut Bitmap, size: i32) {
    let s = size as f32;
    fill_shape(
        out,
        (0.0, 0.0, s, s),
        |p| {
            let head = ((p.0 - s * 0.42).powi(2) + (p.1 - s * 0.66).powi(2)).sqrt() - s * 0.09;
            let stem = rounded_rect((p.0 - s * 0.495, p.1 - s * 0.45), (s * 0.018, s * 0.21), 0.0);
            let flag = rounded_rect((p.0 - s * 0.57, p.1 - s * 0.27), (s * 0.08, s * 0.03), s * 0.02);
            head.min(stem).min(flag)
        },
        0.6,
    );
}

/// Play/pause icon, morphing between them: 0 = play triangle, 1 = pause bars.
/// Both are two four-cornered shapes (the triangle is split down the middle), so every
/// corner can glide from one icon to the other.
fn draw_play_pause(out: &mut Bitmap, cx: f32, cy: f32, size: f32, morph: f32) {
    let h = size / 2.0;
    // Play: a triangle split into a left quad and a right (pointed) quad. The halves overlap a
    // little past the split line so the seam between them is always fully covered (two soft
    // edges meeting exactly would leave a faint line).
    let (x0, x1) = (cx - h * 0.75, cx + h);
    let xm = (x0 + x1) / 2.0;
    let overlap = 1.5;
    let top = |x: f32| cy - h + (x - x0) / (x1 - x0) * h; // the triangle's slanted top edge at x
    let bottom = |x: f32| cy + h - (x - x0) / (x1 - x0) * h; // and its bottom edge
    let (xl, xr) = (xm + overlap, xm - overlap);
    let play = [
        [(x0, cy - h), (xl, top(xl)), (xl, bottom(xl)), (x0, cy + h)],
        [(xr, top(xr)), (x1, cy), (x1, cy), (xr, bottom(xr))],
    ];
    // Pause: two bars.
    let (bw, gap) = (size * 0.30, size * 0.16);
    let pause = [
        [(cx - gap - bw, cy - h), (cx - gap, cy - h), (cx - gap, cy + h), (cx - gap - bw, cy + h)],
        [(cx + gap, cy - h), (cx + gap + bw, cy - h), (cx + gap + bw, cy + h), (cx + gap, cy + h)],
    ];
    let m = morph.clamp(0.0, 1.0);
    let mut quads = [[(0.0, 0.0); 4]; 2];
    for k in 0..2 {
        for i in 0..4 {
            quads[k][i] = (
                play[k][i].0 + (pause[k][i].0 - play[k][i].0) * m,
                play[k][i].1 + (pause[k][i].1 - play[k][i].1) * m,
            );
        }
    }
    let bounds = (cx - h - 2.0, cy - h - 2.0, cx + h + 2.0, cy + h + 2.0);
    fill_polygons(out, bounds, &[&quads[0], &quads[1]]);
}

/// Two triangles pointing right (next) or left (previous).
fn draw_skip(out: &mut Bitmap, cx: f32, cy: f32, size: f32, forward: bool) {
    let h = size / 2.0;
    let d = if forward { 1.0 } else { -1.0 };
    for offset in [-h, 0.0] {
        let x = cx + offset * d;
        let tri = [(x, cy - h * 0.8), (x + h * d, cy), (x, cy + h * 0.8)];
        let bounds = (x.min(x + h * d), cy - h, x.max(x + h * d), cy + h);
        fill_polygons(out, bounds, &[&tri]);
    }
}

/// Fills a rounded rectangle given as (left, top, right, bottom) with white at `opacity`.
fn fill_rounded(out: &mut Bitmap, r: (f32, f32, f32, f32), radius: f32, opacity: f32) {
    let c = ((r.0 + r.2) / 2.0, (r.1 + r.3) / 2.0);
    let half = ((r.2 - r.0) / 2.0, (r.3 - r.1) / 2.0);
    fill_shape(out, r, |p| rounded_rect((p.0 - c.0, p.1 - c.1), half, radius.min(half.0).min(half.1)), opacity);
}

/// Fills the pixels inside `bounds` (left, top, right, bottom) by a signed distance function
/// (negative inside), anti-aliased, in white. Only the bounding box is visited.
fn fill_shape(out: &mut Bitmap, bounds: (f32, f32, f32, f32), distance: impl Fn((f32, f32)) -> f32, opacity: f32) {
    let (x0, y0) = ((bounds.0 - 1.0).floor().max(0.0) as i32, (bounds.1 - 1.0).floor().max(0.0) as i32);
    let x1 = ((bounds.2 + 1.0).ceil() as i32).min(out.width);
    let y1 = ((bounds.3 + 1.0).ceil() as i32).min(out.height);
    for y in y0..y1 {
        for x in x0..x1 {
            let d = distance((x as f32 + 0.5, y as f32 + 0.5));
            if d < 1.0 {
                let a = (0.5 - d).clamp(0.0, 1.0) * opacity;
                over(out, x, y, [255.0 * a, 255.0 * a, 255.0 * a, a]);
            }
        }
    }
}

fn rounded_rect(p: (f32, f32), half: (f32, f32), r: f32) -> f32 {
    let q = (p.0.abs() - half.0 + r, p.1.abs() - half.1 + r);
    let outside = (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt();
    outside + q.0.max(q.1).min(0.0) - r
}

/// How many samples per side a pixel on an icon's edge gets (16 = 256 samples per pixel).
/// Pixels away from every edge are uniform, so they're tested just once.
const ICON_SAMPLES: i32 = 16;

/// Fills the union of convex polygons in white, anti-aliased by supersampling the pixels
/// that an edge passes through. Unlike distance-based edges, this stays accurate for
/// needle-thin tips and sharp corners, so a moving point doesn't flicker frame to frame.
fn fill_polygons(out: &mut Bitmap, bounds: (f32, f32, f32, f32), polys: &[&[(f32, f32)]]) {
    // Pre-compute each polygon's winding (sign of its area) once.
    let windings: Vec<f32> = polys
        .iter()
        .map(|pts| {
            let n = pts.len();
            (0..n).map(|i| pts[i].0 * pts[(i + 1) % n].1 - pts[(i + 1) % n].0 * pts[i].1).sum::<f32>()
        })
        .collect();
    let inside = |q: (f32, f32)| {
        polys.iter().zip(&windings).any(|(pts, &area)| {
            if area.abs() < 1e-4 {
                return false;
            }
            let n = pts.len();
            (0..n).all(|i| {
                let (a, b) = (pts[i], pts[(i + 1) % n]);
                let e = (b.0 - a.0, b.1 - a.1);
                if e.0 * e.0 + e.1 * e.1 < 1e-8 {
                    return true; // repeated corner: no edge here
                }
                (e.0 * (q.1 - a.1) - e.1 * (q.0 - a.0)) * area >= 0.0
            })
        })
    };
    let (x0, y0) = (bounds.0.floor().max(0.0) as i32, bounds.1.floor().max(0.0) as i32);
    let x1 = (bounds.2.ceil() as i32 + 1).min(out.width);
    let y1 = (bounds.3.ceil() as i32 + 1).min(out.height);
    // Distance from a point to the nearest polygon edge (to find pixels an edge passes through).
    let edge_distance = |q: (f32, f32)| {
        let mut best = f32::MAX;
        for pts in polys {
            let n = pts.len();
            for i in 0..n {
                let (a, b) = (pts[i], pts[(i + 1) % n]);
                let e = (b.0 - a.0, b.1 - a.1);
                let w = (q.0 - a.0, q.1 - a.1);
                let len2 = e.0 * e.0 + e.1 * e.1;
                let t = if len2 > 1e-8 { ((w.0 * e.0 + w.1 * e.1) / len2).clamp(0.0, 1.0) } else { 0.0 };
                best = best.min((w.0 - e.0 * t).powi(2) + (w.1 - e.1 * t).powi(2));
            }
        }
        best.sqrt()
    };
    let n = ICON_SAMPLES;
    let step = 1.0 / n as f32;
    for y in y0..y1 {
        for x in x0..x1 {
            let center = (x as f32 + 0.5, y as f32 + 0.5);
            // No edge within the pixel (half-diagonal ≈ 0.71): it's fully in or fully out.
            if edge_distance(center) > 0.75 {
                if inside(center) {
                    over(out, x, y, [255.0, 255.0, 255.0, 1.0]);
                }
                continue;
            }
            let mut hits = 0;
            for j in 0..n {
                for i in 0..n {
                    let q = (x as f32 + (i as f32 + 0.5) * step, y as f32 + (j as f32 + 0.5) * step);
                    if inside(q) {
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                let a = hits as f32 / (n * n) as f32;
                over(out, x, y, [255.0 * a, 255.0 * a, 255.0 * a, a]);
            }
        }
    }
}















