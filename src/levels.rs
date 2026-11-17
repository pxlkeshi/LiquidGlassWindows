//! System volume and screen brightness: reading them, following outside changes (keyboard
//! keys, Windows' flyouts), and setting them from the sliders.
//!
//! Each runs on its own background thread, so a slow call never stalls an animation. While
//! a slider is dragged only the latest value is applied (intermediate ones are skipped).

use std::ffi::c_void;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoSetProxyBlanket,
    EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Wmi::{
    IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY,
    WBEM_GENERIC_FLAG_TYPE, WBEM_INFINITE, WbemLocator,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::Win32::System::Variant::{VARENUM, VARIANT, VT_BSTR, VT_I4, VT_UI1, VariantClear};
use windows::core::{BSTR, w};

/// Posted to the slider windows when the volume or brightness changes.
pub const WM_LEVELS_UPDATED: u32 = WM_APP + 5;

#[derive(Clone, Copy, PartialEq, Default)]
pub struct Levels {
    /// 0..1, or None if there's no audio output.
    pub volume: Option<f32>,
    pub muted: bool,
    /// 0..1, or None if the screen's brightness can't be controlled.
    pub brightness: Option<f32>,
}

enum AudioRequest {
    Volume(f32),
    ToggleMute,
}

static LEVELS: Mutex<Levels> = Mutex::new(Levels { volume: None, muted: false, brightness: None });
static AUDIO: OnceLock<Sender<AudioRequest>> = OnceLock::new();
static BRIGHTNESS: OnceLock<Sender<f32>> = OnceLock::new();
static WINDOWS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

pub fn current() -> Levels {
    *LEVELS.lock().unwrap()
}

pub fn set_volume(v: f32) {
    if let Some(tx) = AUDIO.get() {
        let _ = tx.send(AudioRequest::Volume(v.clamp(0.0, 1.0)));
    }
}

pub fn toggle_mute() {
    if let Some(tx) = AUDIO.get() {
        let _ = tx.send(AudioRequest::ToggleMute);
    }
}

pub fn set_brightness(v: f32) {
    if let Some(tx) = BRIGHTNESS.get() {
        let _ = tx.send(v.clamp(0.0, 1.0));
    }
}

/// Starts both background threads; `windows` get WM_LEVELS_UPDATED on changes.
pub fn start(windows: &[HWND]) {
    *WINDOWS.lock().unwrap() = windows.iter().map(|h| h.0 as usize).collect();
    let (tx, rx) = channel();
    let _ = AUDIO.set(tx);
    std::thread::spawn(move || audio_thread(rx));
    let (tx, rx) = channel();
    let _ = BRIGHTNESS.set(tx);
    std::thread::spawn(move || brightness_thread(rx));
}

/// Stores new values and tells the windows, if anything actually changed.
fn publish(update: impl FnOnce(&mut Levels)) {
    let changed = {
        let mut l = LEVELS.lock().unwrap();
        let before = *l;
        update(&mut l);
        *l != before
    };
    if changed {
        for &h in WINDOWS.lock().unwrap().iter() {
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut c_void)), WM_LEVELS_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
    }
}

// ---------- volume ----------

fn audio_thread(rx: Receiver<AudioRequest>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut endpoint: Option<IAudioEndpointVolume> = None;
    let mut acquired = Instant::now() - Duration::from_secs(60);
    loop {
        // Re-find the default output now and then (headphones plugged in, device switched).
        if endpoint.is_none() || acquired.elapsed() > Duration::from_secs(2) {
            endpoint = default_endpoint();
            acquired = Instant::now();
        }
        match &endpoint {
            Some(ep) => unsafe {
                match (ep.GetMasterVolumeLevelScalar(), ep.GetMute()) {
                    (Ok(v), Ok(m)) => publish(|l| {
                        l.volume = Some((v * 1000.0).round() / 1000.0);
                        l.muted = m.as_bool();
                    }),
                    _ => endpoint = None,
                }
            },
            None => publish(|l| l.volume = None),
        }

        // Follow outside changes quickly (~6 times a second); act on slider requests at once.
        let mut request = match rx.recv_timeout(Duration::from_millis(160)) {
            Ok(r) => Some(r),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut toggles = 0;
        let mut volume = None;
        while let Some(r) = request {
            match r {
                AudioRequest::Volume(v) => volume = Some(v), // only the latest matters
                AudioRequest::ToggleMute => toggles += 1,
            }
            request = rx.try_recv().ok();
        }
        if let Some(ep) = &endpoint {
            unsafe {
                if let Some(v) = volume {
                    let _ = ep.SetMasterVolumeLevelScalar(v, std::ptr::null());
                    if v > 0.0 && ep.GetMute().is_ok_and(|m| m.as_bool()) {
                        let _ = ep.SetMute(false, std::ptr::null()); // raising the volume unmutes, like Windows
                    }
                }
                if toggles % 2 == 1 {
                    if let Ok(m) = ep.GetMute() {
                        let _ = ep.SetMute(!m.as_bool(), std::ptr::null());
                    }
                }
            }
        }
    }
}

fn default_endpoint() -> Option<IAudioEndpointVolume> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
        device.Activate(CLSCTX_ALL, None).ok()
    }
}

