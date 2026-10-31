//! The overlay window, its UI thread and the handle other threads use.

mod render;
mod system;
mod thumbs;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VIRTUAL_KEY, VK_ADD, VK_BACK, VK_DELETE, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_NEXT,
    VK_OEM_MINUS, VK_OEM_PLUS, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_SUBTRACT,
    VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetMessageW, HWND_TOPMOST, IDC_ARROW, IsWindow, KillTimer, LoadCursorW, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SetTimer, SetWindowPos,
    ShowWindow, TranslateMessage, WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_KEYDOWN, WM_LBUTTONUP, WM_SYSKEYDOWN, WM_TIMER,
    WNDCLASSEXW, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::w;

use self::render::Gfx;
use self::system::Pads;
use self::thumbs::Thumbs;
use crate::anim::{FADE_IN, FADE_OUT, Fade};
use crate::gamepad::Gamepad;
use crate::layout::{Hit, Layout};
use crate::model::{Action, Effect, Master, Model, Tab, Volume};
use crate::{Backends, Error, Result, Settings};

const WM_APP_SHOW: u32 = WM_APP + 1;
const WM_APP_HIDE: u32 = WM_APP + 2;
const WM_APP_TOGGLE: u32 = WM_APP + 3;
const WM_APP_SETTINGS: u32 = WM_APP + 4;
const WM_APP_QUIT: u32 = WM_APP + 5;

const TIMER_FRAME: usize = 1;
const TIMER_SAMPLE: usize = 2;
const TIMER_REFRESH: usize = 3;
const FRAME_MS: u32 = 16;
/// Delay before re-listing windows after a close/kill, so the window is gone.
const REFRESH_MS: u32 = 400;

/// Handle to the overlay. `Send + Sync`; every method just posts a message
/// to the overlay's UI thread and returns. Dropping it closes the window
/// and joins the thread.
#[derive(Debug)]
pub struct Overlay {
    hwnd: isize,
    visible: Arc<AtomicBool>,
    settings: Arc<Mutex<Settings>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Overlay {
    /// Start the overlay thread with the real audio and stats backends. The
    /// window starts hidden.
    pub fn start(config: &uc_core::Config) -> Result<Self> {
        Self::start_with(Settings::from(config), Backends::real())
    }

    pub fn start_with(settings: Settings, backends: Backends) -> Result<Self> {
        let visible = Arc::new(AtomicBool::new(false));
        let settings = Arc::new(Mutex::new(settings));
        let (tx, rx) = mpsc::channel();
        let thread = {
            let visible = visible.clone();
            let settings = settings.clone();
            std::thread::Builder::new()
                .name("uc-overlay".into())
                .spawn(move || ui_thread(visible, settings, backends, tx))
                .map_err(|source| Error::Os {
                    context: "spawning the overlay thread",
                    source,
                })?
        };
        let hwnd = rx.recv().map_err(|_| Error::ThreadDied)??;
        Ok(Self {
            hwnd,
            visible,
            settings,
            thread: Mutex::new(Some(thread)),
        })
    }

    fn post(&self, msg: u32) {
        // SAFETY: posting to a window owned by our UI thread; if it is gone
        // the call just fails.
        if let Err(e) =
            unsafe { PostMessageW(Some(HWND(self.hwnd as _)), msg, WPARAM(0), LPARAM(0)) }
        {
            tracing::warn!("posting to the overlay failed: {e}");
        }
    }

    pub fn show(&self) {
        self.post(WM_APP_SHOW);
    }

    pub fn hide(&self) {
        self.post(WM_APP_HIDE);
    }

    pub fn toggle(&self) {
        self.post(WM_APP_TOGGLE);
    }

    /// Pick up a changed config (sampling interval).
    pub fn set_config(&self, config: &uc_core::Config) {
        if let Ok(mut s) = self.settings.lock() {
            *s = Settings::from(config);
        }
        self.post(WM_APP_SETTINGS);
    }

    /// Open or opening (true until the fade-out has finished).
    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed)
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.post(WM_APP_QUIT);
        if let Some(t) = self.thread.lock().ok().and_then(|mut t| t.take())
            && t.join().is_err()
        {
            tracing::error!("overlay thread panicked");
        }
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn ui_thread(
    visible: Arc<AtomicBool>,
    settings: Arc<Mutex<Settings>>,
    backends: Backends,
    ready: mpsc::Sender<Result<isize>>,
) {
    // Windows created on this thread are per-monitor DPI aware whatever the
    // process manifest says, so the overlay is sharp on a 4K TV.
    // SAFETY: plain FFI call affecting only this thread.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    STATE.with(|s| *s.borrow_mut() = Some(State::new(hwnd, visible, settings, backends)));
    let _ = ready.send(Ok(hwnd.0 as isize));
    tracing::debug!("overlay thread ready");

    let mut msg = MSG::default();
    // SAFETY: standard message loop on this thread's queue; `msg` outlives
    // each call.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    STATE.with(|s| s.borrow_mut().take());
    tracing::debug!("overlay thread exiting");
}

fn create_window() -> Result<HWND> {
    let class = w!("UltimateConsoleOverlay");
    // SAFETY: the class struct and strings are valid for the calls; the
    // window procedure has the right signature.
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| Error::win("GetModuleHandleW", e))?;
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        // Fails harmlessly with "class already exists" for a second overlay.
        RegisterClassExW(&wc);
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP,
            class,
            w!("Ultimate Console overlay"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| Error::win("CreateWindowExW", e))
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_DESTROY => {
            // SAFETY: plain FFI call on this thread.
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        // Alt+F4 hides instead of destroying; only the handle's Drop quits.
        WM_CLOSE => {
            // SAFETY: posting to our own window.
            let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_HIDE, WPARAM(0), LPARAM(0)) };
            return LRESULT(0);
        }
        _ => {}
    }
    // Win32 calls made while handling a message can send us another one
    // synchronously; those re-entrant messages get default handling.
    let handled = STATE.with(|s| match s.try_borrow_mut() {
        Ok(mut s) => s.as_mut().and_then(|s| s.on_message(msg, wp, lp)),
        Err(_) => None,
    });
    match handled {
        Some(r) => r,
        // SAFETY: forwarding the original arguments.
        None => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Hidden,
    Shown,
    Hiding,
}

