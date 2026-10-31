//! Hosts the integrated overlay (WP-07) inside the agent (plan §2.9).
//!
//! The agent decides *when* the overlay may show (gaming mode, or desktop
//! mode when `overlay.in_desktop_mode` is set); the overlay crate draws it on
//! its own UI thread. Until `uc-overlay` lands this host only logs, so the
//! rest of the agent is already wired to it.

use uc_core::Config;

pub struct OverlayHost {
    _private: (),
}

impl OverlayHost {
    /// Creates the (hidden) overlay. Never fails: without an overlay the
    /// agent still does everything else.
    pub fn start(_config: &Config) -> Self {
        tracing::info!("overlay not available in this build yet (WP-07)");
        Self { _private: () }
    }

    /// Shows or hides it. Call from the thread that received the hotkey.
    pub fn toggle(&self) {
        tracing::info!("overlay hotkey: overlay not available in this build yet (WP-07)");
    }

    /// Hides it if shown (e.g. when gaming mode ends).
    pub fn hide(&self) {}

    pub fn set_config(&self, _config: &Config) {}
}
