//! Hotkey actions and the conflict checks that need the OS (plan §2.13).
//!
//! `uc-core` does the layout-independent checks (reserved list, modifiers,
//! duplicates). Here: the full check behind the settings app's `CheckHotkey`,
//! and on Windows the AltGr check over every installed keyboard layout.
//! Registration itself lives on the window thread (`window.rs`), which owns
//! the hotkeys.

use uc_core::config::Hotkeys;
use uc_core::hotkey::{Hotkey, HotkeyCheck};
use uc_core::ipc::HotkeyAction;

pub const ACTIONS: [HotkeyAction; 3] = [
    HotkeyAction::GamingToggle,
    HotkeyAction::Overlay,
    HotkeyAction::PanicRestore,
];

/// `RegisterHotKey` id of an action.
pub fn id(action: HotkeyAction) -> i32 {
    match action {
        HotkeyAction::GamingToggle => 1,
        HotkeyAction::Overlay => 2,
        HotkeyAction::PanicRestore => 3,
    }
}

pub fn from_id(id: i32) -> Option<HotkeyAction> {
    ACTIONS.into_iter().find(|a| self::id(*a) == id)
}

pub fn name(action: HotkeyAction) -> &'static str {
    match action {
        HotkeyAction::GamingToggle => "gaming_toggle",
        HotkeyAction::Overlay => "overlay",
        HotkeyAction::PanicRestore => "panic_restore",
    }
}

pub fn of(hotkeys: &Hotkeys, action: HotkeyAction) -> Hotkey {
    match action {
        HotkeyAction::GamingToggle => hotkeys.gaming_toggle,
        HotkeyAction::Overlay => hotkeys.overlay,
        HotkeyAction::PanicRestore => hotkeys.panic_restore,
    }
}

/// Everything about a candidate shortcut for `action`, given the other two
/// current shortcuts. `altgr` lists the characters the chord types on the
/// installed layouts; `register_error` is set when `RegisterHotKey` refused it.
pub fn full_check(
    action: HotkeyAction,
    candidate: Hotkey,
    current: &Hotkeys,
    altgr: &[String],
    register_error: Option<String>,
) -> HotkeyCheck {
    let mut check = candidate.check();
    for other in ACTIONS.into_iter().filter(|a| *a != action) {
        if of(current, other) == candidate {
            check
                .errors
                .push(format!("same shortcut as {}", name(other)));
        }
    }
    if check.needs_altgr_check {
        for clash in altgr {
            check.warnings.push(format!(
                "AltGr clash: it {clash}, so typing that character would trigger the shortcut"
            ));
        }
        check.needs_altgr_check = false;
    }
    if let Some(e) = register_error {
        check.errors.push(e);
    }
    check
}

#[cfg(windows)]
pub use win::altgr_clashes;

