//! Hides the mouse pointer in gaming mode while the mouse is still, and
//! brings it back the moment the mouse moves.
//!
//! Built to cost nothing while you play, and without a low-level mouse hook
//! (those sit in the input path of every mouse event and cause stutter):
//!
//! - **Pointer shown:** a 250 ms window timer compares `GetCursorPos`. After
//!   `cursor.hide_after_secs` without a change the pointer is hidden.
//! - **Pointer hidden:** the agent registers for raw mouse input
//!   (`RIDEV_INPUTSINK`, which only copies events to us and never delays
//!   them for the game). Real movement (more than sensor jitter), a click
//!   or the wheel shows the pointer again and unregisters. The same timer
//!   keeps watching `GetCursorPos` as a backup: Windows withholds raw input
//!   from a normal process while an elevated (admin) window has the focus,
//!   which some games and launchers are.
//!
//! Hiding swaps every system cursor for a blank one (`SetSystemCursor`), so
//! it covers every app that uses the standard pointers. Windows reloads the
//! real pointers on some display and settings changes, so while hidden the
//! blank ones are put back after those and every few seconds. Showing
//! reloads the user's cursor scheme (`SPI_SETCURSORS`). The agent also
//! reloads it at start, so a crash while hidden can't leave the pointer
//! invisible.

use std::time::{Duration, Instant};

/// How often the pointer position is checked.
pub const POLL: Duration = Duration::from_millis(250);
/// While hidden, the blank pointers are put back every this many checks.
pub const REBLANK_EVERY: u32 = 20;
/// Relative raw-input counts (|dx| + |dy|) that count as moving the mouse.
/// A resting optical sensor can report a count or two of jitter.
const WAKE_COUNTS: i64 = 8;
/// The same for absolute devices (touchpads, pens, remote desktop), whose
/// coordinates run 0..=65535 across the screen.
const WAKE_ABSOLUTE: i64 = 300;
/// Jitter adds up only while it keeps coming; a pause this long starts over.
const WAKE_WINDOW: Duration = Duration::from_millis(500);
/// Pointer travel in pixels that counts as moving, for the backup check.
const WAKE_PIXELS: i64 = 3;

/// One raw mouse report.
#[derive(Debug, Clone, Copy)]
pub enum Motion {
    Relative { dx: i32, dy: i32 },
    Absolute { x: i32, y: i32 },
}

/// Decides whether raw mouse input while hidden is the user moving the
/// mouse, not a resting sensor's jitter.
#[derive(Debug, Default)]
pub struct Wake {
    travel: i64,
    last: Option<Instant>,
    origin: Option<(i32, i32)>,
}

impl Wake {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Takes one report; true when the pointer should come back.
    pub fn input(&mut self, motion: Motion, buttons: bool, now: Instant) -> bool {
        if buttons {
            return true;
        }
        if self
            .last
            .is_some_and(|t| now.duration_since(t) > WAKE_WINDOW)
        {
            self.reset();
        }
        self.last = Some(now);
        match motion {
            Motion::Relative { dx, dy } => {
                self.travel += i64::from(dx).abs() + i64::from(dy).abs();
                self.travel >= WAKE_COUNTS
            }
            Motion::Absolute { x, y } => {
                let (ox, oy) = *self.origin.get_or_insert((x, y));
                distance((ox, oy), (x, y)) >= WAKE_ABSOLUTE
            }
        }
    }
}

/// Whether the pointer moved far enough between two positions to count.
pub fn moved(from: (i32, i32), to: (i32, i32)) -> bool {
    distance(from, to) >= WAKE_PIXELS
}

fn distance(a: (i32, i32), b: (i32, i32)) -> i64 {
    (i64::from(a.0) - i64::from(b.0)).abs() + (i64::from(a.1) - i64::from(b.1)).abs()
}

/// Decides when a still pointer should be hidden, from position samples.
#[derive(Debug)]
pub struct Idle {
    after: Duration,
    last: Option<(i32, i32)>,
    since: Instant,
}

impl Idle {
    pub fn new(after: Duration, now: Instant) -> Self {
        Self {
            after,
            last: None,
            since: now,
        }
    }

