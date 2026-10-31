//! Start the agent at logon: a value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (plan §2.2).
//!
//! The value is named [`uc_core::paths::AUTOSTART_VALUE_NAME`] and holds the
//! quoted path of `uc-agent.exe`. Under Velopack that path is
//! `…\UltimateConsole\current\uc-agent.exe`, which stays the same across
//! updates, so the value only needs writing at install time.

use std::path::Path;

use crate::Result;

/// Registry path of the per-user Run key, relative to HKCU.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The command line stored in the Run value for `exe`.
pub fn run_command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// Starts `exe` at every logon (overwrites an existing value).
pub fn register(exe: &Path) -> Result<()> {
    sys::set(&run_command(exe))?;
    tracing::info!(exe = %exe.display(), "autostart registered");
    Ok(())
}

/// Removes the Run value. Not an error if it isn't there.
pub fn unregister() -> Result<()> {
    sys::delete()?;
    tracing::info!("autostart removed");
    Ok(())
}

/// The current Run value, if any.
pub fn registered_command() -> Option<String> {
    sys::get()
}

#[cfg(windows)]
mod sys {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };
    use windows::core::HSTRING;

    use super::RUN_KEY;
    use crate::{Error, Result};
    use uc_core::paths::AUTOSTART_VALUE_NAME;

    fn check(context: &'static str, err: WIN32_ERROR) -> Result<()> {
        err.ok().map_err(|e| Error::win32(context, e))
    }

    pub fn set(command: &str) -> Result<()> {
        let wide: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer of exactly the
        // byte length passed, alive for the whole call.
        let err = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                &HSTRING::from(AUTOSTART_VALUE_NAME),
                REG_SZ.0,
                Some(wide.as_ptr().cast()),
                (wide.len() * 2) as u32,
            )
        };
        check("RegSetKeyValueW(Run)", err)
    }

    pub fn delete() -> Result<()> {
        // SAFETY: plain registry call with owned, NUL-terminated strings.
        let err = unsafe {
            RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                &HSTRING::from(AUTOSTART_VALUE_NAME),
            )
        };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        check("RegDeleteKeyValueW(Run)", err)
    }

    pub fn get() -> Option<String> {
        let (key, value) = (HSTRING::from(RUN_KEY), HSTRING::from(AUTOSTART_VALUE_NAME));
        let mut size = 0u32;
        // SAFETY: the first call only reports the size; the second writes at
        // most `size` bytes into a buffer at least that large.
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &key,
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
                &key,
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
}

#[cfg(not(windows))]
mod sys {
    use crate::{Error, Result};

    pub fn set(_command: &str) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub fn delete() -> Result<()> {
        Ok(())
    }

    pub fn get() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_quoted() {
        assert_eq!(
            run_command(Path::new(
                r"C:\Users\Dhia\AppData\Local\UltimateConsole\current\uc-agent.exe"
            )),
            r#""C:\Users\Dhia\AppData\Local\UltimateConsole\current\uc-agent.exe""#
        );
    }
}
