//! Which top-level window is Big Picture (plan §2.8).

use uc_core::config::Steam;

/// A top-level window, as seen by a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// `HWND` value. Only valid while the window exists.
    pub hwnd: isize,
    pub pid: u32,
    pub class: String,
    pub title: String,
    pub visible: bool,
}

/// Matches the Big Picture window by class and title.
///
/// Both compare whole strings, ignoring case and surrounding whitespace. An
/// empty field matches anything, but at least one must be set: `SDL_app` on
/// its own also matches SDL games, so the title is what really identifies
/// Big Picture. Steam localises the title, which is why both come from the
/// config (`[steam] bp_window_class` / `bp_window_title`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowMatcher {
    pub class: String,
    pub title: String,
}

impl WindowMatcher {
    pub fn new(class: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            title: title.into(),
        }
    }

    pub fn from_config(steam: &Steam) -> Self {
        Self::new(&steam.bp_window_class, &steam.bp_window_title)
    }

    /// False when both fields are empty (it would match every window).
    pub fn is_usable(&self) -> bool {
        !self.class.trim().is_empty() || !self.title.trim().is_empty()
    }

    /// True if `class` alone could be Big Picture. Used to skip a full scan
    /// for events on unrelated windows.
    pub fn class_matches(&self, class: &str) -> bool {
        field_matches(&self.class, class)
    }

    pub fn matches(&self, window: &WindowInfo) -> bool {
        window.visible && self.identifies(window)
    }

    /// Class and title match, whether or not the window is visible. Steam can
    /// create the Big Picture window hidden when it was minimised to the tray.
    pub fn identifies(&self, window: &WindowInfo) -> bool {
        self.is_usable()
            && field_matches(&self.class, &window.class)
            && field_matches(&self.title, &window.title)
    }

    /// Follows a Big Picture window seen before (`last`) by its handle, for
    /// when Steam retitles it for a moment: the same `HWND` and process,
    /// still visible and of the right class, counts as Big Picture. Except
    /// when it is now titled like the desktop Steam window (`main_title`):
    /// that is Steam leaving Big Picture in place.
    pub fn follow<'a>(
        &self,
        last: &WindowInfo,
        windows: impl IntoIterator<Item = &'a WindowInfo>,
        main_title: &str,
    ) -> Option<&'a WindowInfo> {
        windows.into_iter().find(|w| {
            w.hwnd == last.hwnd
                && w.pid == last.pid
                && w.visible
                && self.class_matches(&w.class)
                && !w.title.trim().eq_ignore_ascii_case(main_title.trim())
        })
    }

    /// The first window that would match if it were visible, but is hidden.
    pub fn find_hidden<'a>(
        &self,
        windows: impl IntoIterator<Item = &'a WindowInfo>,
    ) -> Option<&'a WindowInfo> {
        windows
            .into_iter()
            .find(|w| !w.visible && self.identifies(w))
    }

    /// The first matching window, in the order given (z-order for scans).
    pub fn find<'a>(
        &self,
        windows: impl IntoIterator<Item = &'a WindowInfo>,
    ) -> Option<&'a WindowInfo> {
        windows.into_iter().find(|w| self.matches(w))
    }
}

fn field_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim();
    pattern.is_empty() || pattern.to_lowercase() == value.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(class: &str, title: &str, visible: bool) -> WindowInfo {
        WindowInfo {
            hwnd: 1,
            pid: 2,
            class: class.into(),
            title: title.into(),
            visible,
        }
    }

    #[test]
    fn default_config_matches_big_picture_only() {
        let m = WindowMatcher::from_config(&Steam::default());
        assert!(m.matches(&win("SDL_app", "Steam Big Picture Mode", true)));
        assert!(m.matches(&win("sdl_app", " steam big picture mode ", true)));
        assert!(!m.matches(&win("SDL_app", "Some SDL Game", true)));
        assert!(!m.matches(&win("Chrome_WidgetWin_1", "Steam Big Picture Mode", true)));
        assert!(!m.matches(&win("SDL_app", "Steam Big Picture Mode - reddit", true)));
    }

    #[test]
    fn hidden_windows_never_match() {
        let m = WindowMatcher::from_config(&Steam::default());
        assert!(!m.matches(&win("SDL_app", "Steam Big Picture Mode", false)));
    }

    #[test]
    fn hidden_big_picture_is_found_separately() {
        let m = WindowMatcher::from_config(&Steam::default());
        let mut hidden = win("SDL_app", "Steam Big Picture Mode", false);
        hidden.hwnd = 7;
        let list = [win("SDL_app", "Game", false), hidden];
        assert_eq!(m.find(&list), None);
        assert_eq!(m.find_hidden(&list).map(|w| w.hwnd), Some(7));
        let visible = [win("SDL_app", "Steam Big Picture Mode", true)];
        assert_eq!(m.find_hidden(&visible), None);
    }

    #[test]
    fn big_picture_is_followed_by_handle_through_a_retitle() {
        let m = WindowMatcher::from_config(&Steam::default());
        let last = win("SDL_app", "Steam Big Picture Mode", true);
        let retitled = [win("SDL_app", "Steam Keyboard", true)];
        assert_eq!(m.find(&retitled), None);
        assert!(m.follow(&last, &retitled, "Steam").is_some());
        // Turned into the desktop Steam window, hidden, or another process.
        assert!(
            m.follow(&last, &[win("SDL_app", "Steam", true)], "Steam")
                .is_none()
        );
        assert!(
            m.follow(&last, &[win("SDL_app", "x", false)], "Steam")
                .is_none()
        );
        let mut other = win("SDL_app", "x", true);
        other.pid = 99;
        assert!(m.follow(&last, &[other], "Steam").is_none());
    }

    #[test]
    fn empty_field_is_a_wildcard_but_not_both() {
        let title_only = WindowMatcher::new("", "Steam Big Picture Mode");
        assert!(title_only.matches(&win("Anything", "Steam Big Picture Mode", true)));
        let none = WindowMatcher::new(" ", "");
        assert!(!none.is_usable());
        assert!(!none.matches(&win("SDL_app", "Steam Big Picture Mode", true)));
    }

    #[test]
    fn localised_title_from_config() {
        let m = WindowMatcher::new("SDL_app", "Steam – Mode Big Picture");
        assert!(m.matches(&win("SDL_app", "STEAM – MODE BIG PICTURE", true)));
    }

    #[test]
    fn find_returns_first_match() {
        let m = WindowMatcher::from_config(&Steam::default());
        let mut a = win("SDL_app", "Steam Big Picture Mode", true);
        a.hwnd = 10;
        let mut b = a.clone();
        b.hwnd = 20;
        let list = [win("SDL_app", "Game", true), a, b];
        assert_eq!(m.find(&list).map(|w| w.hwnd), Some(10));
    }
}
