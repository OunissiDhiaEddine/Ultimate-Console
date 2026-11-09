//! The bits of Logitech's HID++ 2.0 protocol needed to read a battery:
//! building long reports and decoding the answers. Platform independent; the
//! Windows layer does the I/O.
//!
//! A long report is 20 bytes: `0x11, device index, feature index,
//! function << 4 | software id, 16 parameter bytes`. Feature index 0 is
//! `IRoot`, whose function 0 maps a feature id to its index on the device.

use crate::{Kind, Level, Power, logitech_voltage_percent};

pub const LONG_REPORT: u8 = 0x11;
pub const LONG_LEN: usize = 20;
/// Our software id, so answers to G HUB's requests aren't taken for ours.
pub const SW_ID: u8 = 0x0B;

pub const FEATURE_ROOT: u16 = 0x0000;
pub const FEATURE_DEVICE_NAME: u16 = 0x0005;
pub const FEATURE_BATTERY_STATUS: u16 = 0x1000;
pub const FEATURE_BATTERY_VOLTAGE: u16 = 0x1001;
pub const FEATURE_UNIFIED_BATTERY: u16 = 0x1004;
/// Headsets such as the G535 report a raw battery voltage through this one.
pub const FEATURE_ADC_MEASUREMENT: u16 = 0x1F20;

/// The battery features in the order to try them (newest first), with the
/// function that reads the current state.
pub const BATTERY_FEATURES: [(u16, u8); 4] = [
    (FEATURE_UNIFIED_BATTERY, 1),
    (FEATURE_BATTERY_STATUS, 0),
    (FEATURE_BATTERY_VOLTAGE, 0),
    (FEATURE_ADC_MEASUREMENT, 0),
];

pub fn request(device: u8, feature_index: u8, function: u8, params: &[u8]) -> [u8; LONG_LEN] {
    let mut r = [0u8; LONG_LEN];
    r[0] = LONG_REPORT;
    r[1] = device;
    r[2] = feature_index;
    r[3] = (function << 4) | SW_ID;
    let n = params.len().min(LONG_LEN - 4);
    r[4..4 + n].copy_from_slice(&params[..n]);
    r
}

/// What a report read back means for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply<'a> {
    /// The answer: its 16 parameter bytes.
    Ok(&'a [u8]),
    /// The device refused (HID++ 2.0 error code).
    Error(u8),
    /// Some other report (a notification, someone else's answer).
    Other,
}

pub fn reply<'a>(report: &'a [u8], req: &[u8; LONG_LEN]) -> Reply<'a> {
    if report.len() < LONG_LEN || report[0] != LONG_REPORT || report[1] != req[1] {
        return Reply::Other;
    }
    if report[2] == req[2] && report[3] == req[3] {
        return Reply::Ok(&report[4..LONG_LEN]);
    }
    if report[2] == 0xFF && report[3] == req[2] && report[4] == req[3] {
        return Reply::Error(report[5]);
    }
    Reply::Other
}

/// Decode a battery feature's answer into a level and charging state.
pub fn battery(feature: u16, p: &[u8]) -> Option<(Level, Power)> {
    match feature {
        FEATURE_UNIFIED_BATTERY => {
            let power = match p.get(2)? {
                0 => Power::Discharging,
                1 | 2 => Power::Charging,
                3 => Power::Full,
                _ => Power::Unknown,
            };
            Some((Level::Percent((*p.first()?).min(100)), power))
        }
        FEATURE_BATTERY_STATUS => {
            let power = match p.get(2)? {
                0 => Power::Discharging,
                1 | 2 | 4 => Power::Charging,
                3 => Power::Full,
                _ => Power::Unknown,
            };
            let level = *p.first()?;
            // 0 means "unknown" for this feature on some devices.
            (level > 0 || power == Power::Charging)
                .then_some((Level::Percent(level.min(100)), power))
        }
        FEATURE_BATTERY_VOLTAGE => {
            let mv = u16::from_be_bytes([*p.first()?, *p.get(1)?]);
            let flags = *p.get(2)?;
            let power = if flags & 0x80 != 0 {
                match flags & 0x07 {
                    0 => Power::Charging,
                    1 => Power::Full,
                    _ => Power::Unknown,
                }
            } else {
                Power::Discharging
            };
            (mv > 0).then(|| (Level::Percent(logitech_voltage_percent(mv)), power))
        }
        FEATURE_ADC_MEASUREMENT => {
            let mv = u16::from_be_bytes([*p.first()?, *p.get(1)?]);
            let flags = *p.get(2)?;
            // Bit 0: the measurement is valid; bit 1: charging.
            if flags & 0x01 == 0 || mv == 0 {
                return None;
            }
            let power = if flags & 0x02 != 0 {
                Power::Charging
            } else {
                Power::Discharging
            };
            Some((Level::Percent(logitech_voltage_percent(mv)), power))
        }
        _ => None,
    }
}

