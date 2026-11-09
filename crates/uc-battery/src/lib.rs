//! Battery levels of controllers, headsets, mice and other wireless devices,
//! read straight from Windows: no vendor app (Xbox Accessories, G HUB, …)
//! has to be installed or running.
//!
//! [`scan`] asks every source once and returns one [`Device`] per gadget:
//!
//! - **Xbox and other gamepads**: `Windows.Gaming.Input` battery reports
//!   (a percentage), falling back to XInput's four coarse levels.
//! - **Bluetooth**: the battery level Windows itself keeps for paired
//!   devices (the `DEVPKEY_Bluetooth_Battery` property that the Settings app
//!   shows; hands-free headsets report it), plus the standard Bluetooth LE
//!   Battery Service (0x180F) read through the Win32 GATT API.
//! - **Logitech** wireless mice and keyboards on a LIGHTSPEED, Unifying or
//!   Bolt receiver, and Logitech wireless headsets (G535, …) on their own
//!   dongle: a few HID++ 2.0 requests over the vendor HID collection, the
//!   same protocol G HUB speaks.
//!
//! Nothing runs in the background by itself. A scan costs a SetupAPI pass
//! over present devices plus a handful of tiny HID / GATT requests, so it is
//! meant to run every 30 s or so, and only while someone is looking: the
//! overlay starts a [`Monitor`] when its Battery tab opens and drops it when
//! the tab closes. Scans run on the monitor's own thread because a Bluetooth
//! read of a sleeping device can take a while.
//!
//! The pure parts (name and class-of-device heuristics, the Logitech
//! voltage curve, merging duplicates) are platform independent and unit
//! tested.

use std::cmp::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub mod hidpp;
#[cfg(windows)]
mod win;

/// What kind of gadget, for the icon and the sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Controller,
    Headset,
    Mouse,
    Keyboard,
    Other,
}

/// XInput only knows four levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coarse {
    Empty,
    Low,
    Medium,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 0–100.
    Percent(u8),
    Coarse(Coarse),
}

/// Whether the battery is being used or filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Power {
    #[default]
    Unknown,
    Discharging,
    Charging,
    /// Plugged in and full.
    Full,
    /// A cable and no battery reading at all (a wired pad).
    Wired,
}

/// Where a reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// `Windows.Gaming.Input` gamepad battery report.
    GamingInput,
    /// HID++ over a Logitech receiver.
    Logitech,
    /// Bluetooth LE Battery Service.
    BluetoothLe,
    /// Battery level Windows keeps on the Bluetooth device.
    Bluetooth,
    /// `XInputGetBatteryInformation`.
    XInput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub kind: Kind,
    /// `None` when the device is there but has no reading (wired, asleep).
    pub level: Option<Level>,
    pub power: Power,
    pub source: Source,
}

impl Device {
    /// 0.0–1.0 for drawing a bar. Coarse levels sit in the middle of their band.
    pub fn fraction(&self) -> Option<f32> {
        Some(match self.level? {
            Level::Percent(p) => f32::from(p.min(100)) / 100.0,
            Level::Coarse(Coarse::Empty) => 0.05,
            Level::Coarse(Coarse::Low) => 0.25,
            Level::Coarse(Coarse::Medium) => 0.6,
            Level::Coarse(Coarse::Full) => 1.0,
        })
    }

    /// `85%`, `Medium`, or what's known about the power when there's no level.
    pub fn level_text(&self) -> String {
        match self.level {
            Some(Level::Percent(p)) => format!("{}%", p.min(100)),
            Some(Level::Coarse(c)) => match c {
                Coarse::Empty => "Empty",
                Coarse::Low => "Low",
                Coarse::Medium => "Medium",
                Coarse::Full => "Full",
            }
            .to_owned(),
            None if self.power == Power::Wired => "Wired".to_owned(),
            None => "Unknown".to_owned(),
        }
    }

