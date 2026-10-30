//! Power events and how `WM_POWERBROADCAST` maps to them. Pure logic.

use std::fmt;
use std::time::{Duration, Instant};

/// `WM_POWERBROADCAST`.
pub const WM_POWERBROADCAST: u32 = 0x0218;
/// The system is about to suspend.
pub const PBT_APMSUSPEND: usize = 0x0004;
/// Resumed, and the resume was caused by user input.
pub const PBT_APMRESUMESUSPEND: usize = 0x0007;
/// Resumed (always sent, whatever woke the PC).
pub const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;
/// A registered power setting changed (here: the console display state).
pub const PBT_POWERSETTINGCHANGE: usize = 0x8013;

/// Something the agent may want to react to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PowerEvent {
    /// `PBT_APMSUSPEND`: the PC is about to sleep. The handler runs before
    /// the suspend continues and has about 1.5 s (see the crate docs).
    Suspend,
    /// `PBT_APMRESUMEAUTOMATIC`: the PC woke up. Always sent first on resume,
    /// whether or not a user is present.
    ResumeAutomatic,
    /// `PBT_APMRESUMESUSPEND`: the PC woke up because of user input. Follows
    /// [`PowerEvent::ResumeAutomatic`] when it is sent at all.
    ResumeSuspend,
    /// The console display turned off, on or dimmed
    /// (`GUID_CONSOLE_DISPLAY_STATE`). Windows sends the current state right
    /// after registration.
    Display(DisplayState),
}

impl PowerEvent {
    /// True for both resume events.
    pub fn is_resume(self) -> bool {
        matches!(self, Self::ResumeAutomatic | Self::ResumeSuspend)
    }
}

impl fmt::Display for PowerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Suspend => f.write_str("suspend"),
            Self::ResumeAutomatic => f.write_str("resume (automatic)"),
            Self::ResumeSuspend => f.write_str("resume (user input)"),
            Self::Display(s) => write!(f, "display {s}"),
        }
    }
}

/// Values of `GUID_CONSOLE_DISPLAY_STATE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisplayState {
    Off,
    On,
    Dimmed,
}

impl DisplayState {
    /// Maps the setting's DWORD value (0 off, 1 on, 2 dimmed).
    pub fn from_raw(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::On),
            2 => Some(Self::Dimmed),
            _ => None,
        }
    }
}

impl fmt::Display for DisplayState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Dimmed => "dimmed",
        })
    }
}

/// Maps the `wParam` of a `WM_POWERBROADCAST` to an event, for every type
/// except `PBT_POWERSETTINGCHANGE` (which needs the `lParam`, see
/// `decode_message` on Windows). Unknown types give `None`.
pub fn decode_pbt(wparam: usize) -> Option<PowerEvent> {
    match wparam {
        PBT_APMSUSPEND => Some(PowerEvent::Suspend),
        PBT_APMRESUMEAUTOMATIC => Some(PowerEvent::ResumeAutomatic),
        PBT_APMRESUMESUSPEND => Some(PowerEvent::ResumeSuspend),
        _ => None,
    }
}

/// Drops repeated events so each real transition is reported once.
///
/// A window can see the same suspend/resume twice (once from the
/// registration, once as a broadcast to top-level windows), and Windows
/// re-sends an unchanged display state. Rules:
///
/// - suspend/resume: an event of the same kind as one seen less than
///   [`EventFilter::REPEAT_WINDOW`] ago is dropped (tracked per kind, since
///   the two copies of a resume pair can interleave);
/// - display: a state equal to the last reported display state is dropped.
#[derive(Debug, Default)]
pub struct EventFilter {
    /// Last time each of suspend / resume automatic / resume suspend was seen.
    last_power: [Option<Instant>; 3],
    last_display: Option<DisplayState>,
}

