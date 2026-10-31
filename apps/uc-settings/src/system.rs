//! Everything that touches the OS: the named pipe, starting the agent,
//! opening folders, uninstalling, and the post-uninstall check.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use uc_core::ipc::IpcRequest;

use crate::client::Connector;

#[cfg(windows)]
use uc_core::APP_NAME;
use uc_core::Paths;

#[cfg(windows)]
pub use win::PipeConnector;

/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value the agent
/// registers itself under (WP-08).
#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// "Apps & features" entry written by Velopack (WP-10) for app id [`APP_NAME`].
#[cfg(windows)]
fn uninstall_key() -> String {
    format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{APP_NAME}")
}

fn exe_dir() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.parent().unwrap_or(Path::new(".")).to_path_buf())
}

/// Starts `uc-agent.exe` from the folder this exe is in (Velopack installs
/// both side by side).
pub fn start_agent() -> io::Result<()> {
    let agent = exe_dir()?.join(if cfg!(windows) {
        "uc-agent.exe"
    } else {
        "uc-agent"
    });
    if !agent.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} not found", agent.display()),
        ));
    }
    let mut cmd = Command::new(&agent);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Not tied to this app: closing settings must not stop the agent.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()?;
    tracing::info!(path = %agent.display(), "started the agent");
    Ok(())
}

/// Longest the uninstaller waits for the agent to restore desktop and exit.
/// Velopack kills the uninstall hook after 30 s.
#[cfg(windows)]
const STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// Uninstall hook (WP-10): asks a running agent to restore desktop mode and
/// exit, and waits until its pipe is gone. Returns at once if it isn't running.
pub fn stop_agent() {
    #[cfg(windows)]
    match win::PipeConnector::for_current_user() {
        Ok(connector) => stop_agent_with(&connector, STOP_TIMEOUT),
        Err(e) => tracing::warn!("cannot reach the agent to stop it: {e}"),
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn stop_agent_with(connector: &dyn Connector, timeout: Duration) {
    let Ok(mut conn) = connector.connect() else {
        return;
    };
    // An agent of another version may refuse the handshake; ask anyway.
    let _ = conn.handshake();
    match conn.call(IpcRequest::Shutdown) {
        Ok(response) => tracing::info!(?response, "asked the agent to shut down"),
        Err(e) => tracing::warn!("shutdown request failed: {e}"),
    }
    drop(conn);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if connector.connect().is_err() {
            tracing::info!("agent stopped");
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    tracing::warn!("agent still answering after {timeout:?}; uninstalling anyway");
}

/// Opens a folder in the file manager, creating it first if needed.
pub fn open_folder(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let opener = if cfg!(windows) {
        "explorer.exe"
    } else {
        "xdg-open"
    };
    Command::new(opener).arg(dir).spawn()?;
    Ok(())
}

/// Program and arguments that uninstall the app, as Windows would run them
/// from "Apps & features". `None` when this copy wasn't installed (a dev or
/// CI build).
pub fn uninstall_command() -> Option<(PathBuf, Vec<String>)> {
    #[cfg(windows)]
    if let Some(cmd) = win::reg_string(&uninstall_key(), "UninstallString") {
        return split_command_line(&cmd);
    }
    // Velopack layout: <root>\Update.exe and <root>\current\uc-settings.exe.
    let update = exe_dir().ok()?.parent()?.join("Update.exe");
    update
        .exists()
        .then(|| (update, vec!["--uninstall".to_string()]))
}

/// Splits `"C:\path with spaces\Update.exe" --uninstall` into program and args.
#[cfg_attr(not(windows), allow(dead_code))]
fn split_command_line(cmd: &str) -> Option<(PathBuf, Vec<String>)> {
    let cmd = cmd.trim();
    let (program, rest) = match cmd.strip_prefix('"') {
        Some(quoted) => {
            let end = quoted.find('"')?;
            (&quoted[..end], &quoted[end + 1..])
        }
        None => cmd.split_once(' ').unwrap_or((cmd, "")),
    };
    if program.is_empty() {
        return None;
    }
    let args = rest.split_whitespace().map(str::to_string).collect();
    Some((PathBuf::from(program), args))
}

/// Runs the uninstaller. The caller should close the app right after.
pub fn run_uninstaller() -> io::Result<()> {
    let (program, args) = uninstall_command().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "this copy was not installed with the installer, so there is nothing to uninstall",
        )
    })?;
    tracing::info!(program = %program.display(), ?args, "running the uninstaller");
    Command::new(program).args(args).spawn()?;
    Ok(())
}

