use std::time::Duration;

pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by `uc-display`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{call} failed with Win32 error {code}")]
    Win32 { call: &'static str, code: u32 },

    #[error("display not found: {0}")]
    NotFound(String),

    #[error("more than one display matches {0}; pick the display again in the settings app")]
    Ambiguous(String),

    #[error("display {display} did not become available within {timeout:?}")]
    Timeout { display: String, timeout: Duration },

    #[error("saved topology is unusable: {0}")]
    BadTopology(String),

    #[error("display switching is only supported on Windows")]
    Unsupported,
}

impl Error {
    #[cfg(windows)]
    pub(crate) fn win32(call: &'static str, code: impl TryInto<u32>) -> Self {
        Self::Win32 {
            call,
            code: code.try_into().unwrap_or(u32::MAX),
        }
    }
}
