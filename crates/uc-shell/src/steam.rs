//! Find Steam and open/close Big Picture (plan §2.3 step 3).

use std::path::PathBuf;
use std::time::Duration;

/// Opens Big Picture in a running Steam.
pub const OPEN_BIG_PICTURE_URL: &str = "steam://open/bigpicture";
/// Leaves Big Picture (Steam stays running in desktop mode).
pub const CLOSE_BIG_PICTURE_URL: &str = "steam://close/bigpicture";
/// Shows the desktop Steam window. Used to wake a Steam that sits in the
/// tray and ignored `steam://open/bigpicture`.
pub const OPEN_MAIN_URL: &str = "steam://open/main";

/// A retry while waiting for Big Picture to show up.
///
/// A Steam that was closed to the tray (process running, no window) can
/// swallow the first `steam://open/bigpicture`, or create the Big Picture
/// window hidden. The wait therefore asks again on a schedule instead of
/// sitting out the whole timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nudge {
    /// Hand `steam://open/bigpicture` to Steam again.
    Resend,
    /// Bring up the desktop Steam window first, then ask for Big Picture.
    OpenMainThenResend,
}

/// When each [`Nudge`] fires, counted from the launch.
pub const NUDGES: [(Duration, Nudge); 3] = [
    (Duration::from_secs(4), Nudge::Resend),
    (Duration::from_secs(9), Nudge::OpenMainThenResend),
    (Duration::from_secs(16), Nudge::Resend),
];

/// The next nudge to send, given how long we have waited and how many
/// nudges were already sent. At most one per call, so a late poll never
/// fires two at once.
pub fn nudge_due(elapsed: Duration, sent: usize) -> Option<Nudge> {
    NUDGES
        .get(sent)
        .filter(|(at, _)| elapsed >= *at)
        .map(|(_, n)| *n)
}

/// How [`launch_big_picture`] got Big Picture going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMethod {
    /// Steam was not running and was started with the configured arguments
    /// (default `-gamepadui`).
    StartedSteam,
    /// Steam was running and was handed `steam://open/bigpicture`.
    OpenedInRunningSteam,
}

/// Arguments for `steam.exe`. Both cases go through `steam.exe` itself rather
/// than the `steam://` protocol handler, so a broken file association can't
/// get in the way; a running Steam forwards the URL to itself.
pub fn launch_args(steam_running: bool, launch_args: &[String]) -> (LaunchMethod, Vec<String>) {
    if steam_running {
        (
            LaunchMethod::OpenedInRunningSteam,
            vec![OPEN_BIG_PICTURE_URL.to_owned()],
        )
    } else {
        (LaunchMethod::StartedSteam, launch_args.to_vec())
    }
}

/// Title of the desktop Steam window (not localised).
pub const MAIN_WINDOW_TITLE: &str = "Steam";

/// What Steam looks like right now, for deciding whether Big Picture was
/// really left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SteamView {
    pub steam_running: bool,
    /// App id from `RunningAppID`, if a game runs.
    pub game: Option<u32>,
    pub big_picture_visible: bool,
    /// A Big Picture window exists but is hidden.
    pub big_picture_hidden: bool,
    /// A desktop Steam window is showing that was not showing when gaming
    /// mode started (one left open on the desktop doesn't count).
    pub main_window_visible: bool,
}

/// Whether Big Picture was left, after the watcher saw its window go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BpVerdict {
    /// Big Picture is showing again.
    #[default]
    Back,
    /// A game runs; Big Picture hides behind it and comes back after.
    GameRunning,
    /// Clearly left: Steam quit, or it is showing its desktop window.
    Left,
    /// No window, no game, no desktop Steam: for example right after a game
    /// quit, before Big Picture redraws. Not a reason to leave gaming mode.
    Unsure,
}

impl SteamView {
    pub fn verdict(&self) -> BpVerdict {
        if self.big_picture_visible {
            BpVerdict::Back
        } else if self.game.is_some() {
            BpVerdict::GameRunning
        } else if !self.steam_running || (self.main_window_visible && !self.big_picture_hidden) {
            BpVerdict::Left
        } else {
            BpVerdict::Unsure
        }
    }
}

/// Steam stores paths in the registry with forward slashes and lowercase
/// (`c:/program files (x86)/steam/steam.exe`). Windows accepts that, but
/// backslashes keep logs and comparisons sane.
pub fn normalize_registry_path(raw: &str) -> PathBuf {
    PathBuf::from(raw.trim().trim_matches('"').replace('/', "\\"))
}

#[cfg(windows)]
pub use imp::*;