    /// Starts the still time again (the pointer was just shown).
    pub fn reset(&mut self, now: Instant) {
        self.last = None;
        self.since = now;
    }

    /// Takes a pointer position; true once it has stayed put long enough.
    pub fn sample(&mut self, pos: (i32, i32), now: Instant) -> bool {
        if self.last != Some(pos) {
            self.last = Some(pos);
            self.since = now;
            return false;
        }
        now.duration_since(self.since) >= self.after
    }
}

#[cfg(windows)]
pub use win::{Hider, TIMER_ID};

#[cfg(windows)]
mod win {
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{HWND, LPARAM, POINT};
    use windows::Win32::UI::Input::{
        GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
        RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEMOUSE, RegisterRawInputDevices,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateCursor, GetCursorPos, KillTimer, OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP,
        OCR_IBEAM, OCR_NO, OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE,
        OCR_SIZEWE, OCR_UP, OCR_WAIT, SPI_SETCURSORS, SYSTEM_CURSOR_ID,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetSystemCursor, SetTimer, SystemParametersInfoW,
    };

    use super::{Idle, Motion, POLL, REBLANK_EVERY, Wake, moved};

    /// Window timer id used for the position checks.
    pub const TIMER_ID: usize = 0x5543;

    const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
    const HID_USAGE_GENERIC_MOUSE: u16 = 0x02;

    /// Every system cursor that gets blanked (OCR_PIN/OCR_PERSON are only
    /// on newer Windows and are rarely shown; leaving them is harmless).
    const CURSORS: [SYSTEM_CURSOR_ID; 14] = [
        OCR_NORMAL,
        OCR_IBEAM,
        OCR_WAIT,
        OCR_CROSS,
        OCR_UP,
        OCR_SIZENWSE,
        OCR_SIZENESW,
        OCR_SIZEWE,
        OCR_SIZENS,
        OCR_SIZEALL,
        OCR_NO,
        OCR_HAND,
        OCR_APPSTARTING,
        OCR_HELP,
    ];

    /// Lives on the window thread; driven by its messages.
    pub struct Hider {
        idle: Option<Idle>,
        /// `Some` while hidden.
        hidden: Option<Hidden>,
    }

    struct Hidden {
        /// Pointer position when it was hidden (backup check).
        at: Option<(i32, i32)>,
        wake: Wake,
        ticks: u32,
    }

    impl Hider {
        pub fn start() -> Self {
            // A previous agent may have crashed with the pointer hidden.
            restore_cursors();
            Self {
                idle: None,
                hidden: None,
            }
        }

        /// `Some(delay)` turns auto-hide on (gaming mode), `None` turns it
        /// off and shows the pointer. Cheap to repeat.
        pub fn set(&mut self, hwnd: HWND, after: Option<Duration>) {
            match after {
                Some(after) => {
                    let now = Instant::now();
                    match &mut self.idle {
                        Some(idle) if idle.after == after => return,
                        Some(idle) => idle.after = after,
                        None => self.idle = Some(Idle::new(after, now)),
                    }
                    start_timer(hwnd);
                    tracing::info!(?after, "pointer auto-hide on");
                }
                None => {
                    if self.idle.take().is_none() {
                        return;
                    }
                    stop_timer(hwnd);
                    self.show(hwnd);
                    tracing::info!("pointer auto-hide off");
                }
            }
        }

        /// `WM_TIMER` with [`TIMER_ID`].
        pub fn on_timer(&mut self, hwnd: HWND) {
            if self.idle.is_none() {
                stop_timer(hwnd);
                return;
            }
            let pos = cursor_pos();
            if let Some(hidden) = &mut self.hidden {
                hidden.ticks += 1;
                let back = match (hidden.at, pos) {
                    (Some(at), Some(p)) => moved(at, p),
                    (None, p) => {
                        hidden.at = p;
                        false
                    }
                    (_, None) => false,
                };
                if back {
                    tracing::debug!("pointer moved without raw input (elevated window?)");
                    self.show(hwnd);
                } else if hidden.ticks % REBLANK_EVERY == 0 {
                    blank_cursors();
                }
                return;
            }
            // GetCursorPos fails on the secure desktop (UAC, lock screen);
            // skip that sample.
            let Some(p) = pos else { return };
            let still = self
                .idle
                .as_mut()
                .is_some_and(|idle| idle.sample(p, Instant::now()));
            if still {
                self.hide(hwnd, p);
            }
        }

