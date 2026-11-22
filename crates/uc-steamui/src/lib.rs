//! The Ultimate Console tab in Steam Big Picture's Quick Access Menu
//! (Steam + A, also in the in-game overlay). It replaces the full-screen
//! overlay: volume, sound output, windows, performance and batteries now live
//! inside Steam's own UI, where the controller already works.
//!
//! How it gets there:
//!
//! 1. Steam's UI is Chromium (CEF). When the file
//!    `.cef-enable-remote-debugging` is in Steam's folder at startup, Steam
//!    opens the Chrome DevTools Protocol on `127.0.0.1:8080` ([`flag`]).
//! 2. [`SteamMenu`] runs one thread that finds Steam's `SharedJSContext`
//!    target ([`cdp`]), registers a DevTools binding and evaluates
//!    `js/menu.js` there. The script hooks the Quick Access Menu's React
//!    component and adds the tab (see the comment at the top of the script).
//! 3. The tab calls back into the agent through the binding
//!    ([`protocol`]); a [`Handler`] answers on the same thread. The real one,
//!    [`SystemHandler`], uses `uc-audio`, `uc-sysinfo`, `uc-battery` and the
//!    overlay's Win32 helpers, and asks the agent ([`Host`]) for policy
//!    (mode, leaving gaming mode).
//!
//! Steam reloads its UI now and then (leaving Big Picture, updates); the
//! thread notices and injects again. Idle cost while Steam is not reachable:
//! one localhost connect attempt every few seconds.
//!
//! Steam's UI internals are not an API and change with Steam updates. The
//! script looks everything up defensively and reports what it found
//! ([`Status::Ready`]); `uc-probe steam-menu` shows the details.

pub mod cdp;
pub mod flag;
pub mod handler;
pub mod protocol;

mod bridge;

pub use bridge::{Settings, Status, SteamMenu};
pub use handler::{Handler, Host, HostStatus, SystemHandler};

/// Errors from talking to Steam's DevTools port.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Steam's DevTools port {port} is not open ({source})")]
    NoDebugPort {
        port: u16,
        #[source]
        source: std::io::Error,
    },

    #[error("I/O error talking to Steam: {0}")]
    Io(#[from] std::io::Error),

    #[error("bad answer from Steam's DevTools port: {0}")]
    Http(String),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("the DevTools connection closed")]
    Closed,

    #[error("Steam did not answer {0} in time")]
    Timeout(String),

    #[error("{method} failed: {message}")]
    Cdp { method: String, message: String },

    #[error("the script failed in Steam: {0}")]
    Script(String),

    #[error("not connected to Steam")]
    NotConnected,

    #[error("bad JSON: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