struct State {
    hwnd: HWND,
    visible: Arc<AtomicBool>,
    settings: Arc<Mutex<Settings>>,
    backends: Backends,
    phase: Phase,
    model: Model,
    layout: Layout,
    /// Pixels per DIP.
    scale: f32,
    size: (u32, u32),
    fade: Fade,
    last_opacity: f32,
    dirty: bool,
    /// The window that was in front when the overlay opened.
    previous: HWND,
    gfx: Option<Gfx>,
    thumbs: Thumbs,
    audio: Option<Box<dyn uc_audio::AudioControl>>,
    audio_error: Option<String>,
    stats: Option<Box<dyn uc_sysinfo::StatsSource>>,
    pads: Pads,
    gamepad: Gamepad,
}

impl State {
    fn new(
        hwnd: HWND,
        visible: Arc<AtomicBool>,
        settings: Arc<Mutex<Settings>>,
        backends: Backends,
    ) -> Self {
        let layout = Layout::new(1920.0, 1080.0);
        Self {
            hwnd,
            visible,
            settings,
            backends,
            phase: Phase::Hidden,
            model: Model::new(layout.columns),
            layout,
            scale: 1.0,
            size: (1920, 1080),
            fade: Fade::settled(0.0, Instant::now()),
            last_opacity: 0.0,
            dirty: false,
            previous: HWND::default(),
            gfx: None,
            thumbs: Thumbs::default(),
            audio: None,
            audio_error: None,
            stats: None,
            pads: Pads::new(),
            gamepad: Gamepad::default(),
        }
    }

