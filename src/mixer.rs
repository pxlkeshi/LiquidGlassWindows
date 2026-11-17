//! Per-app volume (the Windows volume mixer): which apps are playing sound, their names and
//! icons, and setting each one's volume or mute.
//!
//! A background thread lists the audio sessions of the default output device: about three
//! times a second while the mixer is open, every couple of seconds otherwise (so the list
//! is ready the moment it opens). Apps with several sessions (e.g. browsers) are grouped.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits,
    GetObjectW, HGDIOBJ,
};
use windows::Win32::Media::Audio::{
    AudioSessionStateExpired, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, ISimpleAudioVolume,
    MMDeviceEnumerator, eMultimedia, eRender,
};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO, PostMessageW, WM_APP};
use windows::core::{Interface, PCWSTR, PWSTR};

/// Posted to the volume window when the list of apps or their levels change.
pub const WM_MIXER_UPDATED: u32 = WM_APP + 6;

#[derive(Clone)]
pub struct App {
    /// Stable identity: the program's path (or "system" for system sounds).
    pub key: String,
    pub name: String,
    pub volume: f32,
    pub muted: bool,
    pub icon: Option<Arc<image::RgbaImage>>,
}

impl PartialEq for App {
    fn eq(&self, o: &App) -> bool {
        self.key == o.key && self.name == o.name && (self.volume - o.volume).abs() < 0.001 && self.muted == o.muted
    }
}

enum Request {
    Volume(String, f32),
    ToggleMute(String),
    Refresh,
}

static APPS: Mutex<Vec<App>> = Mutex::new(Vec::new());
static OPEN: AtomicBool = AtomicBool::new(false);
static REQUESTS: OnceLock<Sender<Request>> = OnceLock::new();
static WINDOW: Mutex<usize> = Mutex::new(0);

pub fn apps() -> Vec<App> {
    APPS.lock().unwrap().clone()
}

/// While open, the list is refreshed about three times a second.
pub fn set_open(open: bool) {
    let was = OPEN.swap(open, Ordering::Relaxed);
    if open && !was {
        send(Request::Refresh);
    }
}

pub fn set_volume(key: &str, v: f32) {
    send(Request::Volume(key.to_string(), v.clamp(0.0, 1.0)));
}

pub fn toggle_mute(key: &str) {
    send(Request::ToggleMute(key.to_string()));
}

fn send(r: Request) {
    if let Some(tx) = REQUESTS.get() {
        let _ = tx.send(r);
    }
}

pub fn start(volume_window: HWND) {
    *WINDOW.lock().unwrap() = volume_window.0 as usize;
    let (tx, rx) = channel();
    let _ = REQUESTS.set(tx);
    std::thread::spawn(move || worker(rx));
}

fn worker(rx: Receiver<Request>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut icons: HashMap<String, Option<Arc<image::RgbaImage>>> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
    loop {
        let sessions = list_sessions();
        let apps = group(&sessions, &mut icons, &mut names);
        let changed = {
            let mut a = APPS.lock().unwrap();
            let changed = *a != apps;
            *a = apps;
            changed
        };
        if changed {
            let h = *WINDOW.lock().unwrap();
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut c_void)), WM_MIXER_UPDATED, WPARAM(0), LPARAM(0));
            }
        }

        let wait = if OPEN.load(Ordering::Relaxed) { 300 } else { 2000 };
        let first = match rx.recv_timeout(Duration::from_millis(wait)) {
            Ok(r) => Some(r),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Apply everything queued; for volume only the latest value per app matters.
        let mut volumes: HashMap<String, f32> = HashMap::new();
        let mut toggles: HashMap<String, u32> = HashMap::new();
        let mut next = first;
        while let Some(r) = next {
            match r {
                Request::Volume(k, v) => {
                    volumes.insert(k, v);
                }
                Request::ToggleMute(k) => *toggles.entry(k).or_default() += 1,
                Request::Refresh => {}
            }
            next = rx.try_recv().ok();
        }
        if !volumes.is_empty() || !toggles.is_empty() {
            let sessions = list_sessions();
            for s in &sessions {
                unsafe {
                    if let Some(v) = volumes.get(&s.key) {
                        let _ = s.control.SetMasterVolume(*v, std::ptr::null());
                        if *v > 0.0 && s.muted {
                            let _ = s.control.SetMute(false, std::ptr::null());
                        }
                    }
                    if toggles.get(&s.key).is_some_and(|n| n % 2 == 1) {
                        let _ = s.control.SetMute(!s.muted, std::ptr::null());
                    }
                }
            }
        }
    }
}

struct Session {
    key: String,
    path: Option<String>,
    system: bool,
    volume: f32,
    muted: bool,
    control: ISimpleAudioVolume,
}

