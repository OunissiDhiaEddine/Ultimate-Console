//! Reports when Big Picture appears and when it is gone (plan §2.8).
//!
//! A dedicated thread installs out-of-context WinEvent hooks for top-level
//! window create/destroy/show/hide/rename and rescans on relevant ones, plus a
//! 1 s poll as a fallback. Scans feed a [`BpTracker`] so a Big Picture
//! restart shorter than the debounce produces no `Closed`.

use std::cell::RefCell;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_NAMECHANGE, GA_ROOT, GetAncestor, GetMessageW, KillTimer, MSG, OBJID_WINDOW,
    PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetTimer, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_QUIT, WM_TIMER,
};

use crate::{BpEvent, BpTracker, Error, Result, WindowMatcher, win};

const POLL_MS: u32 = 1000;

type Callback = Box<dyn FnMut(BpEvent) + Send>;

struct Ctx {
    matcher: WindowMatcher,
    tracker: BpTracker,
    on_event: Callback,
    deadline_timer: usize,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

/// Watches for the Big Picture window on a background thread.
///
/// `on_event` runs on the watcher thread; keep it short (send to a channel).
/// If Big Picture is already open when the watcher starts, `Appeared` fires
/// right away. Dropping the watcher stops the thread.
pub struct BigPictureWatcher {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl BigPictureWatcher {
    pub fn start(
        matcher: WindowMatcher,
        debounce: Duration,
        on_event: impl FnMut(BpEvent) + Send + 'static,
    ) -> Result<Self> {
        if !matcher.is_usable() {
            return Err(Error::Watcher(
                "the Big Picture window class and title are both empty".into(),
            ));
        }
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<u32, String>>();
        let on_event: Callback = Box::new(on_event);
        let join = std::thread::Builder::new()
            .name("uc-bp-watcher".into())
            .spawn(move || run(matcher, debounce, on_event, ready_tx))
            .map_err(|e| Error::Watcher(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                join: Some(join),
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(Error::Watcher(e))
            }
            Err(_) => {
                let _ = join.join();
                Err(Error::Watcher("watcher thread exited during start".into()))
            }
        }
    }

    /// Stops the thread and waits for it.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(join) = self.join.take() {
            // SAFETY: plain FFI call; the thread has a message queue (it was
            // created before the ready signal was sent).
            if unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }.is_ok()
            {
                let _ = join.join();
            } else {
                tracing::warn!("could not signal the Big Picture watcher thread; detaching it");
            }
        }
    }
}

impl Drop for BigPictureWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(
    matcher: WindowMatcher,
    debounce: Duration,
    on_event: Callback,
    ready: mpsc::Sender<std::result::Result<u32, String>>,
) {
    let mut msg = MSG::default();
    // SAFETY: forces creation of this thread's message queue so
    // PostThreadMessage works once `ready` is sent.
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: plain FFI call.
    let thread_id = unsafe { GetCurrentThreadId() };

    let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
    // SAFETY: out-of-context hooks with a valid `extern "system"` callback;
    // they are delivered to this thread while it pumps messages below.
    let hooks = unsafe {
        [
            SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_HIDE,
                None,
                Some(on_win_event),
                0,
                0,
                flags,
            ),
            SetWinEventHook(
                EVENT_OBJECT_NAMECHANGE,
                EVENT_OBJECT_NAMECHANGE,
                None,
                Some(on_win_event),
                0,
                0,
                flags,
            ),
        ]
    };
    if hooks.iter().any(|h| h.is_invalid()) {
        // The poll still works without hooks, only slower to react.
        tracing::warn!("SetWinEventHook failed; Big Picture detection falls back to polling");
    }
    // SAFETY: thread timer (no window); WM_TIMER lands in this thread's queue.
    let poll_timer = unsafe { SetTimer(None, 0, POLL_MS, None) };
    if poll_timer == 0 {
        unhook(&hooks);
        let _ = ready.send(Err("SetTimer failed".into()));
        return;
    }

    CTX.with(|c| {
        *c.borrow_mut() = Some(Ctx {
            matcher,
            tracker: BpTracker::new(debounce),
            on_event,
            deadline_timer: 0,
        })
    });
    let _ = ready.send(Ok(thread_id));
    tracing::debug!(thread_id, "Big Picture watcher running");
    scan();

    loop {
        // SAFETY: `msg` is a valid out pointer; returns 0 on WM_QUIT, -1 on error.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if r == 0 || r == -1 {
            break;
        }
        if msg.message == WM_TIMER && msg.hwnd.is_invalid() {
            if msg.wParam.0 != poll_timer {
                // One-shot debounce timer.
                // SAFETY: kills a thread timer this thread created.
                let _ = unsafe { KillTimer(None, msg.wParam.0) };
                CTX.with(|c| {
                    if let Ok(mut c) = c.try_borrow_mut()
                        && let Some(ctx) = c.as_mut()
                        && ctx.deadline_timer == msg.wParam.0
                    {
                        ctx.deadline_timer = 0;
                    }
                });
            }
            scan();
        }
        // Out-of-context WinEvents are delivered inside GetMessage; nothing
        // else is posted to this thread, so there is nothing to dispatch.
    }

    // SAFETY: kills the thread timer created above.
    let _ = unsafe { KillTimer(None, poll_timer) };
    unhook(&hooks);
    CTX.with(|c| c.borrow_mut().take());
    tracing::debug!("Big Picture watcher stopped");
}

