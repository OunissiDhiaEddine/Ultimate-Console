//! The alt-tab filter (plan §2.9): which top-level windows count as tasks.

/// What the filter needs to know about one top-level window. Filled in by
/// the Win32 layer from `GetWindowLong`, `GetWindow`, `DwmGetWindowAttribute`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowFacts {
    pub visible: bool,
    /// Has an owner window (`GetWindow(GW_OWNER)`).
    pub owned: bool,
    /// `WS_EX_TOOLWINDOW`.
    pub tool_window: bool,
    /// `WS_EX_APPWINDOW`: forces a taskbar/alt-tab entry even when owned.
    pub app_window: bool,
    /// `WS_EX_NOACTIVATE`.
    pub no_activate: bool,
    /// `DWMWA_CLOAKED` is non-zero (UWP apps suspended in the background,
    /// windows on other virtual desktops).
    pub cloaked: bool,
    pub title: String,
    pub class: String,
}

/// Window classes that pass the generic rules but are never tasks.
const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    // Windows Input Experience / touch keyboard host.
    "Windows.UI.Core.CoreWindow",
];

/// The usual alt-tab rule: visible, not cloaked, has a title, and either an
/// app window or an unowned window that is not a tool window and can be
/// activated.
pub fn is_task(w: &WindowFacts) -> bool {
    if !w.visible || w.cloaked || w.title.trim().is_empty() {
        return false;
    }
    if SHELL_CLASSES.contains(&w.class.as_str()) {
        return false;
    }
    if w.app_window {
        return true;
    }
    !w.owned && !w.tool_window && !w.no_activate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normal() -> WindowFacts {
        WindowFacts {
            visible: true,
            title: "Elden Ring".into(),
            class: "ELDEN RING".into(),
            ..WindowFacts::default()
        }
    }

    #[test]
    fn plain_app_window_is_a_task() {
        assert!(is_task(&normal()));
    }

    #[test]
    fn hidden_cloaked_or_untitled_are_not() {
        for f in [
            WindowFacts {
                visible: false,
                ..normal()
            },
            WindowFacts {
                cloaked: true,
                ..normal()
            },
            WindowFacts {
                title: "  ".into(),
                ..normal()
            },
        ] {
            assert!(!is_task(&f), "{f:?}");
        }
    }

    #[test]
    fn owned_tool_and_noactivate_need_app_window() {
        for f in [
            WindowFacts {
                owned: true,
                ..normal()
            },
            WindowFacts {
                tool_window: true,
                ..normal()
            },
            WindowFacts {
                no_activate: true,
                ..normal()
            },
        ] {
            assert!(!is_task(&f), "{f:?}");
            assert!(is_task(&WindowFacts {
                app_window: true,
                ..f
            }));
        }
    }

    #[test]
    fn shell_windows_are_never_tasks() {
        let f = WindowFacts {
            class: "Progman".into(),
            title: "Program Manager".into(),
            app_window: true,
            ..normal()
        };
        assert!(!is_task(&f));
    }
}
