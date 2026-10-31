//! User configuration: `%APPDATA%\UltimateConsole\config.toml` (plan §2.11).
//!
//! Every field has a default, so an empty or partial file is valid and a
//! missing file means "all defaults". The agent hot-reloads this file; the
//! settings app changes it through IPC (`SetConfig`), not by writing it.
//!
//! Adding a field: give it a default and `#[serde(default)]` (the struct-level
//! attribute already covers this). Renaming or changing the meaning of a
//! field: bump [`SCHEMA_VERSION`] and migrate in [`Config::migrate`].

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hotkey::{self, Hotkey};
use crate::{Error, Result, fsutil};

/// Current config schema. Files with a higher number are refused.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema: u32,
    pub general: General,
    pub hotkeys: Hotkeys,
    pub display: Display,
    pub audio: Audio,
    pub tv: Tv,
    pub steam: Steam,
    pub power: Power,
    pub overlay: Overlay,
    pub updates: Updates,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: SCHEMA_VERSION,
            general: General::default(),
            hotkeys: Hotkeys::default(),
            display: Display::default(),
            audio: Audio::default(),
            tv: Tv::default(),
            steam: Steam::default(),
            power: Power::default(),
            overlay: Overlay::default(),
            updates: Updates::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Register the agent under HKCU Run.
    pub start_with_windows: bool,
    /// Log filter (`tracing` env-filter syntax), e.g. `"info"` or `"uc_tv=debug,info"`.
    pub log_level: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            start_with_windows: true,
            log_level: "info".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub gaming_toggle: Hotkey,
    pub overlay: Hotkey,
    pub panic_restore: Hotkey,
}

impl Default for Hotkeys {
    fn default() -> Self {
        let p = |s: &str| s.parse().expect("default hotkey parses");
        Self {
            gaming_toggle: p("Ctrl+Alt+G"),
            overlay: p("Ctrl+Alt+O"),
            panic_restore: p("Ctrl+Alt+Shift+R"),
        }
    }
}

