//! Stocks: a glass card under the battery pill, the same width, in the style of Apple's Stocks
//! widget. One row per symbol (the S&P 500, NVIDIA and the five biggest US companies): ticker
//! and name on the left, price and a green/red change pill on the right.
//!
//! Prices come from Yahoo Finance's public chart endpoint, read on a background thread: every
//! 5 minutes while the US market is open, once more just after it closes (for the closing
//! prices), and not at all while it's closed. The thread only looks at the clock once a
//! minute in between, and the card is redrawn only when something it shows changes.

use crate::battery;
use crate::glass;
use crate::render::{self, Bitmap};
use crate::tiles::fill;
use crate::window;
use chrono::{Datelike, NaiveDate, TimeZone, Timelike, Utc};
use serde::Deserialize;
use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SetTimer, WM_APP};

/// Posted to the stocks window when prices or the market's open/closed state change.
pub const WM_STOCKS_UPDATED: u32 = WM_APP + 8;
/// Timer for noticing wallpaper changes and the cards above moving.
pub const TIMER_ID: usize = 11;
const TICK_MS: u32 = 2000;

/// (Yahoo symbol, ticker shown, name shown).
const SYMBOLS: [(&str, &str, &str); 7] = [
    ("%5EGSPC", "SPX", "S&P 500"),
    ("NVDA", "NVDA", "NVIDIA"),
    ("AAPL", "AAPL", "Apple"),
    ("MSFT", "MSFT", "Microsoft"),
    ("AMZN", "AMZN", "Amazon"),
    ("GOOGL", "GOOGL", "Alphabet"),
    ("META", "META", "Meta Platforms"),
];

/// How often to refresh while the market is open, and how soon to retry after a failure.
const REFRESH_OPEN: Duration = Duration::from_secs(5 * 60);
const RETRY: Duration = Duration::from_secs(60);
/// In trading hours with no known session (holiday, or stale session info): ask this often.
const RECHECK: Duration = Duration::from_secs(30 * 60);
/// How often the thread checks the clock between refreshes (no network involved).
const CLOCK_CHECK: Duration = Duration::from_secs(60);

/// Gap above the card (same as between the other cards), as a fraction of the screen height.
const GAP_FRACTION: f32 = 0.012;
/// Corner radius and inner padding, as fractions of the screen height (as the music card).
const RADIUS_FRACTION: f32 = 0.022;
const PAD_FRACTION: f32 = 0.016;
/// Header (title line) and row heights, as fractions of the screen height.
const HEADER_FRACTION: f32 = 0.026;
const ROW_FRACTION: f32 = 0.034;
/// Text sizes (digit height), as fractions of the screen height.
const TITLE_FRACTION: f32 = 0.0102;
const STATUS_FRACTION: f32 = 0.0082;
const TICKER_FRACTION: f32 = 0.0092;
const NAME_FRACTION: f32 = 0.0074;
const PRICE_FRACTION: f32 = 0.0092;
const CHANGE_FRACTION: f32 = 0.0078;
/// Change pill size and corner radius, as fractions of the screen height.
const PILL_W_FRACTION: f32 = 0.054;
const PILL_H_FRACTION: f32 = 0.0205;
const PILL_RADIUS_FRACTION: f32 = 0.0055;
/// Opacity of secondary text and of the hairlines between rows.
const SECONDARY: f32 = 0.62;
const HAIRLINE: f32 = 0.1;

const WHITE: (f32, f32, f32) = (255.0, 255.0, 255.0);
const GREEN: (f32, f32, f32) = (48.0, 209.0, 88.0);
const RED: (f32, f32, f32) = (255.0, 69.0, 58.0);

#[derive(Clone, Copy, PartialEq)]
struct Quote {
    price: f64,
    /// Change since the previous close, in percent.
    change: f64,
}

#[derive(Clone, Copy, PartialEq, Default)]
struct Market {
    quotes: [Option<Quote>; 7],
    open: bool,
}

static MARKET: Mutex<Market> = Mutex::new(Market { quotes: [None; 7], open: false });
static HWND_SELF: AtomicIsize = AtomicIsize::new(0);

