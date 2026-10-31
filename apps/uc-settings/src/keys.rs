//! Turns egui key presses into [`Hotkey`]s for the shortcut recorder.

use eframe::egui;
use uc_core::{Hotkey, Key, Modifiers};

/// Win32 virtual-key code for an egui key, for the keys a hotkey may use.
fn vk(key: egui::Key) -> Option<u16> {
    use egui::Key as K;
    let name = key.name();
    // Letters and digits: VK code is the ASCII code of the uppercase char.
    if let [c] = name.as_bytes()
        && c.is_ascii_alphanumeric()
    {
        return Some(u16::from(c.to_ascii_uppercase()));
    }
    Some(match key {
        K::Space => 0x20,
        K::Enter => 0x0D,
        K::Tab => 0x09,
        K::Escape => 0x1B,
        K::Backspace => 0x08,
        K::Insert => 0x2D,
        K::Delete => 0x2E,
        K::Home => 0x24,
        K::End => 0x23,
        K::PageUp => 0x21,
        K::PageDown => 0x22,
        K::ArrowLeft => 0x25,
        K::ArrowUp => 0x26,
        K::ArrowRight => 0x27,
        K::ArrowDown => 0x28,
        _ => {
            let n: u16 = name.strip_prefix('F')?.parse().ok()?;
            return (1..=24).contains(&n).then(|| 0x70 + n - 1);
        }
    })
}

/// What a key press means while the recorder is listening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    Hotkey(Hotkey),
    /// Esc without modifiers: stop recording, keep the old shortcut.
    Cancel,
}

/// Interprets one pressed key. Returns `None` for keys a hotkey can't use
/// (they are ignored and recording goes on).
pub fn record(key: egui::Key, modifiers: egui::Modifiers) -> Option<Recorded> {
    let m = Modifiers {
        ctrl: modifiers.ctrl,
        alt: modifiers.alt,
        shift: modifiers.shift,
        // egui doesn't report the Windows key; Win+ shortcuts are refused anyway.
        win: false,
    };
    if key == egui::Key::Escape && m.is_empty() {
        return Some(Recorded::Cancel);
    }
    let key = Key::from_vk(vk(key)?)?;
    Some(Recorded::Hotkey(Hotkey::new(m, key)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool, alt: bool, shift: bool) -> egui::Modifiers {
        egui::Modifiers {
            ctrl,
            alt,
            shift,
            ..Default::default()
        }
    }

    fn rec(key: egui::Key, m: egui::Modifiers) -> String {
        match record(key, m) {
            Some(Recorded::Hotkey(h)) => h.to_string(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn records_the_defaults() {
        assert_eq!(rec(egui::Key::G, mods(true, true, false)), "Ctrl+Alt+G");
        assert_eq!(rec(egui::Key::O, mods(true, true, false)), "Ctrl+Alt+O");
        assert_eq!(
            rec(egui::Key::R, mods(true, true, true)),
            "Ctrl+Alt+Shift+R"
        );
    }

    #[test]
    fn maps_digits_function_and_named_keys() {
        assert_eq!(rec(egui::Key::Num7, mods(true, true, false)), "Ctrl+Alt+7");
        assert_eq!(rec(egui::Key::F10, mods(false, true, false)), "Alt+F10");
        assert_eq!(rec(egui::Key::F24, mods(true, false, false)), "Ctrl+F24");
        assert_eq!(
            rec(egui::Key::PageDown, mods(true, false, true)),
            "Ctrl+Shift+PageDown"
        );
        assert_eq!(
            rec(egui::Key::ArrowUp, mods(true, true, false)),
            "Ctrl+Alt+Up"
        );
    }

    #[test]
    fn escape_cancels_and_odd_keys_are_ignored() {
        assert_eq!(
            record(egui::Key::Escape, egui::Modifiers::NONE),
            Some(Recorded::Cancel)
        );
        assert_eq!(
            rec(egui::Key::Escape, mods(true, true, false)),
            "Ctrl+Alt+Esc"
        );
        assert_eq!(record(egui::Key::F25, mods(true, false, false)), None);
        assert_eq!(record(egui::Key::Semicolon, mods(true, false, false)), None);
    }
}
