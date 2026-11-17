//! Reads the desktop wallpaper and maps it onto the screen the way Windows does,
//! so the glass can show (and bend) exactly what's behind it.

use image::{DynamicImage, GenericImageView};
use std::ffi::c_void;
use std::path::PathBuf;
use std::time::SystemTime;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};
use windows::core::{PCWSTR, w};

/// Identifies the wallpaper currently on screen; if this changes, the wallpaper changed.
#[derive(Clone, PartialEq, Default)]
pub struct Source {
    path: PathBuf,
    modified: Option<SystemTime>,
    fit: Fit,
}

impl Source {
    /// Cheap to call often: just a registry read and a file timestamp.
    pub fn current() -> Source {
        let path = image_path().unwrap_or_default();
        let modified = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        Source { path, modified, fit: Fit::current() }
    }

    /// The wallpaper pixels behind a screen rectangle, as RGB (0..255). Falls back to dark grey
    /// if the wallpaper can't be read.
    pub fn patch(&self, x: i32, y: i32, w: i32, h: i32) -> Vec<[f32; 3]> {
        let image = image::ImageReader::open(&self.path)
            .ok()
            .and_then(|r| r.with_guessed_format().ok())
            .and_then(|r| r.decode().ok());
        match image {
            Some(img) => self.map(&img, x, y, w, h),
            None => vec![[24.0; 3]; (w * h) as usize],
        }
    }

    fn map(&self, img: &DynamicImage, x: i32, y: i32, w: i32, h: i32) -> Vec<[f32; 3]> {
        let (sw, sh) = crate::window::screen_size();
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        let (sw, sh) = (sw as f32, sh as f32);
        let (sx, sy) = match self.fit {
            Fit::Fill => (sw / iw).max(sh / ih).pair(),
            Fit::Fit => (sw / iw).min(sh / ih).pair(),
            Fit::Stretch => (sw / iw, sh / ih),
            Fit::Center | Fit::Tile => (1.0, 1.0),
        };
        let (ox, oy) = match self.fit {
            Fit::Tile => (0.0, 0.0),
            _ => ((sw - iw * sx) / 2.0, (sh - ih * sy) / 2.0),
        };

        let mut out = Vec::with_capacity((w * h) as usize);
        for py in 0..h {
            for px in 0..w {
                // Clamp to the screen: the glass border can reach past the screen edge.
                let screen_x = ((x + px) as f32).clamp(0.0, sw - 1.0) + 0.5;
                let screen_y = ((y + py) as f32).clamp(0.0, sh - 1.0) + 0.5;
                let mut u = (screen_x - ox) / sx - 0.5;
                let mut v = (screen_y - oy) / sy - 0.5;
                if self.fit == Fit::Tile {
                    u = u.rem_euclid(iw);
                    v = v.rem_euclid(ih);
                }
                out.push(if u < -0.5 || v < -0.5 || u > iw - 0.5 || v > ih - 0.5 {
                    [0.0; 3] // outside the picture (Fit/Center): Windows shows the background colour
                } else {
                    bilinear(img, u, v)
                });
            }
        }
        out
    }
}

fn bilinear(img: &DynamicImage, u: f32, v: f32) -> [f32; 3] {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let (x0, y0) = (u.floor() as i32, v.floor() as i32);
    let (fx, fy) = (u - x0 as f32, v - y0 as f32);
    let at = |x: i32, y: i32| img.get_pixel(x.clamp(0, w - 1) as u32, y.clamp(0, h - 1) as u32).0;
    let (a, b, c, d) = (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1));
    let mut out = [0.0; 3];
    for i in 0..3 {
        let top = a[i] as f32 * (1.0 - fx) + b[i] as f32 * fx;
        let bottom = c[i] as f32 * (1.0 - fx) + d[i] as f32 * fx;
        out[i] = top * (1.0 - fy) + bottom * fy;
    }
    out
}

trait Pair {
    fn pair(self) -> (f32, f32);
}
impl Pair for f32 {
    fn pair(self) -> (f32, f32) {
        (self, self)
    }
}

/// The "Choose a fit for your desktop image" setting.
#[derive(Clone, Copy, PartialEq, Default)]
enum Fit {
    #[default]
    Fill,
    Fit,
    Stretch,
    Center,
    Tile,
}

impl Fit {
    fn current() -> Fit {
        if desktop_setting(w!("TileWallpaper")).as_deref() == Some("1") {
            return Fit::Tile;
        }
        match desktop_setting(w!("WallpaperStyle")).as_deref() {
            Some("6") => Fit::Fit,
            Some("2") => Fit::Stretch,
            Some("0") => Fit::Center,
            _ => Fit::Fill, // 10 = Fill, 22 = Span (same as Fill on one screen)
        }
    }
}

fn desktop_setting(name: PCWSTR) -> Option<String> {
    let mut buf = [0u16; 64];
    let mut size = (buf.len() * 2) as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Control Panel\\Desktop"),
            name,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if err.is_err() {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Windows keeps its own copy of the image actually on screen (this also covers slideshows
/// and Spotlight); fall back to the original file if that copy is missing.
fn image_path() -> Option<PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let cached = PathBuf::from(appdata).join("Microsoft\\Windows\\Themes\\TranscodedWallpaper");
        if cached.exists() {
            return Some(cached);
        }
    }
    let mut buf = [0u16; 1024];
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buf.len() as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    let path = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
    (ok.is_ok() && len > 0 && path.exists()).then_some(path)
}