        /// `WM_INPUT`: raw mouse input, which only arrives while hidden.
        pub fn on_input(&mut self, hwnd: HWND, lparam: LPARAM) {
            let Some(hidden) = &mut self.hidden else {
                return;
            };
            let Some((motion, buttons)) = read_mouse(lparam) else {
                return;
            };
            if hidden.wake.input(motion, buttons, Instant::now()) {
                self.show(hwnd);
            }
        }

        /// Windows may have reloaded the real pointers (display mode, DPI or
        /// settings change, resume): blank them again if hidden.
        pub fn refresh(&self) {
            if self.hidden.is_some() {
                blank_cursors();
            }
        }

        fn hide(&mut self, hwnd: HWND, at: (i32, i32)) {
            // Listen first, so a move right after hiding still shows it.
            if let Err(e) = raw_mouse(hwnd, true) {
                // The position check alone still brings it back.
                tracing::warn!("raw mouse input unavailable, using position checks only: {e}");
            }
            blank_cursors();
            self.hidden = Some(Hidden {
                at: Some(at),
                wake: Wake::default(),
                ticks: 0,
            });
            tracing::debug!("pointer hidden");
        }

        fn show(&mut self, hwnd: HWND) {
            if self.hidden.take().is_none() {
                return;
            }
            restore_cursors();
            if let Err(e) = raw_mouse(hwnd, false) {
                tracing::debug!("raw mouse input not removed: {e}");
            }
            if let Some(idle) = &mut self.idle {
                idle.reset(Instant::now());
            }
            tracing::debug!("pointer shown");
        }
    }

    impl Drop for Hider {
        fn drop(&mut self) {
            if self.hidden.is_some() {
                restore_cursors();
            }
        }
    }

    fn cursor_pos() -> Option<(i32, i32)> {
        let mut p = POINT::default();
        // SAFETY: plain FFI call with a valid out pointer.
        unsafe { GetCursorPos(&mut p) }.ok().map(|()| (p.x, p.y))
    }

    /// The mouse report in a `WM_INPUT`, and whether it has a button or
    /// wheel change. `None` for anything else.
    fn read_mouse(lparam: LPARAM) -> Option<(Motion, bool)> {
        let mut raw = RAWINPUT::default();
        let mut size = size_of::<RAWINPUT>() as u32;
        // SAFETY: the buffer is a whole RAWINPUT, which holds any mouse
        // report; `size` says how big it is. The handle comes from WM_INPUT.
        let got = unsafe {
            GetRawInputData(
                HRAWINPUT(lparam.0 as *mut _),
                RID_INPUT,
                Some((&mut raw as *mut RAWINPUT).cast()),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if got == u32::MAX || got == 0 || raw.header.dwType != RIM_TYPEMOUSE.0 {
            return None;
        }
        // SAFETY: dwType says the union holds a mouse report.
        let mouse = unsafe { raw.data.mouse };
        // SAFETY: both union views are plain integers.
        let buttons = unsafe { mouse.Anonymous.Anonymous.usButtonFlags } != 0;
        let (x, y) = (mouse.lLastX, mouse.lLastY);
        let motion = if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
            Motion::Absolute { x, y }
        } else {
            Motion::Relative { dx: x, dy: y }
        };
        Some((motion, buttons))
    }

    fn start_timer(hwnd: HWND) {
        // SAFETY: a timer on our own window; it posts WM_TIMER to it.
        let _ = unsafe { SetTimer(Some(hwnd), TIMER_ID, POLL.as_millis() as u32, None) };
    }

    fn stop_timer(hwnd: HWND) {
        // SAFETY: as above; fails harmlessly if there is no timer.
        let _ = unsafe { KillTimer(Some(hwnd), TIMER_ID) };
    }

    fn raw_mouse(hwnd: HWND, on: bool) -> windows::core::Result<()> {
        let device = RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_MOUSE,
            dwFlags: if on { RIDEV_INPUTSINK } else { RIDEV_REMOVE },
            hwndTarget: if on { hwnd } else { HWND::default() },
        };
        // SAFETY: one valid RAWINPUTDEVICE with its own size.
        unsafe { RegisterRawInputDevices(&[device], size_of::<RAWINPUTDEVICE>() as u32) }
    }

