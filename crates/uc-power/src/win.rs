//! Win32 side: power notifications on a window, adapter snapshot.

use std::io;
use std::net::Ipv4Addr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_BUFFER_OVERFLOW, ERROR_CLASS_ALREADY_EXISTS, ERROR_NO_DATA, GetLastError, HANDLE, HWND,
    LPARAM, LRESULT, WPARAM,
};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, IpDadStatePreferred, SOCKADDR_IN};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    HPOWERNOTIFY, POWERBROADCAST_SETTING, RegisterPowerSettingNotification,
    RegisterSuspendResumeNotification, UnregisterPowerSettingNotification,
    UnregisterSuspendResumeNotification,
};
use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, MSG, PostMessageW, PostQuitMessage,
    RegisterClassW, SetWindowLongPtrW, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::{
    AdapterInfo, DisplayState, EventFilter, Ipv4Net, NetworkWait, PBT_POWERSETTINGCHANGE,
    PowerEvent, WM_POWERBROADCAST, decode_pbt, network_ready, wait_until,
};

/// How often [`wait_for_network`] re-reads the adapters.
const NETWORK_POLL: Duration = Duration::from_millis(250);

/// Suspend/resume and display-state registrations for one window. Dropping it
/// unregisters both.
///
/// The window then receives `WM_POWERBROADCAST`; pass those messages to
/// [`decode_message`].
#[derive(Debug)]
pub struct Registration {
    suspend: HPOWERNOTIFY,
    display: HPOWERNOTIFY,
}

