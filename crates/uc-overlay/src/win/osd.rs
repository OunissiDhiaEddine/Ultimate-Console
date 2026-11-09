//! The volume popup window (see [`crate::osd`]). It lives on the overlay's
//! UI thread; its timers are set on the overlay's main window, which forwards
//! them here.
//!
//! It never takes focus (`WS_EX_NOACTIVATE`, shown with `SWP_NOACTIVATE`), so
//! the game or Big Picture keeps the keyboard and controller.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetForegroundWindow, HWND_TOPMOST, IsIconic,
    KillTimer, MA_NOACTIVATE, RegisterClassExW, SW_HIDE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SetTimer, SetWindowPos, ShowWindow, WM_MOUSEACTIVATE, WNDCLASSEXW,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::w;

use super::render::Gfx;
use super::system;
use crate::anim::{FADE_IN, FADE_OUT, Fade};
use crate::osd::{self, Parts, View, VolumeKey};
use crate::{AudioFactory, Error, Result};

/// Timer ids on the overlay's main window.
pub const TIMER_OSD_FRAME: usize = 10;
pub const TIMER_OSD_HIDE: usize = 11;
const FRAME_MS: u32 = 16;
/// How long after opening a game minimising itself counts as the popup's doing.
const KNOCKOUT_GRACE: Duration = Duration::from_millis(1500);

pub struct Osd {
    hwnd: HWND,
    /// The overlay's main window, which owns the timers.
    timers: HWND,
    audio_factory: AudioFactory,
    audio: Option<Box<dyn uc_audio::AudioControl>>,
    gfx: Option<Gfx>,
    parts: Parts,
    view: View,
    fade: Fade,
    last_opacity: f32,
    /// On screen or fading out.
    shown: bool,
    /// The window in front when the popup opened, and when.
    front: HWND,
    opened_at: Instant,
    /// A window that dropped out of fullscreen because of the popup: no
    /// popup over it again (just the volume change).
    no_popup_over: HWND,
}

impl Osd {
    pub fn new(timers: HWND, audio_factory: AudioFactory) -> Result<Self> {
        Ok(Self {
            hwnd: create_window()?,
            timers,
            audio_factory,
            audio: None,
            gfx: None,
            parts: Parts::new(osd::WIDTH, osd::HEIGHT),
            view: View::Level {
                level: 0.0,
                muted: false,
            },
            fade: Fade::settled(0.0, Instant::now()),
            last_opacity: 0.0,
            shown: false,
            front: HWND::default(),
            opened_at: Instant::now(),
            no_popup_over: HWND::default(),
        })
    }

    /// A volume key was pressed: change the volume and show the popup.
    pub fn key(&mut self, key: VolumeKey) {
        self.view = self.change_volume(key);
        // SAFETY: plain FFI call.
        let front = unsafe { GetForegroundWindow() };
        if !self.shown && (front == self.no_popup_over || system::exclusive_fullscreen()) {
            // Any window on top of a true exclusive-fullscreen game knocks it
            // out of fullscreen (focus or not), so just change the volume.
            tracing::debug!("exclusive-fullscreen game in front: volume popup skipped");
            return;
        }
        if !self.shown {
            if !self.open() {
                return;
            }
        } else if self.fade.target() < 1.0 {
            // Pressed again while fading out: come back.
            self.fade = self.fade.retarget(1.0, Instant::now(), FADE_IN);
            // SAFETY: a timer on our own window.
            unsafe { SetTimer(Some(self.timers), TIMER_OSD_FRAME, FRAME_MS, None) };
        }
        self.draw();
        // (Re)start the countdown to fading out.
        // SAFETY: timers on our own windows.
        unsafe { SetTimer(Some(self.timers), TIMER_OSD_HIDE, osd::SHOW_FOR_MS, None) };
    }

    fn change_volume(&mut self, key: VolumeKey) -> View {
        if self.audio.is_none() {
            match (self.audio_factory)() {
                Ok(a) => self.audio = Some(a),
                Err(e) => {
                    tracing::warn!("volume key: audio unavailable: {e}");
                    return View::Unavailable(e.to_string());
                }
            }
        }
        let Some(a) = &self.audio else {
            return View::Unavailable("no audio".into());
        };
        let result = (|| {
            let (level, muted) = key.apply(a.volume()?, a.muted()?);
            match key {
                VolumeKey::Mute => a.set_muted(muted)?,
                VolumeKey::Up | VolumeKey::Down => {
                    a.set_volume(level)?;
                    if a.muted()? {
                        a.set_muted(false)?;
                    }
                }
            }
            // Read back what Windows actually has now.
            Ok::<_, uc_audio::Error>(View::Level {
                level: a.volume()?,
                muted: a.muted()?,
            })
        })();
        match result {
            Ok(v) => {
                tracing::debug!(?key, ?v, "volume key");
                v
            }
            Err(e) => {
                tracing::warn!(?key, "volume key failed: {e}");
                // The default device may have changed: start over next time.
                self.audio = None;
                match e {
                    uc_audio::Error::NoDefaultDevice => {
                        View::Unavailable("no output device".into())
                    }
                    e => View::Unavailable(e.to_string()),
                }
            }
        }
    }

