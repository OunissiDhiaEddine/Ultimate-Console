//! Turns polled XInput state into overlay [`Action`]s: edge detection,
//! auto-repeat for held directions and volume triggers, and ignoring
//! whatever was already held when the overlay opened (so the Steam Input
//! chord mapped to the overlay hotkey, e.g. Guide+Y, doesn't fire "kill").

use std::time::{Duration, Instant};

use crate::model::Action;

/// `XINPUT_GAMEPAD` button bits.
pub mod buttons {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

/// One controller's raw state, as read by `XInputGetState`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PadState {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
}

const STICK_THRESHOLD: i32 = 16_000;
const TRIGGER_THRESHOLD: u8 = 100;
const REPEAT_DELAY: Duration = Duration::from_millis(400);
const REPEAT_EVERY: Duration = Duration::from_millis(110);

// Logical inputs, after merging D-pad with stick and triggers with buttons.
const UP: u32 = 1 << 0;
const DOWN: u32 = 1 << 1;
const LEFT: u32 = 1 << 2;
const RIGHT: u32 = 1 << 3;
const A: u32 = 1 << 4;
const B: u32 = 1 << 5;
const X: u32 = 1 << 6;
const Y: u32 = 1 << 7;
const LB: u32 = 1 << 8;
const RB: u32 = 1 << 9;
const LT: u32 = 1 << 10;
const RT: u32 = 1 << 11;
const START: u32 = 1 << 12;
const BACK: u32 = 1 << 13;

/// Which logical inputs map to which action, and whether holding repeats.
const MAP: &[(u32, Action, bool)] = &[
    (UP, Action::Up, true),
    (DOWN, Action::Down, true),
    (LEFT, Action::Left, true),
    (RIGHT, Action::Right, true),
    (A, Action::Accept, false),
    (B, Action::Back, false),
    (BACK, Action::Back, false),
    (START, Action::Accept, false),
    (X, Action::CloseTask, false),
    (Y, Action::KillTask, false),
    (LB, Action::PrevTab, false),
    (RB, Action::NextTab, false),
    (LT, Action::VolumeDown, true),
    (RT, Action::VolumeUp, true),
];

fn logical(p: &PadState) -> u32 {
    use buttons as b;
    let mut l = 0;
    let mut set = |cond: bool, bit: u32| {
        if cond {
            l |= bit;
        }
    };
    set(
        p.buttons & b::DPAD_UP != 0 || i32::from(p.thumb_ly) > STICK_THRESHOLD,
        UP,
    );
    set(
        p.buttons & b::DPAD_DOWN != 0 || i32::from(p.thumb_ly) < -STICK_THRESHOLD,
        DOWN,
    );
    set(
        p.buttons & b::DPAD_LEFT != 0 || i32::from(p.thumb_lx) < -STICK_THRESHOLD,
        LEFT,
    );
    set(
        p.buttons & b::DPAD_RIGHT != 0 || i32::from(p.thumb_lx) > STICK_THRESHOLD,
        RIGHT,
    );
    set(p.buttons & b::A != 0, A);
    set(p.buttons & b::B != 0, B);
    set(p.buttons & b::X != 0, X);
    set(p.buttons & b::Y != 0, Y);
    set(p.buttons & b::LEFT_SHOULDER != 0, LB);
    set(p.buttons & b::RIGHT_SHOULDER != 0, RB);
    set(p.left_trigger > TRIGGER_THRESHOLD, LT);
    set(p.right_trigger > TRIGGER_THRESHOLD, RT);
    set(p.buttons & b::START != 0, START);
    set(p.buttons & b::BACK != 0, BACK);
    l
}

/// Edge detector and auto-repeat over every connected controller.
#[derive(Debug, Default)]
pub struct Gamepad {
    held: u32,
    /// Held when the overlay opened; ignored until released.
    blocked: u32,
    /// When each repeating input next fires.
    next_repeat: [Option<Instant>; 14],
}

