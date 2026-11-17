//! Frameless, transparent windows that live on the desktop: above the wallpaper,
//! below every app, never activated by clicks. Their whole appearance is a
//! premultiplied bitmap handed to Windows with UpdateLayeredWindow.

use crate::render::Bitmap;
use std::ffi::c_void;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

/// Creates a hidden desktop window; `kind` is stored on it so the window procedure knows which widget it is.
pub fn create(instance: HINSTANCE, class: PCWSTR, title: PCWSTR, kind: isize) -> Result<HWND> {
    unsafe {
        // Owning the window by the desktop shell (Progman) keeps it on the desktop
        // and stops Win+D from hiding it.
        let desktop = FindWindowW(w!("Progman"), None).ok();
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            title,
            WS_POPUP,
            0,
            0,
            1,
            1,
            desktop,
            None,
            Some(instance),
            None,
        )?;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, kind);
        Ok(hwnd)
    }
}

pub fn show(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

pub fn kind(hwnd: HWND) -> isize {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) }
}

pub fn screen_size() -> (i32, i32) {
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

/// Window messages every desktop widget handles the same way. Returns None for everything else.
pub fn handle_common(msg: u32, lparam: LPARAM) -> Option<LRESULT> {
    match msg {
        // Every z-order change puts us back at the bottom, under all other windows.
        WM_WINDOWPOSCHANGING => {
            let pos = unsafe { &mut *(lparam.0 as *mut WINDOWPOS) };
            if (pos.flags & SWP_NOZORDER).0 == 0 {
                pos.hwndInsertAfter = HWND_BOTTOM;
            }
            Some(LRESULT(0))
        }
        // Clicking a widget never activates (and so never raises) it.
        WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
        _ => None,
    }
}

/// A Windows bitmap kept per window and reused frame after frame (re-created only when the
/// size changes), so animating doesn't allocate and free a bitmap 60 times a second.
struct Surface {
    dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    width: i32,
    height: i32,
}

impl Surface {
    fn new(width: i32, height: i32) -> Result<Surface> {
        unsafe {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height, // negative = top row first
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let dc = CreateCompatibleDC(None);
            let mut bits: *mut c_void = std::ptr::null_mut();
            let dib = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(d) => d,
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(e);
                }
            };
            let old = SelectObject(dc, dib.into());
            Ok(Surface { dc, dib, old, bits: bits as *mut u32, width, height })
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.dib.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

thread_local! {
    static SURFACES: std::cell::RefCell<std::collections::HashMap<isize, Surface>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Hands a premultiplied bitmap to Windows as the window's entire appearance, at `pos` on screen.
pub fn present(hwnd: HWND, bmp: &Bitmap, pos: POINT) -> Result<()> {
    SURFACES.with(|surfaces| {
        let mut surfaces = surfaces.borrow_mut();
        let key = hwnd.0 as isize;
        if surfaces.get(&key).is_none_or(|s| s.width != bmp.width || s.height != bmp.height) {
            surfaces.remove(&key);
            surfaces.insert(key, Surface::new(bmp.width, bmp.height)?);
        }
        let surface = &surfaces[&key];
        unsafe {
            std::ptr::copy_nonoverlapping(bmp.pixels.as_ptr(), surface.bits, bmp.pixels.len());
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let size = SIZE { cx: bmp.width, cy: bmp.height };
            let src = POINT::default();
            UpdateLayeredWindow(
                hwnd,
                None,
                Some(&pos),
                Some(&size),
                Some(surface.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        }
    })
}

/// Shows a right-click menu at the cursor; returns the chosen item id (0 if dismissed).
/// Items are (id, label, checked); an id of 0 makes a separator.
pub fn popup_menu(hwnd: HWND, items: &[(usize, &str, bool)]) -> usize {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        for &(id, label, checked) in items {
            if id == 0 {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            } else {
                let label: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
                let flags = MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED };
                let _ = AppendMenuW(menu, flags, id, PCWSTR(label.as_ptr()));
            }
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Needed so the menu closes when clicking elsewhere; the z-order rule keeps us at the bottom.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, Some(0), hwnd, None);
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    }
}
