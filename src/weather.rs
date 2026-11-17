//! Minimal weather widget in the top-right corner: a solid sun / moon / cloud,
//! the temperature, and a one-word condition. Data from Open-Meteo (free, no key).

use crate::config::Config;
use crate::glass;
use crate::icons::{self, WeatherIcon as Icon};
use crate::render::{self, Bitmap};
use crate::window;
use serde::Deserialize;
use std::ffi::c_void;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SetTimer, WM_APP};

/// Posted to the weather window when new data has arrived.
pub const WM_WEATHER_UPDATED: u32 = WM_APP + 1;

const REFRESH_EVERY: Duration = Duration::from_secs(15 * 60);

/// Distance from the top-right screen corner, as a fraction of the screen height.
pub const MARGIN_FRACTION: f32 = 0.015;
/// Height of the temperature digits, as a fraction of the screen height.
pub const TEMP_HEIGHT_FRACTION: f32 = 0.034;
/// Height of the condition text's capitals, as a fraction of the screen height.
pub const LABEL_HEIGHT_FRACTION: f32 = 0.0112;
/// Height of the "H:29°  L:24°" line's digits, as a fraction of the screen height.
pub const RANGE_HEIGHT_FRACTION: f32 = 0.0096;
/// Opacity of the secondary "H/L" line.
pub const SECONDARY: f32 = 0.62;
/// The layout's spacing unit, as a fraction of the screen height: every gap is a multiple of it.
const UNIT: f32 = 0.008;
/// Padding between the weather and the edge of its glass card, as a fraction of the screen height.
pub const GLASS_PAD_FRACTION: f32 = 0.016;
/// Corner radius of the glass card, as a fraction of the screen height.
/// Corner radius as a share of half the card's height (1.0 = a full pill).
pub const CORNER_ROUNDNESS: f32 = 0.9;
/// Colour of everything on the weather card.
pub const INK: (u8, u8, u8) = render::WHITE;
/// Icon size relative to the temperature's height.
pub const ICON_SCALE: f32 = 1.5;

#[derive(Clone)]
enum State {
    Loading,
    NoCity,
    Failed(String),
    Ready { temp: f64, low: Option<f64>, high: Option<f64>, code: i64, is_day: bool },
}

static STATE: Mutex<State> = Mutex::new(State::Loading);
static REFRESH: OnceLock<Sender<()>> = OnceLock::new();

/// Starts the background thread that fetches the weather and tells `hwnd` when it changes.
pub fn start(hwnd: HWND) {
    let (tx, rx) = channel();
    let _ = REFRESH.set(tx);
    let hwnd_raw = hwnd.0 as usize; // HWND isn't Send; pass the raw value
    std::thread::spawn(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
        let mut place_cache: Option<(String, f64, f64)> = None;
        loop {
            let city = Config::load().city.trim().to_string();
            let next = if city.is_empty() {
                State::NoCity
            } else {
                match fetch(&agent, &city, &mut place_cache) {
                    Ok(s) => s,
                    // Keep showing the last good reading through brief outages.
                    Err(e) => match &*STATE.lock().unwrap() {
                        s @ State::Ready { .. } => s.clone(),
                        _ => State::Failed(e),
                    },
                }
            };
            *STATE.lock().unwrap() = next;
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd_raw as *mut c_void)), WM_WEATHER_UPDATED, WPARAM(0), LPARAM(0));
            }
            match rx.recv_timeout(REFRESH_EVERY) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// Fetches again right away (e.g. after the city was changed).
pub fn refresh() {
    if let Some(tx) = REFRESH.get() {
        let _ = tx.send(());
    }
}

/// Opens the settings file in Notepad and refreshes the weather once Notepad is closed.
pub fn edit_city() {
    Config::ensure_file();
    std::thread::spawn(|| {
        let _ = std::process::Command::new("notepad.exe").arg(Config::path()).status();
        refresh();
    });
}

