//! The big centred clock with the date underneath.

use crate::render::{self, Bitmap};
use crate::window;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::WindowsAndMessaging::SetTimer;

pub const TIMER_ID: usize = 1;

/// Height of the time digits as a fraction of the screen height.
pub const HEIGHT_FRACTION: f32 = 0.32;
/// Horizontal squeeze of the digits: < 1.0 makes them narrower than the font draws them.
pub const NARROW: f32 = 1.0;
/// Height of the date line's capital letters as a fraction of the screen height.
pub const DATE_HEIGHT_FRACTION: f32 = 0.018;
/// Space between the digits and the date, as a fraction of the screen height.
pub const DATE_GAP_FRACTION: f32 = 0.008;
/// Horizontal stretch of the date letters: > 1.0 makes them wider than the font draws them.
pub const DATE_WIDEN: f32 = 1.3;
/// Gray at the top of the minutes' gradient.
pub const MINUTE_TOP_GRAY: u8 = 0x80;
/// How far down the digits the gradient reaches full white (0 = top, 1 = bottom).
pub const MINUTE_WHITE_AT: f32 = 0.6;
/// Opacity of the time digits (1.0 = solid; a little lower lets the wallpaper show through).
pub const TIME_OPACITY: f32 = 0.85;
/// How far above the screen centre the clock sits, as a fraction of the screen height.
pub const LIFT_FRACTION: f32 = 0.05;

/// Draws the current time, centres it on the screen, and schedules the next update.
pub fn redraw(hwnd: HWND, clock_24h: bool) {
    let now = chrono::Local::now();
    let time = now.format(if clock_24h { "%H:%M" } else { "%-I:%M" }).to_string();
    let date = now.format("%A, %B %-d").to_string();

    let (sw, sh) = window::screen_size();
    let bmp = render(&time, &date, sh as f32);
    let lift = (sh as f32 * LIFT_FRACTION) as i32;
    let pos = POINT { x: (sw - bmp.width) / 2, y: (sh - bmp.height) / 2 - lift };
    if let Err(e) = window::present(hwnd, &bmp, pos) {
        eprintln!("could not draw clock: {e}");
    }

    // Wake again just after the next minute starts.
    let ms_into_minute = chrono::Timelike::second(&now) * 1000 + now.timestamp_subsec_millis() % 1000;
    unsafe {
        SetTimer(Some(hwnd), TIMER_ID, 60_000 - ms_into_minute + 50, None);
    }
}

fn render(time: &str, date: &str, screen_height: f32) -> Bitmap {
    // Hour and colon are white; the minutes fade from gray at the top to white at the bottom.
    let colon = time.chars().position(|c| c == ':').unwrap_or(usize::MAX);
    let font = render::pillar();
    let mut time_bmp = render::line_spaced(
        &font,
        time,
        screen_height * HEIGHT_FRACTION,
        NARROW,
        |i, v| if i <= colon { render::WHITE } else { minute_gradient(v) },
        |prev, c, px| tuck_one(&font, prev, c, px),
    );
    time_bmp.fade(TIME_OPACITY);
    let date_bmp = render::line(&render::sf(), date, screen_height * DATE_HEIGHT_FRACTION, DATE_WIDEN, render::WHITE);
    let gap = (screen_height * DATE_GAP_FRACTION) as i32;
    let margin = 2;

    let width = time_bmp.width.max(date_bmp.width) + 2 * margin;
    let height = time_bmp.height + gap + date_bmp.height + 2 * margin;
    let mut out = Bitmap::new(width, height);
    out.blit(&time_bmp, (width - time_bmp.width) / 2, margin);
    out.blit(&date_bmp, (width - date_bmp.width) / 2, margin + time_bmp.height + gap);
    out
}

/// The minutes' colour at height `v` (0 = top of the digits, 1 = bottom): gray fading to white,
/// reaching full white at MINUTE_WHITE_AT.
fn minute_gradient(v: f32) -> (u8, u8, u8) {
    let t = (v / MINUTE_WHITE_AT).clamp(0.0, 1.0);
    let g = (MINUTE_TOP_GRAY as f32 + (255.0 - MINUTE_TOP_GRAY as f32) * t).round() as u8;
    (g, g, g)
}

/// Optical spacing for Pillar's "1": it has a thin flag sticking out at the top left, so its
/// stem starts well inside the glyph and the gap before it looks too wide. After the colon
/// (whose dots sit below the flag) the 1 slides left until its stem is at the normal gap; after
/// another digit it slides only part of the way, so the flag never touches that digit.
fn tuck_one(font: &ab_glyph::FontRef, prev: Option<char>, c: char, px: f32) -> f32 {
    use ab_glyph::{Font, ScaleFont};
    let Some(prev) = prev else { return 0.0 };
    if c != '1' {
        return 0.0;
    }
    let scale = ab_glyph::PxScale::from(px);
    let Some(one) = font.outline_glyph(font.glyph_id('1').with_scale(scale)) else { return 0.0 };
    // Where the stem starts: the first solid pixel halfway down the glyph.
    let b = one.px_bounds();
    let (w, mid) = (b.width().ceil() as usize + 1, (b.height() / 2.0) as u32);
    let mut row = vec![0.0f32; w];
    one.draw(|x, y, v| {
        if y == mid {
            row[x as usize] = v;
        }
    });
    let flag = row.iter().position(|&v| v > 0.5).unwrap_or(0) as f32;
    if prev == ':' {
        return -flag;
    }
    // The normal gap between the previous character's ink and the 1's ink.
    let scaled = font.as_scaled(scale);
    let prev_id = font.glyph_id(prev);
    let prev_right = font
        .outline_glyph(prev_id.with_scale(scale))
        .map(|o| scaled.h_advance(prev_id) - o.px_bounds().max.x)
        .unwrap_or(0.0);
    let gap = prev_right + b.min.x;
    -flag.min(gap * 0.5)
}

