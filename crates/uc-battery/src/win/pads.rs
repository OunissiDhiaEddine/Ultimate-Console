//! Gamepads: Windows.Gaming.Input battery reports, else XInput.

use windows::Gaming::Input::RawGameController;
use windows::System::Power::BatteryStatus;
use windows::Win32::UI::Input::XboxController::{
    BATTERY_DEVTYPE_GAMEPAD, BATTERY_LEVEL_EMPTY, BATTERY_LEVEL_FULL, BATTERY_LEVEL_LOW,
    BATTERY_TYPE_DISCONNECTED, BATTERY_TYPE_UNKNOWN, BATTERY_TYPE_WIRED,
    XINPUT_BATTERY_INFORMATION, XINPUT_STATE, XInputGetBatteryInformation, XInputGetState,
};

use crate::{Coarse, Device, Kind, Level, Power, Source, kind_from_name};

pub fn scan() -> Vec<Device> {
    let wgi = gaming_input();
    if wgi.is_empty() {
        // The first call in a process often lists nothing yet; XInput
        // covers that scan (and machines where WGI doesn't work).
        xinput()
    } else {
        wgi
    }
}

fn gaming_input() -> Vec<Device> {
    let list = match RawGameController::RawGameControllers() {
        Ok(l) => l,
        Err(e) => {
            tracing::debug!("Windows.Gaming.Input unavailable: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for c in &list {
        let name = c
            .DisplayName()
            .map(|n| n.to_string())
            .ok()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "Game controller".to_owned());
        let wireless = c.IsWireless().unwrap_or(true);
        let (level, power) = match c.TryGetBatteryReport() {
            Ok(r) => {
                let remaining = r
                    .RemainingCapacityInMilliwattHours()
                    .and_then(|v| v.Value());
                let full = r
                    .FullChargeCapacityInMilliwattHours()
                    .and_then(|v| v.Value());
                let level = match (remaining, full) {
                    (Ok(rem), Ok(full)) if full > 0 => Some(Level::Percent(
                        ((i64::from(rem.max(0)) * 100 + i64::from(full) / 2) / i64::from(full))
                            .clamp(0, 100) as u8,
                    )),
                    _ => None,
                };
                let power = match r.Status() {
                    Ok(BatteryStatus::Charging) => Power::Charging,
                    Ok(BatteryStatus::Discharging) => Power::Discharging,
                    // Idle: plugged in and not charging, i.e. full.
                    Ok(BatteryStatus::Idle) => Power::Full,
                    Ok(BatteryStatus::NotPresent) => Power::Wired,
                    _ if !wireless => Power::Wired,
                    _ => Power::Unknown,
                };
                (level, power)
            }
            Err(_) if !wireless => (None, Power::Wired),
            Err(_) => (None, Power::Unknown),
        };
        tracing::debug!(%name, ?level, ?power, wireless, "gaming input controller");
        let kind = match kind_from_name(&name) {
            Kind::Other => Kind::Controller,
            k => k,
        };
        out.push(Device {
            name,
            kind,
            level,
            power,
            source: Source::GamingInput,
        });
    }
    out
}

fn xinput() -> Vec<Device> {
    let mut out = Vec::new();
    for user in 0..4u32 {
        let mut state = XINPUT_STATE::default();
        // SAFETY: `state` is a valid out struct.
        if unsafe { XInputGetState(user, &mut state) } != 0 {
            continue;
        }
        let mut info = XINPUT_BATTERY_INFORMATION::default();
        // SAFETY: `info` is a valid out struct.
        if unsafe { XInputGetBatteryInformation(user, BATTERY_DEVTYPE_GAMEPAD, &mut info) } != 0 {
            continue;
        }
        let (level, power) = match info.BatteryType {
            BATTERY_TYPE_DISCONNECTED => continue,
            BATTERY_TYPE_WIRED => (None, Power::Wired),
            BATTERY_TYPE_UNKNOWN => (None, Power::Unknown),
            _ => (
                Some(Level::Coarse(match info.BatteryLevel {
                    BATTERY_LEVEL_EMPTY => Coarse::Empty,
                    BATTERY_LEVEL_LOW => Coarse::Low,
                    BATTERY_LEVEL_FULL => Coarse::Full,
                    _ => Coarse::Medium,
                })),
                Power::Discharging,
            ),
        };
        out.push(Device {
            name: format!("Xbox controller {}", user + 1),
            kind: Kind::Controller,
            level,
            power,
            source: Source::XInput,
        });
    }
    out
}
