//! Process-level Win32 bits: single instance, restart after a crash, the
//! user's SID for the pipe name.

use std::io;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::System::Recovery::{
    REGISTER_APPLICATION_RESTART_FLAGS, RESTART_NO_PATCH, RESTART_NO_REBOOT,
    RegisterApplicationRestart,
};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};
use windows::core::{HSTRING, PWSTR, w};

/// Holds the single-instance mutex for the life of the process.
pub struct InstanceGuard(HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: our own mutex handle, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// `None` if another agent already runs in this session.
pub fn single_instance() -> io::Result<Option<InstanceGuard>> {
    let name = HSTRING::from(uc_core::paths::agent_mutex_name());
    // SAFETY: plain FFI call; GetLastError is read right after it.
    unsafe {
        let handle = CreateMutexW(None, false, &name)?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            return Ok(None);
        }
        Ok(Some(InstanceGuard(handle)))
    }
}

/// Asks Windows to start the agent again if it crashes or hangs (plan §2.3).
/// Windows only does this for processes that ran at least 60 s. Not after
/// reboots or updates: the Run key starts it then.
pub fn register_restart() {
    let flags = REGISTER_APPLICATION_RESTART_FLAGS(RESTART_NO_PATCH.0 | RESTART_NO_REBOOT.0);
    // SAFETY: plain FFI call with a static string.
    if let Err(e) = unsafe { RegisterApplicationRestart(w!("--restarted"), flags) } {
        tracing::warn!("RegisterApplicationRestart failed: {e}");
    }
}

/// String SID of the user running this process, e.g. `S-1-5-21-…`.
pub fn current_user_sid() -> io::Result<String> {
    // SAFETY: the token handle is closed before returning; the buffer is
    // u64-aligned and sized by the first GetTokenInformation call, so it holds
    // a valid TOKEN_USER; the SID string is freed with LocalFree as
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
