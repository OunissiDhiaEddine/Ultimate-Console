//! In-memory [`AudioControl`] for tests and for building the overlay without
//! real audio hardware.

use std::sync::Mutex;

use crate::{AppSession, AudioControl, AudioDevice, Error, Result, check_level};

#[derive(Debug)]
struct Inner {
    devices: Vec<AudioDevice>,
    volume: f32,
    muted: bool,
    sessions: Vec<AppSession>,
}

/// Fake audio stack. Starts with two devices (`speakers` is the default,
/// plus `tv`), volume 0.5, unmuted and no sessions.
#[derive(Debug)]
pub struct FakeAudio(Mutex<Inner>);

impl Default for FakeAudio {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeAudio {
    pub fn new() -> Self {
        let dev = |id: &str, name: &str, is_default| AudioDevice {
            id: id.into(),
            name: name.into(),
            is_default,
        };
        Self(Mutex::new(Inner {
            devices: vec![dev("speakers", "Speakers", true), dev("tv", "TV", false)],
            volume: 0.5,
            muted: false,
            sessions: Vec::new(),
        }))
    }

    /// Adds an app session, as if `pid` had started playing sound.
    pub fn add_session(&self, pid: u32, name: &str, volume: f32) {
        self.lock().sessions.push(AppSession {
            pid,
            name: name.into(),
            volume,
            muted: false,
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding the lock can only come from a test; keep going.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn with_app(&self, pid: u32, f: impl Fn(&mut AppSession)) -> Result<()> {
        let mut inner = self.lock();
        let mut found = false;
        for s in inner.sessions.iter_mut().filter(|s| s.pid == pid) {
            f(s);
            found = true;
        }
        if found {
            Ok(())
        } else {
            Err(Error::NoSession(pid))
        }
    }
}

impl AudioControl for FakeAudio {
    fn outputs(&self) -> Result<Vec<AudioDevice>> {
        Ok(self.lock().devices.clone())
    }

    fn default_output(&self) -> Result<Option<AudioDevice>> {
        Ok(self.lock().devices.iter().find(|d| d.is_default).cloned())
    }

    fn set_default_output(&self, id: &str) -> Result<()> {
        let mut inner = self.lock();
        if !inner.devices.iter().any(|d| d.id == id) {
            return Err(Error::DeviceNotFound(id.into()));
        }
        for d in &mut inner.devices {
            d.is_default = d.id == id;
        }
        Ok(())
    }

    fn volume(&self) -> Result<f32> {
        Ok(self.lock().volume)
    }

    fn set_volume(&self, level: f32) -> Result<()> {
        self.lock().volume = check_level(level)?;
        Ok(())
    }

    fn muted(&self) -> Result<bool> {
        Ok(self.lock().muted)
    }

    fn set_muted(&self, muted: bool) -> Result<()> {
        self.lock().muted = muted;
        Ok(())
    }

    fn sessions(&self) -> Result<Vec<AppSession>> {
        Ok(crate::merge_sessions(self.lock().sessions.clone()))
    }

    fn set_app_volume(&self, pid: u32, level: f32) -> Result<()> {
        let level = check_level(level)?;
        self.with_app(pid, |s| s.volume = level)
    }

    fn set_app_muted(&self, pid: u32, muted: bool) -> Result<()> {
        self.with_app(pid, |s| s.muted = muted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_device_switch() {
        let audio = FakeAudio::new();
        assert_eq!(audio.default_output().unwrap().unwrap().id, "speakers");
        audio.set_default_output("tv").unwrap();
        let outputs = audio.outputs().unwrap();
        assert_eq!(outputs.iter().filter(|d| d.is_default).count(), 1);
        assert_eq!(audio.default_output().unwrap().unwrap().id, "tv");
        assert!(matches!(
            audio.set_default_output("nope"),
            Err(Error::DeviceNotFound(_))
        ));
    }

    #[test]
    fn app_volume_and_mute() {
        let audio = FakeAudio::new();
        audio.add_session(42, "game.exe", 1.0);
        audio.add_session(42, "game.exe", 1.0);
        audio.set_app_volume(42, 0.25).unwrap();
        audio.set_app_muted(42, true).unwrap();
        let s = audio.app_session(42).unwrap().unwrap();
        assert_eq!((s.volume, s.muted), (0.25, true));
        assert_eq!(audio.sessions().unwrap().len(), 1);
        assert!(matches!(
            audio.set_app_volume(7, 0.5),
            Err(Error::NoSession(7))
        ));
        assert!(matches!(
            audio.set_app_volume(42, 2.0),
            Err(Error::InvalidLevel(_))
        ));
        assert!(audio.app_session(7).unwrap().is_none());
    }
}
