//! Small Win32 helpers: processes in this session and top-level windows.

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_TERMINATE, TerminateProcess,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::BOOL;

use crate::{Error, Result, WindowInfo};

/// A running process in the current session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    /// Executable file name, e.g. `explorer.exe`.
    pub name: String,
}

/// Closes a handle on drop.
pub(crate) struct OwnedHandle(pub HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful Win32 call and is closed
        // exactly once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn session_of(pid: u32) -> Option<u32> {
    let mut session = 0;
    // SAFETY: `session` is a valid out pointer for the duration of the call.
    unsafe { ProcessIdToSessionId(pid, &mut session) }
        .ok()
        .map(|()| session)
}

/// Processes in the current session (other users' Explorers are left alone).
pub fn list_processes() -> Result<Vec<ProcessInfo>> {
    // SAFETY: plain FFI call without pointers.
    let own_session = session_of(unsafe { GetCurrentProcessId() });
    // SAFETY: plain FFI call; the returned handle is owned below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|e| Error::win32("CreateToolhelp32Snapshot", e))?;
    let snap = OwnedHandle(snap);

    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    // SAFETY: `snap` is a live snapshot handle and `entry.dwSize` is set.
    let mut more = unsafe { Process32FirstW(snap.0, &mut entry) }.is_ok();
    while more {
        let pid = entry.th32ProcessID;
        if pid != 0 && session_of(pid) == own_session {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            out.push(ProcessInfo {
                pid,
                name: String::from_utf16_lossy(&entry.szExeFile[..len]),
            });
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snap.0, &mut entry) }.is_ok();
    }
    Ok(out)
}

/// PIDs of processes called `name` (case-insensitive) in this session.
pub(crate) fn session_pids_named(name: &str) -> Result<Vec<u32>> {
    Ok(list_processes()?
        .into_iter()
        .filter(|p| p.name.eq_ignore_ascii_case(name))
        .map(|p| p.pid)
        .collect())
}

pub(crate) fn terminate(pid: u32) -> Result<()> {
    // SAFETY: plain FFI call; the handle is owned below.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }
        .map_err(|e| Error::win32("OpenProcess", e))?;
    let handle = OwnedHandle(handle);
    // SAFETY: `handle` was opened with PROCESS_TERMINATE.
    unsafe { TerminateProcess(handle.0, 1) }.map_err(|e| Error::win32("TerminateProcess", e))
}

pub(crate) fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: `buf` is a valid writable buffer; a stale `hwnd` just fails.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub(crate) fn window_info(hwnd: HWND) -> WindowInfo {
    let mut title = [0u16; 512];
    // SAFETY: `title` is a valid writable buffer; a stale `hwnd` just fails.
    let n = unsafe { GetWindowTextW(hwnd, &mut title) };
    let mut pid = 0;
    // SAFETY: `pid` is a valid out pointer for the duration of the call.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    WindowInfo {
        hwnd: hwnd.0 as isize,
        pid,
        class: window_class(hwnd),
        title: String::from_utf16_lossy(&title[..n.max(0) as usize]),
        // SAFETY: plain FFI call on a window handle.
        visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
    }
}

/// All top-level windows, in z-order (topmost first).
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `&mut Vec` passed to EnumWindows below,
        // which is alive and not otherwise borrowed during enumeration.
        let out = unsafe { &mut *(lparam.0 as *mut Vec<WindowInfo>) };
        out.push(window_info(hwnd));
        BOOL::from(true)
    }
    let mut out: Vec<WindowInfo> = Vec::new();
    // SAFETY: the callback only uses `lparam` as the vector above.
    unsafe { EnumWindows(Some(collect), LPARAM(&mut out as *mut _ as isize)) }
        .map_err(|e| Error::win32("EnumWindows", e))?;
    Ok(out)
}
