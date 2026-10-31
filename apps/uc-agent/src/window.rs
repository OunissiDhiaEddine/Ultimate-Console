//! The agent's hidden window on the main thread (plan §3): global hotkeys,
//! suspend/resume notifications, the overlay hotkey, and in gaming mode the
//! media volume keys (Explorer normally handles those, and it isn't running).
//!
//! It is a hidden top-level window rather than a message-only one, like
//! `uc-power`'s, because message-only windows miss some power broadcasts.
//!
//! Nothing here blocks for long: hotkeys become [`Command`]s for the
//! orchestrator thread. The one exception is the suspend handler, which must
//! send the TV sleep before it returns (Windows suspends right after), and
//! stays inside `power.suspend_budget_ms`.

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::Sender;
use std::sync::{Arc, TryLockError};
use std::time::{Duration, Instant};

use uc_core::config::Hotkeys;
use uc_core::hotkey::Hotkey;
use uc_core::ipc::{HotkeyAction, TvPowerState};
use uc_overlay::VolumeKey;
use uc_power::{EventFilter, PowerEvent, Registration};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, ERROR_HOTKEY_ALREADY_REGISTERED, GetLastError, HWND, LPARAM,
    LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW,
    GetWindowLongPtrW, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetWindowLongPtrW, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_HOTKEY,
    WM_POWERBROADCAST, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::hotkeys::{self, ACTIONS};
use crate::orchestrator::{overlay_allowed, tv_sleep_on_suspend};
use crate::overlay::OverlayHost;
use crate::platform::SharedTv;
use crate::service::Command;
use crate::shared::Shared;

const CLASS_NAME: PCWSTR = w!("UltimateConsole.Agent");
/// lparam: `*mut ApplyRequest`.
const WM_APP_APPLY_HOTKEYS: u32 = WM_APP + 1;
/// lparam: `*mut ProbeRequest`.
const WM_APP_PROBE_HOTKEY: u32 = WM_APP + 2;
/// wparam: 1 to take the volume keys, 0 to give them back.
const WM_APP_VOLUME_KEYS: u32 = WM_APP + 3;
/// Temporary id for probing whether a shortcut can be registered.
const PROBE_ID: i32 = 100;
/// Hotkey ids of the volume keys: this plus the index in [`VolumeKey::ALL`].
const VOLUME_KEY_ID: i32 = 10;

fn volume_key_from_id(id: i32) -> Option<VolumeKey> {
    let i = usize::try_from(id.checked_sub(VOLUME_KEY_ID)?).ok()?;
    VolumeKey::ALL.get(i).copied()
}

struct ApplyRequest {
    hotkeys: Hotkeys,
    errors: Vec<(HotkeyAction, String)>,
}

struct ProbeRequest {
    hotkey: Hotkey,
    error: Option<String>,
}

/// Owned by the window thread, reached through `GWLP_USERDATA`.
struct WindowState {
    shared: Arc<Shared>,
    tv: SharedTv,
    commands: Sender<Command>,
    overlay: Arc<OverlayHost>,
    /// What is registered now, per action (index as in [`ACTIONS`]).
    registered: [Option<Hotkey>; 3],
    /// The volume keys are registered (gaming mode).
    volume_keys: bool,
    filter: EventFilter,
}

pub struct Window {
    hwnd: HWND,
    state: *mut WindowState,
    _power: Option<Registration>,
}

/// Lets other threads talk to the window. All calls are synchronous
/// `SendMessage`s, handled on the window thread.
#[derive(Debug, Clone, Copy)]
pub struct WindowHandle(usize);

// SAFETY: a window handle is just a number; every use goes through
// SendMessage/PostMessage, which are safe to call from any thread.
unsafe impl Send for WindowHandle {}
// SAFETY: as above.
unsafe impl Sync for WindowHandle {}