// ---------- brightness ----------

fn brightness_thread(rx: Receiver<f32>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let Some(wmi) = connect_wmi() else {
        publish(|l| l.brightness = None);
        return;
    };
    loop {
        match read_brightness(&wmi) {
            Some(b) => publish(|l| l.brightness = Some(b)),
            None => publish(|l| l.brightness = None),
        }
        // Outside changes (brightness keys) are picked up twice a second; slider requests at once.
        let mut wanted = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(v) => Some(v),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        while let Some(v) = wanted {
            // Skip to the newest value: setting brightness takes a moment.
            let mut latest = v;
            while let Ok(newer) = rx.try_recv() {
                latest = newer;
            }
            set_brightness_wmi(&wmi, latest);
            wanted = rx.try_recv().ok();
        }
    }
}

pub(crate) fn connect_wmi() -> Option<IWbemServices> {
    unsafe {
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
        let services = locator
            .ConnectServer(&BSTR::from("ROOT\\WMI"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None)
            .ok()?;
        CoSetProxyBlanket(
            &services,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            None,
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
        .ok()?;
        Some(services)
    }
}

/// The first object a WMI query returns.
pub(crate) fn query_first(wmi: &IWbemServices, query: &str) -> Option<IWbemClassObject> {
    unsafe {
        let results = wmi
            .ExecQuery(&BSTR::from("WQL"), &BSTR::from(query), WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY, None)
            .ok()?;
        let mut objects = [None];
        let mut returned = 0u32;
        let _ = results.Next(WBEM_INFINITE, &mut objects, &mut returned);
        if returned == 0 {
            return None;
        }
        objects[0].take()
    }
}

fn read_brightness(wmi: &IWbemServices) -> Option<f32> {
    let obj = query_first(wmi, "SELECT CurrentBrightness FROM WmiMonitorBrightness WHERE Active=TRUE")?;
    let mut value = VARIANT::default();
    unsafe { obj.Get(w!("CurrentBrightness"), 0, &mut value, None, None).ok()? };
    let level = variant_number(&value)?;
    Some(level as f32 / 100.0)
}

fn set_brightness_wmi(wmi: &IWbemServices, level: f32) {
    let percent = (level * 100.0).round().clamp(0.0, 100.0) as u8;
    unsafe {
        let Some(instance) = query_first(wmi, "SELECT * FROM WmiMonitorBrightnessMethods WHERE Active=TRUE") else {
            return;
        };
        let mut path = VARIANT::default();
        if instance.Get(w!("__PATH"), 0, &mut path, None, None).is_err() {
            return;
        }
        let Some(path_str) = variant_string(&path) else { return };
        let path = BSTR::from(path_str);
        let mut class = None;
        if wmi.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), WBEM_GENERIC_FLAG_TYPE(0), None, Some(&mut class), None).is_err() {
            return;
        }
        let Some(class) = class else { return };
        let mut params_def = None;
        if class.GetMethod(w!("WmiSetBrightness"), 0, &mut params_def, std::ptr::null_mut()).is_err() {
            return;
        }
        let Some(params_def) = params_def else { return };
        let Ok(params) = params_def.SpawnInstance(0) else { return };
        let _ = params.Put(w!("Timeout"), 0, &variant_i32(0), 0);
        let _ = params.Put(w!("Brightness"), 0, &variant_u8(percent), 0);
        let _ = wmi.ExecMethod(&path, &BSTR::from("WmiSetBrightness"), WBEM_GENERIC_FLAG_TYPE(0), None, &params, None, None);
    }
}

// ---------- VARIANT helpers (WMI hands values over in this tagged union) ----------

/// A small whole number from a VARIANT (WMI reports brightness as a byte, sometimes as an int).
pub(crate) fn variant_number(v: &VARIANT) -> Option<u32> {
    unsafe {
        let inner = &v.Anonymous.Anonymous;
        match inner.vt {
            VT_UI1 => Some(inner.Anonymous.bVal as u32),
            VT_I4 => Some(inner.Anonymous.lVal.max(0) as u32),
            VARENUM(18) => Some(inner.Anonymous.uiVal as u32), // VT_UI2
            VARENUM(19) => Some(inner.Anonymous.ulVal),        // VT_UI4
            VARENUM(2) => Some(inner.Anonymous.iVal.max(0) as u32), // VT_I2
            _ => None,
        }
    }
}

pub(crate) fn variant_string(v: &VARIANT) -> Option<String> {
    unsafe {
        let inner = &v.Anonymous.Anonymous;
        (inner.vt == VT_BSTR).then(|| inner.Anonymous.bstrVal.to_string())
    }
}

fn variant_i32(value: i32) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_I4;
        inner.Anonymous.lVal = value;
    }
    v
}

fn variant_u8(value: u8) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_UI1;
        inner.Anonymous.bVal = value;
    }
    v
}

#[allow(dead_code)]
fn clear(v: &mut VARIANT) {
    unsafe {
        let _ = VariantClear(v);
    }
}

