#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window in release builds

//! Desktop widgets: bare layered windows that sit above the wallpaper and below
//! every other app. Each widget draws itself on the CPU into a bitmap and hands it
//! to Windows — no GPU, no UI framework.

mod anim;
mod battery;
mod clock;
mod config;
mod connectivity;
mod glass;
mod icons;
mod levels;
mod media;
mod mixer;
mod music;
mod render;
mod sliders;
mod stocks;
mod tiles;
mod wallpaper;
mod weather;
mod window;

use config::Config;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Result, w};

const KIND_CLOCK: isize = 1;
const KIND_WEATHER: isize = 2;
const KIND_MUSIC: isize = 3;
const KIND_NETWORK: isize = 4;
const KIND_BLUETOOTH: isize = 5;
const KIND_VOLUME: isize = 6;
const KIND_BRIGHTNESS: isize = 7;
const KIND_BATTERY: isize = 8;
const KIND_STOCKS: isize = 9;

/// Which slider a window is, if any.
fn slider_kind(hwnd: HWND) -> Option<sliders::Kind> {
    match window::kind(hwnd) {
        KIND_VOLUME => Some(sliders::Kind::Volume),
        KIND_BRIGHTNESS => Some(sliders::Kind::Brightness),
        _ => None,
    }
}

/// Which network/Bluetooth card a window is, if any.
fn tile_kind(hwnd: HWND) -> Option<tiles::Kind> {
    match window::kind(hwnd) {
        KIND_NETWORK => Some(tiles::Kind::Network),
        KIND_BLUETOOTH => Some(tiles::Kind::Bluetooth),
        _ => None,
    }
}

/// Mouse position from a mouse message's lparam (window coordinates).
fn mouse_xy(lparam: LPARAM) -> (i32, i32) {
    ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32)
}
/// Sent when the mouse leaves a window we asked to track (TrackMouseEvent).
const WM_MOUSELEAVE: u32 = 0x02A3;

const MENU_24H: usize = 1;
const MENU_SET_CITY: usize = 2;
const MENU_REFRESH: usize = 3;
const MENU_QUIT: usize = 9;

fn main() -> Result<()> {
    unsafe {
        // Work in real pixels so everything is crisp at any display scaling.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        Config::ensure_file();

        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("DesktopWidget");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        });

        let clock = window::create(instance, class, w!("Desktop Clock"), KIND_CLOCK)?;
        clock::redraw(clock, Config::load().clock_24h);
        window::show(clock);

        let weather = window::create(instance, class, w!("Desktop Weather"), KIND_WEATHER)?;
        weather::redraw(weather);
        window::show(weather);
        weather::start(weather);

        let music = window::create(instance, class, w!("Desktop Music"), KIND_MUSIC)?;
        music::set_anchor(weather);
        music::redraw(music);
        window::show(music);
        media::start(music);

        // Network and Bluetooth cards, stacked to the left of the weather card.
        tiles::set_anchor(weather);
        let network = window::create(instance, class, w!("Desktop Network"), KIND_NETWORK)?;
        let bluetooth = window::create(instance, class, w!("Desktop Bluetooth"), KIND_BLUETOOTH)?;
        tiles::register(tiles::Kind::Network, network);
        tiles::register(tiles::Kind::Bluetooth, bluetooth);
        connectivity::start(&[network, bluetooth]);
        tiles::redraw(tiles::Kind::Network, network);
        tiles::redraw(tiles::Kind::Bluetooth, bluetooth);
        window::show(network);
        window::show(bluetooth);

        // Volume and brightness sliders under the Bluetooth card, down to the music card's bottom.
        sliders::set_anchors(bluetooth, music);
        let volume = window::create(instance, class, w!("Desktop Volume"), KIND_VOLUME)?;
        let brightness = window::create(instance, class, w!("Desktop Brightness"), KIND_BRIGHTNESS)?;
        sliders::register(sliders::Kind::Volume, volume);
        sliders::register(sliders::Kind::Brightness, brightness);
        levels::start(&[volume, brightness]);
        mixer::start(volume);
        sliders::redraw(sliders::Kind::Volume, volume);
        sliders::redraw(sliders::Kind::Brightness, brightness);
        window::show(volume);
        window::show(brightness);

        // Battery pill under everything, from the volume pill's left edge to the music card's right.
        battery::set_anchor(music);
        let power = window::create(instance, class, w!("Desktop Battery"), KIND_BATTERY)?;
        battery::register(power);
        battery::redraw(power);
        window::show(power);

        // Stocks card under the battery pill, the same width.
        let market = window::create(instance, class, w!("Desktop Stocks"), KIND_STOCKS)?;
        stocks::register(market);
        stocks::start(market);
        stocks::redraw(market);
        window::show(market);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

