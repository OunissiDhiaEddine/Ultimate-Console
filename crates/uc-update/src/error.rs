use std::path::PathBuf;

/// Errors produced by `uc-update`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[cfg(windows)]
    #[error("{context} failed: {source}")]
    Win32 {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },

    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Running from a dev or CI build, not from a Velopack install.
    #[error("this copy was not installed with the installer, so it can't update itself")]
    NotInstalled,

    #[error("update failed: {0}")]
    Velopack(#[from] velopack::Error),

    #[error("invalid GitHub token: {0}")]
    InvalidToken(&'static str),

    #[error("invalid update repo {0:?}: expected \"owner/name\"")]
    InvalidRepo(String),

    #[error("not supported on this platform")]
    Unsupported,
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    #[cfg(windows)]
    pub(crate) fn win32(context: &'static str, source: windows::core::Error) -> Self {
        Self::Win32 { context, source }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
