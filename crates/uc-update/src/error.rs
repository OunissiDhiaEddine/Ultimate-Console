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

    /// Running from a dev or CI build, not from a Velopack install.
    #[error("this copy was not installed with the installer, so it can't update itself")]
    NotInstalled,

    #[error("update failed: {0}")]
    Velopack(#[from] velopack::Error),

    #[error("invalid update repo {0:?}: expected \"owner/name\"")]
    InvalidRepo(String),

    #[error("not supported on this platform")]
    Unsupported,
}

#[cfg(windows)]
impl Error {
    pub(crate) fn win32(context: &'static str, source: windows::core::Error) -> Self {
        Self::Win32 { context, source }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