#[cfg(windows)]
mod imp {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use uc_core::config::Steam;
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
    };
    use windows::core::{HSTRING, PCWSTR};

    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{SW_SHOW, ShowWindow};

    use super::{
        CLOSE_BIG_PICTURE_URL, LaunchMethod, Nudge, OPEN_BIG_PICTURE_URL, OPEN_MAIN_URL, SteamView,
        launch_args, normalize_registry_path, nudge_due,
    };
    use crate::{Error, Result, WindowInfo, WindowMatcher, win};

    const STEAM_KEY: &str = r"Software\Valve\Steam";

    /// `steam.exe`: the configured path if set, else `HKCU\Software\Valve\Steam`
    /// (`SteamExe`, then `SteamPath`), else the default install folder.
    pub fn find_steam_exe(configured: Option<&Path>) -> Result<PathBuf> {
        if let Some(path) = configured {
            return if path.is_file() {
                Ok(path.to_path_buf())
            } else {
                Err(Error::SteamNotFound(format!(
                    "configured path {} does not exist",
                    path.display()
                )))
            };
        }
        let mut candidates = Vec::new();
        if let Some(exe) = read_hkcu_string(STEAM_KEY, "SteamExe") {
            candidates.push(normalize_registry_path(&exe));
        }
        if let Some(dir) = read_hkcu_string(STEAM_KEY, "SteamPath") {
            candidates.push(normalize_registry_path(&dir).join("steam.exe"));
        }
        if let Some(pf) = std::env::var_os("ProgramFiles(x86)") {
            candidates.push(PathBuf::from(pf).join(r"Steam\steam.exe"));
        }
        candidates
            .iter()
            .find(|p| p.is_file())
            .cloned()
            .ok_or_else(|| {
                let tried: Vec<_> = candidates.iter().map(|p| p.display().to_string()).collect();
                Error::SteamNotFound(format!("tried: {}", tried.join(", ")))
            })
    }

    /// The app id of the game Steam is running now (`HKCU\Software\Valve\
    /// Steam\RunningAppID`), or `None` when no game runs.
    pub fn running_game() -> Option<u32> {
        let key = HSTRING::from(STEAM_KEY);
        let value = HSTRING::from("RunningAppID");
        let mut data = 0u32;
        let mut bytes = size_of::<u32>() as u32;
        // SAFETY: `data` is writable for `bytes` bytes; the strings are
        // NUL-terminated HSTRINGs that outlive the call.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut data as *mut u32).cast()),
                Some(&mut bytes),
            )
        };
        (status.is_ok() && data != 0).then_some(data)
    }

    /// Whether `steam.exe` runs in this session.
    pub fn steam_running() -> Result<bool> {
        Ok(!win::session_pids_named("steam.exe")?.is_empty())
    }

    /// Opens Big Picture: hands `steam://open/bigpicture` to a running Steam,
    /// or starts Steam with `launch_args` (default `-gamepadui`). Returns as
    /// soon as the command is started; use [`wait_for_big_picture`] to know
    /// when the window is up.
    pub fn launch_big_picture(cfg: &Steam) -> Result<LaunchMethod> {
        let exe = find_steam_exe(cfg.exe_path.as_deref())?;
        let (method, args) = launch_args(steam_running()?, &cfg.launch_args);
        tracing::info!(exe = %exe.display(), ?args, ?method, "launching Big Picture");
        spawn(&exe, &args)?;
        Ok(method)
    }

    /// Asks a running Steam to leave Big Picture. Does nothing if Steam isn't
    /// running (and never starts it). Returns whether the request was sent.
    pub fn close_big_picture(cfg: &Steam) -> Result<bool> {
        if !steam_running()? {
            return Ok(false);
        }
        let exe = find_steam_exe(cfg.exe_path.as_deref())?;
        tracing::info!("asking Steam to close Big Picture");
        spawn(&exe, &[CLOSE_BIG_PICTURE_URL.to_owned()])?;
        Ok(true)
    }

    fn steam_pids() -> Vec<u32> {
        ["steam.exe", "steamwebhelper.exe"]
            .iter()
            .filter_map(|n| win::session_pids_named(n).ok())
            .flatten()
            .collect()
    }

    /// Handles of the desktop Steam windows showing now.
    pub fn main_windows() -> Vec<isize> {
        let pids = steam_pids();
        win::list_windows()
            .unwrap_or_default()
            .iter()
            .filter(|w| {
                w.visible
                    && pids.contains(&w.pid)
                    && w.title
                        .trim()
                        .eq_ignore_ascii_case(super::MAIN_WINDOW_TITLE)
            })
            .map(|w| w.hwnd)
            .collect()
    }

    /// Steam's state as seen now; see [`SteamView::verdict`]. Desktop Steam
    /// windows in `known_main` (open since before gaming mode) are ignored.
    pub fn view(matcher: &WindowMatcher, known_main: &[isize]) -> Result<SteamView> {
        let pids = steam_pids();
        let windows = win::list_windows()?;
        let steam_running = !win::session_pids_named("steam.exe")?.is_empty();
        Ok(SteamView {
            steam_running,
            game: running_game(),
            big_picture_visible: matcher.find(&windows).is_some(),
            big_picture_hidden: matcher.find_hidden(&windows).is_some(),
            main_window_visible: windows.iter().any(|w| {
                w.visible
                    && !known_main.contains(&w.hwnd)
                    && pids.contains(&w.pid)
                    && w.title
                        .trim()
                        .eq_ignore_ascii_case(super::MAIN_WINDOW_TITLE)
            }),
        })
    }

    /// Logs Steam's top-level windows (for diagnosing Big Picture closes).
    pub fn log_windows(why: &str) {
        match win::list_windows() {
            Ok(w) => log_steam_windows(&w, why),
            Err(e) => tracing::warn!("window scan failed: {e}"),
        }
    }

    /// The Big Picture window, if it is on screen now.
    pub fn find_big_picture(matcher: &WindowMatcher) -> Result<Option<WindowInfo>> {
        Ok(matcher.find(&win::list_windows()?).cloned())
    }

    /// Polls for the Big Picture window until it shows up or `timeout` passes.
    pub fn wait_for_big_picture(
        matcher: &WindowMatcher,
        timeout: Duration,
    ) -> Result<Option<WindowInfo>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(w) = find_big_picture(matcher)? {
                return Ok(Some(w));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Like [`wait_for_big_picture`], for right after [`launch_big_picture`]:
    /// shows a Big Picture window that Steam created hidden, and asks Steam
    /// again on the [`NUDGES`](super::NUDGES) schedule. This is what makes a
    /// Steam that sits in the tray open Big Picture reliably. On a timeout,
    /// logs Steam's windows so the cause can be seen in the agent log.
    pub fn wait_for_big_picture_nudging(
        cfg: &Steam,
        matcher: &WindowMatcher,
        timeout: Duration,
    ) -> Result<Option<WindowInfo>> {
        let start = Instant::now();
        let deadline = start + timeout;
        let mut sent = 0;
        let mut shown_hidden = false;
        loop {
            let windows = win::list_windows()?;
            if let Some(w) = matcher.find(&windows) {
                tracing::info!(
                    after_ms = start.elapsed().as_millis() as u64,
                    nudges = sent,
                    "Big Picture window is up"
                );
                return Ok(Some(w.clone()));
            }
            // Give Steam a moment to show the window itself (SDL creates it
            // hidden), then show it ourselves.
            if !shown_hidden
                && start.elapsed() >= Duration::from_secs(2)
                && let Some(w) = matcher.find_hidden(&windows)
            {
                tracing::info!(
                    hwnd = w.hwnd,
                    "Big Picture window exists but is hidden; showing it"
                );
                // SAFETY: plain FFI call; a stale handle just fails.
                let _ = unsafe { ShowWindow(HWND(w.hwnd as *mut _), SW_SHOW) };
                shown_hidden = true;
                continue;
            }
            let now = Instant::now();
            if now >= deadline {
                log_steam_windows(
                    &windows,
                    "Big Picture never showed up (set [steam] bp_window_title if one of these is Big Picture)",
                );
                return Ok(None);
            }
            if let Some(nudge) = nudge_due(now - start, sent) {
                sent += 1;
                // Each nudge gets a fresh chance to un-hide a window.
                shown_hidden = false;
                if let Err(e) = send_nudge(cfg, nudge) {
                    tracing::warn!(?nudge, "could not nudge Steam: {e}");
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn send_nudge(cfg: &Steam, nudge: Nudge) -> Result<()> {
        let exe = find_steam_exe(cfg.exe_path.as_deref())?;
        if !steam_running()? {
            // Still starting up from our own launch; nothing to forward to yet.
            tracing::info!(
                ?nudge,
                "Big Picture not up yet and steam.exe not running; waiting"
            );
            return Ok(());
        }
        tracing::info!(?nudge, "Big Picture not up yet; asking Steam again");
        if nudge == Nudge::OpenMainThenResend {
            spawn(&exe, &[OPEN_MAIN_URL.to_owned()])?;
            std::thread::sleep(Duration::from_millis(1500));
        }
        spawn(&exe, &[OPEN_BIG_PICTURE_URL.to_owned()])
    }

    /// Steam's top-level windows, for the log when Big Picture never showed.
    fn log_steam_windows(windows: &[WindowInfo], why: &str) {
        let pids: Vec<u32> = ["steam.exe", "steamwebhelper.exe"]
            .iter()
            .filter_map(|n| win::session_pids_named(n).ok())
            .flatten()
            .collect();
        let steam_windows: Vec<_> = windows
            .iter()
            .filter(|w| pids.contains(&w.pid) && !w.title.is_empty())
            .map(|w| {
                format!(
                    "{:#x} {:?} {:?} visible={}",
                    w.hwnd, w.class, w.title, w.visible
                )
            })
            .collect();
        tracing::info!(steam_pids = ?pids, windows = ?steam_windows, "{why}; Steam's windows");
    }

    fn spawn(exe: &Path, args: &[String]) -> Result<()> {
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        // Not waited on: Steam (or the forwarding instance) lives on its own.
        cmd.spawn().map(drop).map_err(|source| Error::Spawn {
            path: exe.to_path_buf(),
            source,
        })
    }

    fn read_hkcu_string(key: &str, value: &str) -> Option<String> {
        let key = HSTRING::from(key);
        let value = HSTRING::from(value);
        let mut buf = vec![0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `buf` is writable for `bytes` bytes; the key/value strings
        // are NUL-terminated HSTRINGs that outlive the call.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if status.is_err() {
            return None;
        }
        let len = (bytes as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..len]);
        let s = s.trim_end_matches('\0');
        (!s.is_empty()).then(|| s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_steam_gets_the_url() {
        let (m, args) = launch_args(true, &["-gamepadui".into()]);
        assert_eq!(m, LaunchMethod::OpenedInRunningSteam);
        assert_eq!(args, vec![OPEN_BIG_PICTURE_URL.to_owned()]);
    }

    #[test]
    fn stopped_steam_gets_configured_args() {
        let (m, args) = launch_args(false, &["-gamepadui".into(), "-silent".into()]);
        assert_eq!(m, LaunchMethod::StartedSteam);
        assert_eq!(args, vec!["-gamepadui".to_owned(), "-silent".to_owned()]);
    }

    #[test]
    fn big_picture_verdicts() {
        let base = SteamView {
            steam_running: true,
            ..SteamView::default()
        };
        let v = |f: fn(&mut SteamView)| {
            let mut s = base;
            f(&mut s);
            s.verdict()
        };
        assert_eq!(v(|s| s.big_picture_visible = true), BpVerdict::Back);
        assert_eq!(v(|s| s.game = Some(207610)), BpVerdict::GameRunning);
        assert_eq!(v(|s| s.steam_running = false), BpVerdict::Left);
        assert_eq!(v(|s| s.main_window_visible = true), BpVerdict::Left);
        // Nothing to go on (a game just quit): stay.
        assert_eq!(v(|_| {}), BpVerdict::Unsure);
        assert_eq!(
            v(|s| {
                s.main_window_visible = true;
                s.big_picture_hidden = true;
            }),
            BpVerdict::Unsure
        );
    }

    #[test]
    fn nudges_fire_once_each_in_order() {
        let s = Duration::from_secs;
        assert_eq!(nudge_due(s(1), 0), None);
        assert_eq!(nudge_due(s(4), 0), Some(Nudge::Resend));
        assert_eq!(nudge_due(s(5), 1), None);
        // A late poll still sends only the next one.
        assert_eq!(nudge_due(s(30), 1), Some(Nudge::OpenMainThenResend));
        assert_eq!(nudge_due(s(30), 2), Some(Nudge::Resend));
        assert_eq!(nudge_due(s(30), NUDGES.len()), None);
    }

    #[test]
    fn nudges_fit_in_the_default_timeout() {
        let timeout = Duration::from_secs(
            uc_core::config::Steam::default()
                .bp_launch_timeout_secs
                .into(),
        );
        assert!(NUDGES.iter().all(|(at, _)| *at < timeout));
    }

    #[test]
    fn registry_paths_get_backslashes() {
        let p = normalize_registry_path("c:/program files (x86)/steam/steam.exe\n");
        assert_eq!(
            p.to_string_lossy(),
            r"c:\program files (x86)\steam\steam.exe"
        );
        let q = normalize_registry_path("\"D:/Games/Steam\"");
        assert_eq!(q.to_string_lossy(), r"D:\Games\Steam");
    }
}