    /// `Charging`, `Full` … or empty.
    pub fn power_text(&self) -> &'static str {
        match self.power {
            Power::Charging => "Charging",
            Power::Full => "Charged",
            Power::Wired | Power::Discharging | Power::Unknown => "",
        }
    }

    /// 20 % or less (or XInput's Empty/Low), and not on a charger.
    pub fn is_low(&self) -> bool {
        let low = match self.level {
            Some(Level::Percent(p)) => p <= 20,
            Some(Level::Coarse(c)) => matches!(c, Coarse::Empty | Coarse::Low),
            None => false,
        };
        low && !matches!(self.power, Power::Charging | Power::Full)
    }

    /// How good this reading is, to pick one when a gadget shows up twice.
    fn quality(&self) -> (u8, std::cmp::Reverse<Source>) {
        let level = match self.level {
            Some(Level::Percent(_)) => 2,
            Some(Level::Coarse(_)) => 1,
            None => 0,
        };
        (level, std::cmp::Reverse(self.source))
    }
}

/// Guess the kind of gadget from its name.
pub fn kind_from_name(name: &str) -> Kind {
    let n = name.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
    if has(&[
        "controller",
        "gamepad",
        "joystick",
        "dualsense",
        "dualshock",
        "joy-con",
        "8bitdo",
    ]) {
        Kind::Controller
    } else if has(&[
        "headset",
        "headphone",
        "earbud",
        "buds",
        "airpods",
        "hands-free",
        "handsfree",
        "earphone",
        "speaker",
        "wh-",
        "wf-",
    ]) {
        Kind::Headset
    } else if has(&["mouse", "trackball", "mx master", "mx anywhere"]) {
        Kind::Mouse
    } else if has(&["keyboard", "keys"]) {
        Kind::Keyboard
    } else {
        Kind::Other
    }
}

/// The kind of a Bluetooth device from its Class of Device, when it says.
pub fn kind_from_class_of_device(cod: u32) -> Option<Kind> {
    let major = (cod >> 8) & 0x1F;
    let minor = (cod >> 2) & 0x3F;
    match major {
        // Audio/Video: headsets, headphones, speakers.
        0x04 => Some(Kind::Headset),
        // Peripheral: bits 4-5 of the minor say keyboard/pointer, bits 0-3
        // the device type.
        0x05 => match (minor >> 4, minor & 0x0F) {
            (_, 0x01 | 0x02) => Some(Kind::Controller),
            (0b01, _) => Some(Kind::Keyboard),
            (0b10, _) => Some(Kind::Mouse),
            _ => None,
        },
        _ => None,
    }
}

/// Windows names the hands-free and audio service nodes of a headset after
/// it with a suffix; drop the suffix.
pub fn clean_bluetooth_name(name: &str) -> String {
    const SUFFIXES: [&str; 5] = [
        " Hands-Free AG",
        " Hands-Free",
        " Avrcp Transport",
        " Stereo",
        " Hands-Free HF Audio",
    ];
    let mut n = name.trim();
    for s in SUFFIXES {
        if let Some(stripped) = n.strip_suffix(s) {
            n = stripped.trim_end();
        }
    }
    n.to_owned()
}

/// Logitech's HID++ `BATTERY_VOLTAGE` (0x1001) feature gives millivolts; this
/// is the lithium cell discharge curve Solaar and libratbag use for it.
pub fn logitech_voltage_percent(millivolts: u16) -> u8 {
    const CURVE: [(u16, u8); 13] = [
        (4186, 100),
        (4067, 90),
        (3989, 80),
        (3922, 70),
        (3859, 60),
        (3811, 50),
        (3778, 40),
        (3751, 30),
        (3717, 20),
        (3671, 10),
        (3646, 5),
        (3579, 2),
        (3500, 0),
    ];
    let mv = millivolts;
    if mv >= CURVE[0].0 {
        return 100;
    }
    for pair in CURVE.windows(2) {
        let ((hi_v, hi_p), (lo_v, lo_p)) = (pair[0], pair[1]);
        if mv >= lo_v {
            let t = f32::from(mv - lo_v) / f32::from(hi_v - lo_v);
            return (f32::from(lo_p) + t * f32::from(hi_p - lo_p)).round() as u8;
        }
    }
    0
}