/// `uc-settings --check-uninstall`: lists every file, folder and registry
/// entry the app can create (plan §2.12) and whether it is still there.
/// Exit code 0 when everything is gone.
pub fn check_uninstall() -> ExitCode {
    let paths = match Paths::from_env() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut left = 0;
    let mut report = |what: String, present: bool| {
        println!(
            "{}  {what}",
            if present {
                "STILL THERE"
            } else {
                "gone       "
            }
        );
        left += usize::from(present);
    };
    let mut items = paths.uninstall_manifest();
    items.push(paths.data_dir.clone());
    for path in items {
        report(path.display().to_string(), path.exists());
    }
    #[cfg(windows)]
    {
        report(
            format!(r"HKCU\{RUN_KEY} value {APP_NAME}"),
            win::reg_string(RUN_KEY, APP_NAME).is_some(),
        );
        report(
            format!(r"HKCU\{}", uninstall_key()),
            win::reg_key_exists(&uninstall_key()),
        );
    }
    if left == 0 {
        println!("\nEverything is gone.");
        ExitCode::SUCCESS
    } else {
        println!("\n{left} item(s) left behind.");
        ExitCode::FAILURE
    }
}

/// Lets a GUI-subsystem exe print to the console it was started from.
pub fn attach_console() {
    #[cfg(windows)]
    // SAFETY: plain Win32 call with a constant argument; failing (no parent
    // console, or output already redirected) is fine.
    unsafe {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(windows)]
mod win {
    use std::fs::OpenOptions;
    use std::io;
    use std::thread;
    use std::time::Duration;

    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegGetValueW, RegOpenKeyExW,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{HSTRING, PWSTR};

    use crate::client::{Connection, Connector};

    const ERROR_PIPE_BUSY: i32 = 231;

    /// Connects to `\\.\pipe\UltimateConsole-<SID>` (plan §2.10).
    pub struct PipeConnector {
        name: String,
    }

    impl PipeConnector {
        pub fn for_current_user() -> io::Result<Self> {
            Ok(Self {
                name: uc_core::paths::pipe_name(&current_user_sid()?),
            })
        }
    }

    impl Connector for PipeConnector {
        fn connect(&self) -> io::Result<Connection> {
            let mut tries = 0;
            let file = loop {
                match OpenOptions::new().read(true).write(true).open(&self.name) {
                    // All pipe instances busy: the agent is between accepts.
                    Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 20 => {
                        tries += 1;
                        thread::sleep(Duration::from_millis(50));
                    }
                    other => break other?,
                }
            };
            Ok(Connection::new(file.try_clone()?, file))
        }
    }

    /// String SID of the user running this process, e.g. `S-1-5-21-…`.
    fn current_user_sid() -> io::Result<String> {
        // SAFETY: the token handle is closed before returning; the buffer is
        // u64-aligned and sized by the first GetTokenInformation call, so it
        // holds a valid TOKEN_USER; the SID string is freed with LocalFree as
        // ConvertSidToStringSidW requires.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
            let mut len = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            let got = GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast()),
                len,
                &mut len,
            );
            let _ = CloseHandle(token);
            got?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut sid)?;
            let text = sid.to_string();
            let _ = LocalFree(Some(HLOCAL(sid.0.cast())));
            text.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
    }

    /// Reads a `REG_SZ` value under HKCU. `None` if the key or value is missing.
    pub fn reg_string(subkey: &str, value: &str) -> Option<String> {
        let (subkey, value) = (HSTRING::from(subkey), HSTRING::from(value));
        let mut size = 0u32;
        // SAFETY: first call only asks for the size; the second writes at
        // most `size` bytes into a buffer of that many bytes.
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &subkey,
                &value,
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut size),
            )
            .ok()
            .ok()?;
            let mut buf = vec![0u16; (size as usize).div_ceil(2)];
            RegGetValueW(
                HKEY_CURRENT_USER,
                &subkey,
                &value,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
            .ok()
            .ok()?;
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..len]))
        }
    }

    pub fn reg_key_exists(subkey: &str) -> bool {
        let mut key = HKEY::default();
        // SAFETY: opens and immediately closes a key handle we own.
        unsafe {
            let opened = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(subkey),
                None,
                KEY_READ,
                &mut key,
            )
            .is_ok();
            if opened {
                let _ = RegCloseKey(key);
            }
            opened
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NotRunning;
    impl Connector for NotRunning {
        fn connect(&self) -> io::Result<crate::client::Connection> {
            Err(io::ErrorKind::NotFound.into())
        }
    }

    #[test]
    fn stop_agent_returns_at_once_when_not_running() {
        let start = Instant::now();
        stop_agent_with(&NotRunning, Duration::from_secs(5));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn stop_agent_gives_up_after_timeout() {
        let agent = crate::mock::MockConnector::new(crate::mock::MockAgent::new());
        let start = Instant::now();
        stop_agent_with(&agent, Duration::from_millis(300));
        assert!(start.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn splits_uninstall_strings() {
        assert_eq!(
            split_command_line(
                r#""C:\Users\Dhia\AppData\Local\UltimateConsole\Update.exe" --uninstall"#
            ),
            Some((
                PathBuf::from(r"C:\Users\Dhia\AppData\Local\UltimateConsole\Update.exe"),
                vec!["--uninstall".into()]
            ))
        );
        assert_eq!(
            split_command_line(r"C:\x\Update.exe --uninstall --silent"),
            Some((
                PathBuf::from(r"C:\x\Update.exe"),
                vec!["--uninstall".into(), "--silent".into()]
            ))
        );
        assert_eq!(split_command_line(r#""unterminated"#), None);
        assert_eq!(split_command_line("  "), None);
    }
}