pub fn redraw(hwnd: HWND) {
    let state = STATE.lock().unwrap().clone();
    let (sw, sh) = window::screen_size();
    let sh_f = sh as f32;
    let bmp = match state {
        State::Ready { temp, low, high, code, is_day } => {
            let (icon, label) = describe(code, is_day);
            let range = match (low, high) {
                (Some(l), Some(h)) => Some(format!("H:{}  L:{}", format_temp(h), format_temp(l))),
                _ => None,
            };
            compose(icon, &format_temp(temp), label, range.as_deref(), sh_f)
        }
        State::Loading => message("Loading weather…", sh_f),
        State::NoCity => message("Right-click to set your city", sh_f),
        State::Failed(e) => {
            eprintln!("weather: {e}");
            message(if e.contains("not found") { "City not found" } else { "Weather unavailable" }, sh_f)
        }
    };
    // The glass card's top-right corner sits `margin` from the screen corner.
    let margin = (sh_f * MARGIN_FRACTION) as i32;
    let pad = (sh_f * GLASS_PAD_FRACTION) as i32;
    let (cw, ch) = (bmp.width + 2 * pad, bmp.height + 2 * pad);
    let pos = POINT { x: sw - margin - cw, y: margin };

    LIVE.with(|live| {
        let mut live = live.borrow_mut();
        // The lens only needs rebuilding when the card changes size.
        let glass = match live.take() {
            Some(l) if l.glass.size() == (cw, ch) => l.glass,
            _ => glass::Glass::new(cw, ch, ch as f32 / 2.0 * CORNER_ROUNDNESS, sh_f),
        };
        *live = Some(Live { content: bmp, pad, pos, glass });
    });
    glass_frame(hwnd, true);
    unsafe {
        SetTimer(Some(hwnd), GLASS_TIMER_ID, GLASS_FRAME_MS, None);
    }
}

/// Timer id for checking whether the wallpaper changed, and how often to check.
pub const GLASS_TIMER_ID: usize = 2;
const GLASS_FRAME_MS: u32 = 2000;

/// What's needed to redraw the glass each frame: the weather content, where the card is, and its lens.
struct Live {
    content: Bitmap,
    pad: i32,
    pos: POINT,
    glass: glass::Glass,
}

thread_local! {
    static LIVE: std::cell::RefCell<Option<Live>> = const { std::cell::RefCell::new(None) };
}

/// Redraws the glass if the wallpaper changed (or always, when `force` is set).
pub fn glass_frame(hwnd: HWND, force: bool) {
    LIVE.with(|live| {
        let mut live = live.borrow_mut();
        let Some(l) = live.as_mut() else { return };
        if let Some(bmp) = l.glass.render(&l.content, l.pad, (l.pos.x, l.pos.y), force) {
            if let Err(e) = window::present(hwnd, &bmp, l.pos) {
                eprintln!("could not draw weather: {e}");
            }
        }
    });
}

fn format_temp(t: f64) -> String {
    let t = t.round();
    format!("{}°", if t == 0.0 { 0 } else { t as i64 }) // avoid "-0°"
}

/// WMO weather code → (icon, label).
fn describe(code: i64, is_day: bool) -> (Icon, &'static str) {
    let clear = if is_day { Icon::Sun } else { Icon::Moon };
    let partly = if is_day { Icon::PartlySunny } else { Icon::PartlyMoon };
    match code {
        0 => (clear, "Clear"),
        1 => (clear, "Mostly clear"),
        2 => (partly, "Partly cloudy"),
        3 => (Icon::Cloudy, "Cloudy"),
        45 | 48 => (Icon::Fog, "Foggy"),
        51..=57 => (Icon::Drizzle, "Drizzle"),
        61..=67 | 80..=82 => (Icon::Rain, "Rain"),
        71..=77 | 85 | 86 => (Icon::Snow, "Snow"),
        95..=99 => (Icon::Thunder, "Thunderstorm"),
        _ => (Icon::Cloudy, "Cloudy"),
    }
}

// ---------- drawing ----------

