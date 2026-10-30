//! Hotkey type and the layout-independent conflict rules (plan §2.13).
//!
//! A hotkey is written as `"Ctrl+Alt+G"` in `config.toml` and on the wire.
//! The checks here need no Win32: the reserved list, "needs a modifier", and
//! the known-overlay warnings. The checks that need the OS (`RegisterHotKey`
//! failing, AltGr producing a character on an installed layout) are done by
//! the agent (WP-08) and reported through the same [`HotkeyCheck`] type.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Error;

/// Modifier keys. [`Modifiers::to_win32`] gives the `RegisterHotKey` flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        win: false,
    };

    pub fn is_empty(self) -> bool {
        self == Self::NONE
    }

    pub fn count(self) -> usize {
        [self.ctrl, self.alt, self.shift, self.win]
            .into_iter()
            .filter(|m| *m)
            .count()
    }

    /// `MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN` bits for `RegisterHotKey`.
    /// The caller adds `MOD_NOREPEAT`.
    pub fn to_win32(self) -> u32 {
        (self.alt as u32)
            | (self.ctrl as u32) << 1
            | (self.shift as u32) << 2
            | (self.win as u32) << 3
    }
}

/// A non-modifier key, stored as its Win32 virtual-key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(u16);

/// Names accepted for keys that are not letters, digits or F-keys.
/// The first name for a code is the canonical one.
const NAMED_KEYS: &[(&str, u16)] = &[
    ("Space", 0x20),
    ("Enter", 0x0D),
    ("Return", 0x0D),
    ("Tab", 0x09),
    ("Esc", 0x1B),
    ("Escape", 0x1B),
    ("Backspace", 0x08),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Del", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
    ("PrintScreen", 0x2C),
    ("Pause", 0x13),
    ("Num0", 0x60),
    ("Num1", 0x61),
    ("Num2", 0x62),
    ("Num3", 0x63),
    ("Num4", 0x64),
    ("Num5", 0x65),
    ("Num6", 0x66),
    ("Num7", 0x67),
    ("Num8", 0x68),
    ("Num9", 0x69),
    ("NumMultiply", 0x6A),
    ("NumAdd", 0x6B),
    ("NumSubtract", 0x6D),
    ("NumDecimal", 0x6E),
    ("NumDivide", 0x6F),
];

const VK_F1: u16 = 0x70;

impl Key {
    /// Win32 virtual-key code (`VK_*`).
    pub fn vk(self) -> u16 {
        self.0
    }

    /// Builds a key from a virtual-key code, if it is one we can name.
    pub fn from_vk(vk: u16) -> Option<Self> {
        let key = Self(vk);
        key.name().map(|_| key)
    }

    pub fn is_function_key(self) -> bool {
        (VK_F1..VK_F1 + 24).contains(&self.0)
    }

    fn name(self) -> Option<String> {
        match self.0 {
            0x30..=0x39 | 0x41..=0x5A => Some(char::from(self.0 as u8).to_string()),
            vk if self.is_function_key() => Some(format!("F{}", vk - VK_F1 + 1)),
            vk => NAMED_KEYS
                .iter()
                .find(|(_, code)| *code == vk)
                .map(|(name, _)| (*name).to_string()),
        }
    }
}

impl FromStr for Key {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let bytes = s.as_bytes();
        if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() {
            return Ok(Self(u16::from(bytes[0].to_ascii_uppercase())));
        }
        if let Some(n) = s
            .strip_prefix(['F', 'f'])
            .and_then(|n| n.parse::<u16>().ok())
            .filter(|n| (1..=24).contains(n))
        {
            return Ok(Self(VK_F1 + n - 1));
        }
        NAMED_KEYS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(s))
            .map(|(_, vk)| Self(*vk))
            .ok_or_else(|| format!("unknown key {s:?}"))
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => f.write_str(&name),
            None => write!(f, "VK{:#04X}", self.0),
        }
    }
}

/// A global hotkey: modifiers plus one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hotkey {
    pub modifiers: Modifiers,
    pub key: Key,
}

impl Hotkey {
    pub fn new(modifiers: Modifiers, key: Key) -> Self {
        Self { modifiers, key }
    }

    /// `Ctrl+Alt+<key>` without Win is the same chord as AltGr+<key> on
    /// layouts like AZERTY or Arabic. The agent must then check with
    /// `VkKeyScanEx`/`ToUnicodeEx` whether it types a character (plan §2.13 rule 3).
    pub fn may_clash_with_altgr(&self) -> bool {
        self.modifiers.ctrl && self.modifiers.alt && !self.modifiers.win
    }

