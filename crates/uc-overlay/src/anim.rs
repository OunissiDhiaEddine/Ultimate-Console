//! Fade in/out timing.

use std::time::{Duration, Instant};

pub const FADE_IN: Duration = Duration::from_millis(140);
pub const FADE_OUT: Duration = Duration::from_millis(110);

/// Opacity moving from one value to another over a fixed time, eased out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fade {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
}

impl Fade {
    pub fn new(from: f32, to: f32, start: Instant, duration: Duration) -> Self {
        Self {
            from,
            to,
            start,
            duration,
        }
    }

    /// A fade that has already finished at `value`.
    pub fn settled(value: f32, now: Instant) -> Self {
        Self::new(value, value, now, Duration::ZERO)
    }

    pub fn target(&self) -> f32 {
        self.to
    }

    fn progress(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let t =
            now.saturating_duration_since(self.start).as_secs_f32() / self.duration.as_secs_f32();
        t.clamp(0.0, 1.0)
    }

    /// Opacity at `now`, 0–1.
    pub fn value(&self, now: Instant) -> f32 {
        let t = self.progress(now);
        let eased = 1.0 - (1.0 - t) * (1.0 - t);
        self.from + (self.to - self.from) * eased
    }

    pub fn done(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }

    /// Start moving towards `to` from wherever the current fade is now.
    pub fn retarget(&self, to: f32, now: Instant, duration: Duration) -> Self {
        Self::new(self.value(now), to, now, duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fades_from_start_to_end() {
        let t = Instant::now();
        let f = Fade::new(0.0, 1.0, t, Duration::from_millis(100));
        assert_eq!(f.value(t), 0.0);
        assert!(f.value(t + Duration::from_millis(50)) > 0.5);
        assert_eq!(f.value(t + Duration::from_millis(100)), 1.0);
        assert!(f.done(t + Duration::from_millis(100)));
        assert!(!f.done(t + Duration::from_millis(99)));
    }

    #[test]
    fn retarget_continues_from_current_value() {
        let t = Instant::now();
        let f = Fade::new(0.0, 1.0, t, Duration::from_millis(100));
        let mid = t + Duration::from_millis(50);
        let g = f.retarget(0.0, mid, Duration::from_millis(100));
        assert_eq!(g.value(mid), f.value(mid));
        assert_eq!(g.target(), 0.0);
        assert!(Fade::settled(1.0, t).done(t));
    }
}