    /// Replaces every system cursor with a fully transparent one.
    fn blank_cursors() {
        // 32x32 monochrome: AND mask all ones (keep the screen), XOR all
        // zeros (change nothing), so nothing is drawn.
        const AND: [u8; 128] = [0xff; 128];
        const XOR: [u8; 128] = [0x00; 128];
        for id in CURSORS {
            // SAFETY: the masks are 32x32 bits each. SetSystemCursor takes
            // ownership of the new cursor (and destroys it), so one per id.
            let done = unsafe {
                CreateCursor(None, 0, 0, 32, 32, AND.as_ptr().cast(), XOR.as_ptr().cast())
                    .and_then(|c| SetSystemCursor(c, id))
            };
            if let Err(e) = done {
                tracing::debug!(id = id.0, "cursor not blanked: {e}");
            }
        }
    }

    /// Reloads the user's cursor scheme, undoing [`blank_cursors`].
    fn restore_cursors() {
        // SAFETY: SPI_SETCURSORS takes no buffer; 0 = don't write or broadcast.
        if let Err(e) = unsafe {
            SystemParametersInfoW(
                SPI_SETCURSORS,
                0,
                None,
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        } {
            tracing::warn!("could not restore the mouse pointers: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_only_after_staying_still_long_enough() {
        let t0 = Instant::now();
        let s = |ms| t0 + Duration::from_millis(ms);
        let mut idle = Idle::new(Duration::from_secs(3), t0);
        assert!(!idle.sample((10, 10), s(0)));
        assert!(!idle.sample((10, 10), s(2_000)));
        // Moved: the still time starts again.
        assert!(!idle.sample((11, 10), s(2_500)));
        assert!(!idle.sample((11, 10), s(5_000)));
        assert!(idle.sample((11, 10), s(5_500)));
    }

    #[test]
    fn jitter_does_not_wake_but_movement_and_clicks_do() {
        let t0 = Instant::now();
        let s = |ms| t0 + Duration::from_millis(ms);
        let rel = |dx, dy| Motion::Relative { dx, dy };
        let mut wake = Wake::default();
        // A count of jitter now and then, never adding up.
        for i in 0..20 {
            assert!(!wake.input(rel(1, 0), false, s(i * 1_000)));
        }
        // A real push.
        assert!(!wake.input(rel(3, 1), false, s(30_000)));
        assert!(wake.input(rel(4, -2), false, s(30_008)));
        wake.reset();
        assert!(wake.input(rel(0, 0), true, s(31_000)));
    }

    #[test]
    fn absolute_devices_wake_on_distance() {
        let t0 = Instant::now();
        let abs = |x, y| Motion::Absolute { x, y };
        let mut wake = Wake::default();
        assert!(!wake.input(abs(30_000, 30_000), false, t0));
        assert!(!wake.input(abs(30_050, 30_000), false, t0));
        assert!(wake.input(abs(30_400, 30_000), false, t0));
    }

    #[test]
    fn backup_check_ignores_a_pixel_of_drift() {
        assert!(!moved((100, 100), (100, 100)));
        assert!(!moved((100, 100), (101, 101)));
        assert!(moved((100, 100), (103, 100)));
        assert!(moved((i32::MAX, 0), (i32::MIN, 0)));
    }

    #[test]
    fn reset_needs_a_fresh_still_period() {
        let t0 = Instant::now();
        let s = |ms| t0 + Duration::from_millis(ms);
        let mut idle = Idle::new(Duration::from_secs(1), t0);
        assert!(!idle.sample((0, 0), s(0)));
        assert!(idle.sample((0, 0), s(1_000)));
        idle.reset(s(1_000));
        assert!(!idle.sample((0, 0), s(1_250)));
        assert!(!idle.sample((0, 0), s(2_000)));
        assert!(idle.sample((0, 0), s(2_250)));
    }
}