/// One refined row: icon, a large light temperature, and beside it the condition (medium
/// weight) over "H:29°  L:24°" (softer). The text block and the icon are centred on the
/// temperature's digits, and every gap is a multiple of one spacing unit.
fn compose(icon: Icon, temp: &str, label: &str, range: Option<&str>, sh: f32) -> Bitmap {
    let u = sh * UNIT;
    let temp_bmp = render::line(&render::sf_light(), temp, sh * TEMP_HEIGHT_FRACTION, 1.0, INK);
    let label_bmp = render::line(&render::sf_medium(), label, sh * LABEL_HEIGHT_FRACTION, 1.0, INK);
    let range_bmp = range.map(|t| {
        let mut b = render::line(&render::sf(), t, sh * RANGE_HEIGHT_FRACTION, 1.0, INK);
        b.fade(SECONDARY);
        b
    });

    // Vertical layout, with the top of the temperature's digits at y = 0. Spacing uses each
    // line's cap height (its baseline), so letters dipping below the line (y, g) don't push
    // things around.
    let digits = temp_bmp.baseline.max(1);
    let icon_size = (digits as f32 * ICON_SCALE).round() as i32;
    let icon_top = (digits - icon_size) / 2;
    let line_gap = (u * 0.75).round() as i32;
    let block_h = label_bmp.baseline + range_bmp.as_ref().map_or(0, |r| line_gap + r.baseline);
    let label_top = (digits - block_h) / 2;
    let range_top = label_top + label_bmp.baseline + line_gap;

    // Horizontal layout: a little extra room at the rounded ends, then icon, temperature, text.
    let side = u.round() as i32;
    let icon_x = side;
    let temp_x = icon_x + icon_size + (u * 1.5).round() as i32;
    let text_x = temp_x + temp_bmp.width + (u * 1.75).round() as i32;
    let text_w = label_bmp.width.max(range_bmp.as_ref().map_or(0, |r| r.width));
    let width = text_x + text_w + side;

    let top = 0.min(icon_top).min(label_top);
    let mut bottom = temp_bmp.height.max(icon_top + icon_size).max(label_top + label_bmp.height);
    if let Some(r) = &range_bmp {
        bottom = bottom.max(range_top + r.height);
    }
    let mut out = Bitmap::new(width, bottom - top);
    let dy = -top;
    out.blit(&icons::draw(icon, icon_size), icon_x, icon_top + dy);
    out.blit(&temp_bmp, temp_x, dy);
    out.blit(&label_bmp, text_x, label_top + dy);
    if let Some(r) = &range_bmp {
        out.blit(r, text_x, range_top + dy);
    }
    out
}

fn message(text: &str, sh: f32) -> Bitmap {
    let line = render::line(&render::sf(), text, sh * LABEL_HEIGHT_FRACTION, 1.0, INK);
    let mut out = Bitmap::new(line.width + 4, line.height + 4);
    out.blit(&line, 2, 2);
    out
}

// ---------- fetching ----------

#[derive(Deserialize)]
struct GeoResponse {
    results: Option<Vec<GeoPlace>>,
}
#[derive(Deserialize)]
struct GeoPlace {
    latitude: f64,
    longitude: f64,
}
#[derive(Deserialize)]
struct Forecast {
    current: Current,
    daily: Daily,
}
#[derive(Deserialize)]
struct Daily {
    temperature_2m_min: Vec<Option<f64>>,
    temperature_2m_max: Vec<Option<f64>>,
}
#[derive(Deserialize)]
struct Current {
    temperature_2m: f64,
    weather_code: i64,
    is_day: i64,
}

fn fetch(agent: &ureq::Agent, city: &str, cache: &mut Option<(String, f64, f64)>) -> Result<State, String> {
    let (lat, lon) = match cache {
        Some((c, lat, lon)) if c == city => (*lat, *lon),
        _ => {
            let geo: GeoResponse = agent
                .get("https://geocoding-api.open-meteo.com/v1/search")
                .query("name", city)
                .query("count", "1")
                .call()
                .and_then(|mut r| r.body_mut().read_json())
                .map_err(|e| format!("city lookup failed: {e}"))?;
            let place = geo
                .results
                .and_then(|v| v.into_iter().next())
                .ok_or_else(|| format!("city \"{city}\" not found"))?;
            *cache = Some((city.to_string(), place.latitude, place.longitude));
            (place.latitude, place.longitude)
        }
    };

    let f: Forecast = agent
        .get("https://api.open-meteo.com/v1/forecast")
        .query("latitude", lat.to_string())
        .query("longitude", lon.to_string())
        .query("current", "temperature_2m,weather_code,is_day")
        .query("daily", "temperature_2m_min,temperature_2m_max")
        .query("forecast_days", "1")
        .query("timezone", "auto") // "today" in the city's own time zone
        .query("temperature_unit", "celsius")
        .call()
        .and_then(|mut r| r.body_mut().read_json())
        .map_err(|e| format!("forecast failed: {e}"))?;

    Ok(State::Ready {
        temp: f.current.temperature_2m,
        low: f.daily.temperature_2m_min.first().copied().flatten(),
        high: f.daily.temperature_2m_max.first().copied().flatten(),
        code: f.current.weather_code,
        is_day: f.current.is_day == 1,
    })
}

