//! Shared animation machinery: damped springs, and a frame clock synced to the display.
//!
//! Frame clock: Windows timers tick every ~15.6 ms, so a "16 ms" timer really fires at 16, 31
//! or 47 ms: uneven frames that look like stutter. Instead, one small thread waits for screen
//! refreshes (DwmFlush returns right after Windows composes a frame) and, on every Nth one,
//! posts WM_WIDGET_FRAME to each widget window that's currently animating. Frames therefore
//! line up with the display and are evenly spaced.
//!
//! Frames are capped near TARGET_FPS: N is the whole number of refreshes closest to it
//! (at 90: 165 Hz -> every 2nd = 82.5 fps, 144 Hz -> every 2nd = 72 fps, 60 Hz -> every one).
//! Animations are timed by the clock, not by frames, so they move at the same speed.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Roughly how many animation frames per second to draw.
pub const TARGET_FPS: f32 = 90.0;

/// Posted to an animating widget window once per frame.
pub const WM_WIDGET_FRAME: u32 = WM_APP + 3;

// ---------- springs ----------

/// A damped spring, like SwiftUI's `.spring(response:dampingFraction:)`.
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    /// Seconds for one undamped oscillation (smaller = snappier).
    pub response: f32,
    /// 1.0 = no overshoot; below 1.0 bounces a little.
    pub damping: f32,
}

impl Spring {
    pub fn new(value: f32, response: f32, damping: f32) -> Spring {
        Spring { value, velocity: 0.0, target: value, response, damping }
    }

    pub fn step(&mut self, dt: f32) {
        let omega = std::f32::consts::TAU / self.response;
        // Small fixed sub-steps keep the integration stable at any frame rate.
        let mut left = dt;
        while left > 0.0 {
            let h = left.min(0.004);
            let accel = -omega * omega * (self.value - self.target) - 2.0 * self.damping * omega * self.velocity;
            self.velocity += accel * h;
            self.value += self.velocity * h;
            left -= h;
        }
        // Close enough and nearly still: finish exactly on target, so the animation ends
        // (otherwise it creeps toward the target forever and frames never stop).
        if (self.value - self.target).abs() < 0.002 && self.velocity.abs() < 0.02 {
            self.snap();
        }
    }

    pub fn settled(&self) -> bool {
        (self.value - self.target).abs() < 0.001 && self.velocity.abs() < 0.01
    }

    pub fn snap(&mut self) {
        self.value = self.target;
        self.velocity = 0.0;
    }
}

// ---------- frame clock ----------

/// Windows that want frames, and whether a frame message is already on its way to each.
static WANTED: Mutex<Vec<(isize, bool)>> = Mutex::new(Vec::new());
static CLOCK: OnceLock<std::thread::Thread> = OnceLock::new();

/// Starts or stops frames for `hwnd`.
pub fn set_animating(hwnd: HWND, on: bool) {
    let key = hwnd.0 as isize;
    {
        let mut wanted = WANTED.lock().unwrap();
        let present = wanted.iter().any(|(h, _)| *h == key);
        if on && !present {
            wanted.push((key, false));
        } else if !on {
            wanted.retain(|(h, _)| *h != key);
            return;
        }
    }
    CLOCK.get_or_init(start_clock).unpark();
}

/// Call when handling WM_WIDGET_FRAME, so the next frame can be sent.
pub fn frame_done(hwnd: HWND) {
    let key = hwnd.0 as isize;
    if let Some(entry) = WANTED.lock().unwrap().iter_mut().find(|(h, _)| *h == key) {
        entry.1 = false;
    }
}

fn start_clock() -> std::thread::Thread {
    std::thread::spawn(|| {
        let mut every = refreshes_per_frame();
        let mut count = 0u32;
        loop {
            if WANTED.lock().unwrap().is_empty() {
                std::thread::park();
                every = refreshes_per_frame(); // the display may have changed meanwhile
                count = 0;
                continue;
            }
            if unsafe { DwmFlush() }.is_err() {
                std::thread::sleep(Duration::from_millis(16));
            }
            count += 1;
            if count < every {
                continue;
            }
            count = 0;
            // One frame request per window at a time: never queue up frames we can't draw.
            for (hwnd, pending) in WANTED.lock().unwrap().iter_mut() {
                if !*pending {
                    *pending = true;
                    unsafe {
                        let _ = PostMessageW(Some(HWND(*hwnd as *mut std::ffi::c_void)), WM_WIDGET_FRAME, WPARAM(0), LPARAM(0));
                    }
                }
            }
        }
    })
    .thread()
    .clone()
}

/// How many screen refreshes to wait between frames, to land as close to TARGET_FPS as possible.
fn refreshes_per_frame() -> u32 {
    let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    let hz = if unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool() {
        mode.dmDisplayFrequency.max(1) as f32
    } else {
        60.0
    };
    (hz / TARGET_FPS).round().max(1.0) as u32
}