impl Window {
    pub fn create(
        shared: Arc<Shared>,
        tv: SharedTv,
        commands: Sender<Command>,
        overlay: Arc<OverlayHost>,
    ) -> io::Result<Self> {
        let state = Box::into_raw(Box::new(WindowState {
            shared,
            tv,
            commands,
            overlay,
            registered: [None; 3],
            volume_keys: false,
            filter: EventFilter::new(),
        }));
        // SAFETY: standard class registration and window creation; the class
        // name and window procedure are statics. `state` stays alive until
        // `run` returns (the window is destroyed by then).
        let hwnd = unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                drop(Box::from_raw(state));
                return Err(io::Error::last_os_error());
            }
            match CreateWindowExW(
                WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0),
                CLASS_NAME,
                w!("Ultimate Console agent"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            ) {
                Ok(hwnd) => hwnd,
                Err(e) => {
                    drop(Box::from_raw(state));
                    return Err(e.into());
                }
            }
        };
        // SAFETY: our own window, created just above on this thread.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
        let power = match Registration::register(hwnd) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::error!("suspend/resume notifications unavailable: {e}");
                None
            }
        };
        Ok(Self {
            hwnd,
            state,
            _power: power,
        })
    }

    pub fn handle(&self) -> WindowHandle {
        WindowHandle(self.hwnd.0 as usize)
    }

    /// Runs the message loop until the window is closed.
    pub fn run(self) {
        let mut msg = MSG::default();
        // SAFETY: plain message loop on the thread that owns the window.
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: destroying our own window (a no-op if already destroyed),
        // then freeing the state nothing can reach any more: GWLP_USERDATA is
        // cleared first and the window procedure only runs on this thread.
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
            drop(Box::from_raw(self.state));
        }
    }
}

impl WindowHandle {
    fn hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }

    /// Registers `hotkeys`, replacing the current ones. A shortcut that
    /// can't be registered keeps the old one for that action and is
    /// returned with the reason.
    pub fn apply_hotkeys(self, hotkeys: &Hotkeys) -> Vec<(HotkeyAction, String)> {
        let mut req = ApplyRequest {
            hotkeys: hotkeys.clone(),
            errors: Vec::new(),
        };
        // SAFETY: SendMessage is synchronous, so `req` outlives its use by
        // the window procedure. Without the window it just returns 0.
        unsafe {
            SendMessageW(
                self.hwnd(),
                WM_APP_APPLY_HOTKEYS,
                None,
                Some(LPARAM(&mut req as *mut ApplyRequest as isize)),
            )
        };
        req.errors
    }

    /// Whether `hotkey` could be registered. One of the agent's own current
    /// shortcuts counts as fine (duplicates are checked elsewhere).
    /// `Some(reason)` if not.
    pub fn probe_hotkey(self, hotkey: Hotkey) -> Option<String> {
        let mut req = ProbeRequest {
            hotkey,
            error: None,
        };
        // SAFETY: as in `apply_hotkeys`.
        unsafe {
            SendMessageW(
                self.hwnd(),
                WM_APP_PROBE_HOTKEY,
                None,
                Some(LPARAM(&mut req as *mut ProbeRequest as isize)),
            )
        };
        req.error
    }

    /// Takes over the volume keys (`true`, gaming mode) or gives them back
    /// to Windows (`false`). Cheap to repeat.
    pub fn set_volume_keys(self, on: bool) {
        // SAFETY: as in `apply_hotkeys`; nothing is passed by pointer.
        unsafe {
            SendMessageW(
                self.hwnd(),
                WM_APP_VOLUME_KEYS,
                Some(WPARAM(usize::from(on))),
                None,
            )
        };
    }

    /// Ends the message loop, which ends the agent.
    pub fn close(self) {
        // SAFETY: posting to our own window; fails harmlessly if it is gone.
        let _ = unsafe { PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
}

fn register(hwnd: HWND, id: i32, hotkey: Hotkey) -> Result<(), String> {
    let mods = HOT_KEY_MODIFIERS(hotkey.modifiers.to_win32() | MOD_NOREPEAT.0);
    // SAFETY: plain FFI call on our own window.
    unsafe { RegisterHotKey(Some(hwnd), id, mods, u32::from(hotkey.key.vk())) }.map_err(|e| {
        if e.code() == ERROR_HOTKEY_ALREADY_REGISTERED.to_hresult() {
            format!("{hotkey} is already used by another program")
        } else {
            format!("could not register {hotkey}: {e}")
        }
    })
}

fn unregister(hwnd: HWND, id: i32) {
    // SAFETY: plain FFI call on our own window.
    let _ = unsafe { UnregisterHotKey(Some(hwnd), id) };
}

/// Registers or unregisters the bare volume keys. No `MOD_NOREPEAT`: holding
/// a key keeps changing the volume, like Windows does.
fn set_volume_keys(hwnd: HWND, on: bool) {
    for (i, key) in VolumeKey::ALL.into_iter().enumerate() {
        let id = VOLUME_KEY_ID + i as i32;
        if !on {
            unregister(hwnd, id);
            continue;
        }
        // SAFETY: plain FFI call on our own window.
        if let Err(e) =
            unsafe { RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(0), u32::from(key.vk())) }
        {
            tracing::warn!(?key, "could not take the volume key: {e}");
        }
    }
    tracing::info!(on, "volume keys handled by the agent");
}

