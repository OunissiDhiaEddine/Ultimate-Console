//! Shared building blocks for every Ultimate Console crate and app.
//!
//! This crate is pure Rust with no Win32 calls, so it builds and tests on any
//! platform. It owns the contracts the other work packages rely on:
//!
//! - [`Config`]: the user's settings (`%APPDATA%\UltimateConsole\config.toml`).
//! - [`RuntimeState`]: crash-recovery state (`%LOCALAPPDATA%\UltimateConsole\state.json`).
//! - [`ipc`]: the JSON-lines protocol between `uc-settings` and `uc-agent`.
//! - [`Paths`]: where every file lives.
//! - [`logging`]: `tracing` setup with daily rolling files.

pub mod config;
pub mod error;
pub mod fsutil;
pub mod hotkey;
pub mod ipc;
pub mod logging;
pub mod paths;
pub mod state;

pub use config::Config;
pub use error::{Error, Result};
pub use hotkey::{Hotkey, Key, Modifiers};
pub use paths::Paths;
pub use state::{Mode, RuntimeState};

/// Product name, used for directory names, the pipe name and window titles.
pub const APP_NAME: &str = "UltimateConsole";

/// Version of the workspace, shared by every binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