fn list_sessions() -> Vec<Session> {
    let mut out = Vec::new();
    unsafe {
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return out;
        };
        let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) else { return out };
        let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { return out };
        let Ok(list) = manager.GetSessionEnumerator() else { return out };
        let count = list.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(control) = list.GetSession(i) else { continue };
            if control.GetState().is_ok_and(|s| s == AudioSessionStateExpired) {
                continue;
            }
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
            let Ok(volume) = control.cast::<ISimpleAudioVolume>() else { continue };
            let system = control2.IsSystemSoundsSession().0 == 0; // S_OK
            let pid = control2.GetProcessId().unwrap_or(0);
            let path = if system || pid == 0 { None } else { process_path(pid) };
            // Windows' own audio engine process (hidden in Windows' mixer too).
            if path.as_deref().is_some_and(|p| p.to_lowercase().ends_with("audiodg.exe")) {
                continue;
            }
            let key = if system { "system".to_string() } else { path.clone().unwrap_or_else(|| format!("pid:{pid}")).to_lowercase() };
            out.push(Session {
                key,
                path,
                system,
                volume: volume.GetMasterVolume().unwrap_or(1.0),
                muted: volume.GetMute().map(|m| m.as_bool()).unwrap_or(false),
                control: volume,
            });
        }
    }
    out
}

/// One row per program (several sessions of the same program are grouped), system sounds last.
fn group(
    sessions: &[Session],
    icons: &mut HashMap<String, Option<Arc<image::RgbaImage>>>,
    names: &mut HashMap<String, String>,
) -> Vec<App> {
    let mut apps: Vec<App> = Vec::new();
    for s in sessions {
        if let Some(a) = apps.iter_mut().find(|a| a.key == s.key) {
            a.volume = a.volume.max(s.volume);
            a.muted &= s.muted;
            continue;
        }
        let (name, icon) = if s.system {
            ("System Sounds".to_string(), None)
        } else if let Some(path) = &s.path {
            let name = names.entry(path.clone()).or_insert_with(|| friendly_name(path)).clone();
            let icon = icons.entry(path.clone()).or_insert_with(|| exe_icon(path).map(Arc::new)).clone();
            (name, icon)
        } else {
            ("Unknown app".to_string(), None)
        };
        apps.push(App { key: s.key.clone(), name, volume: s.volume, muted: s.muted, icon });
    }
    apps.sort_by(|a, b| (a.key == "system").cmp(&(b.key == "system")).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    apps
}

fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(handle);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// The program's description from its file details ("Google Chrome"), or its file name.
fn friendly_name(path: &str) -> String {
    let stem = std::path::Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let fallback = {
        let mut c = stem.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or(stem.clone())
    };
    unsafe {
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return fallback;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, data.as_mut_ptr() as *mut c_void).is_err() {
            return fallback;
        }
        // The first language/codepage pair, then its FileDescription string.
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let query: Vec<u16> = "\\VarFileInfo\\Translation".encode_utf16().chain(Some(0)).collect();
        if !VerQueryValueW(data.as_ptr() as *const c_void, PCWSTR(query.as_ptr()), &mut ptr, &mut len).as_bool() || len < 4 {
            return fallback;
        }
        let lang = *(ptr as *const u16);
        let codepage = *(ptr as *const u16).add(1);
        let sub = format!("\\StringFileInfo\\{lang:04x}{codepage:04x}\\FileDescription");
        let query: Vec<u16> = sub.encode_utf16().chain(Some(0)).collect();
        if !VerQueryValueW(data.as_ptr() as *const c_void, PCWSTR(query.as_ptr()), &mut ptr, &mut len).as_bool() || len == 0 {
            return fallback;
        }
        let text = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        let s = String::from_utf16_lossy(&text[..end]).trim().to_string();
        if s.is_empty() { fallback } else { s }
    }
}

/// The program's icon as RGBA pixels.
fn exe_icon(path: &str) -> Option<image::RgbaImage> {
    unsafe {
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let mut large = HICON::default();
        if ExtractIconExW(PCWSTR(wide.as_ptr()), 0, Some(&mut large), None, 1) == 0 || large.is_invalid() {
            return None;
        }
        let img = icon_pixels(large);
        let _ = DestroyIcon(large);
        img
    }
}

unsafe fn icon_pixels(icon: HICON) -> Option<image::RgbaImage> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).ok()?;
        let color = info.hbmColor;
        let mut bm = BITMAP::default();
        let got = GetObjectW(HGDIOBJ(color.0), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void));
        let mut result = None;
        if got != 0 && bm.bmWidth > 0 && bm.bmHeight > 0 {
            let (w, h) = (bm.bmWidth, bm.bmHeight);
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut px = vec![0u8; (w * h * 4) as usize];
            let dc = CreateCompatibleDC(None);
            if GetDIBits(dc, color, 0, h as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS) != 0 {
                // BGRA -> RGBA. Old-style icons have no alpha: treat them as opaque.
                let has_alpha = px.chunks(4).any(|c| c[3] != 0);
                for c in px.chunks_mut(4) {
                    c.swap(0, 2);
                    if !has_alpha {
                        c[3] = 255;
                    }
                }
                result = image::RgbaImage::from_raw(w as u32, h as u32, px);
            }
            let _ = DeleteDC(dc);
        }
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        result
    }
}