    /// Place the window on the foreground window's monitor, build the GPU
    /// objects and start fading in.
    fn open(&mut self) -> bool {
        let mon = system::foreground_monitor();
        // SAFETY: plain FFI call.
        self.front = unsafe { GetForegroundWindow() };
        self.opened_at = Instant::now();
        // SAFETY: plain FFI call on our own window. Its DPI follows the
        // monitor it is on, so position it there first, then size it.
        let place = |x, y, w, h| unsafe {
            SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE)
        };
        let _ = place(mon.left, mon.top, 1, 1);
        // SAFETY: plain FFI call on our own window.
        let dpi = match unsafe { GetDpiForWindow(self.hwnd) } {
            0 => 96.0,
            d => d as f32,
        };
        let scale = dpi / 96.0;
        let screen_w = (mon.right - mon.left) as f32 / scale;
        let screen_h = (mon.bottom - mon.top) as f32 / scale;
        let r = osd::window_rect(screen_w, screen_h);
        self.parts = Parts::new(r.w, r.h);
        let px = r.scaled(scale);
        let (w, h) = ((px.w.round() as i32).max(1), (px.h.round() as i32).max(1));
        if let Err(e) = place(
            mon.left + px.x.round() as i32,
            mon.top + px.y.round() as i32,
            w,
            h,
        ) {
            tracing::warn!("volume popup SetWindowPos failed: {e}");
        }
        self.gfx = match Gfx::new(self.hwnd, w as u32, h as u32, dpi) {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::error!("volume popup graphics setup failed: {e}");
                return false;
            }
        };
        self.last_opacity = 0.0;
        self.fade = Fade::new(0.0, 1.0, Instant::now(), FADE_IN);
        self.shown = true;
        // SAFETY: showing our own window without activating it, and a timer
        // on our own window.
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOMOVE | SWP_NOSIZE,
            );
            SetTimer(Some(self.timers), TIMER_OSD_FRAME, FRAME_MS, None);
        }
        true
    }

    fn draw(&mut self) {
        let Some(g) = &self.gfx else { return };
        if let Err(e) = g.draw_osd(&self.view, &self.parts) {
            tracing::warn!("volume popup draw failed ({e}); closing it");
            self.close();
        }
    }

    /// A timer set by this popup fired on the main window.
    pub fn on_timer(&mut self, id: usize) {
        match id {
            TIMER_OSD_HIDE => {
                // SAFETY: killing our own one-shot timer.
                let _ = unsafe { KillTimer(Some(self.timers), TIMER_OSD_HIDE) };
                if self.shown {
                    self.fade = self.fade.retarget(0.0, Instant::now(), FADE_OUT);
                    // SAFETY: as above.
                    unsafe { SetTimer(Some(self.timers), TIMER_OSD_FRAME, FRAME_MS, None) };
                }
            }
            TIMER_OSD_FRAME => self.frame(),
            _ => {}
        }
    }

    fn frame(&mut self) {
        let now = Instant::now();
        let watching = now.duration_since(self.opened_at) < KNOCKOUT_GRACE;
        // SAFETY: plain FFI call; a stale handle just reads as not iconic.
        if self.shown && watching && unsafe { IsIconic(self.front) }.as_bool() {
            // A true exclusive-fullscreen game minimises when any window is
            // on top of it. Get out of its way and give it back.
            tracing::info!("the game left fullscreen under the volume popup; restoring it");
            self.no_popup_over = self.front;
            self.close();
            system::switch_to(self.front);
            return;
        }
        let opacity = self.fade.value(now);
        if opacity != self.last_opacity {
            self.last_opacity = opacity;
            if let Some(g) = &self.gfx
                && let Err(e) = g.set_opacity(opacity)
            {
                tracing::warn!("volume popup opacity failed: {e}");
            }
        }
        if self.fade.done(now) && !watching {
            // SAFETY: the fade is over; stop the frame timer until the next one.
            let _ = unsafe { KillTimer(Some(self.timers), TIMER_OSD_FRAME) };
            if self.fade.target() == 0.0 {
                self.close();
            }
        }
    }

    /// Hide now and release the GPU objects and the audio client.
    pub fn close(&mut self) {
        // SAFETY: our own window and timers.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            for t in [TIMER_OSD_FRAME, TIMER_OSD_HIDE] {
                let _ = KillTimer(Some(self.timers), t);
            }
        }
        self.gfx = None;
        self.audio = None;
        self.shown = false;
        self.last_opacity = 0.0;
        self.fade = Fade::settled(0.0, Instant::now());
    }
}

impl Drop for Osd {
    fn drop(&mut self) {
        self.close();
        // SAFETY: destroying our own window on the thread that created it.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

fn create_window() -> Result<HWND> {
    let class = w!("UltimateConsoleVolumePopup");
    // SAFETY: the class struct and strings are valid for the calls; the
    // window procedure has the right signature.
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| Error::win("GetModuleHandleW", e))?;
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        // Fails harmlessly with "class already exists" for a second overlay.
        RegisterClassExW(&wc);
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
            class,
            w!("Ultimate Console volume"),
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
        .map_err(|e| Error::win("CreateWindowExW (volume popup)", e))
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        // A click on the popup must not take focus from the game.
        return LRESULT(MA_NOACTIVATE as isize);
    }
    // SAFETY: forwarding the original arguments.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}
