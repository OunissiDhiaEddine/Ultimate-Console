//! "Identify": a big number in a box on each active display, like Windows'
//! own Settings → Display → Identify. Plain GDI; the window title carries the
//! number so the window procedure needs no shared state.

use std::time::{Duration, Instant};

use tracing::{info, warn};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET,
    DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FW_BOLD, FillRect,
    GetStockObject, HBRUSH, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetWindowTextW, LWA_ALPHA, MSG, PM_REMOVE, PeekMessageW, RegisterClassW, SW_SHOWNOACTIVATE,
    SetLayeredWindowAttributes, ShowWindow, TranslateMessage, WM_PAINT, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::{Error, Result};

const CLASS: PCWSTR = w!("UltimateConsole.Identify");

pub fn identify(duration: Duration) -> Result<()> {
    // SAFETY: only changes this thread's DPI mode; restored below. Physical
    // pixels are needed because CCD positions are physical.
    let old_dpi =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let result = show(duration);
    if !old_dpi.is_invalid() {
        // SAFETY: puts back the value returned above.
        unsafe { SetThreadDpiAwarenessContext(old_dpi) };
    }
    result
}

fn show(duration: Duration) -> Result<()> {
    let rects = super::ccd::active_rects()?;
    // SAFETY: `None` asks for the current module, which outlives the windows.
    let hinstance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| Error::win32("GetModuleHandleW", e.code().0 as u32))?
        .into();

    let class = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance,
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        lpszClassName: CLASS,
        ..Default::default()
    };
    // SAFETY: `class` is fully initialised; a second registration in the
    // same process fails harmlessly (ERROR_CLASS_ALREADY_EXISTS).
    unsafe { RegisterClassW(&class) };

    let mut windows = Vec::new();
    for (index, r) in &rects {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let size = (w.min(h) / 3).max(120);
        let title: Vec<u16> = index.to_string().encode_utf16().chain([0]).collect();
        // SAFETY: class registered above; strings are NUL-terminated and
        // outlive the call.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT,
                CLASS,
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                r.left + (w - size) / 2,
                r.top + (h - size) / 2,
                size,
                size,
                None,
                None,
                Some(hinstance),
                None,
            )
        };
        match hwnd {
            Ok(hwnd) => {
                // SAFETY: `hwnd` was just created on this thread.
                unsafe {
                    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 220, LWA_ALPHA);
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
                windows.push(hwnd);
            }
            Err(e) => warn!(index, "identify window failed: {e}"),
        }
    }
    info!(displays = windows.len(), "identify shown");

    let end = Instant::now() + duration;
    while Instant::now() < end {
        let mut msg = MSG::default();
        // SAFETY: standard message pump for windows owned by this thread.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    }

    for hwnd in windows {
        // SAFETY: created above on this thread and not destroyed yet.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
    Ok(())
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_PAINT {
        // SAFETY: WM_PAINT for our own window; every GDI object created here
        // is deselected and deleted before returning.
        unsafe { paint(hwnd) };
        return LRESULT(0);
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// # Safety
/// Must be called from `WM_PAINT` of a window on this thread.
unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let mut text = [0u16; 16];
    let mut rc = RECT::default();
    // SAFETY: guaranteed by the caller; buffers are local.
    unsafe {
        let hdc = BeginPaint(hwnd, &mut ps);
        let len = GetWindowTextW(hwnd, &mut text).max(0) as usize;
        let _ = GetClientRect(hwnd, &mut rc);
        FillRect(hdc, &rc, HBRUSH(GetStockObject(BLACK_BRUSH).0));
        let font = CreateFontW(
            (rc.bottom - rc.top) * 7 / 10,
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            w!("Segoe UI"),
        );
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(0x00FF_FFFF));
        DrawTextW(
            hdc,
            &mut text[..len],
            &mut rc,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
        let _ = EndPaint(hwnd, &ps);
    }
}