impl Gamepad {
    /// Start a session: anything held right now is ignored until released.
    pub fn prime(&mut self, pads: &[PadState]) {
        let now = pads.iter().map(logical).fold(0, |a, b| a | b);
        self.held = now;
        self.blocked = now;
        self.next_repeat = [None; 14];
    }

    /// Feed the latest state of all connected pads; returns the actions to run.
    pub fn update(&mut self, pads: &[PadState], now: Instant) -> Vec<Action> {
        let cur = pads.iter().map(logical).fold(0, |a, b| a | b);
        self.blocked &= cur;
        let active = cur & !self.blocked;
        let pressed = active & !self.held;
        let mut out = Vec::new();
        for &(bit, action, repeats) in MAP {
            let i = bit.trailing_zeros() as usize;
            if pressed & bit != 0 {
                out.push(action);
                self.next_repeat[i] = repeats.then(|| now + REPEAT_DELAY);
            } else if active & bit != 0 {
                if self.next_repeat[i].is_some_and(|t| now >= t) {
                    out.push(action);
                    self.next_repeat[i] = Some(now + REPEAT_EVERY);
                }
            } else {
                self.next_repeat[i] = None;
            }
        }
        self.held = active;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(buttons: u16) -> PadState {
        PadState {
            buttons,
            ..PadState::default()
        }
    }

    #[test]
    fn press_fires_once_per_edge() {
        let mut g = Gamepad::default();
        let t = Instant::now();
        g.prime(&[pad(0)]);
        assert_eq!(g.update(&[pad(buttons::A)], t), vec![Action::Accept]);
        assert!(
            g.update(&[pad(buttons::A)], t + Duration::from_secs(2))
                .is_empty()
        );
        assert!(g.update(&[pad(0)], t).is_empty());
        assert_eq!(g.update(&[pad(buttons::A)], t), vec![Action::Accept]);
    }

    #[test]
    fn buttons_held_at_open_are_ignored_until_released() {
        let mut g = Gamepad::default();
        let t = Instant::now();
        g.prime(&[pad(buttons::Y)]);
        assert!(g.update(&[pad(buttons::Y)], t).is_empty());
        assert!(g.update(&[pad(0)], t).is_empty());
        assert_eq!(g.update(&[pad(buttons::Y)], t), vec![Action::KillTask]);
    }

    #[test]
    fn directions_repeat_after_delay() {
        let mut g = Gamepad::default();
        let t = Instant::now();
        g.prime(&[]);
        let right = [pad(buttons::DPAD_RIGHT)];
        assert_eq!(g.update(&right, t), vec![Action::Right]);
        assert!(g.update(&right, t + Duration::from_millis(300)).is_empty());
        assert_eq!(
            g.update(&right, t + Duration::from_millis(400)),
            vec![Action::Right]
        );
        assert!(g.update(&right, t + Duration::from_millis(450)).is_empty());
        assert_eq!(
            g.update(&right, t + Duration::from_millis(510)),
            vec![Action::Right]
        );
    }

    #[test]
    fn stick_and_triggers_map_like_buttons() {
        let mut g = Gamepad::default();
        let t = Instant::now();
        g.prime(&[]);
        let s = PadState {
            thumb_ly: -30_000,
            right_trigger: 255,
            ..PadState::default()
        };
        assert_eq!(g.update(&[s], t), vec![Action::Down, Action::VolumeUp]);
        // Small stick movements stay inside the dead zone.
        let s = PadState {
            thumb_lx: 8_000,
            ..PadState::default()
        };
        assert!(g.update(&[s], t).is_empty());
    }

    #[test]
    fn pads_are_merged() {
        let mut g = Gamepad::default();
        let t = Instant::now();
        g.prime(&[pad(0), pad(0)]);
        assert_eq!(
            g.update(&[pad(buttons::LEFT_SHOULDER), pad(buttons::B)], t),
            vec![Action::Back, Action::PrevTab]
        );
    }
}
