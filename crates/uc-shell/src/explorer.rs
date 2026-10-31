//! Exit and restart the Explorer shell without signing out (plan §2.3).
//!
//! Exit posts `0x5B4` to the taskbar (`Shell_TrayWnd`): the same message as
//! Ctrl+Shift+right-click on the taskbar → "Exit Explorer". A shell that exits
//! cleanly is not restarted by Winlogon. A *killed* shell is, so the shell
//! process itself is never terminated here; only leftover non-shell
//! `explorer.exe` processes (separate-process folder windows) are.
//!
//! Start launches `%WINDIR%\explorer.exe` with this process's (normal, non-
//! elevated) token. With no shell running it becomes the shell.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};
use windows::core::w;

use crate::{Error, Result, win};

/// Undocumented "exit Explorer" message understood by `Shell_TrayWnd`.
const WM_EXIT_EXPLORER: u32 = 0x5B4;
const POLL: Duration = Duration::from_millis(100);
/// How long leftover folder-window processes get to close on their own after
/// the shell has gone before they are terminated.
const STRAGGLER_GRACE: Duration = Duration::from_secs(1);
/// When `explorer.exe` processes exist but no taskbar yet, how long to wait
/// for one of them to become the shell before launching another.
const STARTING_SHELL_GRACE: Duration = Duration::from_secs(3);
/// How often the exit message is posted again while the taskbar is still up.
/// A shell that only just started (e.g. we relaunched it a few seconds ago)
/// can ignore the first one.
const REPOST_EVERY: Duration = Duration::from_millis(1500);
/// Once the taskbar is gone, how long the shell process gets to end before
/// we move on anyway. It can take several more seconds to save its state;
/// it is left to finish on its own (never killed, see the module docs).
const SHELL_LINGER: Duration = Duration::from_secs(2);

/// What [`exit_explorer`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExitReport {
    /// No shell and no `explorer.exe` was running; nothing was done.
    pub already_exited: bool,
    /// PID of the shell that was asked to exit.
    pub shell_pid: Option<u32>,
    /// Leftover `explorer.exe` processes that were terminated.
    pub terminated: Vec<u32>,
    pub elapsed: Duration,
}

/// What [`start_explorer`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartReport {
    /// A shell (taskbar) was already there; nothing was launched.
    AlreadyRunning,
    /// An `explorer.exe` that was already starting became the shell.
    BecameShell { elapsed: Duration },
    /// `explorer.exe` was launched. `shell_ready` is false if the taskbar did
    /// not show up within the wait (Explorer may still be starting).
    Launched {
        pid: u32,
        shell_ready: bool,
        elapsed: Duration,
    },
}

fn tray() -> Option<HWND> {
    // SAFETY: plain FFI call with static, NUL-terminated strings.
    unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }
        .ok()
        .filter(|h| !h.is_invalid())
}

/// True if the Explorer shell (the taskbar) is up.
pub fn shell_running() -> bool {
    tray().is_some()
}

/// PIDs of every `explorer.exe` in this session (shell and folder windows).
pub fn explorer_pids() -> Result<Vec<u32>> {
    win::session_pids_named("explorer.exe")
}

/// True if any `explorer.exe` runs in this session.
pub fn explorer_running() -> Result<bool> {
    Ok(!explorer_pids()?.is_empty())
}

