//! The volume popup: a small card that shows the master volume for a couple
//! of seconds when a volume key is pressed.
//!
//! In gaming mode Explorer isn't running, and Explorer is what normally turns
//! the keyboard / headset volume keys into a volume change and shows the
//! Windows flyout. The agent registers those keys itself in gaming mode and
//! hands them to [`Overlay::volume_key`](crate::Overlay::volume_key), which
//! changes the volume and shows this popup (or, if the full overlay is open,
//! moves its master slider instead).
//!
//! This module is the platform-independent part: key codes, the volume step,
//! the popup's geometry and timing.

use std::time::Duration;

use crate::layout::Rect;

/// One press of a volume key changes the master volume by this much, like
/// the Windows flyout (2 %).
pub const STEP: f32 = 0.02;

/// How long the popup stays after the last key press, before fading out.
pub const SHOW_FOR_MS: u32 = 2000;
pub const SHOW_FOR: Duration = Duration::from_millis(SHOW_FOR_MS as u64);

/// Popup size in DIPs.
pub const WIDTH: f32 = 380.0;
pub const HEIGHT: f32 = 76.0;
/// Gap between the popup and the bottom of the screen, in DIPs.
pub const BOTTOM_MARGIN: f32 = 96.0;

/// A media volume key (`VK_VOLUME_UP` / `VK_VOLUME_DOWN` / `VK_VOLUME_MUTE`).
/// Headset and multimedia keyboard buttons arrive as these too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VolumeKey {
    Up,
    Down,
    Mute,
}

impl VolumeKey {
    pub const ALL: [VolumeKey; 3] = [VolumeKey::Up, VolumeKey::Down, VolumeKey::Mute];

    /// The Windows virtual-key code.
    pub const fn vk(self) -> u16 {
        match self {
            VolumeKey::Mute => 0xAD,
            VolumeKey::Down => 0xAE,
            VolumeKey::Up => 0xAF,
        }
    }

    pub fn from_vk(vk: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.vk() == vk)
    }

    /// Packs into a message parameter.
    pub const fn to_raw(self) -> usize {
        self.vk() as usize
    }

    pub fn from_raw(raw: usize) -> Option<Self> {
        u16::try_from(raw).ok().and_then(Self::from_vk)
    }

    /// The new master `(level, muted)` after this key, like Windows does it:
    /// up and down move in [`STEP`]s (snapping to whole steps so 0 % and
    /// 100 % are reachable) and unmute; mute toggles.
    pub fn apply(self, level: f32, muted: bool) -> (f32, bool) {
        let step = |delta: f32| {
            let l = ((level + delta) / STEP).round() * STEP;
            l.clamp(0.0, 1.0)
        };
        match self {
            VolumeKey::Up => (step(STEP), false),
            VolumeKey::Down => (step(-STEP), false),
            VolumeKey::Mute => (level, !muted),
        }
    }
}

/// What the popup shows.
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    Level {
        level: f32,
        muted: bool,
    },
    /// No default output device, or Core Audio failed.
    Unavailable(String),
}

impl View {
    /// The big number on the right.
    pub fn value_text(&self) -> String {
        match self {
            View::Level { muted: true, .. } => "Muted".into(),
            View::Level { level, .. } => format!("{:.0}", level.clamp(0.0, 1.0) * 100.0),
            View::Unavailable(_) => String::new(),
        }
    }
}

/// Where the popup goes on a monitor of `screen_w` × `screen_h` DIPs:
/// centred horizontally, near the bottom, like the Windows flyout. Relative
/// to the monitor's top-left corner.
pub fn window_rect(screen_w: f32, screen_h: f32) -> Rect {
    let w = WIDTH.min(screen_w);
    let h = HEIGHT.min(screen_h);
    let margin = BOTTOM_MARGIN.min((screen_h - h).max(0.0));
    Rect::new((screen_w - w) / 2.0, screen_h - h - margin, w, h)
}

/// The parts of the popup, in DIPs relative to its own top-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Parts {
    pub card: Rect,
    pub icon: Rect,
    pub track: Rect,
    pub value: Rect,
}

impl Parts {
    pub fn new(w: f32, h: f32) -> Self {
        let card = Rect::new(0.0, 0.0, w, h);
        let pad = 20.0;
        let icon = Rect::new(pad, 0.0, 44.0, h);
        let value_w = 76.0;
        let value = Rect::new(w - pad - value_w, 0.0, value_w, h);
        let track_x = icon.right() + 8.0;
        let track = Rect::new(
            track_x,
            h / 2.0 - 4.0,
            (value.x - 16.0 - track_x).max(10.0),
            8.0,
        );
        Self {
            card,
            icon,
            track,
            value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vk_roundtrip() {
        for k in VolumeKey::ALL {
            assert_eq!(VolumeKey::from_vk(k.vk()), Some(k));
            assert_eq!(VolumeKey::from_raw(k.to_raw()), Some(k));
        }
        assert_eq!(VolumeKey::from_vk(0x41), None);
        assert_eq!(VolumeKey::from_raw(usize::MAX), None);
    }

    #[test]
    fn up_and_down_step_snap_clamp_and_unmute() {
        let (l, m) = VolumeKey::Up.apply(0.5, true);
        assert!((l - 0.52).abs() < 1e-5);
        assert!(!m);
        // Off-grid levels snap to whole steps.
        let (l, _) = VolumeKey::Down.apply(0.513, false);
        assert!((l - 0.50).abs() < 1e-5, "{l}");
        assert_eq!(VolumeKey::Up.apply(0.995, false).0, 1.0);
        assert_eq!(VolumeKey::Down.apply(0.01, false).0, 0.0);
        assert_eq!(VolumeKey::Down.apply(0.0, true), (0.0, false));
    }

    #[test]
    fn mute_toggles_and_keeps_level() {
        assert_eq!(VolumeKey::Mute.apply(0.3, false), (0.3, true));
        assert_eq!(VolumeKey::Mute.apply(0.3, true), (0.3, false));
    }

    #[test]
    fn value_text() {
        let v = |level, muted| View::Level { level, muted }.value_text();
        assert_eq!(v(0.42, false), "42");
        assert_eq!(v(1.0, false), "100");
        assert_eq!(v(0.42, true), "Muted");
        assert_eq!(View::Unavailable("x".into()).value_text(), "");
    }

    #[test]
    fn sits_bottom_centre_and_fits_small_screens() {
        let r = window_rect(1920.0, 1080.0);
        assert_eq!(r.w, WIDTH);
        assert!((r.x + r.w / 2.0 - 960.0).abs() < 0.01);
        assert!((r.bottom() - (1080.0 - BOTTOM_MARGIN)).abs() < 0.01);
        let r = window_rect(300.0, 100.0);
        assert!(r.x >= 0.0 && r.right() <= 300.0);
        assert!(r.y >= 0.0 && r.bottom() <= 100.0);
    }

    #[test]
    fn parts_are_inside_and_ordered() {
        let p = Parts::new(WIDTH, HEIGHT);
        assert!(p.icon.right() <= p.track.x);
        assert!(p.track.right() <= p.value.x);
        assert!(p.value.right() <= p.card.right());
        assert!(p.track.w > 100.0);
    }
}