fn redraw(hwnd: HWND) {
    match window::kind(hwnd) {
        KIND_CLOCK => clock::redraw(hwnd, Config::load().clock_24h),
        KIND_WEATHER => {
            weather::redraw(hwnd);
            // The weather card may have changed size: the cards around it follow.
            music::follow_anchor();
            tiles::follow_anchor();
            sliders::follow_anchor();
            battery::follow_anchor();
            stocks::follow_anchor();
        }
        KIND_MUSIC => music::redraw(hwnd),
        KIND_BATTERY => battery::redraw(hwnd),
        KIND_STOCKS => stocks::redraw(hwnd),
        _ => {
            if let Some(kind) = tile_kind(hwnd) {
                tiles::redraw(kind, hwnd);
            } else if let Some(kind) = slider_kind(hwnd) {
                sliders::redraw(kind, hwnd);
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if let Some(result) = window::handle_common(msg, lparam) {
        return result;
    }
    match msg {
        WM_TIMER if wparam.0 == clock::TIMER_ID => redraw(hwnd),
        WM_TIMER if wparam.0 == weather::GLASS_TIMER_ID => weather::glass_frame(hwnd, false),
        WM_TIMER if wparam.0 == music::TIMER_ID => music::tick(hwnd),
        WM_TIMER if wparam.0 == battery::TIMER_ID => battery::tick(hwnd),
        WM_TIMER if wparam.0 == stocks::TIMER_ID => stocks::tick(hwnd),
        stocks::WM_STOCKS_UPDATED => stocks::redraw(hwnd),
        // Plugged in / unplugged / battery level changed: update right away.
        WM_POWERBROADCAST if window::kind(hwnd) == KIND_BATTERY => {
            battery::tick(hwnd);
            return LRESULT(1);
        }
        weather::WM_WEATHER_UPDATED | media::WM_MUSIC_UPDATED => redraw(hwnd),
        WM_TIMER if wparam.0 == music::ANIM_TIMER_ID => music::redraw(hwnd),
        anim::WM_WIDGET_FRAME => match (window::kind(hwnd), tile_kind(hwnd)) {
            (KIND_MUSIC, _) => music::frame(hwnd),
            (_, Some(kind)) => tiles::frame(kind, hwnd),
            _ => match slider_kind(hwnd) {
                Some(kind) => sliders::frame(kind, hwnd),
                None => anim::frame_done(hwnd),
            },
        },
        levels::WM_LEVELS_UPDATED => redraw(hwnd),
        mixer::WM_MIXER_UPDATED => sliders::mixer_updated(hwnd),
        sliders::WM_MIXER_CLOSE => sliders::close_mixer(hwnd),
        WM_TIMER if wparam.0 == sliders::PREWARM_TIMER_ID => sliders::prewarm(hwnd),
        WM_TIMER if wparam.0 == sliders::TIMER_ID => {
            if let Some(kind) = slider_kind(hwnd) {
                sliders::tick(kind, hwnd);
            }
        }
        // Sliders: drag (relative), click the speaker to mute, scroll wheel, hand cursor.
        WM_LBUTTONDOWN if slider_kind(hwnd).is_some() => {
            let (x, y) = mouse_xy(lparam);
            sliders::press(slider_kind(hwnd).unwrap(), hwnd, x, y);
        }
        WM_MOUSEMOVE if slider_kind(hwnd).is_some() => {
            let (x, y) = mouse_xy(lparam);
            sliders::mouse_move(slider_kind(hwnd).unwrap(), hwnd, x, y);
        }
        WM_LBUTTONUP if slider_kind(hwnd).is_some() => {
            let (x, y) = mouse_xy(lparam);
            sliders::release(slider_kind(hwnd).unwrap(), hwnd, x, y);
        }
        WM_CAPTURECHANGED if slider_kind(hwnd).is_some() => sliders::cancel(slider_kind(hwnd).unwrap(), hwnd),
        WM_MOUSEWHEEL if slider_kind(hwnd).is_some() => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16;
            sliders::wheel(slider_kind(hwnd).unwrap(), hwnd, delta);
        }
        WM_SETCURSOR if slider_kind(hwnd).is_some() => unsafe {
            if sliders::over_control(slider_kind(hwnd).unwrap()) {
                if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                    SetCursor(Some(hand));
                }
                return LRESULT(1);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        },
        connectivity::WM_CONNECTIVITY_UPDATED => redraw(hwnd),
        WM_TIMER if wparam.0 == tiles::TIMER_ID => {
            if let Some(kind) = tile_kind(hwnd) {
                tiles::tick(kind, hwnd);
            }
        }
        // Network / Bluetooth cards: press, click, cancelled press, hand cursor.
        WM_LBUTTONDOWN if tile_kind(hwnd).is_some() => {
            let (x, y) = mouse_xy(lparam);
            tiles::press(tile_kind(hwnd).unwrap(), hwnd, x, y);
        }
        WM_LBUTTONUP if tile_kind(hwnd).is_some() => {
            let (x, y) = mouse_xy(lparam);
            tiles::release(tile_kind(hwnd).unwrap(), hwnd, x, y);
        }
        WM_CAPTURECHANGED if tile_kind(hwnd).is_some() => tiles::cancel_press(tile_kind(hwnd).unwrap(), hwnd),
        WM_SETCURSOR if tile_kind(hwnd).is_some() => unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            if tiles::over_control(tile_kind(hwnd).unwrap(), pt.x - r.left, pt.y - r.top) {
                if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                    SetCursor(Some(hand));
                }
                return LRESULT(1);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        },
        // Presses and clicks on the music card's buttons (coordinates are relative to the window).
        WM_LBUTTONDOWN if window::kind(hwnd) == KIND_MUSIC => {
            let (x, y) = ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32);
            music::press(hwnd, x, y);
        }
        WM_CAPTURECHANGED if window::kind(hwnd) == KIND_MUSIC => music::cancel_press(hwnd),
        WM_MOUSEMOVE if window::kind(hwnd) == KIND_MUSIC => {
            let (x, y) = ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32);
            music::mouse_move(hwnd, x, y);
        }
        WM_MOUSELEAVE if window::kind(hwnd) == KIND_MUSIC => music::mouse_leave(hwnd),
        // A hand cursor over the music card's buttons and scrubber.
        WM_SETCURSOR if window::kind(hwnd) == KIND_MUSIC => unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            if music::over_control(pt.x - r.left, pt.y - r.top) {
                if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                    SetCursor(Some(hand));
                }
                return LRESULT(1);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        },
        WM_LBUTTONUP if window::kind(hwnd) == KIND_MUSIC => {
            let (x, y) = ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32);
            music::release(hwnd, x, y);
        }
        // Screen resolution or scaling changed: re-size and re-position.
        WM_DISPLAYCHANGE | WM_DPICHANGED => redraw(hwnd),
        WM_RBUTTONUP => show_menu(hwnd),
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