impl WindowState {
    fn apply(&mut self, hwnd: HWND, req: &mut ApplyRequest) {
        let wanted = ACTIONS.map(|a| hotkeys::of(&req.hotkeys, a));
        // Unregister every changed one first, so swapping two shortcuts works.
        let mut old = [None; 3];
        for (i, action) in ACTIONS.into_iter().enumerate() {
            if self.registered[i] != Some(wanted[i]) {
                old[i] = self.registered[i].take();
                if old[i].is_some() {
                    unregister(hwnd, hotkeys::id(action));
                }
            }
        }
        for (i, action) in ACTIONS.into_iter().enumerate() {
            if self.registered[i].is_some() {
                continue;
            }
            match register(hwnd, hotkeys::id(action), wanted[i]) {
                Ok(()) => {
                    tracing::info!(action = hotkeys::name(action), hotkey = %wanted[i], "hotkey registered");
                    self.registered[i] = Some(wanted[i]);
                }
                Err(e) => {
                    tracing::warn!(action = hotkeys::name(action), "{e}");
                    req.errors.push((action, e));
                    if let Some(prev) = old[i]
                        && register(hwnd, hotkeys::id(action), prev).is_ok()
                    {
                        self.registered[i] = Some(prev);
                    }
                }
            }
        }
    }

    fn probe(&self, hwnd: HWND, req: &mut ProbeRequest) {
        if self.registered.contains(&Some(req.hotkey)) {
            // Ours already: fine for its own action; a duplicate otherwise,
            // which the caller reports.
            return;
        }
        match register(hwnd, PROBE_ID, req.hotkey) {
            Ok(()) => unregister(hwnd, PROBE_ID),
            Err(e) => req.error = Some(e),
        }
    }

    fn on_hotkey(&self, action: HotkeyAction) {
        tracing::info!(action = hotkeys::name(action), "hotkey pressed");
        match action {
            HotkeyAction::GamingToggle => {
                if self.shared.try_queue_toggle() {
                    let pressed_in = self.shared.mode();
                    let _ = self.commands.send(Command::Toggle { pressed_in });
                } else {
                    tracing::info!("a mode change is already queued; ignoring");
                }
            }
            HotkeyAction::PanicRestore => {
                self.shared.request_abort();
                let _ = self.commands.send(Command::Panic);
            }
            HotkeyAction::Overlay => {
                // Called on the thread that received WM_HOTKEY, which holds
                // the foreground right the overlay needs.
                if overlay_allowed(self.shared.mode(), &self.shared.config()) {
                    self.overlay.toggle();
                } else {
                    tracing::info!("overlay is off outside gaming mode");
                }
            }
        }
    }