struct State {
    glass: Option<glass::Glass>,
    /// What's on screen, so redraws can be skipped when nothing changed.
    shown: Option<(Market, (i32, i32, i32, i32))>,
    content: Option<(Market, (i32, i32), Bitmap)>,
}

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State { glass: None, shown: None, content: None }) };
}

pub fn register(hwnd: HWND) {
    HWND_SELF.store(hwnd.0 as isize, Ordering::Relaxed);
}

/// Starts the price thread; it posts WM_STOCKS_UPDATED to `hwnd` when something changes.
pub fn start(hwnd: HWND) {
    let raw = hwnd.0 as usize;
    std::thread::spawn(move || price_thread(raw));
}

/// Redraws after the cards above moved or changed size.
pub fn follow_anchor() {
    let raw = HWND_SELF.load(Ordering::Relaxed);
    if raw != 0 {
        redraw(HWND(raw as *mut c_void));
    }
}

/// Where the card sits: x, y, width, height.
fn geometry() -> Option<(i32, i32, i32, i32)> {
    let shf = window::screen_size().1 as f32;
    let (bx, by, bw, bh) = battery::rect()?;
    let h = (shf * (2.0 * PAD_FRACTION + HEADER_FRACTION + ROW_FRACTION * SYMBOLS.len() as f32)).round() as i32;
    Some((bx, by + bh + (shf * GAP_FRACTION) as i32, bw, h))
}

pub fn redraw(hwnd: HWND) {
    let shf = window::screen_size().1 as f32;
    let Some(geom) = geometry() else { return };
    let (x, y, w, h) = geom;
    let market = *MARKET.lock().unwrap();
    STATE.with(|st| {
        let mut st = st.borrow_mut();
        // The text only needs drawing again when the prices or the size changed.
        if !st.content.as_ref().is_some_and(|(m, size, _)| *m == market && *size == (w, h)) {
            st.content = Some((market, (w, h), draw(&market, w, h, shf)));
        }
        let mut glass = match st.glass.take() {
            Some(g) if g.size() == (w, h) => g,
            _ => glass::Glass::new(w, h, shf * RADIUS_FRACTION, shf),
        };
        let content = &st.content.as_ref().expect("drawn above").2;
        if let Some(bmp) = glass.render(content, 0, (x, y), true) {
            if let Err(e) = window::present(hwnd, &bmp, POINT { x, y }) {
                eprintln!("could not draw stocks: {e}");
            }
        }
        st.glass = Some(glass);
        st.shown = Some((market, geom));
    });
    unsafe {
        SetTimer(Some(hwnd), TIMER_ID, TICK_MS, None);
    }
}

/// Redraws if the cards above moved or the wallpaper behind the glass changed.
pub fn tick(hwnd: HWND) {
    let changed = STATE.with(|st| {
        let mut st = st.borrow_mut();
        let Some((_, geom)) = st.shown else { return true };
        if Some(geom) != geometry() {
            return true;
        }
        st.glass.as_mut().is_some_and(|g| g.render(&Bitmap::new(1, 1), 0, (geom.0, geom.1), false).is_some())
    });
    if changed {
        redraw(hwnd);
    }
}

// ---------- prices ----------

#[derive(Deserialize)]
struct ChartResponse {
    chart: ChartBody,
}

#[derive(Deserialize)]
struct ChartBody {
    result: Option<Vec<ChartResult>>,
}

#[derive(Deserialize)]
struct ChartResult {
    meta: Meta,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    regular_market_price: f64,
    regular_market_change_percent: Option<f64>,
    chart_previous_close: Option<f64>,
    current_trading_period: Option<TradingPeriods>,
}

#[derive(Deserialize)]
struct TradingPeriods {
    regular: Period,
}

#[derive(Deserialize, Clone, Copy)]
struct Period {
    start: i64,
    end: i64,
}

fn fetch(agent: &ureq::Agent, symbol: &str) -> Option<(Quote, Option<Period>)> {
    let r: ChartResponse = agent
        .get(format!("https://query1.finance.yahoo.com/v8/finance/chart/{symbol}?range=1d&interval=1d"))
        .header("User-Agent", "Mozilla/5.0")
        .call()
        .and_then(|mut r| r.body_mut().read_json())
        .ok()?;
    let meta = r.chart.result?.into_iter().next()?.meta;
    let price = meta.regular_market_price;
    let change = meta.regular_market_change_percent.or_else(|| {
        meta.chart_previous_close.filter(|&p| p > 0.0).map(|p| (price / p - 1.0) * 100.0)
    })?;
    Some((Quote { price, change }, meta.current_trading_period.map(|p| p.regular)))
}

