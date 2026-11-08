//! Hides the mouse pointer in gaming mode while the mouse is still, and
//! brings it back the moment the mouse moves.
//!
//! Built to cost nothing while you play, and without a low-level mouse hook
//! (those sit in the input path of every mouse event and cause stutter):
//!
//! - **Pointer shown:** a 250 ms window timer compares `GetCursorPos`. After
//!   `cursor.hide_after_secs` without a change the pointer is hidden.
//! - **Pointer hidden:** no timer at all. The agent registers for raw mouse
//!   input (`RIDEV_INPUTSINK`, which only copies events to us and never
//!   delays them for the game) and the first `WM_INPUT` shows the pointer
//!   again and unregisters.
//!
//! Hiding swaps every system cursor for a blank one (`SetSystemCursor`), so
//! it covers every app that uses the standard pointers. Showing reloads the
//! user's cursor scheme (`SPI_SETCURSORS`). The agent also reloads it at
//! start, so a crash while hidden can't leave the pointer invisible.

use std::time::{Duration, Instant};

/// How often the pointer position is checked while it is shown.
pub const POLL: Duration = Duration::from_millis(250);

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

    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::UI::Input::{
        RAWINPUTDEVICE, RIDEV_INPUTSINK, RIDEV_REMOVE, RegisterRawInputDevices,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateCursor, GetCursorPos, KillTimer, OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP,
        OCR_IBEAM, OCR_NO, OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE,
        OCR_SIZEWE, OCR_UP, OCR_WAIT, SPI_SETCURSORS, SYSTEM_CURSOR_ID,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetSystemCursor, SetTimer, SystemParametersInfoW,
    };

    use super::{Idle, POLL};

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
        hidden: bool,
    }

    impl Hider {
        pub fn start() -> Self {
            // A previous agent may have crashed with the pointer hidden.
            restore_cursors();
            Self {
                idle: None,
                hidden: false,
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
                    if !self.hidden {
                        start_timer(hwnd);
                    }
                    tracing::info!(?after, "pointer auto-hide on");
                }
                None => {
                    if self.idle.take().is_none() {
                        return;
                    }
                    stop_timer(hwnd);
                    if self.hidden {
                        self.show(hwnd);
                    }
                    tracing::info!("pointer auto-hide off");
                }
            }
        }

        /// `WM_TIMER` with [`TIMER_ID`].
        pub fn on_timer(&mut self, hwnd: HWND) {
            let Some(idle) = &mut self.idle else {
                stop_timer(hwnd);
                return;
            };
            if self.hidden {
                return;
            }
            let mut p = POINT::default();
            // SAFETY: plain FFI call with a valid out pointer. It fails on
            // the secure desktop (UAC, lock screen); skip that sample.
            if unsafe { GetCursorPos(&mut p) }.is_err() {
                return;
            }
            let still = idle.sample((p.x, p.y), Instant::now());
            if still {
                self.hide(hwnd);
            }
        }

        /// `WM_INPUT`: the mouse did something while the pointer was hidden.
        pub fn on_input(&mut self, hwnd: HWND) {
            if self.hidden {
                self.show(hwnd);
                if self.idle.is_some() {
                    start_timer(hwnd);
                }
            }
        }

        fn hide(&mut self, hwnd: HWND) {
            // Listen first, so a move right after hiding still shows it.
            if let Err(e) = raw_mouse(hwnd, true) {
                tracing::warn!("cannot watch the mouse, keeping the pointer: {e}");
                if let Some(idle) = &mut self.idle {
                    idle.reset(Instant::now());
                }
                return;
            }
            stop_timer(hwnd);
            blank_cursors();
            self.hidden = true;
            tracing::debug!("pointer hidden");
        }

        fn show(&mut self, hwnd: HWND) {
            restore_cursors();
            if let Err(e) = raw_mouse(hwnd, false) {
                tracing::debug!("raw mouse input not removed: {e}");
            }
            self.hidden = false;
            if let Some(idle) = &mut self.idle {
                idle.reset(Instant::now());
            }
            tracing::debug!("pointer shown");
        }
    }

    impl Drop for Hider {
        fn drop(&mut self) {
            if self.hidden {
                restore_cursors();
            }
        }
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
