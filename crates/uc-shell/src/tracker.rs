//! Debounced Big Picture presence (plan §2.8): Big Picture briefly closes and
//! reopens its window when it restarts itself (updates, display changes), so
//! "closed" is only reported once it has been gone for the whole debounce.

use std::time::{Duration, Instant};

use crate::WindowInfo;

/// What the watcher reports. Events always alternate, starting with
/// `Appeared`: a window that comes back within the debounce (even with a new
/// `HWND`) produces no event; [`BpTracker::current`] has the new handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BpEvent {
    Appeared(WindowInfo),
    Closed,
}

#[derive(Debug, Clone)]
enum State {
    Absent,
    Present(WindowInfo),
    /// Gone since `since`, not reported yet.
    Missing {
        since: Instant,
        last: WindowInfo,
    },
}

/// Turns a series of scans into [`BpEvent`]s.
#[derive(Debug, Clone)]
pub struct BpTracker {
    debounce: Duration,
    state: State,
}

impl BpTracker {
    pub fn new(debounce: Duration) -> Self {
        Self {
            debounce,
            state: State::Absent,
        }
    }

    /// Feeds one scan result taken at `now`.
    pub fn observe(&mut self, found: Option<WindowInfo>, now: Instant) -> Option<BpEvent> {
        let state = std::mem::replace(&mut self.state, State::Absent);
        let (next, event) = match (state, found) {
            (State::Absent, None) => (State::Absent, None),
            (State::Absent, Some(w)) => (State::Present(w.clone()), Some(BpEvent::Appeared(w))),
            (State::Present(_) | State::Missing { .. }, Some(w)) => (State::Present(w), None),
            (State::Present(last), None) => {
                self.state = State::Missing { since: now, last };
                return self.check_deadline(now);
            }
            (State::Missing { since, last }, None) => {
                self.state = State::Missing { since, last };
                return self.check_deadline(now);
            }
        };
        self.state = next;
        event
    }

    fn check_deadline(&mut self, now: Instant) -> Option<BpEvent> {
        match self.state {
            State::Missing { since, .. }
                if now.saturating_duration_since(since) >= self.debounce =>
            {
                self.state = State::Absent;
                Some(BpEvent::Closed)
            }
            _ => None,
        }
    }

    /// When a pending close will be reported if nothing reappears. The
    /// watcher sets a timer for it so the close isn't late by a poll interval.
    pub fn close_deadline(&self) -> Option<Instant> {
        match self.state {
            State::Missing { since, .. } => Some(since + self.debounce),
            _ => None,
        }
    }

    /// True between `Appeared` and `Closed`.
    pub fn is_open(&self) -> bool {
        !matches!(self.state, State::Absent)
    }

    /// The Big Picture window, if it is on screen right now.
    pub fn current(&self) -> Option<&WindowInfo> {
        match &self.state {
            State::Present(w) => Some(w),
            _ => None,
        }
    }

    /// The handle tracked last, including one that just disappeared.
    pub fn last_known(&self) -> Option<&WindowInfo> {
        match &self.state {
            State::Present(w) | State::Missing { last: w, .. } => Some(w),
            State::Absent => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bp(hwnd: isize) -> WindowInfo {
        WindowInfo {
            hwnd,
            pid: 1,
            class: "SDL_app".into(),
            title: "Steam Big Picture Mode".into(),
            visible: true,
        }
    }

    const DEBOUNCE: Duration = Duration::from_secs(2);

    #[test]
    fn appear_then_close_after_debounce() {
        let t0 = Instant::now();
        let mut t = BpTracker::new(DEBOUNCE);
        assert_eq!(t.observe(None, t0), None);
        assert_eq!(t.observe(Some(bp(1)), t0), Some(BpEvent::Appeared(bp(1))));
        assert_eq!(t.observe(Some(bp(1)), t0 + Duration::from_secs(1)), None);

        let gone = t0 + Duration::from_secs(5);
        assert_eq!(t.observe(None, gone), None);
        assert_eq!(t.close_deadline(), Some(gone + DEBOUNCE));
        assert!(t.is_open());
        assert!(t.current().is_none());
        assert_eq!(t.last_known(), Some(&bp(1)));

        assert_eq!(t.observe(None, gone + Duration::from_millis(1999)), None);
        assert_eq!(t.observe(None, gone + DEBOUNCE), Some(BpEvent::Closed));
        assert!(!t.is_open());
        assert_eq!(t.observe(None, gone + DEBOUNCE * 2), None);
    }

    #[test]
    fn restart_within_debounce_is_silent() {
        let t0 = Instant::now();
        let mut t = BpTracker::new(DEBOUNCE);
        t.observe(Some(bp(1)), t0);
        assert_eq!(t.observe(None, t0 + Duration::from_secs(1)), None);
        // Comes back with a new window handle: no event, new handle tracked.
        assert_eq!(t.observe(Some(bp(2)), t0 + Duration::from_secs(2)), None);
        assert_eq!(t.current(), Some(&bp(2)));
        assert_eq!(t.close_deadline(), None);
        // The old disappearance doesn't count any more.
        assert_eq!(t.observe(None, t0 + Duration::from_secs(3)), None);
        assert_eq!(t.observe(None, t0 + Duration::from_secs(4)), None);
        assert_eq!(
            t.observe(None, t0 + Duration::from_secs(5)),
            Some(BpEvent::Closed)
        );
    }

    #[test]
    fn zero_debounce_closes_immediately() {
        let t0 = Instant::now();
        let mut t = BpTracker::new(Duration::ZERO);
        t.observe(Some(bp(1)), t0);
        assert_eq!(t.observe(None, t0), Some(BpEvent::Closed));
    }

    #[test]
    fn reopen_after_close_appears_again() {
        let t0 = Instant::now();
        let mut t = BpTracker::new(DEBOUNCE);
        t.observe(Some(bp(1)), t0);
        t.observe(None, t0);
        assert_eq!(t.observe(None, t0 + DEBOUNCE), Some(BpEvent::Closed));
        assert_eq!(
            t.observe(Some(bp(3)), t0 + DEBOUNCE * 2),
            Some(BpEvent::Appeared(bp(3)))
        );
    }
}
