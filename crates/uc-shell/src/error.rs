use std::path::PathBuf;
use std::time::Duration;

/// Errors produced by `uc-shell`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[cfg(windows)]
    #[error("{context} failed: {source}")]
    Win32 {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },

    /// The shell (the process owning the taskbar) was asked to exit but is
    /// still there. It is deliberately not killed: a killed shell is
    /// restarted by Winlogon, which is exactly what we are trying to avoid.
    #[error("Explorer did not exit within {0:?}")]
    ExplorerExitTimeout(Duration),

    #[error("steam.exe not found ({0})")]
    SteamNotFound(String),

    #[error("could not start {path}: {source}")]
    Spawn {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Big Picture watcher failed to start: {0}")]
    Watcher(String),
}

#[cfg(windows)]
impl Error {
    pub(crate) fn win32(context: &'static str, source: windows::core::Error) -> Self {
        Self::Win32 { context, source }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