    /// Layout-independent checks. See the module docs for what is left to the agent.
    pub fn check(&self) -> HotkeyCheck {
        let mut check = HotkeyCheck::default();
        let m = self.modifiers;

        if !(m.ctrl || m.alt || m.win) {
            check.errors.push(if m.shift {
                "Shift alone is not enough: add Ctrl or Alt".into()
            } else {
                "needs at least one modifier (Ctrl or Alt)".into()
            });
        }
        if m.win {
            check.errors.push(
                "Win+ shortcuts are reserved by Windows and behave differently without Explorer"
                    .into(),
            );
        }
        if let Some(reason) = reserved_reason(self) {
            check.errors.push(format!("reserved by Windows ({reason})"));
        }
        if let Some(owner) = known_overlay(self) {
            check.warnings.push(format!(
                "commonly used by {owner}; pick another if you use it"
            ));
        }
        if m.count() == 1 && (m.ctrl || m.alt) && !self.key.is_function_key() {
            check.warnings.push(
                "a single Ctrl or Alt chord is used by many apps; it will be taken from them globally"
                    .into(),
            );
        }
        if self.may_clash_with_altgr() {
            check.needs_altgr_check = true;
        }
        check
    }
}

/// Result of checking a hotkey. Errors block saving; warnings are shown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyCheck {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// True when the agent still has to run the per-layout AltGr check.
    #[serde(default)]
    pub needs_altgr_check: bool,
}

impl HotkeyCheck {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

fn hk(ctrl: bool, alt: bool, shift: bool, key: u16) -> Hotkey {
    Hotkey::new(
        Modifiers {
            ctrl,
            alt,
            shift,
            win: false,
        },
        Key(key),
    )
}

fn reserved_reason(h: &Hotkey) -> Option<&'static str> {
    const DEL: u16 = 0x2E;
    const ESC: u16 = 0x1B;
    const TAB: u16 = 0x09;
    const SPACE: u16 = 0x20;
    const F4: u16 = VK_F1 + 3;
    const PRINT: u16 = 0x2C;

    if h.key.0 == PRINT {
        return Some("PrintScreen combos take screenshots");
    }
    let table = [
        (hk(true, true, false, DEL), "Ctrl+Alt+Del"),
        (hk(true, true, true, DEL), "Ctrl+Alt+Del"),
        (hk(true, false, true, ESC), "Task Manager"),
        (hk(true, false, false, ESC), "Start menu"),
        (hk(false, true, false, TAB), "Alt+Tab"),
        (hk(false, true, true, TAB), "Alt+Tab"),
        (hk(true, true, false, TAB), "Alt+Tab"),
        (hk(false, true, false, ESC), "window cycling"),
        (hk(false, true, false, F4), "close window"),
        (hk(false, true, false, SPACE), "window menu"),
    ];
    table.iter().find(|(r, _)| r == h).map(|(_, why)| *why)
}

fn known_overlay(h: &Hotkey) -> Option<&'static str> {
    let table = [
        (hk(false, true, false, u16::from(b'Z')), "NVIDIA overlay"),
        (hk(false, true, false, VK_F1), "NVIDIA screenshot"),
        (hk(false, true, false, VK_F1 + 8), "NVIDIA recording"),
        (hk(false, true, false, VK_F1 + 9), "NVIDIA instant replay"),
        (hk(false, true, false, u16::from(b'R')), "AMD overlay"),
        (hk(false, false, true, 0x09), "Steam overlay"),
        (hk(false, true, false, 0x0D), "fullscreen toggle in games"),
    ];
    table.iter().find(|(r, _)| r == h).map(|(_, who)| *who)
}

/// Checks a whole set of hotkeys: each one on its own, plus "must all differ".
pub fn check_set(hotkeys: &[(&str, Hotkey)]) -> Vec<(String, HotkeyCheck)> {
    hotkeys
        .iter()
        .enumerate()
        .map(|(i, (name, h))| {
            let mut c = h.check();
            for (other, oh) in hotkeys.iter().take(i) {
                if oh == h {
                    c.errors.push(format!("same shortcut as {other}"));
                }
            }
            ((*name).to_string(), c)
        })
        .collect()
}