/// One entry per gadget (by name, ignoring case), keeping the best reading,
/// sorted controllers first, then headsets, mice, keyboards, the rest.
pub fn merge(devices: Vec<Device>) -> Vec<Device> {
    let mut out: Vec<Device> = Vec::new();
    for d in devices {
        if d.name.trim().is_empty() {
            continue;
        }
        match out
            .iter_mut()
            .find(|o| o.name.eq_ignore_ascii_case(d.name.trim()))
        {
            Some(o) => {
                if d.quality() > o.quality() {
                    let kind = if d.kind == Kind::Other {
                        o.kind
                    } else {
                        d.kind
                    };
                    *o = Device { kind, ..d };
                } else if o.kind == Kind::Other {
                    o.kind = d.kind;
                }
                // A charging flag from either reading is worth keeping.
                if o.power == Power::Unknown {
                    o.power = d.power;
                }
            }
            None => out.push(Device {
                name: d.name.trim().to_owned(),
                ..d
            }),
        }
    }
    out.sort_by(|a, b| match a.kind.cmp(&b.kind) {
        Ordering::Equal => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        o => o,
    });
    out
}

/// Ask every source once. Blocking: a Bluetooth read can take a second or
/// two when a device is asleep, so call it off the UI thread ([`Monitor`]).
/// Returns nothing on other platforms.
pub fn scan() -> Vec<Device> {
    #[cfg(windows)]
    {
        merge(win::scan())
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Something that reads devices: [`scan`] for real, a fake in tests.
pub type Scanner = Arc<dyn Fn() -> Vec<Device> + Send + Sync>;

/// The real [`Scanner`].
pub fn real_scanner() -> Scanner {
    Arc::new(scan)
}

/// After the first scan, a second one this soon: Windows.Gaming.Input only
/// lists gamepads a moment after a process first asks.
pub const WARM_UP: Duration = Duration::from_secs(2);

/// Scans on its own thread every `interval` until dropped.
pub struct Monitor {
    _stop: mpsc::Sender<()>,
    latest: Arc<Mutex<Option<Vec<Device>>>>,
}

impl Monitor {
    /// Scan now, again after [`WARM_UP`], then every `interval`. `notify` is
    /// called on the monitor thread after each scan; read the result with
    /// [`Monitor::latest`].
    pub fn start(
        scanner: Scanner,
        interval: Duration,
        notify: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let latest = Arc::new(Mutex::new(None));
        let out = latest.clone();
        std::thread::Builder::new()
            .name("uc-battery".into())
            .spawn(move || {
                let mut wait = WARM_UP;
                loop {
                    let devices = scanner();
                    if let Ok(mut l) = out.lock() {
                        *l = Some(devices);
                    }
                    notify();
                    // Dropping the Monitor disconnects the channel; a scan in
                    // flight finishes first, but nobody waits for it.
                    match rx.recv_timeout(wait) {
                        Err(RecvTimeoutError::Timeout) => wait = interval,
                        _ => break,
                    }
                }
            })?;
        Ok(Self {
            _stop: stop,
            latest,
        })
    }

    /// The last scan, once one has finished.
    pub fn latest(&self) -> Option<Vec<Device>> {
        self.latest.lock().ok().and_then(|l| l.clone())
    }
}

impl std::fmt::Debug for Monitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Monitor")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, kind: Kind, level: Option<Level>, source: Source) -> Device {
        Device {
            name: name.into(),
            kind,
            level,
            power: Power::Unknown,
            source,
        }
    }

    #[test]
    fn names_to_kinds() {
        assert_eq!(kind_from_name("Xbox Wireless Controller"), Kind::Controller);
        assert_eq!(kind_from_name("DualSense Wireless"), Kind::Controller);
        assert_eq!(kind_from_name("WH-1000XM4"), Kind::Headset);
        assert_eq!(kind_from_name("Galaxy Buds2 Pro"), Kind::Headset);
        assert_eq!(
            kind_from_name("G305 LIGHTSPEED Wireless Mouse"),
            Kind::Mouse
        );
        assert_eq!(kind_from_name("MX Keys"), Kind::Keyboard);
        assert_eq!(kind_from_name("Pixel Watch"), Kind::Other);
    }

    #[test]
    fn class_of_device() {
        // Headphones: major 0x04, minor 0x06.
        assert_eq!(kind_from_class_of_device(0x240418), Some(Kind::Headset));
        // Gamepad: major 0x05, minor 0x02.
        assert_eq!(kind_from_class_of_device(0x000508), Some(Kind::Controller));
        // Keyboard: minor 0x10; mouse: minor 0x20.
        assert_eq!(kind_from_class_of_device(0x000540), Some(Kind::Keyboard));
        assert_eq!(kind_from_class_of_device(0x002580), Some(Kind::Mouse));
        // Phone.
        assert_eq!(kind_from_class_of_device(0x5a020c), None);
    }

    #[test]
    fn bluetooth_suffixes() {
        assert_eq!(
            clean_bluetooth_name("WH-1000XM4 Hands-Free AG"),
            "WH-1000XM4"
        );
        assert_eq!(clean_bluetooth_name("Buds Stereo"), "Buds");
        assert_eq!(clean_bluetooth_name("  Plain  "), "Plain");
    }

    #[test]
    fn voltage_curve() {
        assert_eq!(logitech_voltage_percent(4300), 100);
        assert_eq!(logitech_voltage_percent(4186), 100);
        assert_eq!(logitech_voltage_percent(3811), 50);
        // Halfway between 3811 (50) and 3859 (60).
        assert_eq!(logitech_voltage_percent(3835), 55);
        assert_eq!(logitech_voltage_percent(3500), 0);
        assert_eq!(logitech_voltage_percent(3000), 0);
    }

    #[test]
    fn merge_keeps_best_reading_and_sorts() {
        let merged = merge(vec![
            dev(
                "G305",
                Kind::Mouse,
                Some(Level::Percent(70)),
                Source::Logitech,
            ),
            dev(
                "Xbox Wireless Controller",
                Kind::Controller,
                Some(Level::Coarse(Coarse::Medium)),
                Source::XInput,
            ),
            dev(
                "xbox wireless controller",
                Kind::Other,
                Some(Level::Percent(64)),
                Source::BluetoothLe,
            ),
            dev("Headset", Kind::Headset, None, Source::Bluetooth),
            dev("", Kind::Other, None, Source::Bluetooth),
        ]);
        let names: Vec<_> = merged.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["xbox wireless controller", "Headset", "G305"]);
        // The percentage won, the kind from the other reading was kept.
        assert_eq!(merged[0].level, Some(Level::Percent(64)));
        assert_eq!(merged[0].kind, Kind::Controller);
    }

    #[test]
    fn merge_prefers_gaming_input_on_a_tie() {
        let merged = merge(vec![
            dev(
                "Pad",
                Kind::Controller,
                Some(Level::Percent(40)),
                Source::Bluetooth,
            ),
            dev(
                "Pad",
                Kind::Controller,
                Some(Level::Percent(41)),
                Source::GamingInput,
            ),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, Source::GamingInput);
    }

    #[test]
    fn texts_and_low() {
        let mut d = dev(
            "Pad",
            Kind::Controller,
            Some(Level::Percent(15)),
            Source::XInput,
        );
        assert_eq!(d.level_text(), "15%");
        assert!(d.is_low());
        d.power = Power::Charging;
        assert!(!d.is_low());
        assert_eq!(d.power_text(), "Charging");
        d.level = None;
        d.power = Power::Wired;
        assert_eq!(d.level_text(), "Wired");
        assert_eq!(d.fraction(), None);
        d.level = Some(Level::Coarse(Coarse::Full));
        assert_eq!(d.fraction(), Some(1.0));
    }

    #[test]
    fn monitor_scans_and_stops() {
        let (tx, rx) = mpsc::channel();
        let m = Monitor::start(
            Arc::new(|| vec![dev("Pad", Kind::Controller, None, Source::XInput)]),
            Duration::from_secs(60),
            move || {
                let _ = tx.send(());
            },
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(m.latest().unwrap().len(), 1);
        drop(m);
    }
}
