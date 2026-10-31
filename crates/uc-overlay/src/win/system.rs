//! Win32 helpers that aren't drawing: task enumeration, focus, window and
//! process actions, monitor geometry, XInput polling.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_TERMINATE, TerminateProcess,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VK_MENU,
};
use windows::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetForegroundWindow, GetWindow,
    GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    PostMessageW, SW_RESTORE, SetForegroundWindow, ShowWindow, WM_CLOSE, WS_EX_APPWINDOW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows::core::BOOL;

use crate::gamepad::PadState;
use crate::model::Task;
use crate::tasks::{WindowFacts, is_task};

fn text(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is a valid writable buffer; a stale `hwnd` just fails.
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: `buf` is a valid writable buffer; a stale `hwnd` just fails.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn facts(hwnd: HWND) -> WindowFacts {
    // SAFETY: plain FFI calls on a window handle; a stale handle returns 0/false.
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    // SAFETY: as above.
    let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid());
    let mut cloaked = 0u32;
    // SAFETY: `cloaked` is a u32, the documented type for DWMWA_CLOAKED, and
    // the size passed matches it.
    let _ = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        )
    };
    WindowFacts {
        // SAFETY: plain FFI call on a window handle.
        visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
        owned,
        tool_window: ex & WS_EX_TOOLWINDOW.0 != 0,
        app_window: ex & WS_EX_APPWINDOW.0 != 0,
        no_activate: ex & WS_EX_NOACTIVATE.0 != 0,
        cloaked: cloaked != 0,
        title: text(hwnd),
        class: class(hwnd),
    }
}

/// Alt-tab-eligible windows in z-order (the one that was in front first),
/// leaving out `own`.
pub fn list_tasks(own: HWND) -> Vec<Task> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `&mut Vec` passed to EnumWindows below,
        // alive and not otherwise borrowed during enumeration.
        let out = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        out.push(hwnd);
        BOOL::from(true)
    }
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: the callback only uses `lparam` as the vector above.
    if let Err(e) = unsafe { EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize)) } {
        tracing::warn!("EnumWindows failed: {e}");
    }
    all.into_iter()
        .filter(|&h| h != own)
        .filter_map(|hwnd| {
            let f = facts(hwnd);
            if !is_task(&f) {
                return None;
            }
            let mut pid = 0;
            // SAFETY: `pid` is a valid out pointer for the duration of the call.
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            Some(Task {
                hwnd: hwnd.0 as isize,
                pid,
                title: f.title,
            })
        })
        .collect()
}

/// Tap Alt, which lets this process take the foreground when Windows'
/// foreground lock would otherwise refuse (plan §2.9 fallback).
fn tap_alt() {
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_MENU,
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    // SAFETY: `inputs` is a valid slice of INPUT and the size matches.
    unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
}

/// Make `hwnd` the foreground window, using the Alt trick if needed.
pub fn bring_to_front(hwnd: HWND) -> bool {
    // SAFETY: plain FFI calls on a window handle.
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
            return true;
        }
        tap_alt();
        SetForegroundWindow(hwnd).as_bool()
    }
}

/// Restore (if minimised) and focus a task window.
pub fn switch_to(hwnd: HWND) {
    // SAFETY: plain FFI calls on a window handle; a stale one just fails.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
    }
    if !bring_to_front(hwnd) {
        tracing::warn!(
            hwnd = hwnd.0 as isize,
            "could not bring task to the foreground"
        );
    }
}

/// Ask a window to close, like clicking its X.
pub fn close_window(hwnd: HWND) {
    // SAFETY: posting a message to a possibly stale handle just fails.
    if let Err(e) = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) } {
        tracing::warn!("WM_CLOSE to {:?} failed: {e}", hwnd.0);
    }
}

/// End a process. Refuses to end the agent itself.
pub fn kill_process(pid: u32) {
    // SAFETY: plain FFI call.
    if pid == 0 || pid == unsafe { GetCurrentProcessId() } {
        tracing::warn!(pid, "refusing to end this process");
        return;
    }
    // SAFETY: plain FFI call; the handle is closed below.
    match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
        Ok(h) => {
            // SAFETY: `h` is a live process handle opened with PROCESS_TERMINATE.
            match unsafe { TerminateProcess(h, 1) } {
                Ok(()) => tracing::info!(pid, "process ended from the overlay"),
                Err(e) => tracing::warn!(pid, "TerminateProcess failed: {e}"),
            }
            // SAFETY: closes the handle opened above, once.
            let _ = unsafe { CloseHandle(h) };
        }
        Err(e) => tracing::warn!(pid, "OpenProcess failed: {e}"),
    }
}

/// Bounds of the monitor the foreground window is on, in physical pixels.
pub fn foreground_monitor() -> RECT {
    let mut mi = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: plain FFI calls; `mi.cbSize` is set and `mi` outlives the call.
    unsafe {
        let mon = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            };
        }
    }
    mi.rcMonitor
}

const RESCAN: Duration = Duration::from_secs(1);

/// Polls XInput. Disconnected slots are slow to query, so they are only
/// re-checked once a second.
#[derive(Debug)]
pub struct Pads {
    connected: [bool; 4],
    last_scan: Option<Instant>,
}

impl Pads {
    pub fn new() -> Self {
        Self {
            connected: [false; 4],
            last_scan: None,
        }
    }

    pub fn poll(&mut self) -> Vec<PadState> {
        let now = Instant::now();
        let scan = self.last_scan.is_none_or(|t| now - t >= RESCAN);
        if scan {
            self.last_scan = Some(now);
        }
        let mut out = Vec::new();
        for (i, connected) in self.connected.iter_mut().enumerate() {
            if !*connected && !scan {
                continue;
            }
            let mut st = XINPUT_STATE::default();
            // SAFETY: `st` is a valid out pointer for the duration of the call.
            *connected = unsafe { XInputGetState(i as u32, &mut st) } == 0;
            if *connected {
                let g = st.Gamepad;
                out.push(PadState {
                    buttons: g.wButtons.0,
                    left_trigger: g.bLeftTrigger,
                    right_trigger: g.bRightTrigger,
                    thumb_lx: g.sThumbLX,
                    thumb_ly: g.sThumbLY,
                });
            }
        }
        out
    }
}
