//! Start with Windows: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! (plan §2.2). The value is the quoted path of this exe; under Velopack that
//! is `%LOCALAPPDATA%\UltimateConsole\current\uc-agent.exe`, which stays the
//! same across updates.

use std::io;

use uc_core::paths::AUTOSTART_VALUE_NAME;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows::core::HSTRING;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

fn check(status: WIN32_ERROR) -> io::Result<()> {
    if status.is_ok() {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status.0 as i32))
    }
}

/// Adds or removes the Run value.
pub fn set(enabled: bool) -> io::Result<()> {
    let key = HSTRING::from(RUN_KEY);
    let name = HSTRING::from(AUTOSTART_VALUE_NAME);
    if enabled {
        let exe = std::env::current_exe()?;
        let value: Vec<u16> = format!("\"{}\"", exe.display())
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: `value` is a NUL-terminated UTF-16 buffer of the given byte
        // length; key and name are NUL-terminated HSTRINGs.
        check(unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &key,
                &name,
                REG_SZ.0,
                Some(value.as_ptr().cast()),
                (value.len() * 2) as u32,
            )
        })?;
        tracing::info!(exe = %exe.display(), "autostart registered");
    } else {
        // SAFETY: plain FFI call with NUL-terminated strings.
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &name) };
        if status != ERROR_FILE_NOT_FOUND {
            check(status)?;
            tracing::info!("autostart removed");
        }
    }
    Ok(())
}
