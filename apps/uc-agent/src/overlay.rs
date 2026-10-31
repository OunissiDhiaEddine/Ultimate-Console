//! Hosts the integrated overlay (`uc-overlay`, WP-07) inside the agent
//! (plan §2.9).
//!
//! The agent decides *when* the overlay may show (gaming mode, or desktop
//! mode when `overlay.in_desktop_mode` is set); the overlay crate draws it on
//! its own UI thread.

use uc_core::Config;
use uc_overlay::Overlay;

pub struct OverlayHost {
    /// `None` if the overlay could not start; the agent still does
    /// everything else.
    overlay: Option<Overlay>,
}

impl OverlayHost {
    /// Creates the (hidden) overlay. Never fails.
    pub fn start(config: &Config) -> Self {
        let overlay = match Overlay::start(config) {
            Ok(o) => Some(o),
            Err(e) => {
                tracing::error!("overlay unavailable: {e}");
                None
            }
        };
        Self { overlay }
    }

    /// Shows or hides it. Call from the thread that received the hotkey.
    pub fn toggle(&self) {
        match &self.overlay {
            Some(o) => o.toggle(),
            None => tracing::info!("overlay hotkey: the overlay failed to start, see above"),
        }
    }

    /// Hides it if shown (e.g. when gaming mode ends).
    pub fn hide(&self) {
        if let Some(o) = &self.overlay
            && o.is_visible()
        {
            o.hide();
        }
    }

    pub fn set_config(&self, config: &Config) {
        if let Some(o) = &self.overlay {
            o.set_config(config);
        }
    }
}