/// Asks the shell to exit cleanly, waits up to `timeout` for it to go, then
/// terminates leftover non-shell `explorer.exe` processes.
///
/// Fails with [`Error::ExplorerExitTimeout`] if the shell is still there; in
/// that case nothing was killed and the desktop is intact.
pub fn exit_explorer(timeout: Duration) -> Result<ExitReport> {
    let start = Instant::now();
    let deadline = start + timeout;
    let mut report = ExitReport::default();

    if let Some(hwnd) = tray() {
        let window = win::window_info(hwnd);
        report.shell_pid = Some(window.pid);
        tracing::info!(shell_pid = window.pid, "asking Explorer to exit");
        post_exit(hwnd)?;
        let mut last_post = Instant::now();
        let mut posts = 1u32;

        let shell_pid = window.pid;
        let mut tray_gone_at: Option<Instant> = None;
        loop {
            let now = Instant::now();
            match tray() {
                Some(hwnd) => {
                    tray_gone_at = None;
                    if now.duration_since(last_post) >= REPOST_EVERY {
                        tracing::info!(posts, "taskbar still up; asking Explorer again");
                        // A failed re-post is retried on the next round.
                        let _ = post_exit(hwnd);
                        last_post = now;
                        posts += 1;
                    }
                }
                None => {
                    let since = *tray_gone_at.get_or_insert(now);
                    if !explorer_pids()?.contains(&shell_pid) {
                        break;
                    }
                    if now.duration_since(since) >= SHELL_LINGER {
                        tracing::info!(
                            shell_pid,
                            "taskbar gone; the shell process is still finishing, moving on"
                        );
                        break;
                    }
                }
            }
            if now >= deadline {
                tracing::warn!(?timeout, posts, "Explorer did not exit in time");
                return Err(Error::ExplorerExitTimeout(timeout));
            }
            std::thread::sleep(POLL);
        }
    } else if explorer_pids()?.is_empty() {
        report.already_exited = true;
        report.elapsed = start.elapsed();
        return Ok(report);
    }

    // Folder windows opened "in a separate process" survive the shell's exit.
    // The old shell process may still be finishing; it is never killed.
    let others = || -> Result<Vec<u32>> {
        Ok(explorer_pids()?
            .into_iter()
            .filter(|p| Some(*p) != report.shell_pid)
            .collect())
    };
    let grace_end = Instant::now() + STRAGGLER_GRACE;
    let mut left = others()?;
    while !left.is_empty() && Instant::now() < grace_end {
        std::thread::sleep(POLL);
        left = others()?;
    }
    for pid in left {
        match win::terminate(pid) {
            Ok(()) => {
                tracing::info!(pid, "terminated leftover explorer.exe");
                report.terminated.push(pid);
            }
            Err(e) => tracing::warn!(pid, "could not terminate leftover explorer.exe: {e}"),
        }
    }
    report.elapsed = start.elapsed();
    tracing::info!(?report, "Explorer exited");
    Ok(report)
}

fn post_exit(hwnd: HWND) -> Result<()> {
    // SAFETY: plain FFI call; a stale handle just fails.
    unsafe { PostMessageW(Some(hwnd), WM_EXIT_EXPLORER, WPARAM(0), LPARAM(0)) }
        .map_err(|e| Error::win32("PostMessage(Shell_TrayWnd, 0x5B4)", e))
}

/// Starts the Explorer shell if it isn't running and waits up to `wait` for
/// the taskbar to appear.
pub fn start_explorer(wait: Duration) -> Result<StartReport> {
    let start = Instant::now();
    if shell_running() {
        return Ok(StartReport::AlreadyRunning);
    }
    // An explorer.exe without a taskbar may be a shell that's still starting
    // (e.g. Winlogon restarted it). Give it a moment rather than racing it.
    if explorer_running()? && wait_for_shell(STARTING_SHELL_GRACE.min(wait)) {
        return Ok(StartReport::BecameShell {
            elapsed: start.elapsed(),
        });
    }

    let exe = explorer_exe();
    tracing::info!(exe = %exe.display(), "starting Explorer");
    let child = Command::new(&exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| Error::Spawn { path: exe, source })?;
    let pid = child.id();
    drop(child);

    let remaining = wait.saturating_sub(start.elapsed());
    let shell_ready = wait_for_shell(remaining);
    if !shell_ready {
        tracing::warn!(pid, ?wait, "Explorer started but no taskbar yet");
    }
    Ok(StartReport::Launched {
        pid,
        shell_ready,
        elapsed: start.elapsed(),
    })
}

/// Waits up to `timeout` for the taskbar. Returns whether it appeared.
pub fn wait_for_shell(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if shell_running() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

fn explorer_exe() -> PathBuf {
    let windir = std::env::var_os("WINDIR")
        .or_else(|| std::env::var_os("SystemRoot"))
        .unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(windir).join("explorer.exe")
}