fn show_menu(hwnd: HWND) {
    let choice = match window::kind(hwnd) {
        KIND_CLOCK => {
            let cfg = Config::load();
            window::popup_menu(hwnd, &[(MENU_24H, "24-hour", cfg.clock_24h), (0, "", false), (MENU_QUIT, "Quit", false)])
        }
        KIND_WEATHER => window::popup_menu(
            hwnd,
            &[
                (MENU_SET_CITY, "Set city…", false),
                (MENU_REFRESH, "Refresh now", false),
                (0, "", false),
                (MENU_QUIT, "Quit", false),
            ],
        ),
        KIND_MUSIC | KIND_NETWORK | KIND_BLUETOOTH | KIND_VOLUME | KIND_BRIGHTNESS | KIND_BATTERY | KIND_STOCKS => {
            window::popup_menu(hwnd, &[(MENU_QUIT, "Quit", false)])
        }
        _ => 0,
    };
    match choice {
        MENU_24H => {
            let mut cfg = Config::load();
            cfg.clock_24h = !cfg.clock_24h;
            cfg.save();
            redraw(hwnd);
        }
        MENU_SET_CITY => weather::edit_city(),
        MENU_REFRESH => weather::refresh(),
        MENU_QUIT => unsafe { PostQuitMessage(0) },
        _ => {}
    }
}