impl EventFilter {
    /// How close two equal suspend/resume events must be to count as one.
    pub const REPEAT_WINDOW: Duration = Duration::from_secs(2);

    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if `event` should be reported.
    pub fn accept(&mut self, event: PowerEvent, now: Instant) -> bool {
        match event {
            PowerEvent::Display(state) => {
                let new = self.last_display != Some(state);
                self.last_display = Some(state);
                new
            }
            PowerEvent::Suspend | PowerEvent::ResumeAutomatic | PowerEvent::ResumeSuspend => {
                let slot = &mut self.last_power[match event {
                    PowerEvent::Suspend => 0,
                    PowerEvent::ResumeAutomatic => 1,
                    _ => 2,
                }];
                let repeat =
                    slot.is_some_and(|at| now.saturating_duration_since(at) < Self::REPEAT_WINDOW);
                *slot = Some(now);
                !repeat
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_suspend_and_resume() {
        assert_eq!(decode_pbt(4), Some(PowerEvent::Suspend));
        assert_eq!(decode_pbt(0x12), Some(PowerEvent::ResumeAutomatic));
        assert_eq!(decode_pbt(7), Some(PowerEvent::ResumeSuspend));
        // PBT_APMPOWERSTATUSCHANGE (battery/AC) and setting changes are not
        // plain events.
        assert_eq!(decode_pbt(0xA), None);
        assert_eq!(decode_pbt(PBT_POWERSETTINGCHANGE), None);
    }

    #[test]
    fn display_state_values() {
        assert_eq!(DisplayState::from_raw(0), Some(DisplayState::Off));
        assert_eq!(DisplayState::from_raw(1), Some(DisplayState::On));
        assert_eq!(DisplayState::from_raw(2), Some(DisplayState::Dimmed));
        assert_eq!(DisplayState::from_raw(3), None);
    }

    #[test]
    fn filter_drops_quick_repeats_only() {
        let t0 = Instant::now();
        let mut f = EventFilter::new();
        assert!(f.accept(PowerEvent::Suspend, t0));
        assert!(!f.accept(PowerEvent::Suspend, t0 + Duration::from_millis(5)));
        assert!(f.accept(PowerEvent::ResumeAutomatic, t0 + Duration::from_secs(30)));
        assert!(f.accept(PowerEvent::ResumeSuspend, t0 + Duration::from_secs(30)));
        // A second sleep later is a real event.
        assert!(f.accept(PowerEvent::Suspend, t0 + Duration::from_secs(60)));
        assert!(f.accept(PowerEvent::Suspend, t0 + Duration::from_secs(63)));
    }

    #[test]
    fn filter_drops_interleaved_resume_copies() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut f = EventFilter::new();
        assert!(f.accept(PowerEvent::ResumeAutomatic, t0));
        assert!(f.accept(PowerEvent::ResumeSuspend, t0 + ms(10)));
        assert!(!f.accept(PowerEvent::ResumeAutomatic, t0 + ms(20)));
        assert!(!f.accept(PowerEvent::ResumeSuspend, t0 + ms(30)));
    }

    #[test]
    fn filter_reports_display_changes_only() {
        let t0 = Instant::now();
        let mut f = EventFilter::new();
        assert!(f.accept(PowerEvent::Display(DisplayState::On), t0));
        assert!(!f.accept(PowerEvent::Display(DisplayState::On), t0));
        assert!(f.accept(PowerEvent::Display(DisplayState::Dimmed), t0));
        assert!(f.accept(PowerEvent::Display(DisplayState::Off), t0));
        // Display events don't disturb suspend de-duplication.
        assert!(f.accept(PowerEvent::Suspend, t0));
        assert!(f.accept(PowerEvent::Display(DisplayState::On), t0));
        assert!(!f.accept(PowerEvent::Suspend, t0));
    }

    #[test]
    fn resume_helper() {
        assert!(PowerEvent::ResumeAutomatic.is_resume());
        assert!(PowerEvent::ResumeSuspend.is_resume());
        assert!(!PowerEvent::Suspend.is_resume());
    }
}