/// `DEVICE_NAME` function 2 (device type).
pub fn kind(device_type: u8) -> Kind {
    match device_type {
        0 | 2 => Kind::Keyboard,
        3..=5 => Kind::Mouse,
        8 => Kind::Headset,
        11 | 12 => Kind::Controller,
        _ => Kind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout() {
        let r = request(1, 0, 0, &[0x10, 0x04]);
        assert_eq!(&r[..6], &[0x11, 1, 0, SW_ID, 0x10, 0x04]);
        assert_eq!(r.len(), 20);
    }

    #[test]
    fn replies() {
        let req = request(1, 6, 1, &[]);
        let mut ok = [0u8; 20];
        ok[..6].copy_from_slice(&[0x11, 1, 6, 0x10 | SW_ID, 77, 8]);
        assert!(matches!(reply(&ok, &req), Reply::Ok(p) if p[0] == 77));
        let mut err = [0u8; 20];
        err[..6].copy_from_slice(&[0x11, 1, 0xFF, 6, 0x10 | SW_ID, 2]);
        assert_eq!(reply(&err, &req), Reply::Error(2));
        // Another program's answer (different software id).
        let mut other = ok;
        other[3] = 0x11;
        assert_eq!(reply(&other, &req), Reply::Other);
        assert_eq!(reply(&ok[..10], &req), Reply::Other);
    }

    #[test]
    fn battery_features() {
        assert_eq!(
            battery(FEATURE_UNIFIED_BATTERY, &[64, 4, 1, 1]),
            Some((Level::Percent(64), Power::Charging))
        );
        assert_eq!(
            battery(FEATURE_BATTERY_STATUS, &[50, 30, 0]),
            Some((Level::Percent(50), Power::Discharging))
        );
        assert_eq!(battery(FEATURE_BATTERY_STATUS, &[0, 0, 0]), None);
        // 3811 mV = 50 %, not charging.
        assert_eq!(
            battery(FEATURE_BATTERY_VOLTAGE, &[0x0E, 0xE3, 0x00]),
            Some((Level::Percent(50), Power::Discharging))
        );
        assert_eq!(
            battery(FEATURE_BATTERY_VOLTAGE, &[0x10, 0x5A, 0x81]),
            Some((Level::Percent(100), Power::Full))
        );
        // G535: 3811 mV, valid, charging.
        assert_eq!(
            battery(FEATURE_ADC_MEASUREMENT, &[0x0E, 0xE3, 0x03]),
            Some((Level::Percent(50), Power::Charging))
        );
        // Headset off: the dongle says the reading isn't valid.
        assert_eq!(battery(FEATURE_ADC_MEASUREMENT, &[0x0E, 0xE3, 0x00]), None);
    }

    #[test]
    fn device_types() {
        assert_eq!(kind(3), Kind::Mouse);
        assert_eq!(kind(0), Kind::Keyboard);
        assert_eq!(kind(8), Kind::Headset);
        assert_eq!(kind(7), Kind::Other);
    }
}
