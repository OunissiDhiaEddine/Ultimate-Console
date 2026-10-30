//! Plain data types shared by every [`AudioControl`](crate::AudioControl) implementation.

use uc_core::ipc::AudioDeviceInfo;

/// An output endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    /// Endpoint id (`IMMDevice::GetId`). Stable across reboots; this is what
    /// `config.audio.device_id` and `state.saved_audio_device` store.
    pub id: String,
    /// Friendly name, e.g. `Speakers (Realtek(R) Audio)` or `IRIS (NVIDIA High Definition Audio)`.
    pub name: String,
    pub is_default: bool,
}

impl From<AudioDevice> for AudioDeviceInfo {
    fn from(d: AudioDevice) -> Self {
        Self {
            id: d.id,
            name: d.name,
            is_default: d.is_default,
        }
    }
}

/// One app's audio on the default output.
#[derive(Debug, Clone, PartialEq)]
pub struct AppSession {
    /// Process id. `0` for the Windows system sounds session.
    pub pid: u32,
    /// Executable file name (`steam.exe`), or the session's display name when
    /// the process can't be opened.
    pub name: String,
    /// `0.0..=1.0`, relative to the master volume.
    pub volume: f32,
    pub muted: bool,
}

/// Collapses several sessions of the same process into one entry, keeping the
/// first-seen order. Apps often open more than one session (one per stream);
/// the volume is taken from the first one and the app counts as muted only if
/// every session is muted.
pub fn merge_sessions(sessions: impl IntoIterator<Item = AppSession>) -> Vec<AppSession> {
    let mut out: Vec<AppSession> = Vec::new();
    for s in sessions {
        match out.iter_mut().find(|o| o.pid == s.pid) {
            Some(o) => {
                o.muted &= s.muted;
                if o.name.is_empty() {
                    o.name = s.name;
                }
            }
            None => out.push(s),
        }
    }
    out
}

/// `40` → `0.4`. Values above 100 are clamped.
pub fn percent_to_level(percent: u8) -> f32 {
    f32::from(percent.min(100)) / 100.0
}

/// `0.4` → `40`, rounded to the nearest percent like the Windows flyout.
pub fn level_to_percent(level: f32) -> u8 {
    // The clamp keeps the cast in range; NaN becomes 0.
    (level.clamp(0.0, 1.0) * 100.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(pid: u32, name: &str, volume: f32, muted: bool) -> AppSession {
        AppSession {
            pid,
            name: name.into(),
            volume,
            muted,
        }
    }

    #[test]
    fn merge_keeps_order_and_combines_mute() {
        let merged = merge_sessions([
            s(10, "game.exe", 0.8, true),
            s(0, "System Sounds", 1.0, false),
            s(10, "", 0.3, false),
            s(20, "", 0.5, true),
            s(20, "steam.exe", 0.5, true),
        ]);
        assert_eq!(
            merged,
            vec![
                s(10, "game.exe", 0.8, false),
                s(0, "System Sounds", 1.0, false),
                s(20, "steam.exe", 0.5, true),
            ]
        );
    }

    #[test]
    fn percent_round_trip() {
        for p in 0..=100u8 {
            assert_eq!(level_to_percent(percent_to_level(p)), p);
        }
        assert_eq!(percent_to_level(250), 1.0);
        assert_eq!(level_to_percent(1.7), 100);
        assert_eq!(level_to_percent(-1.0), 0);
        assert_eq!(level_to_percent(f32::NAN), 0);
    }

    #[test]
    fn device_converts_to_ipc_info() {
        let info: AudioDeviceInfo = AudioDevice {
            id: "{0.0.0.00000000}.{abc}".into(),
            name: "TV".into(),
            is_default: true,
        }
        .into();
        assert_eq!(info.id, "{0.0.0.00000000}.{abc}");
        assert!(info.is_default);
    }
}