    fn on_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_APP_SHOW => self.show(),
            WM_APP_HIDE => self.begin_hide(true),
            WM_APP_TOGGLE => match self.phase {
                Phase::Shown => self.begin_hide(true),
                Phase::Hidden | Phase::Hiding => self.show(),
            },
            WM_APP_SETTINGS => {
                if self.phase != Phase::Hidden {
                    self.start_sample_timer();
                }
            }
            WM_APP_QUIT => {
                self.finish_hide();
                // SAFETY: destroying our own window on its thread.
                let _ = unsafe { DestroyWindow(self.hwnd) };
            }
            WM_TIMER => match wp.0 {
                TIMER_FRAME => self.frame(),
                TIMER_SAMPLE => self.sample(),
                TIMER_REFRESH => {
                    // SAFETY: killing our own one-shot timer.
                    let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_REFRESH) };
                    self.refresh_tasks();
                }
                _ => return None,
            },
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let action = key_action(VIRTUAL_KEY(wp.0 as u16))?;
                self.act(action);
            }
            WM_LBUTTONUP => {
                let x = (lp.0 & 0xFFFF) as i16 as f32 / self.scale;
                let y = ((lp.0 >> 16) & 0xFFFF) as i16 as f32 / self.scale;
                self.click(x, y);
            }
            WM_ACTIVATE => {
                if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE && self.phase == Phase::Shown {
                    self.begin_hide(false);
                }
                return None;
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED => {
                if self.phase != Phase::Hidden {
                    self.place();
                    self.dirty = true;
                }
                return None;
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }

    /// Cover the foreground window's monitor and (re)build the GPU objects
    /// for its size and DPI.
    fn place(&mut self) -> bool {
        let rc = system::foreground_monitor();
        let (w, h) = ((rc.right - rc.left).max(1), (rc.bottom - rc.top).max(1));
        // SAFETY: positioning our own window.
        if let Err(e) = unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                rc.left,
                rc.top,
                w,
                h,
                SWP_NOACTIVATE,
            )
        } {
            tracing::warn!("SetWindowPos failed: {e}");
        }
        // SAFETY: plain FFI call on our own window.
        let dpi = match unsafe { GetDpiForWindow(self.hwnd) } {
            0 => 96.0,
            d => d as f32,
        };
        self.scale = dpi / 96.0;
        self.size = (w as u32, h as u32);
        self.layout = Layout::new(w as f32 / self.scale, h as f32 / self.scale);
        self.model.columns = self.layout.columns;
        self.model.ensure_visible(self.layout.visible_rows());
        self.gfx = None;
        match Gfx::new(self.hwnd, self.size.0, self.size.1, dpi) {
            Ok(g) => {
                let _ = g.set_opacity(self.last_opacity);
                self.gfx = Some(g);
                true
            }
            Err(e) => {
                tracing::error!("overlay graphics setup failed: {e}");
                false
            }
        }
    }

    fn show(&mut self) {
        let now = Instant::now();
        match self.phase {
            Phase::Shown => return,
            Phase::Hiding => {
                self.phase = Phase::Shown;
                self.fade = self.fade.retarget(1.0, now, FADE_IN);
                system::bring_to_front(self.hwnd);
                return;
            }
            Phase::Hidden => {}
        }
        // SAFETY: plain FFI call.
        self.previous = unsafe { GetForegroundWindow() };
        self.last_opacity = 0.0;
        if !self.place() {
            return;
        }
        match (self.backends.audio)() {
            Ok(a) => {
                self.audio = Some(a);
                self.audio_error = None;
            }
            Err(e) => {
                tracing::warn!("overlay audio unavailable: {e}");
                self.audio = None;
                self.audio_error = Some(e.to_string());
            }
        }
        self.stats = Some((self.backends.stats)());
        self.model.reset();
        self.refresh_tasks();
        self.refresh_volume();
        self.refresh_stats();

        self.phase = Phase::Shown;
        self.fade = Fade::new(0.0, 1.0, now, FADE_IN);
        self.render();
        // SAFETY: showing our own window.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
        }
        if !system::bring_to_front(self.hwnd) {
            tracing::warn!(
                "overlay could not take the foreground; keyboard input may not reach it"
            );
        }
        self.gamepad.prime(&self.pads.poll());
        // SAFETY: timers on our own window.
        unsafe { SetTimer(Some(self.hwnd), TIMER_FRAME, FRAME_MS, None) };
        self.start_sample_timer();
        self.visible.store(true, Ordering::Relaxed);
        tracing::info!("overlay shown");
    }

    fn start_sample_timer(&self) {
        let ms = self
            .settings
            .lock()
            .map(|s| s.sample_interval.as_millis())
            .unwrap_or(1000)
            .clamp(100, 60_000) as u32;
        // SAFETY: (re)arming a timer on our own window.
        unsafe { SetTimer(Some(self.hwnd), TIMER_SAMPLE, ms, None) };
    }

    /// Start fading out. With `restore_focus`, give the foreground back to
    /// the window that had it before the overlay opened.
    fn begin_hide(&mut self, restore_focus: bool) {
        if self.phase != Phase::Shown {
            return;
        }
        self.phase = Phase::Hiding;
        self.model.confirm = None;
        self.fade = self.fade.retarget(0.0, Instant::now(), FADE_OUT);
        // SAFETY: plain FFI calls on window handles; a stale one fails.
        let restore = restore_focus
            && unsafe { GetForegroundWindow() } == self.hwnd
            && unsafe { IsWindow(Some(self.previous)) }.as_bool();
        if restore {
            system::bring_to_front(self.previous);
        }
    }

    /// Hide the window and release everything opened by `show`.
    fn finish_hide(&mut self) {
        // SAFETY: our own window and timers.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            for t in [TIMER_FRAME, TIMER_SAMPLE, TIMER_REFRESH] {
                let _ = KillTimer(Some(self.hwnd), t);
            }
        }
        self.thumbs.clear();
        self.gfx = None;
        self.audio = None;
        self.stats = None;
        self.model.confirm = None;
        if self.phase != Phase::Hidden {
            tracing::info!("overlay hidden");
        }
        self.phase = Phase::Hidden;
        self.visible.store(false, Ordering::Relaxed);
    }

    fn frame(&mut self) {
        let now = Instant::now();
        if self.phase == Phase::Shown {
            let pads = self.pads.poll();
            for a in self.gamepad.update(&pads, now) {
                self.act(a);
                if self.phase != Phase::Shown {
                    break;
                }
            }
        }
        let opacity = self.fade.value(now);
        let fading = opacity != self.last_opacity;
        if fading {
            self.last_opacity = opacity;
            if let Some(g) = &self.gfx
                && let Err(e) = g.set_opacity(opacity)
            {
                tracing::warn!("overlay opacity failed: {e}");
            }
        }
        if self.dirty {
            self.render();
        } else if fading {
            self.sync_thumbs();
        }
        if self.phase == Phase::Hiding && self.fade.done(now) {
            self.finish_hide();
        }
    }

    fn render(&mut self) {
        self.dirty = false;
        let Some(g) = &self.gfx else { return };
        if let Err(e) = g.draw(&self.model, &self.layout) {
            // Device lost (driver update, GPU reset, display change): rebuild.
            tracing::warn!("overlay draw failed ({e}); recreating graphics");
            if self.place()
                && let Some(g) = &self.gfx
            {
                let _ = g.draw(&self.model, &self.layout);
            }
        }
        self.sync_thumbs();
    }

    fn sync_thumbs(&mut self) {
        let slots: Vec<_> = if self.phase != Phase::Hidden
            && self.model.tab == Tab::Tasks
            && self.model.confirm.is_none()
        {
            self.layout
                .tiles(self.model.tasks.len(), self.model.scroll_row)
                .into_iter()
                .map(|t| (self.model.tasks[t.index].hwnd, t.thumb))
                .collect()
        } else {
            Vec::new()
        };
        self.thumbs
            .sync(self.hwnd, &slots, self.scale, self.last_opacity);
    }

    fn sample(&mut self) {
        self.refresh_tasks();
        self.refresh_volume();
        self.refresh_stats();
    }

    fn refresh_tasks(&mut self) {
        self.model.set_tasks(system::list_tasks(self.hwnd));
        self.model.ensure_visible(self.layout.visible_rows());
        self.dirty = true;
    }

    fn refresh_stats(&mut self) {
        if let Some(s) = &mut self.stats {
            let pids: Vec<u32> = self.model.tasks.iter().map(|t| t.pid).collect();
            self.model.stats = s.sample(&pids);
            self.dirty = true;
        }
    }

    fn refresh_volume(&mut self) {
        let volume = match &self.audio {
            None => Volume {
                error: self.audio_error.clone(),
                ..Volume::default()
            },
            Some(a) => {
                let master = a.volume().and_then(|level| {
                    Ok(Master {
                        level,
                        muted: a.muted()?,
                    })
                });
                match master {
                    Ok(m) => Volume {
                        master: Some(m),
                        apps: a.sessions().unwrap_or_default(),
                        error: None,
                    },
                    Err(uc_audio::Error::NoDefaultDevice) => Volume::default(),
                    Err(e) => Volume {
                        error: Some(e.to_string()),
                        ..Volume::default()
                    },
                }
            }
        };
        self.model.set_volume(volume);
        self.dirty = true;
    }

    fn act(&mut self, action: Action) {
        if self.phase != Phase::Shown {
            return;
        }
        let effects = self.model.handle(action);
        self.model.ensure_visible(self.layout.visible_rows());
        self.dirty = true;
        for e in effects {
            self.apply(e);
        }
    }

    fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Hide => self.begin_hide(true),
            Effect::SwitchTo(h) => {
                system::switch_to(HWND(h as _));
                self.begin_hide(false);
            }
            Effect::CloseWindow(h) => {
                system::close_window(HWND(h as _));
                self.schedule_refresh();
            }
            Effect::KillProcess(pid) => {
                system::kill_process(pid);
                self.schedule_refresh();
            }
            Effect::SetMasterVolume(v) => self.audio_call(|a| a.set_volume(v)),
            Effect::SetMasterMuted(m) => self.audio_call(|a| a.set_muted(m)),
            Effect::SetAppVolume { pid, level } => {
                self.audio_call(|a| a.set_app_volume(pid, level))
            }
            Effect::SetAppMuted { pid, muted } => self.audio_call(|a| a.set_app_muted(pid, muted)),
        }
    }

    fn audio_call(&self, f: impl FnOnce(&dyn uc_audio::AudioControl) -> uc_audio::Result<()>) {
        if let Some(a) = &self.audio
            && let Err(e) = f(a.as_ref())
        {
            tracing::warn!("overlay volume change failed: {e}");
        }
    }

    fn schedule_refresh(&self) {
        // SAFETY: one-shot timer on our own window (killed when it fires).
        unsafe { SetTimer(Some(self.hwnd), TIMER_REFRESH, REFRESH_MS, None) };
    }

    fn click(&mut self, x: f32, y: f32) {
        let hit = self.layout.hit(
            x,
            y,
            self.model.tab,
            self.model.tasks.len(),
            self.model.scroll_row,
        );
        match hit {
            Hit::Tab(i) if self.model.confirm.is_none() => self.act(Action::SelectTab(i)),
            Hit::Tile(i) if self.model.confirm.is_none() => self.act(Action::ActivateTask(i)),
            Hit::Outside => self.begin_hide(true),
            _ => {}
        }
    }
}

fn key_action(vk: VIRTUAL_KEY) -> Option<Action> {
    // SAFETY: plain FFI call.
    let shift = unsafe { GetKeyState(i32::from(VK_SHIFT.0)) } < 0;
    Some(match vk {
        VK_UP => Action::Up,
        VK_DOWN => Action::Down,
        VK_LEFT => Action::Left,
        VK_RIGHT => Action::Right,
        VK_RETURN | VK_SPACE => Action::Accept,
        VK_ESCAPE | VK_BACK => Action::Back,
        VK_TAB if shift => Action::PrevTab,
        VK_TAB | VK_NEXT => Action::NextTab,
        VK_PRIOR => Action::PrevTab,
        VK_DELETE if shift => Action::KillTask,
        VK_DELETE => Action::CloseTask,
        VK_OEM_PLUS | VK_ADD => Action::VolumeUp,
        VK_OEM_MINUS | VK_SUBTRACT => Action::VolumeDown,
        VIRTUAL_KEY(k) if k == u16::from(b'M') => Action::ToggleMute,
        VIRTUAL_KEY(k) if (u16::from(b'1')..=u16::from(b'3')).contains(&k) => {
            Action::SelectTab(usize::from(k - u16::from(b'1')))
        }
        _ => return None,
    })
}
