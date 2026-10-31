//! Install, update and uninstall for Ultimate Console (WP-10, plan §2.12).
//!
//! The app is packaged with [Velopack](https://velopack.io) as a per-user
//! install in `%LOCALAPPDATA%\UltimateConsole` (no admin). `uc-settings.exe`
//! is Velopack's *main exe*: it owns the Start-menu shortcut, opens after the
//! first install and runs the install/uninstall hooks ([`lifecycle`]).
//! `uc-agent.exe` sits next to it in `current\` and is started at logon from
//! the HKCU Run key ([`autostart`]).
//!
//! - [`lifecycle`]: the Velopack hooks. Install registers autostart, first run
//!   starts the agent, uninstall stops the agent and deletes everything.
//! - [`updater`]: checks and downloads updates from this repo's private GitHub
//!   Releases, and applies one when the agent says it's a good time.
//! - [`token`]: the GitHub token for private releases, DPAPI-encrypted.
//! - [`autostart`]: the HKCU Run value.
//! - [`uninstall`]: deletes the app's own files (config, state, keys, logs).
//!
//! Like every crate here this one has no policy: the agent decides *when* to
//! check and apply (daily, and never in gaming mode).

pub mod autostart;
mod error;
pub mod lifecycle;
pub mod token;
pub mod uninstall;
pub mod updater;

pub use error::{Error, Result};
pub use token::TokenStore;
pub use updater::{PendingUpdate, Updater};

/// Velopack package id. Also the "Apps & features" key name and the install
/// folder name, so it must match [`uc_core::APP_NAME`].
pub const PACKAGE_ID: &str = uc_core::APP_NAME;

/// Agent executable, installed next to the settings app.
pub const AGENT_EXE: &str = if cfg!(windows) {
    "uc-agent.exe"
} else {
    "uc-agent"
};

/// Argument Velopack passes to the main exe (`uc-settings`) when it restarts
/// it after an update: start the agent and exit without showing a window.
pub const START_AGENT_ARG: &str = "--start-agent";