impl Registration {
    /// Registers `hwnd` for suspend/resume (`RegisterSuspendResumeNotification`)
    /// and console display state (`GUID_CONSOLE_DISPLAY_STATE`).
    pub fn register(hwnd: HWND) -> io::Result<Self> {
        let recipient = HANDLE(hwnd.0);
        // SAFETY: `recipient` is a window handle, as DEVICE_NOTIFY_WINDOW_HANDLE says.
        let suspend =
            unsafe { RegisterSuspendResumeNotification(recipient, DEVICE_NOTIFY_WINDOW_HANDLE)? };
        // SAFETY: as above; the GUID is a static that outlives the call.
        let display = unsafe {
            RegisterPowerSettingNotification(
                recipient,
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_WINDOW_HANDLE,
            )
        };
        match display {
            Ok(display) => Ok(Self { suspend, display }),
            Err(e) => {
                // SAFETY: `suspend` was just registered and is not used again.
                let _ = unsafe { UnregisterSuspendResumeNotification(suspend) };
                Err(e.into())
            }
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: both handles came from the matching Register* calls and are
        // unregistered exactly once, here.
        unsafe {
            let _ = UnregisterSuspendResumeNotification(self.suspend);
            let _ = UnregisterPowerSettingNotification(self.display);
        }
    }
}

/// Turns a window message into a [`PowerEvent`], or `None` if it is not one we
/// report. Does no de-duplication; use an [`EventFilter`] for that.
///
/// # Safety
///
/// `msg`, `wparam` and `lparam` must be exactly what the window procedure
/// received, and this must be called from inside that window procedure: for
/// `PBT_POWERSETTINGCHANGE` the `lparam` points to a `POWERBROADCAST_SETTING`
/// that is only valid during the call.
pub unsafe fn decode_message(msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<PowerEvent> {
    if msg != WM_POWERBROADCAST {
        return None;
    }
    if wparam.0 != PBT_POWERSETTINGCHANGE {
        return decode_pbt(wparam.0);
    }
    let setting = lparam.0 as *const POWERBROADCAST_SETTING;
    if setting.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `lparam` is the POWERBROADCAST_SETTING
    // Windows passed with this message. `Data` holds `DataLength` bytes, which
    // we check before reading 4 of them unaligned.
    unsafe {
        if (*setting).PowerSetting != GUID_CONSOLE_DISPLAY_STATE || (*setting).DataLength < 4 {
            return None;
        }
        let data = std::ptr::addr_of!((*setting).Data) as *const u32;
        DisplayState::from_raw(data.read_unaligned()).map(PowerEvent::Display)
    }
}

type Callback = Box<dyn FnMut(PowerEvent) + Send>;

/// Per-window data, owned by the watcher thread, reached through GWLP_USERDATA.
struct WindowState {
    callback: Callback,
    filter: EventFilter,
}

/// Watches power events on a hidden window running on its own thread.
///
/// The callback runs on that thread, once per real transition (repeats are
/// filtered). For [`PowerEvent::Suspend`] Windows waits for it to return, so
/// keep it within the suspend budget (crate docs). Dropping the watcher closes
/// the window and joins the thread.
pub struct PowerWatcher {
    /// The window, as an integer because `HWND` is not `Send`.
    hwnd: usize,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PowerWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PowerWatcher")
            .field("hwnd", &format_args!("{:#x}", self.hwnd))
            .finish()
    }
}

impl PowerWatcher {
    /// Creates the window, registers it and starts delivering events. Returns
    /// once registration succeeded or failed. The current display state
    /// arrives as the first event shortly after.
    pub fn start<F>(callback: F) -> io::Result<Self>
    where
        F: FnMut(PowerEvent) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let callback: Callback = Box::new(callback);
        let thread = std::thread::Builder::new()
            .name("uc-power".into())
            .spawn(move || window_thread(callback, tx))?;
        match rx.recv() {
            Ok(Ok(hwnd)) => Ok(Self {
                hwnd,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(io::Error::other("power watcher thread exited during setup"))
            }
        }
    }
}

impl Drop for PowerWatcher {
    fn drop(&mut self) {
        // SAFETY: posting to a window of ours; if it is already gone the call
        // just fails.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            )
        };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

const CLASS_NAME: PCWSTR = w!("UltimateConsole.PowerWatcher");

fn window_thread(callback: Callback, ready: mpsc::Sender<io::Result<usize>>) {
    let state = Box::into_raw(Box::new(WindowState {
        callback,
        filter: EventFilter::new(),
    }));
    let (hwnd, registration) = match create_window(state) {
        Ok(v) => v,
        Err(e) => {
            // SAFETY: `state` came from Box::into_raw above and no window
            // refers to it any more (create_window destroys it on failure).
            drop(unsafe { Box::from_raw(state) });
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(hwnd.0 as usize));
    tracing::debug!("power watcher running");

    let mut msg = MSG::default();
    // SAFETY: plain message loop on the thread that owns the window.
    unsafe {
        // GetMessageW returns -1 on error, which `as_bool` would treat as true.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            DispatchMessageW(&msg);
        }
    }

    drop(registration);
    // SAFETY: the window is destroyed (the loop ends on WM_QUIT, posted from
    // WM_DESTROY), so nothing can reach `state` through GWLP_USERDATA now.
    drop(unsafe { Box::from_raw(state) });
    tracing::debug!("power watcher stopped");
}

fn create_window(state: *mut WindowState) -> io::Result<(HWND, Registration)> {
    // SAFETY: standard class registration and window creation; the class name
    // and window procedure are statics.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            let err = GetLastError();
            if err != ERROR_CLASS_ALREADY_EXISTS {
                return Err(io::Error::from_raw_os_error(err.0 as i32));
            }
        }
        // A hidden top-level window (never shown) rather than a message-only
        // one, so it also gets the WM_POWERBROADCAST broadcast.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0),
            CLASS_NAME,
            w!("Ultimate Console power"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        match Registration::register(hwnd) {
            Ok(reg) => Ok((hwnd, reg)),
            Err(e) => {
                let _ = DestroyWindow(hwnd);
                Err(e)
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_POWERBROADCAST => {
            // SAFETY: called from this window's procedure with its own message.
            if let Some(event) = unsafe { decode_message(msg, wparam, lparam) } {
                // SAFETY: GWLP_USERDATA is either 0 (not set yet) or the
                // WindowState that outlives the window (see window_thread).
                let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState;
                // SAFETY: only this thread touches the state, and not re-entrantly
                // unless the callback pumps messages, which it must not.
                if let Some(state) = unsafe { state.as_mut() } {
                    deliver(state, event);
                }
            }
            LRESULT(1)
        }
        WM_CLOSE => {
            // SAFETY: destroying our own window from its own thread.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn deliver(state: &mut WindowState, event: PowerEvent) {
    if !state.filter.accept(event, Instant::now()) {
        tracing::debug!(%event, "repeated power event dropped");
        return;
    }
    tracing::info!(%event, "power event");
    // A panic must not unwind into Windows (it would abort the process).
    if catch_unwind(AssertUnwindSafe(|| (state.callback)(event))).is_err() {
        tracing::error!(%event, "power event callback panicked");
    }
}

/// Reads every IPv4 adapter: link state, addresses that finished duplicate
/// address detection, and whether it has a gateway.
pub fn network_snapshot() -> io::Result<Vec<AdapterInfo>> {
    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    // u64 elements keep the buffer 8-byte aligned for the structs inside.
    let mut buf: Vec<u64> = vec![0; 2048];
    for _ in 0..4 {
        let mut size = (buf.len() * 8) as u32;
        let first = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        // SAFETY: `first` points to `size` writable, aligned bytes.
        let rc =
            unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(first), &mut size) };
        if rc == ERROR_BUFFER_OVERFLOW.0 {
            buf = vec![0; (size as usize).div_ceil(8) + 64];
            continue;
        }
        if rc == ERROR_NO_DATA.0 {
            return Ok(Vec::new());
        }
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        // SAFETY: on success the buffer holds a valid linked list starting at
        // `first`, and `buf` stays alive while we walk it.
        return Ok(unsafe { walk_adapters(first) });
    }
    Err(io::Error::other("adapter list kept growing"))
}

/// # Safety
/// `first` must be the head of a list filled in by `GetAdaptersAddresses`.
unsafe fn walk_adapters(mut adapter: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<AdapterInfo> {
    let mut out = Vec::new();
    // SAFETY: every pointer followed here belongs to that list.
    unsafe {
        while let Some(a) = adapter.as_ref() {
            let mut ipv4 = Vec::new();
            let mut u = a.FirstUnicastAddress;
            while let Some(addr) = u.as_ref() {
                let sa = addr.Address.lpSockaddr as *const SOCKADDR_IN;
                if !sa.is_null()
                    && (*sa).sin_family == AF_INET
                    && addr.DadState == IpDadStatePreferred
                {
                    let ip = Ipv4Addr::from((*sa).sin_addr.S_un.S_addr.to_ne_bytes());
                    ipv4.push(Ipv4Net::new(ip, addr.OnLinkPrefixLength));
                }
                u = addr.Next;
            }
            out.push(AdapterInfo {
                name: a.FriendlyName.to_string().unwrap_or_default(),
                up: a.OperStatus == IfOperStatusUp,
                ipv4,
                has_gateway: !a.FirstGatewayAddress.is_null(),
            });
            adapter = a.Next;
        }
    }
    out
}

/// Waits until the network is usable (see [`network_ready`]) or `timeout`
/// passes, re-checking every 250 ms. Call it after resume, before WoL/ADB.
///
/// Fails only if the very first adapter read fails; later read errors count
/// as "not ready yet".
pub fn wait_for_network(target: Option<Ipv4Addr>, timeout: Duration) -> io::Result<NetworkWait> {
    network_snapshot()?;
    Ok(wait_until(timeout, NETWORK_POLL, || {
        network_snapshot().is_ok_and(|a| network_ready(&a, target))
    }))
}
