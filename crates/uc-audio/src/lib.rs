//! Audio endpoints, default device switching, master and per-app volume (WP-04).
//!
//! Plan §2.5: on entering gaming mode the agent can switch the default output
//! to the TV's HDMI endpoint and switch it back on exit; the overlay shows the
//! master volume and mute plus the volume of the selected app.
//!
//! - [`AudioControl`] is the API the agent and the overlay use. It is a trait
//!   so the overlay can be built and tested against [`FakeAudio`].
//! - [`CoreAudio`] implements it with Windows Core Audio
//!   (`IMMDeviceEnumerator`, `IAudioEndpointVolume`, `IAudioSessionManager2`)
//!   and the undocumented `IPolicyConfig` to change the default device. On
//!   other platforms [`CoreAudio::new`] returns [`Error::Unsupported`].
//!
//! Volume levels are linear scalars from `0.0` to `1.0`, the same value the
//! Windows volume flyout shows as a percentage. Only output (render) devices
//! are covered.

mod fake;
mod model;
#[cfg(windows)]
mod win;

pub use fake::FakeAudio;
pub use model::{AppSession, AudioDevice, level_to_percent, merge_sessions, percent_to_level};
#[cfg(windows)]
pub use win::CoreAudio;

/// Errors produced by `uc-audio`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("audio control is only available on Windows")]
    Unsupported,

    #[error("{context} failed: {message} (HRESULT {code:#010X})")]
    Os {
        context: &'static str,
        code: u32,
        message: String,
    },

    #[error("there is no default audio output device")]
    NoDefaultDevice,

    #[error("audio output device not found or not active: {0}")]
    DeviceNotFound(String),

    #[error("process {0} has no audio session on the default output device")]
    NoSession(u32),

    #[error("volume level must be between 0.0 and 1.0, got {0}")]
    InvalidLevel(f32),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Checks that `level` is a usable volume scalar.
pub fn check_level(level: f32) -> Result<f32> {
    if (0.0..=1.0).contains(&level) {
        Ok(level)
    } else {
        Err(Error::InvalidLevel(level))
    }
}

/// Audio operations used by the agent (default device switch) and the overlay
/// (volume panel). All methods act on output devices only.
///
/// Implementations may be tied to the thread that created them (COM), so the
/// trait does not require `Send`.
pub trait AudioControl {
    /// Active output devices, with the current default marked.
    fn outputs(&self) -> Result<Vec<AudioDevice>>;

    /// The current default output device, or `None` if there is none.
    fn default_output(&self) -> Result<Option<AudioDevice>>;

    /// Makes `id` (an [`AudioDevice::id`]) the default output for every role
    /// (console, multimedia, communications).
    fn set_default_output(&self, id: &str) -> Result<()>;

    /// Master volume of the default output, `0.0..=1.0`.
    fn volume(&self) -> Result<f32>;

    /// Sets the master volume of the default output.
    fn set_volume(&self, level: f32) -> Result<()>;

    fn muted(&self) -> Result<bool>;

    fn set_muted(&self, muted: bool) -> Result<()>;

    /// Apps that currently have an audio session on the default output, one
    /// entry per process (see [`merge_sessions`]).
    fn sessions(&self) -> Result<Vec<AppSession>>;

    /// Sets the volume of every session that belongs to `pid`.
    /// Returns [`Error::NoSession`] if the process has none.
    fn set_app_volume(&self, pid: u32, level: f32) -> Result<()>;

    /// Mutes or unmutes every session that belongs to `pid`.
    fn set_app_muted(&self, pid: u32, muted: bool) -> Result<()>;

    /// The session of `pid`, if it has one.
    fn app_session(&self, pid: u32) -> Result<Option<AppSession>> {
        Ok(self.sessions()?.into_iter().find(|s| s.pid == pid))
    }

    /// Changes the master volume by `delta` (e.g. `0.05` for one overlay
    /// step), clamped to `0.0..=1.0`. Returns the new level.
    fn step_volume(&self, delta: f32) -> Result<f32> {
        let level = (self.volume()? + delta).clamp(0.0, 1.0);
        self.set_volume(level)?;
        Ok(level)
    }

    /// Flips the master mute. Returns the new state.
    fn toggle_mute(&self) -> Result<bool> {
        let muted = !self.muted()?;
        self.set_muted(muted)?;
        Ok(muted)
    }
}

#[cfg(not(windows))]
mod unsupported {
    use super::*;

    /// Placeholder so dependent crates build on non-Windows hosts.
    #[derive(Debug)]
    pub struct CoreAudio(());

    impl CoreAudio {
        pub fn new() -> Result<Self> {
            Err(Error::Unsupported)
        }
    }

    impl AudioControl for CoreAudio {
        fn outputs(&self) -> Result<Vec<AudioDevice>> {
            Err(Error::Unsupported)
        }
        fn default_output(&self) -> Result<Option<AudioDevice>> {
            Err(Error::Unsupported)
        }
        fn set_default_output(&self, _: &str) -> Result<()> {
            Err(Error::Unsupported)
        }
        fn volume(&self) -> Result<f32> {
            Err(Error::Unsupported)
        }
        fn set_volume(&self, _: f32) -> Result<()> {
            Err(Error::Unsupported)
        }
        fn muted(&self) -> Result<bool> {
            Err(Error::Unsupported)
        }
        fn set_muted(&self, _: bool) -> Result<()> {
            Err(Error::Unsupported)
        }
        fn sessions(&self) -> Result<Vec<AppSession>> {
            Err(Error::Unsupported)
        }
        fn set_app_volume(&self, _: u32, _: f32) -> Result<()> {
            Err(Error::Unsupported)
        }
        fn set_app_muted(&self, _: u32, _: bool) -> Result<()> {
            Err(Error::Unsupported)
        }
    }
}
#[cfg(not(windows))]
pub use unsupported::CoreAudio;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_level_bounds() {
        assert!(check_level(0.0).is_ok());
        assert!(check_level(1.0).is_ok());
        assert!(matches!(check_level(1.01), Err(Error::InvalidLevel(_))));
        assert!(matches!(check_level(-0.1), Err(Error::InvalidLevel(_))));
        assert!(matches!(check_level(f32::NAN), Err(Error::InvalidLevel(_))));
    }

    #[test]
    fn step_and_toggle_use_trait_defaults() {
        let audio = FakeAudio::new();
        audio.set_volume(0.98).unwrap();
        assert_eq!(audio.step_volume(0.05).unwrap(), 1.0);
        assert_eq!(audio.step_volume(-0.5).unwrap(), 0.5);
        assert_eq!(audio.step_volume(-2.0).unwrap(), 0.0);
        assert!(audio.toggle_mute().unwrap());
        assert!(!audio.toggle_mute().unwrap());
    }
}