impl FromStr for Hotkey {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let err = |reason: String| Error::Hotkey {
            input: input.to_string(),
            reason,
        };
        let mut modifiers = Modifiers::NONE;
        let mut key = None;
        for part in input.split('+').map(str::trim) {
            let flag = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut modifiers.ctrl,
                "alt" => &mut modifiers.alt,
                "shift" => &mut modifiers.shift,
                "win" | "super" | "meta" => &mut modifiers.win,
                "" => return Err(err("empty part".into())),
                _ => {
                    if key.is_some() {
                        return Err(err("more than one non-modifier key".into()));
                    }
                    key = Some(part.parse::<Key>().map_err(err)?);
                    continue;
                }
            };
            if *flag {
                return Err(err(format!("{part} given twice")));
            }
            *flag = true;
        }
        let key = key.ok_or_else(|| err("missing key".into()))?;
        Ok(Self { modifiers, key })
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.modifiers;
        for (on, name) in [
            (m.ctrl, "Ctrl"),
            (m.alt, "Alt"),
            (m.shift, "Shift"),
            (m.win, "Win"),
        ] {
            if on {
                write!(f, "{name}+")?;
            }
        }
        write!(f, "{}", self.key)
    }
}

impl Serialize for Hotkey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Hotkey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Hotkey {
        s.parse().unwrap()
    }

    #[test]
    fn parse_and_display_roundtrip() {
        for s in [
            "Ctrl+Alt+G",
            "Ctrl+Alt+Shift+R",
            "Alt+F10",
            "Ctrl+Shift+PageDown",
            "Ctrl+Alt+7",
        ] {
            assert_eq!(p(s).to_string(), s);
        }
        assert_eq!(
            p(" shift + control + alt + r ").to_string(),
            "Ctrl+Alt+Shift+R"
        );
        assert_eq!(p("ctrl+alt+escape").to_string(), "Ctrl+Alt+Esc");
    }

    #[test]
    fn parse_errors() {
        for s in [
            "",
            "Ctrl+Alt",
            "Ctrl+G+H",
            "Ctrl+Ctrl+G",
            "Ctrl++G",
            "Ctrl+Alt+F25",
            "Ctrl+Alt+Foo",
        ] {
            assert!(s.parse::<Hotkey>().is_err(), "{s} should fail");
        }
    }

    #[test]
    fn vk_codes() {
        assert_eq!(p("Ctrl+Alt+G").key.vk(), 0x47);
        assert_eq!(p("Ctrl+F12").key.vk(), 0x7B);
        assert_eq!(p("Ctrl+Alt+Shift+R").modifiers.to_win32(), 0x1 | 0x2 | 0x4);
        assert_eq!(Key::from_vk(0x2E).unwrap().to_string(), "Delete");
        assert!(Key::from_vk(0xFF).is_none());
    }

    #[test]
    fn defaults_pass_checks() {
        for s in ["Ctrl+Alt+G", "Ctrl+Alt+O", "Ctrl+Alt+Shift+R"] {
            let c = p(s).check();
            assert!(c.is_ok(), "{s}: {c:?}");
            assert!(c.warnings.is_empty(), "{s}: {c:?}");
        }
    }

    #[test]
    fn rejects_reserved_and_weak() {
        for s in [
            "Ctrl+Alt+Delete",
            "Ctrl+Shift+Esc",
            "Alt+Tab",
            "Alt+F4",
            "G",
            "Shift+G",
            "Win+G",
            "Ctrl+PrintScreen",
        ] {
            assert!(!p(s).check().is_ok(), "{s} should be rejected");
        }
    }

    #[test]
    fn warns_on_overlays_and_single_modifier() {
        assert!(!p("Alt+Z").check().warnings.is_empty());
        assert!(!p("Ctrl+S").check().warnings.is_empty());
        assert!(p("Ctrl+F12").check().warnings.is_empty());
    }

    #[test]
    fn altgr_flag() {
        assert!(p("Ctrl+Alt+E").check().needs_altgr_check);
        assert!(!p("Ctrl+Shift+E").check().needs_altgr_check);
    }

    #[test]
    fn set_must_differ() {
        let r = check_set(&[("gaming", p("Ctrl+Alt+G")), ("overlay", p("Ctrl+Alt+G"))]);
        assert!(r[0].1.is_ok());
        assert!(!r[1].1.is_ok());
    }

    #[test]
    fn serde_as_string() {
        #[derive(Serialize, Deserialize)]
        struct W {
            k: Hotkey,
        }
        let t = toml::to_string(&W { k: p("Ctrl+Alt+O") }).unwrap();
        assert_eq!(t.trim(), r#"k = "Ctrl+Alt+O""#);
        let w: W = toml::from_str(&t).unwrap();
        assert_eq!(w.k, p("Ctrl+Alt+O"));
    }
}
