use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Errors produced by `uc-tv`.
#[derive(Debug, thiserror::Error)]
pub enum TvError {
    #[error("the TV's IP address is not set")]
    NoAddress,

    #[error("the TV's MAC address is not set (pair the TV first)")]
    NoMac,

    #[error("could not reach the TV at {addr}: {source}")]
    Connect {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },

    #[error("network error talking to the TV: {0}")]
    Io(#[from] io::Error),

    #[error("timed out waiting for {0}")]
    Timeout(&'static str),

    #[error("ADB protocol error: {0}")]
    Protocol(String),

    #[error("the TV has not authorised this PC: pair it again and pick \"Always allow\" on the TV")]
    NotAuthorized,

    #[error(
        "the TV asked for a TLS (Wireless debugging) connection, which is not supported: \
         turn on \"Network debugging\" (ADB over TCP, port 5555) instead"
    )]
    TlsRequired,

    #[error("ADB key {path}: {reason}")]
    Key { path: PathBuf, reason: String },

    #[error("could not find the MAC address of {ip}: {reason}")]
    MacLookup {
        ip: std::net::IpAddr,
        reason: String,
    },

    #[error(transparent)]
    Core(#[from] uc_core::Error),
}

impl TvError {
    /// Turns read/write timeouts into [`TvError::Timeout`] so callers can tell
    /// "slow" from "broken".
    pub(crate) fn from_io(e: io::Error, what: &'static str) -> Self {
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Self::Timeout(what),
            _ => Self::Io(e),
        }
    }

    /// True for errors where retrying the connection can help (TV still
    /// waking up, Wi-Fi coming back). False for errors that need the user.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Connect { .. } | Self::Io(_) | Self::Timeout(_))
    }
}

pub type Result<T, E = TvError> = std::result::Result<T, E>;