impl Hotkeys {
    /// Name/hotkey pairs, in the order shown in the settings app.
    pub fn named(&self) -> [(&'static str, Hotkey); 3] {
        [
            ("gaming_toggle", self.gaming_toggle),
            ("overlay", self.overlay),
            ("panic_restore", self.panic_restore),
        ]
    }
}

/// Stable identity of a monitor (plan §2.4). Match on `device_path` first,
/// then fall back to `friendly_name` + `connector`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayId {
    /// `DISPLAYCONFIG_TARGET_DEVICE_NAME.monitorDevicePath` (EDID-based).
    pub device_path: String,
    /// `monitorFriendlyDeviceName`, e.g. `"IRIS TV"`.
    pub friendly_name: String,
    /// Output technology / connector, e.g. `"HDMI"`. Empty if unknown.
    pub connector: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    /// Refresh rate in Hz, e.g. `60.0` or `59.94`. `None` keeps the current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_hz: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Display {
    /// The TV. `None` until picked in the settings app; gaming mode refuses to start without it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<DisplayId>,
    /// Mode to apply on the TV. `None` = automatic: the largest 16:9 mode
    /// (3840x2160 rather than a 4096x2160 "preferred" one) at its highest
    /// refresh rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<DisplayMode>,
    /// Turn HDR on for the TV while gaming.
    pub hdr: bool,
    /// How long to wait for the TV to show up as an available display.
    pub wait_timeout_secs: u32,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            target: None,
            mode: None,
            hdr: false,
            wait_timeout_secs: 15,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Audio {
    /// Switch the default output device while gaming.
    pub switch_default: bool,
    /// Endpoint id (`IMMDevice::GetId`) to switch to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tv {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<IpAddr>,
    pub adb_port: u16,
    /// MAC for Wake-on-LAN, `AA:BB:CC:DD:EE:FF`. Filled in during pairing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    pub wol_enabled: bool,
    /// Where to send the magic packet. Broadcast by default.
    pub wol_address: IpAddr,
    pub wol_port: u16,
    /// Extra `adb shell` commands to run after a wake (vendor-specific).
    pub after_wake_commands: Vec<String>,
    pub connect_timeout_ms: u32,
    /// Total time to keep retrying ADB after WoL before giving up.
    pub wake_retry_secs: u32,
}

impl Default for Tv {
    fn default() -> Self {
        Self {
            enabled: false,
            ip: None,
            adb_port: 5555,
            mac: None,
            wol_enabled: true,
            wol_address: IpAddr::from([255, 255, 255, 255]),
            wol_port: 9,
            after_wake_commands: Vec::new(),
            connect_timeout_ms: 2000,
            wake_retry_secs: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Steam {
    /// `steam.exe`. `None` = auto-detect from `HKCU\Software\Valve\Steam\SteamExe`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe_path: Option<PathBuf>,
    /// Arguments used when Steam is not running yet.
    pub launch_args: Vec<String>,
    /// Big Picture window matcher (plan §2.8).
    pub bp_window_class: String,
    pub bp_window_title: String,
    pub bp_launch_timeout_secs: u32,
    /// How long BP must be gone before gaming mode exits.
    pub bp_close_debounce_ms: u32,
    /// Leaving Big Picture (Steam's own "Exit Big Picture") also leaves
    /// gaming mode. Off by default: Steam sometimes drops out of Big Picture
    /// by itself, so the agent opens it again and gaming mode ends only by
    /// the hotkey, the overlay or Steam quitting.
    pub exit_with_big_picture: bool,
}

impl Default for Steam {
    fn default() -> Self {
        Self {
            exe_path: None,
            launch_args: vec!["-gamepadui".into()],
            bp_window_class: "SDL_app".into(),
            bp_window_title: "Steam Big Picture Mode".into(),
            bp_launch_timeout_secs: 30,
            bp_close_debounce_ms: 2000,
            exit_with_big_picture: false,
        }
    }
}

/// Sleep/resume behaviour. Only ever applied in service-triggered gaming mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Power {
    pub tv_sleep_on_suspend: bool,
    pub tv_wake_on_resume: bool,
    /// Hard limit for the TV sleep command inside the suspend handler.
    pub suspend_budget_ms: u32,
    /// How long to wait for the network after resume before waking the TV.
    pub network_wait_secs: u32,
}

impl Default for Power {
    fn default() -> Self {
        Self {
            tv_sleep_on_suspend: true,
            tv_wake_on_resume: true,
            suspend_budget_ms: 1500,
            network_wait_secs: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Overlay {
    /// Allow the overlay hotkey outside gaming mode too.
    pub in_desktop_mode: bool,
    /// Resource sampling interval while the overlay is open.
    pub sample_interval_ms: u32,
    /// In gaming mode, handle the keyboard / headset volume keys (Explorer,
    /// which normally does, isn't running) and show a volume popup.
    pub volume_keys: bool,
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            in_desktop_mode: false,
            sample_interval_ms: 1000,
            volume_keys: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Updates {
    pub auto_check: bool,
    /// GitHub repo whose Releases hold the Velopack packages.
    pub repo: String,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            auto_check: true,
            repo: "OunissiDhiaEddine/Ultimate-Console".into(),
        }
    }
}

impl Config {
    /// Loads the config. A missing file gives the defaults. The result is
    /// migrated to the current schema but not validated; call [`Config::validate`].
    pub fn load(path: &Path) -> Result<Self> {
        match fsutil::read_optional(path)? {
            None => Ok(Self::default()),
            Some(text) => Self::from_toml(&text, path),
        }
    }

    /// Parses TOML. `path` is only used in error messages.
    pub fn from_toml(text: &str, path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(text).map_err(|e| Error::ConfigParse {
            path: path.to_path_buf(),
            source: Box::new(e),
        })?;
        if config.schema > SCHEMA_VERSION {
            return Err(Error::ConfigSchemaTooNew {
                path: path.to_path_buf(),
                found: config.schema,
                supported: SCHEMA_VERSION,
            });
        }
        Ok(config.migrate())
    }

    /// Brings an older schema up to [`SCHEMA_VERSION`]. Nothing to do yet.
    fn migrate(mut self) -> Self {
        self.schema = SCHEMA_VERSION;
        self
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Validates, then writes atomically.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        fsutil::write_atomic(path, self.to_toml()?.as_bytes())
    }

    /// Checks everything that can be checked without the OS. Returns all
    /// problems at once, one per line.
    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();

        for (name, check) in hotkey::check_set(&self.hotkeys.named()) {
            for e in check.errors {
                problems.push(format!("hotkeys.{name}: {e}"));
            }
        }
        if let Some(mode) = &self.display.mode {
            if mode.width == 0 || mode.height == 0 {
                problems.push("display.mode: width and height must be > 0".into());
            }
            if mode.refresh_hz.is_some_and(|hz| !(hz > 0.0 && hz < 1000.0)) {
                problems.push("display.mode.refresh_hz must be between 0 and 1000".into());
            }
        }
        if let Some(mac) = &self.tv.mac
            && parse_mac(mac).is_none()
        {
            problems.push(format!(
                "tv.mac: {mac:?} is not a MAC address like AA:BB:CC:DD:EE:FF"
            ));
        }
        if self.tv.enabled && self.tv.ip.is_none() {
            problems.push("tv.ip is required when tv.enabled is true".into());
        }
        if self.tv.adb_port == 0 {
            problems.push("tv.adb_port must be > 0".into());
        }
        if self.tv.wol_port == 0 {
            problems.push("tv.wol_port must be > 0".into());
        }
        for (name, v) in [
            ("display.wait_timeout_secs", self.display.wait_timeout_secs),
            ("tv.connect_timeout_ms", self.tv.connect_timeout_ms),
            (
                "steam.bp_launch_timeout_secs",
                self.steam.bp_launch_timeout_secs,
            ),
            ("power.suspend_budget_ms", self.power.suspend_budget_ms),
            (
                "overlay.sample_interval_ms",
                self.overlay.sample_interval_ms,
            ),
        ] {
            if v == 0 {
                problems.push(format!("{name} must be > 0"));
            }
        }
        if self.steam.bp_window_class.is_empty() && self.steam.bp_window_title.is_empty() {
            problems.push("steam: set bp_window_class or bp_window_title".into());
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::ConfigInvalid(problems.join("\n")))
        }
    }
}

/// Parses `AA:BB:CC:DD:EE:FF` or `AA-BB-CC-DD-EE-FF` (any case).
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.trim().split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (byte, part) in mac.iter_mut().zip(parts) {
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    Some(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> &'static Path {
        Path::new("config.toml")
    }

    #[test]
    fn defaults_are_valid_and_roundtrip() {
        let c = Config::default();
        c.validate().unwrap();
        let back = Config::from_toml(&c.to_toml().unwrap(), path()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn empty_and_partial_files_use_defaults() {
        assert_eq!(Config::from_toml("", path()).unwrap(), Config::default());
        let c = Config::from_toml("[tv]\nenabled = true\nip = \"192.168.1.50\"\n", path()).unwrap();
        assert!(c.tv.enabled);
        assert_eq!(c.tv.adb_port, 5555);
        assert_eq!(c.hotkeys, Hotkeys::default());
        c.validate().unwrap();
    }

    #[test]
    fn full_file_roundtrip() {
        let mut c = Config::default();
        c.display.target = Some(DisplayId {
            device_path: r"\\?\DISPLAY#IRS0001#5&1&UID4352#{e6f07b5f}".into(),
            friendly_name: "IRIS TV".into(),
            connector: "HDMI".into(),
        });
        c.display.mode = Some(DisplayMode {
            width: 3840,
            height: 2160,
            refresh_hz: Some(59.94),
        });
        c.tv.enabled = true;
        c.tv.ip = Some("192.168.1.50".parse().unwrap());
        c.tv.mac = Some("aa:bb:cc:dd:ee:ff".into());
        c.tv.after_wake_commands = vec!["input keyevent KEYCODE_TV_INPUT_HDMI_1".into()];
        c.steam.exe_path = Some(r"C:\Program Files (x86)\Steam\steam.exe".into());
        c.validate().unwrap();
        let back = Config::from_toml(&c.to_toml().unwrap(), path()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn newer_schema_is_refused() {
        let err = Config::from_toml("schema = 99", path()).unwrap_err();
        assert!(matches!(err, Error::ConfigSchemaTooNew { found: 99, .. }));
    }

    #[test]
    fn bad_hotkey_is_a_parse_error() {
        let err = Config::from_toml("[hotkeys]\noverlay = \"Ctrl+Nope\"", path()).unwrap_err();
        assert!(matches!(err, Error::ConfigParse { .. }), "{err}");
    }

    #[test]
    fn validation_collects_problems() {
        let mut c = Config::default();
        c.hotkeys.overlay = c.hotkeys.gaming_toggle;
        c.hotkeys.panic_restore = "Alt+F4".parse().unwrap();
        c.tv.enabled = true;
        c.tv.mac = Some("nope".into());
        let Err(Error::ConfigInvalid(msg)) = c.validate() else {
            panic!("expected invalid");
        };
        assert!(
            msg.contains("hotkeys.overlay: same shortcut as gaming_toggle"),
            "{msg}"
        );
        assert!(msg.contains("hotkeys.panic_restore"), "{msg}");
        assert!(msg.contains("tv.ip is required"), "{msg}");
        assert!(msg.contains("tv.mac"), "{msg}");
    }

    #[test]
    fn load_missing_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        assert_eq!(Config::load(&p).unwrap(), Config::default());
        let mut c = Config::default();
        c.overlay.in_desktop_mode = true;
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), c);
    }

    #[test]
    fn mac_parsing() {
        assert_eq!(
            parse_mac("AA-bb-01-02-03-04"),
            Some([0xAA, 0xBB, 1, 2, 3, 4])
        );
        assert_eq!(parse_mac("AA:BB:CC:DD:EE"), None);
        assert_eq!(parse_mac("AAA:BB:CC:DD:EE:FF"), None);
    }
}
