//! Explorer exit/start, Steam launch and Big Picture window detection (WP-03).
//!
//! - [`explorer`]: ask the Explorer shell to exit cleanly (no sign-out, no
//!   Winlogon auto-restart) and start it again (plan §2.3).
//! - [`steam`]: find `steam.exe`, open and close Big Picture, find its window.
//! - [`BigPictureWatcher`]: WinEvent hook + 1 s poll that reports when Big
//!   Picture appears and when it has been gone for longer than the debounce
//!   (plan §2.8).
//!
//! The window matching and debounce logic ([`WindowMatcher`], [`BpTracker`])
//! is plain Rust and tested on every platform. Everything that calls Win32 is
//! Windows-only. Like every crate here, this one has no policy: it never
//! decides *when* to exit Explorer, the orchestrator does.

mod error;
pub mod matcher;
pub mod steam;
pub mod tracker;

#[cfg(windows)]
pub mod explorer;
#[cfg(windows)]
mod watcher;
#[cfg(windows)]
mod win;

pub use error::{Error, Result};
pub use matcher::{WindowInfo, WindowMatcher};
pub use tracker::{BpEvent, BpTracker};

#[cfg(windows)]
pub use watcher::BigPictureWatcher;
#[cfg(windows)]
pub use win::{ProcessInfo, list_processes, list_windows};