    fn on_power(&mut self, event: PowerEvent) {
        if !self.filter.accept(event, Instant::now()) {
            return;
        }
        match event {
            PowerEvent::Suspend => self.on_suspend(),
            // Always sent on resume; the user-input one that may follow adds nothing.
            PowerEvent::ResumeAutomatic => {
                let _ = self.commands.send(Command::Resumed);
            }
            PowerEvent::ResumeSuspend | PowerEvent::Display(_) => {}
        }
    }

    /// TV sleep before the PC suspends, gaming mode only (R8). Must return
    /// within the budget: Windows suspends right after.
    fn on_suspend(&self) {
        let config = self.shared.config();
        let mode = self.shared.mode();
        if !tv_sleep_on_suspend(mode, &config) {
            tracing::info!(?mode, "suspend: TV left alone");
            return;
        }
        let budget = Duration::from_millis(config.power.suspend_budget_ms.into());
        let start = Instant::now();
        // The orchestrator may hold the TV briefly (keepalive); wait at most
        // half the budget for it.
        let mut guard = loop {
            match self.tv.controller.try_lock() {
                Ok(g) => break g,
                Err(TryLockError::Poisoned(p)) => break p.into_inner(),
                Err(TryLockError::WouldBlock) if start.elapsed() < budget / 2 => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(TryLockError::WouldBlock) => {
                    tracing::warn!("suspend: TV busy, could not send sleep in time");
                    return;
                }
            }
        };
        if let Some(tv) = guard.as_mut() {
            let left = budget.saturating_sub(start.elapsed());
            if tv.sleep_within(left).is_ok() {
                self.shared.set_tv(TvPowerState::Asleep);
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA is 0 or the WindowState owned by `Window`, which
    // outlives the window; only this thread uses it.
    let state = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState).as_mut() };
    let Some(state) = state else {
        // SAFETY: default handling before the state is attached / after it is gone.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    // A panic must not unwind into Windows (it would abort the process).
    let result = catch_unwind(AssertUnwindSafe(|| match msg {
        WM_HOTKEY => {
            let id = wparam.0 as i32;
            if let Some(action) = hotkeys::from_id(id) {
                state.on_hotkey(action);
            } else if let Some(key) = volume_key_from_id(id) {
                state.overlay.volume_key(key);
            }
            Some(LRESULT(0))
        }
        WM_POWERBROADCAST => {
            // SAFETY: our own window procedure, with the message as received.
            if let Some(event) = unsafe { uc_power::decode_message(msg, wparam, lparam) } {
                state.on_power(event);
            }
            Some(LRESULT(1))
        }
        WM_APP_APPLY_HOTKEYS => {
            // SAFETY: sent by `WindowHandle::apply_hotkeys`, which keeps the
            // request alive for the duration of SendMessage.
            if let Some(req) = unsafe { (lparam.0 as *mut ApplyRequest).as_mut() } {
                state.apply(hwnd, req);
            }
            Some(LRESULT(0))
        }
        WM_APP_PROBE_HOTKEY => {
            // SAFETY: as above, from `WindowHandle::probe_hotkey`.
            if let Some(req) = unsafe { (lparam.0 as *mut ProbeRequest).as_mut() } {
                state.probe(hwnd, req);
            }
            Some(LRESULT(0))
        }
        WM_APP_VOLUME_KEYS => {
            let on = wparam.0 != 0;
            if on != state.volume_keys {
                set_volume_keys(hwnd, on);
                state.volume_keys = on;
            }
            Some(LRESULT(0))
        }
        WM_CLOSE => {
            // SAFETY: destroying our own window from its own thread.
            let _ = unsafe { DestroyWindow(hwnd) };
            Some(LRESULT(0))
        }
        WM_DESTROY => {
            for action in ACTIONS {
                unregister(hwnd, hotkeys::id(action));
            }
            set_volume_keys(hwnd, false);
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            Some(LRESULT(0))
        }
        _ => None,
    }));
    match result {
        Ok(Some(r)) => r,
        Ok(None) => {
            // SAFETY: default handling for everything else.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        Err(_) => {
            tracing::error!(msg, "window message handler panicked");
            LRESULT(0)
        }
    }
}