#[cfg(windows)]
mod win {
    use uc_core::hotkey::Hotkey;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyboardLayoutList, HKL, MAPVK_VK_TO_VSC, MapVirtualKeyExW, ToUnicodeEx, VK_CONTROL,
        VK_LCONTROL, VK_LSHIFT, VK_MENU, VK_RMENU, VK_SHIFT,
    };

    /// Don't change the keyboard state (no dead-key side effects), Win10 1607+.
    const TOUNICODE_NO_STATE_CHANGE: u32 = 0x4;

    pub fn installed_layouts() -> Vec<HKL> {
        // SAFETY: first call only counts; the second fills a buffer of that size.
        unsafe {
            let n = GetKeyboardLayoutList(None).max(0) as usize;
            let mut list = vec![HKL::default(); n];
            let got = GetKeyboardLayoutList(Some(&mut list)).max(0) as usize;
            list.truncate(got);
            list
        }
    }

    /// What `hotkey` (with Ctrl+Alt, i.e. AltGr) types on `layout`, if anything.
    pub fn typed_with_altgr(layout: HKL, hotkey: Hotkey) -> Option<String> {
        let vk = u32::from(hotkey.key.vk());
        let mut state = [0u8; 256];
        for k in [VK_CONTROL, VK_MENU, VK_LCONTROL, VK_RMENU] {
            state[k.0 as usize] = 0x80;
        }
        if hotkey.modifiers.shift {
            state[VK_SHIFT.0 as usize] = 0x80;
            state[VK_LSHIFT.0 as usize] = 0x80;
        }
        let mut buf = [0u16; 8];
        // SAFETY: plain FFI calls with buffers that outlive them.
        let n = unsafe {
            let scan = MapVirtualKeyExW(vk, MAPVK_VK_TO_VSC, Some(layout));
            ToUnicodeEx(
                vk,
                scan,
                &state,
                &mut buf,
                TOUNICODE_NO_STATE_CHANGE,
                Some(layout),
            )
        };
        match n {
            n if n < 0 => Some("a dead key (accent)".into()),
            0 => None,
            n => {
                let s = String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())]);
                // Ctrl+letter gives ASCII control codes on every layout: not a character.
                (!s.chars().all(char::is_control)).then_some(s)
            }
        }
    }

    /// One line per installed layout on which the chord types something.
    pub fn altgr_clashes(hotkey: Hotkey) -> Vec<String> {
        if !hotkey.may_clash_with_altgr() {
            return Vec::new();
        }
        installed_layouts()
            .into_iter()
            .filter_map(|hkl| {
                let typed = typed_with_altgr(hkl, hotkey)?;
                let lang = (hkl.0 as usize) & 0xFFFF;
                Some(format!("types {typed:?} on keyboard layout {lang:04X}"))
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            KLF_NOTELLSHELL, LoadKeyboardLayoutW, UnloadKeyboardLayout,
        };
        use windows::core::HSTRING;

        /// Loads a layout for the test and unloads it again if it wasn't installed.
        fn with_layout(klid: &str, f: impl FnOnce(HKL)) {
            let before = installed_layouts();
            // SAFETY: plain FFI call with a NUL-terminated string.
            let hkl = unsafe { LoadKeyboardLayoutW(&HSTRING::from(klid), KLF_NOTELLSHELL) }
                .unwrap_or_else(|e| panic!("load layout {klid}: {e}"));
            f(hkl);
            if !before.contains(&hkl) {
                // SAFETY: unloading the layout this test loaded.
                let _ = unsafe { UnloadKeyboardLayout(hkl) };
            }
        }

        fn p(s: &str) -> Hotkey {
            s.parse().unwrap()
        }

        /// Plan §2.13: the default G and O chords type nothing on QWERTY (US),
        /// AZERTY (French) and Arabic (101).
        #[test]
        fn default_chords_are_free_on_common_layouts() {
            for klid in ["00000409", "0000040C", "00000401"] {
                with_layout(klid, |hkl| {
                    for hk in ["Ctrl+Alt+G", "Ctrl+Alt+O", "Ctrl+Alt+Shift+R"] {
                        assert_eq!(typed_with_altgr(hkl, p(hk)), None, "{hk} on {klid}");
                    }
                });
            }
        }

        #[test]
        fn azerty_altgr_e_is_the_euro_sign() {
            with_layout("0000040C", |hkl| {
                assert_eq!(typed_with_altgr(hkl, p("Ctrl+Alt+E")).as_deref(), Some("€"));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Hotkey {
        s.parse().unwrap()
    }

    #[test]
    fn ids_roundtrip() {
        for a in ACTIONS {
            assert_eq!(from_id(id(a)), Some(a));
        }
        assert_eq!(from_id(99), None);
    }

    #[test]
    fn duplicate_of_another_action_is_an_error() {
        let current = Hotkeys::default();
        let c = full_check(
            HotkeyAction::Overlay,
            current.gaming_toggle,
            &current,
            &[],
            None,
        );
        assert!(
            c.errors.iter().any(|e| e.contains("gaming_toggle")),
            "{c:?}"
        );
        // Keeping its own current shortcut is fine.
        let c = full_check(HotkeyAction::Overlay, current.overlay, &current, &[], None);
        assert!(c.is_ok(), "{c:?}");
    }

    #[test]
    fn altgr_and_registration_results_are_merged() {
        let current = Hotkeys::default();
        let c = full_check(
            HotkeyAction::Overlay,
            p("Ctrl+Alt+E"),
            &current,
            &["types \"€\" on keyboard layout 040C".into()],
            Some("in use by another program".into()),
        );
        assert!(!c.needs_altgr_check);
        assert!(c.warnings.iter().any(|w| w.contains('€')), "{c:?}");
        assert!(c.errors.iter().any(|e| e.contains("another program")));
    }
}