fn price_thread(hwnd_raw: usize) {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).build().into();
    // Today's regular session as Yahoo reports it (knows about holidays and early closes).
    let mut session: Option<Period> = None;
    // When we last fetched successfully, and whether the market was open then.
    let mut last: Option<(Instant, bool)> = None;
    let mut retry_at: Option<Instant> = None;
    loop {
        let open = market_open(session);
        let due = match last {
            None => true,
            Some((at, was_open)) => {
                let age = at.elapsed();
                // While open: every few minutes. Just after closing: once more for the closing
                // prices. In trading hours but not in a known session (a holiday, or yesterday's
                // session info): check now and then until a new session shows up.
                ((open || was_open) && age >= REFRESH_OPEN) || (trading_hours() && !open && age >= RECHECK)
            }
        };
        let mut market = *MARKET.lock().unwrap();
        if due && retry_at.is_none_or(|t| Instant::now() >= t) {
            let mut any = false;
            for (i, (symbol, _, _)) in SYMBOLS.iter().enumerate() {
                if let Some((quote, period)) = fetch(&agent, symbol) {
                    market.quotes[i] = Some(quote);
                    if i == 0 {
                        session = period;
                    }
                    any = true;
                }
            }
            if any {
                last = Some((Instant::now(), market_open(session)));
                retry_at = None;
            } else {
                retry_at = Some(Instant::now() + RETRY);
            }
        }
        market.open = market_open(session);
        let changed = {
            let mut shown = MARKET.lock().unwrap();
            let changed = *shown != market;
            *shown = market;
            changed
        };
        if changed {
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd_raw as *mut c_void)), WM_STOCKS_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
        std::thread::sleep(CLOCK_CHECK);
    }
}

/// Weekdays 9:30–16:00 New York time.
fn trading_hours() -> bool {
    let now = Utc::now();
    let ny = now + chrono::Duration::hours(new_york_offset(now));
    let minutes = ny.hour() * 60 + ny.minute();
    ny.weekday().num_days_from_monday() < 5 && (9 * 60 + 30..16 * 60).contains(&minutes)
}

/// Whether the US stock market is open now: in trading hours, and inside the session Yahoo
/// last reported (which knows about holidays and early closes).
fn market_open(session: Option<Period>) -> bool {
    let now = Utc::now().timestamp();
    trading_hours() && session.is_none_or(|p| (p.start..p.end).contains(&now))
}

/// New York's offset from UTC in hours: -4 in daylight saving time (second Sunday of March to
/// the first Sunday of November, switching at 2 am local), -5 otherwise.
fn new_york_offset(now: chrono::DateTime<Utc>) -> i64 {
    let year = now.year();
    let sunday = |month: u32, nth: u32| {
        let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid date");
        let to_sunday = (7 - first.weekday().num_days_from_sunday()) % 7;
        first + chrono::Duration::days((to_sunday + 7 * (nth - 1)) as i64)
    };
    let start = Utc.from_utc_datetime(&sunday(3, 2).and_hms_opt(7, 0, 0).expect("valid time")); // 2 am EST
    let end = Utc.from_utc_datetime(&sunday(11, 1).and_hms_opt(6, 0, 0).expect("valid time")); // 2 am EDT
    if now >= start && now < end { -4 } else { -5 }
}

// ---------- drawing ----------

/// "7416.59" → "7,416.59".
fn money(v: f64) -> String {
    let s = format!("{v:.2}");
    let (int, frac) = s.split_once('.').unwrap_or((&s, "00"));
    let (sign, digits) = int.strip_prefix('-').map(|d| ("-", d)).unwrap_or(("", int));
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{sign}{grouped}.{frac}")
}