fn unhook(hooks: &[HWINEVENTHOOK]) {
    for h in hooks.iter().filter(|h| !h.is_invalid()) {
        // SAFETY: unhooks a hook this thread installed, once.
        let _ = unsafe { UnhookWinEvent(*h) };
    }
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd.is_invalid() {
        return;
    }
    let relevant = CTX.with(|c| {
        let Ok(c) = c.try_borrow() else { return false };
        let Some(ctx) = c.as_ref() else { return false };
        if ctx
            .tracker
            .last_known()
            .is_some_and(|w| w.hwnd == hwnd.0 as isize)
        {
            return true;
        }
        if event == EVENT_OBJECT_DESTROY {
            return false;
        }
        // SAFETY: plain FFI call; a stale handle returns null.
        let top_level = unsafe { GetAncestor(hwnd, GA_ROOT) } == hwnd;
        top_level && ctx.matcher.class_matches(&win::window_class(hwnd))
    });
    if relevant {
        scan();
    }
}

/// Rescans top-level windows and reports any resulting event.
fn scan() {
    CTX.with(|c| {
        // Re-entrancy (a WinEvent delivered while we're in here) just skips;
        // the poll catches up.
        let Ok(mut c) = c.try_borrow_mut() else {
            return;
        };
        let Some(ctx) = c.as_mut() else { return };
        let found = match win::list_windows() {
            Ok(list) => ctx.matcher.find(&list).cloned(),
            Err(e) => {
                tracing::warn!("window scan failed: {e}");
                return;
            }
        };
        let now = Instant::now();
        let event = ctx.tracker.observe(found, now);
        arm_deadline(ctx, now);
        if let Some(event) = event {
            tracing::info!(?event, "Big Picture");
            (ctx.on_event)(event);
        }
    });
}

/// Makes sure a pending close is reported on time, not up to a poll late.
fn arm_deadline(ctx: &mut Ctx, now: Instant) {
    match ctx.tracker.close_deadline() {
        Some(deadline) if ctx.deadline_timer == 0 => {
            let ms = deadline.saturating_duration_since(now).as_millis();
            let ms = u32::try_from(ms).unwrap_or(u32::MAX).max(10);
            // SAFETY: thread timer (no window).
            ctx.deadline_timer = unsafe { SetTimer(None, 0, ms, None) };
        }
        None if ctx.deadline_timer != 0 => {
            // SAFETY: kills a thread timer this thread created.
            let _ = unsafe { KillTimer(None, ctx.deadline_timer) };
            ctx.deadline_timer = 0;
        }
        _ => {}
    }
}
