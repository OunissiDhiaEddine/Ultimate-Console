//! Integrated Direct2D/DirectComposition overlay window and panels (WP-07).
//!
//! The overlay lives inside the agent process on its own UI thread (plan
//! §2.9). [`Overlay::start`] creates a hidden `WS_EX_TOPMOST |
//! WS_EX_NOREDIRECTIONBITMAP` window and returns a handle that any thread can
//! use to [`show`](Overlay::show), [`hide`](Overlay::hide) or
//! [`toggle`](Overlay::toggle) it.
//!
//! While hidden it costs nothing but an idle window: the D3D/D2D/DComp
//! resources, the audio client and the stats sampler are created when it
//! opens and dropped when it closes, and no timers run.
//!
//! When open it covers the monitor the foreground window is on with a dimmed
//! backdrop and a centred card with three panels:
//!
//! 1. **Tasks**: alt-tab-eligible windows with live DWM thumbnails. Switch
//!    to (A / Enter / click), close (X / Delete), end process (Y /
//!    Shift+Delete, after confirming).
//! 2. **Volume**: master volume and mute, then per-app volume with the
//!    selected task's app first.
//! 3. **Info**: CPU, GPU, RAM and uptime of the selected task, sampled at
//!    `config.overlay.sample_interval_ms`. The system line is in the footer
//!    of every panel.
//!
//! A separate small **volume popup** ([`osd`]) shows the master volume for a
//! couple of seconds when [`Overlay::volume_key`] is called (volume keys in
//! gaming mode, where Explorer doesn't handle them).
//!
//! Navigation: arrows / D-pad / left stick, Tab / LB / RB between panels,
//! Esc / B closes, `+` `-` / triggers change the master volume, M mutes.
//!
//! Policy (only in gaming mode unless `overlay.in_desktop_mode`) and the
//! hotkey belong to the agent, not here.
//!
//! The pure parts ([`model`], [`layout`], [`gamepad`], [`tasks`], [`anim`])
//! are platform independent and unit tested.

pub mod anim;
pub mod gamepad;
pub mod layout;
pub mod model;
pub mod osd;
pub mod tasks;

#[cfg(windows)]
mod win;

use std::sync::Arc;
use std::time::Duration;

pub use osd::VolumeKey;
use uc_audio::AudioControl;
use uc_sysinfo::StatsSource;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the overlay is only available on Windows")]
    Unsupported,

    #[error("{context} failed: {source}")]
    Os {
        context: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("overlay UI thread exited during startup")]
    ThreadDied,
}

impl Error {
    #[cfg(windows)]
    pub(crate) fn win(context: &'static str, e: windows::core::Error) -> Self {
        Self::Os {
            context,
            source: std::io::Error::other(e),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The parts of the config the overlay uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub sample_interval: Duration,
}

impl From<&uc_core::Config> for Settings {
    fn from(c: &uc_core::Config) -> Self {
        Self {
            sample_interval: Duration::from_millis(u64::from(
                c.overlay.sample_interval_ms.max(100),
            )),
        }
    }
}

/// Builds an audio client on the overlay thread each time it opens.
pub type AudioFactory = Arc<dyn Fn() -> uc_audio::Result<Box<dyn AudioControl>> + Send + Sync>;
/// Builds a stats source on the overlay thread each time it opens.
pub type StatsFactory = Arc<dyn Fn() -> Box<dyn StatsSource> + Send + Sync>;

/// Where the volume and stats panels get their data. [`Backends::real`] for
/// the agent; tests and `uc-probe overlay --fake` pass fakes.
#[derive(Clone)]
pub struct Backends {
    pub audio: AudioFactory,
    pub stats: StatsFactory,
}

impl Backends {
    /// Core Audio and the PDH/`GetProcessTimes` sampler.
    pub fn real() -> Self {
        Self {
            audio: Arc::new(|| Ok(Box::new(uc_audio::CoreAudio::new()?) as Box<dyn AudioControl>)),
            stats: Arc::new(|| Box::new(uc_sysinfo::Sampler::new()) as Box<dyn StatsSource>),
        }
    }
}

impl std::fmt::Debug for Backends {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Backends")
    }
}

#[cfg(windows)]
pub use win::Overlay;

/// Stand-in on other platforms: [`Overlay::start`] always fails.
#[cfg(not(windows))]
#[derive(Debug)]
pub struct Overlay(());

#[cfg(not(windows))]
impl Overlay {
    pub fn start(_config: &uc_core::Config) -> Result<Self> {
        Err(Error::Unsupported)
    }
    pub fn start_with(_settings: Settings, _backends: Backends) -> Result<Self> {
        Err(Error::Unsupported)
    }
    pub fn show(&self) {}
    pub fn hide(&self) {}
    pub fn toggle(&self) {}
    pub fn volume_key(&self, _key: VolumeKey) {}
    pub fn set_config(&self, _config: &uc_core::Config) {}
    pub fn is_visible(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_from_config() {
        let mut c = uc_core::Config::default();
        assert_eq!(Settings::from(&c).sample_interval, Duration::from_secs(1));
        c.overlay.sample_interval_ms = 5;
        assert_eq!(
            Settings::from(&c).sample_interval,
            Duration::from_millis(100)
        );
    }
}