fn draw(m: &Market, w: i32, h: i32, sh: f32) -> Bitmap {
    let mut out = Bitmap::new(w, h);
    let wf = w as f32;
    let pad = sh * PAD_FRACTION;
    let right = wf - pad;

    // Header: "Stocks" on the left, the market's state on the right, both centred on one line.
    let header_cy = pad + sh * HEADER_FRACTION * 0.4;
    let cap = sh * TITLE_FRACTION;
    let baseline = (header_cy + cap / 2.0).round() as i32;
    let title = render::line(&render::sf_bold(), "Stocks", cap, 1.0, render::WHITE);
    out.blit(&title, pad.round() as i32, baseline - title.baseline);
    let mut status = render::line(
        &render::sf(),
        if m.open { "Market Open" } else { "Market Closed" },
        sh * STATUS_FRACTION,
        1.0,
        render::WHITE,
    );
    status.fade(SECONDARY);
    out.blit(&status, right.round() as i32 - status.width, baseline - status.baseline);

    let row_h = sh * ROW_FRACTION;
    let first = pad + sh * HEADER_FRACTION;
    let pill_w = sh * PILL_W_FRACTION;
    let pill_h = sh * PILL_H_FRACTION;
    let pill_r = sh * PILL_RADIUS_FRACTION;
    for (i, (_, ticker, name)) in SYMBOLS.iter().enumerate() {
        let top = first + i as f32 * row_h;
        let cy = top + row_h / 2.0;
        // Hairline above every row.
        let y = top.round();
        fill(&mut out, (pad, y - 1.0, right, y + 2.0), |p| (p.1 - (y + 0.5)).abs() - 0.5, WHITE, HAIRLINE);

        // Ticker over name.
        let t_h = sh * TICKER_FRACTION;
        let n_h = sh * NAME_FRACTION;
        let gap = sh * 0.0042;
        let block = t_h + gap + n_h;
        let t_base = (cy - block / 2.0 + t_h).round() as i32;
        let t = render::line(&render::sf_bold(), ticker, t_h, 1.0, render::WHITE);
        out.blit(&t, pad.round() as i32, t_base - t.baseline);
        let mut n = render::line(&render::sf(), name, n_h, 1.0, render::WHITE);
        n.fade(SECONDARY);
        let n_base = (t_base as f32 + gap + n_h).round() as i32;
        out.blit(&n, pad.round() as i32, n_base - n.baseline);

        // Change pill at the right, price just left of it.
        let quote = m.quotes[i];
        let (px0, px1) = (right - pill_w, right);
        let color = match quote {
            Some(q) if q.change < 0.0 => RED,
            Some(_) => GREEN,
            None => WHITE,
        };
        let alpha = if quote.is_some() { 1.0 } else { 0.16 };
        let centre = ((px0 + px1) / 2.0, cy);
        fill(&mut out, (px0, cy - pill_h / 2.0, px1, cy + pill_h / 2.0), |p| rbox(p, centre, (pill_w / 2.0, pill_h / 2.0), pill_r), color, alpha);
        let change_text = quote.map(|q| format!("{}{:.2}%", if q.change < 0.0 { "−" } else { "+" }, q.change.abs()));
        let c_h = sh * CHANGE_FRACTION;
        let c_base = (cy + c_h / 2.0).round() as i32;
        let c = render::line(&render::sf_medium(), change_text.as_deref().unwrap_or("—"), c_h, 1.0, render::WHITE);
        out.blit_over(&c, (centre.0 - c.width as f32 / 2.0).round() as i32, c_base - c.baseline);

        let p_h = sh * PRICE_FRACTION;
        let p_base = (cy + p_h / 2.0).round() as i32;
        let price_text = quote.map(|q| money(q.price)).unwrap_or_else(|| "—".into());
        let p = render::line(&render::sf_medium(), &price_text, p_h, 1.0, render::WHITE);
        out.blit(&p, (px0 - sh * 0.01).round() as i32 - p.width, p_base - p.baseline);
    }
    out
}

/// Signed distance to a rounded box at `c` with half-size `half` and corner radius `r`.
fn rbox(p: (f32, f32), c: (f32, f32), half: (f32, f32), r: f32) -> f32 {
    let r = r.min(half.0).min(half.1);
    let q = ((p.0 - c.0).abs() - half.0 + r, (p.1 - c.1).abs() - half.1 + r);
    (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt() + q.0.max(q.1).min(0.0) - r
}
