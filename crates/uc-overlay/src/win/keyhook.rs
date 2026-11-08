//! Keyboard input for the overlay while it is shown *without* taking focus.
//!
//! Games in exclusive fullscreen (Shadow of the Tomb Raider, for one)
//! minimise themselves the moment another window takes the foreground, which
//! looked like the overlay alt-tabbing out of the game. Over such a game the
//! overlay stays inactive and reads the keyboard through a low-level hook
//! instead: plain key presses are posted to the overlay and kept from the
//! game, while key releases, modifier keys, media/volume keys and any combo
//! with Ctrl, Alt or Win pass through untouched (so the overlay hotkey,
//! Alt+Tab and the volume keys still work).

use std::cell::Cell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CAPITAL, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU,
    VK_NUMLOCK, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SCROLL, VK_SHIFT, VK_SNAPSHOT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, PostMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

use super::WM_APP_KEY;

thread_local! {
    /// The overlay window the hook posts keys to (0: none). A low-level hook
    /// runs on the thread that installed it, which is the overlay's.
    static TARGET: Cell<isize> = const { Cell::new(0) };
}

/// An installed keyboard hook; dropping it removes the hook.
pub struct KeyHook(HHOOK);

impl KeyHook {
    /// Hook the keyboard for `target`. `None` (logged) if Windows refuses.
    pub fn install(target: HWND) -> Option<Self> {
        TARGET.with(|t| t.set(target.0 as isize));
        // SAFETY: `hook_proc` has the HOOKPROC signature; the module handle
        // is our own executable.
        let hook = unsafe {
            let module = GetModuleHandleW(None).ok();
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module.map(Into::into), 0)
        };
        match hook {
            Ok(h) => Some(Self(h)),
            Err(e) => {
                TARGET.with(|t| t.set(0));
                tracing::warn!("overlay keyboard hook failed: {e}");
                None
            }
        }
    }
}

impl Drop for KeyHook {
    fn drop(&mut self) {
        TARGET.with(|t| t.set(0));
        // SAFETY: removing the hook we installed, on the same thread.
        if let Err(e) = unsafe { UnhookWindowsHookEx(self.0) } {
            tracing::warn!("removing the overlay keyboard hook failed: {e}");
        }
    }
}

unsafe extern "system" fn hook_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
        // SAFETY: for WH_KEYBOARD_LL with HC_ACTION, lparam points to a
        // KBDLLHOOKSTRUCT that is valid for this call.
        let vk = unsafe { (*(lp.0 as *const KBDLLHOOKSTRUCT)).vkCode } as u16;
        let target = TARGET.with(Cell::get);
        if target != 0 && swallow(vk, combo_held()) {
            // Keep the hook fast (Windows drops slow ones): just post it on.
            // SAFETY: posting to our own window; a stale handle just fails.
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(target as _)),
                    WM_APP_KEY,
                    WPARAM(usize::from(vk)),
                    LPARAM(0),
                )
            };
            return LRESULT(1);
        }
    }
    // SAFETY: passing the original arguments down the chain.
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

/// Ctrl, Alt or Win held right now.
fn combo_held() -> bool {
    [VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
        .iter()
        // SAFETY: plain FFI call.
        .any(|k| unsafe { GetAsyncKeyState(i32::from(k.0)) } < 0)
}

/// Whether a key press goes to the overlay (and not to the game).
fn swallow(vk: u16, combo_held: bool) -> bool {
    let passthrough = [
        VK_SHIFT,
        VK_CONTROL,
        VK_MENU,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_LMENU,
        VK_RMENU,
        VK_LWIN,
        VK_RWIN,
        VK_CAPITAL,
        VK_NUMLOCK,
        VK_SCROLL,
        VK_SNAPSHOT,
    ];
    // 0xA6..=0xB7: browser, volume, media and launch keys.
    let media = (0xA6..=0xB7).contains(&vk);
    !combo_held && !media && !passthrough.iter().any(|k| k.0 == vk)
}

#[cfg(test)]
mod tests {
    use super::swallow;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_DOWN, VK_ESCAPE, VK_LMENU, VK_LSHIFT, VK_RETURN, VK_VOLUME_UP,
    };

    #[test]
    fn plain_keys_go_to_the_overlay() {
        for vk in [VK_DOWN.0, VK_RETURN.0, VK_ESCAPE.0, u16::from(b'W')] {
            assert!(swallow(vk, false), "{vk:#x}");
        }
    }

    #[test]
    fn shortcuts_modifiers_and_media_keys_pass_through() {
        assert!(!swallow(u16::from(b'O'), true), "Ctrl+Alt+O hotkey");
        assert!(!swallow(VK_LMENU.0, false));
        assert!(!swallow(VK_LSHIFT.0, false));
        assert!(!swallow(VK_VOLUME_UP.0, false));
    }
}
