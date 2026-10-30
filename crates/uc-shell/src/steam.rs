//! Find Steam and open/close Big Picture (plan §2.3 step 3).

use std::path::PathBuf;

/// Opens Big Picture in a running Steam.
pub const OPEN_BIG_PICTURE_URL: &str = "steam://open/bigpicture";
/// Leaves Big Picture (Steam stays running in desktop mode).
pub const CLOSE_BIG_PICTURE_URL: &str = "steam://close/bigpicture";

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
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::{HSTRING, PCWSTR};

    use super::{CLOSE_BIG_PICTURE_URL, LaunchMethod, launch_args, normalize_registry_path};
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
