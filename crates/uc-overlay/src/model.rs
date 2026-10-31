//! What the overlay shows and how input changes it. Platform independent:
//! the Win32 layer feeds it tasks, volume and stats, turns key presses and
//! gamepad buttons into [`Action`]s, and carries out the [`Effect`]s it returns.

use uc_audio::AppSession;
use uc_sysinfo::{ProcessStats, Snapshot};

/// One step of the volume sliders (5 %).
pub const VOLUME_STEP: f32 = 0.05;

/// The three panels of plan §2.9.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Tasks,
    Volume,
    Info,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Tasks, Tab::Volume, Tab::Info];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Tasks => "Tasks",
            Tab::Volume => "Volume",
            Tab::Info => "Info",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    fn step(self, delta: isize) -> Tab {
        let n = Self::ALL.len() as isize;
        Self::ALL[(self.index() as isize + delta).rem_euclid(n) as usize]
    }
}

/// An alt-tab-eligible top-level window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// `HWND` as an integer, so this type is `Send` and platform independent.
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
}

/// Something the user asked for, from the keyboard, a gamepad or the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    Left,
    Right,
    /// A / Enter: switch to the task, toggle mute, confirm.
    Accept,
    /// B / Esc: cancel the confirmation, otherwise close the overlay.
    Back,
    NextTab,
    PrevTab,
    /// X / Delete: ask the selected window to close (`WM_CLOSE`).
    CloseTask,
    /// Y / Shift+Delete: end the selected task's process, after confirming.
    KillTask,
    /// Master volume up / down and mute, from any panel.
    VolumeUp,
    VolumeDown,
    ToggleMute,
    /// Mouse: select tab `n`.
    SelectTab(usize),
    /// Mouse: select task `n` and switch to it.
    ActivateTask(usize),
}

/// Work for the Win32 layer.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Hide,
    /// Bring this window to the foreground (and hide the overlay).
    SwitchTo(isize),
    /// Post `WM_CLOSE` to this window.
    CloseWindow(isize),
    /// Terminate this process.
    KillProcess(u32),
    SetMasterVolume(f32),
    SetMasterMuted(bool),
    SetAppVolume {
        pid: u32,
        level: f32,
    },
    SetAppMuted {
        pid: u32,
        muted: bool,
    },
}

/// A question shown on top of the panels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    Kill { pid: u32, name: String },
}

/// Master volume as last read from the audio device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Master {
    pub level: f32,
    pub muted: bool,
}

/// What the volume panel shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Volume {
    /// `None` when there is no output device or audio failed.
    pub master: Option<Master>,
    pub apps: Vec<AppSession>,
    /// Shown instead of the sliders when audio is unavailable.
    pub error: Option<String>,
}

/// One line of the volume panel.
#[derive(Debug, Clone, PartialEq)]
pub enum VolumeRow {
    Master(Master),
    App {
        session: AppSession,
        /// This app owns the task selected on the Tasks panel.
        selected_task: bool,
    },
}

impl VolumeRow {
    pub fn level(&self) -> f32 {
        match self {
            VolumeRow::Master(m) => m.level,
            VolumeRow::App { session, .. } => session.volume,
        }
    }

    pub fn muted(&self) -> bool {
        match self {
            VolumeRow::Master(m) => m.muted,
            VolumeRow::App { session, .. } => session.muted,
        }
    }
}

/// The overlay's whole state.
#[derive(Debug, Clone, Default)]
pub struct Model {
    pub tab: Tab,
    pub tasks: Vec<Task>,
    /// Index into `tasks`. Meaningless when `tasks` is empty.
    pub selected: usize,
    /// First visible row of the task grid.
    pub scroll_row: usize,
    /// Tiles per row of the task grid; set by the layout.
    pub columns: usize,
    pub volume: Volume,
    /// Index into [`Model::volume_rows`].
    pub volume_row: usize,
    pub stats: Snapshot,
    pub confirm: Option<Confirm>,
}

impl Model {
    pub fn new(columns: usize) -> Self {
        Self {
            columns: columns.max(1),
            ..Self::default()
        }
    }

    /// Back to the Tasks panel with the first task (the window that was in
    /// front before the overlay opened) selected. Called on every show.
    pub fn reset(&mut self) {
        self.tab = Tab::Tasks;
        self.selected = 0;
        self.scroll_row = 0;
        self.volume_row = 0;
        self.confirm = None;
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.tasks.get(self.selected)
    }

    /// Stats of the selected task's process, once sampled.
    pub fn selected_stats(&self) -> Option<&ProcessStats> {
        self.selected_task().and_then(|t| self.stats.process(t.pid))
    }

    /// Display name of a process: the exe name when known, else the window title.
    pub fn process_name(&self, task: &Task) -> String {
        self.stats
            .process(task.pid)
            .and_then(|p| p.name.clone())
            .unwrap_or_else(|| task.title.clone())
    }

    /// Replace the task list, keeping the same window selected if it still exists.
    pub fn set_tasks(&mut self, tasks: Vec<Task>) {
        let keep = self.selected_task().map(|t| t.hwnd);
        self.tasks = tasks;
        self.selected = keep
            .and_then(|h| self.tasks.iter().position(|t| t.hwnd == h))
            .unwrap_or_else(|| self.selected.min(self.tasks.len().saturating_sub(1)));
    }

    pub fn set_volume(&mut self, volume: Volume) {
        self.volume = volume;
        self.volume_row = self
            .volume_row
            .min(self.volume_rows().len().saturating_sub(1));
    }

    /// Master first, then the selected task's app, then every other app.
    pub fn volume_rows(&self) -> Vec<VolumeRow> {
        let mut rows = Vec::new();
        if let Some(m) = self.volume.master {
            rows.push(VolumeRow::Master(m));
        }
        let pid = self.selected_task().map(|t| t.pid);
        let (mut mine, others): (Vec<_>, Vec<_>) = self
            .volume
            .apps
            .iter()
            .cloned()
            .partition(|s| Some(s.pid) == pid);
        mine.extend(others);
        rows.extend(mine.into_iter().map(|session| VolumeRow::App {
            selected_task: Some(session.pid) == pid,
            session,
        }));
        rows
    }

    /// Keep the selected task's row inside `visible_rows` rows of the grid.
    pub fn ensure_visible(&mut self, visible_rows: usize) {
        let visible_rows = visible_rows.max(1);
        let row = self.selected / self.columns.max(1);
        if row < self.scroll_row {
            self.scroll_row = row;
        } else if row >= self.scroll_row + visible_rows {
            self.scroll_row = row + 1 - visible_rows;
        }
    }

    /// Apply one action. Returns the side effects to carry out, in order.
    pub fn handle(&mut self, action: Action) -> Vec<Effect> {
        if let Some(confirm) = self.confirm.clone() {
            return match action {
                Action::Accept => {
                    self.confirm = None;
                    match confirm {
                        Confirm::Kill { pid, .. } => vec![Effect::KillProcess(pid)],
                    }
                }
                Action::Back | Action::KillTask => {
                    self.confirm = None;
                    Vec::new()
                }
                _ => Vec::new(),
            };
        }

        match action {
            Action::Back => vec![Effect::Hide],
            Action::NextTab => {
                self.tab = self.tab.step(1);
                Vec::new()
            }
            Action::PrevTab => {
                self.tab = self.tab.step(-1);
                Vec::new()
            }
            Action::SelectTab(i) => {
                if let Some(&t) = Tab::ALL.get(i) {
                    self.tab = t;
                }
                Vec::new()
            }
            Action::ActivateTask(i) => match self.tasks.get(i) {
                Some(t) => {
                    self.selected = i;
                    vec![Effect::SwitchTo(t.hwnd)]
                }
                None => Vec::new(),
            },
            Action::VolumeUp => self.adjust_master(VOLUME_STEP),
            Action::VolumeDown => self.adjust_master(-VOLUME_STEP),
            Action::ToggleMute => self.toggle_row_mute(0),
            Action::CloseTask => match self.selected_task() {
                Some(t) => vec![Effect::CloseWindow(t.hwnd)],
                None => Vec::new(),
            },
            Action::KillTask => {
                if let Some(t) = self.selected_task() {
                    self.confirm = Some(Confirm::Kill {
                        pid: t.pid,
                        name: self.process_name(t),
                    });
                }
                Vec::new()
            }
            Action::Up | Action::Down | Action::Left | Action::Right | Action::Accept => {
                match self.tab {
                    Tab::Tasks => self.tasks_nav(action),
                    Tab::Volume => self.volume_nav(action),
                    Tab::Info => self.info_nav(action),
                }
            }
        }
    }

    fn tasks_nav(&mut self, action: Action) -> Vec<Effect> {
        let n = self.tasks.len();
        if n == 0 {
            return Vec::new();
        }
        let cols = self.columns.max(1);
        let s = self.selected;
        self.selected = match action {
            Action::Left => s.saturating_sub(1),
            Action::Right => (s + 1).min(n - 1),
            Action::Up => s.checked_sub(cols).unwrap_or(s),
            // Moving down from a full row onto a shorter last row lands on
            // its last tile instead of doing nothing.
            Action::Down if s / cols < (n - 1) / cols => (s + cols).min(n - 1),
            Action::Down => s,
            Action::Accept => return vec![Effect::SwitchTo(self.tasks[s].hwnd)],
            _ => s,
        };
        Vec::new()
    }

    fn volume_nav(&mut self, action: Action) -> Vec<Effect> {
        let n = self.volume_rows().len();
        if n == 0 {
            return Vec::new();
        }
        match action {
            Action::Up => self.volume_row = self.volume_row.saturating_sub(1),
            Action::Down => self.volume_row = (self.volume_row + 1).min(n - 1),
            Action::Left => return self.adjust_row(self.volume_row, -VOLUME_STEP),
            Action::Right => return self.adjust_row(self.volume_row, VOLUME_STEP),
            Action::Accept => return self.toggle_row_mute(self.volume_row),
            _ => {}
        }
        Vec::new()
    }

    /// Left/Right on the Info panel look at the previous/next task.
    fn info_nav(&mut self, action: Action) -> Vec<Effect> {
        let n = self.tasks.len();
        if n == 0 {
            return Vec::new();
        }
        match action {
            Action::Left | Action::Up => self.selected = self.selected.saturating_sub(1),
            Action::Right | Action::Down => self.selected = (self.selected + 1).min(n - 1),
            Action::Accept => return vec![Effect::SwitchTo(self.tasks[self.selected].hwnd)],
            _ => {}
        }
        Vec::new()
    }

    fn adjust_master(&mut self, delta: f32) -> Vec<Effect> {
        match self.volume.master {
            Some(_) => self.adjust_row(0, delta),
            None => Vec::new(),
        }
    }

    fn adjust_row(&mut self, row: usize, delta: f32) -> Vec<Effect> {
        let Some(r) = self.volume_rows().into_iter().nth(row) else {
            return Vec::new();
        };
        // Round to whole steps so repeated presses land on 0 % and 100 %.
        let level = ((r.level() + delta) / VOLUME_STEP).round() * VOLUME_STEP;
        let level = level.clamp(0.0, 1.0);
        match r {
            VolumeRow::Master(_) => {
                if let Some(m) = &mut self.volume.master {
                    m.level = level;
                }
                vec![Effect::SetMasterVolume(level)]
            }
            VolumeRow::App { session, .. } => {
                for s in self.volume.apps.iter_mut().filter(|s| s.pid == session.pid) {
                    s.volume = level;
                }
                vec![Effect::SetAppVolume {
                    pid: session.pid,
                    level,
                }]
            }
        }
    }

    fn toggle_row_mute(&mut self, row: usize) -> Vec<Effect> {
        let Some(r) = self.volume_rows().into_iter().nth(row) else {
            return Vec::new();
        };
        let muted = !r.muted();
        match r {
            VolumeRow::Master(_) => {
                if let Some(m) = &mut self.volume.master {
                    m.muted = muted;
                }
                vec![Effect::SetMasterMuted(muted)]
            }
            VolumeRow::App { session, .. } => {
                for s in self.volume.apps.iter_mut().filter(|s| s.pid == session.pid) {
                    s.muted = muted;
                }
                vec![Effect::SetAppMuted {
                    pid: session.pid,
                    muted,
                }]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(hwnd: isize, pid: u32) -> Task {
        Task {
            hwnd,
            pid,
            title: format!("window {hwnd}"),
        }
    }

    fn app(pid: u32, volume: f32) -> AppSession {
        AppSession {
            pid,
            name: format!("app{pid}.exe"),
            volume,
            muted: false,
        }
    }

    fn model_with_tasks(n: isize, columns: usize) -> Model {
        let mut m = Model::new(columns);
        m.set_tasks((1..=n).map(|i| task(i, i as u32 * 10)).collect());
        m
    }

    #[test]
    fn grid_navigation_clamps_and_moves_by_rows() {
        // 7 tasks in rows of 3: [0 1 2] [3 4 5] [6]
        let mut m = model_with_tasks(7, 3);
        m.handle(Action::Left);
        assert_eq!(m.selected, 0);
        m.handle(Action::Down);
        assert_eq!(m.selected, 3);
        m.handle(Action::Right);
        m.handle(Action::Right);
        assert_eq!(m.selected, 5);
        // Down onto the short last row lands on its last tile.
        m.handle(Action::Down);
        assert_eq!(m.selected, 6);
        m.handle(Action::Down);
        assert_eq!(m.selected, 6);
        m.handle(Action::Right);
        assert_eq!(m.selected, 6);
        m.handle(Action::Up);
        assert_eq!(m.selected, 3);
        assert_eq!(m.handle(Action::Accept), vec![Effect::SwitchTo(4)]);
    }

    #[test]
    fn empty_task_list_does_nothing() {
        let mut m = Model::new(4);
        for a in [
            Action::Up,
            Action::Accept,
            Action::CloseTask,
            Action::KillTask,
        ] {
            assert!(m.handle(a).is_empty());
        }
        assert!(m.confirm.is_none());
        assert_eq!(m.handle(Action::Back), vec![Effect::Hide]);
    }

    #[test]
    fn tabs_wrap_both_ways() {
        let mut m = Model::new(4);
        m.handle(Action::PrevTab);
        assert_eq!(m.tab, Tab::Info);
        m.handle(Action::NextTab);
        assert_eq!(m.tab, Tab::Tasks);
        m.handle(Action::SelectTab(1));
        assert_eq!(m.tab, Tab::Volume);
        m.handle(Action::SelectTab(9));
        assert_eq!(m.tab, Tab::Volume);
    }

    #[test]
    fn kill_needs_confirmation() {
        let mut m = model_with_tasks(2, 4);
        m.handle(Action::Right);
        assert!(m.handle(Action::KillTask).is_empty());
        assert!(matches!(m.confirm, Some(Confirm::Kill { pid: 20, .. })));
        // Navigation is blocked while the question is open.
        assert!(m.handle(Action::Left).is_empty());
        assert_eq!(m.selected, 1);
        assert!(m.handle(Action::Back).is_empty());
        assert!(m.confirm.is_none());
        // Back with no question closes the overlay.
        assert_eq!(m.handle(Action::Back), vec![Effect::Hide]);

        m.handle(Action::KillTask);
        assert_eq!(m.handle(Action::Accept), vec![Effect::KillProcess(20)]);
        assert!(m.confirm.is_none());
    }

    #[test]
    fn close_posts_to_selected_window() {
        let mut m = model_with_tasks(3, 4);
        m.handle(Action::Right);
        assert_eq!(m.handle(Action::CloseTask), vec![Effect::CloseWindow(2)]);
    }

    #[test]
    fn set_tasks_keeps_selection_by_window() {
        let mut m = model_with_tasks(4, 4);
        m.selected = 2; // hwnd 3
        m.set_tasks(vec![task(3, 30), task(4, 40)]);
        assert_eq!(m.selected, 0);
        // Selected window gone: stay at the same index, clamped.
        m.selected = 1;
        m.set_tasks(vec![task(9, 90)]);
        assert_eq!(m.selected, 0);
        m.set_tasks(Vec::new());
        assert!(m.selected_task().is_none());
    }

    #[test]
    fn volume_rows_put_selected_app_first() {
        let mut m = model_with_tasks(2, 4);
        m.set_volume(Volume {
            master: Some(Master {
                level: 0.5,
                muted: false,
            }),
            apps: vec![app(10, 1.0), app(20, 0.8)],
            error: None,
        });
        m.selected = 1; // pid 20
        let rows = m.volume_rows();
        assert!(matches!(rows[0], VolumeRow::Master(_)));
        assert!(matches!(
            &rows[1],
            VolumeRow::App { session, selected_task: true } if session.pid == 20
        ));
        assert!(matches!(
            &rows[2],
            VolumeRow::App { session, selected_task: false } if session.pid == 10
        ));
    }

    #[test]
    fn volume_steps_round_and_clamp() {
        let mut m = model_with_tasks(1, 4);
        m.set_volume(Volume {
            master: Some(Master {
                level: 0.98,
                muted: false,
            }),
            apps: vec![app(10, 0.02)],
            error: None,
        });
        m.tab = Tab::Volume;
        assert_eq!(m.handle(Action::Right), vec![Effect::SetMasterVolume(1.0)]);
        assert_eq!(m.handle(Action::Right), vec![Effect::SetMasterVolume(1.0)]);
        m.handle(Action::Down);
        assert_eq!(
            m.handle(Action::Left),
            vec![Effect::SetAppVolume {
                pid: 10,
                level: 0.0
            }]
        );
        assert_eq!(m.volume.apps[0].volume, 0.0);
        assert_eq!(
            m.handle(Action::Accept),
            vec![Effect::SetAppMuted {
                pid: 10,
                muted: true
            }]
        );
        // Global volume keys always act on the master.
        assert_eq!(
            m.handle(Action::VolumeDown),
            vec![Effect::SetMasterVolume(0.95)]
        );
        assert_eq!(
            m.handle(Action::ToggleMute),
            vec![Effect::SetMasterMuted(true)]
        );
        assert!(m.volume.master.unwrap().muted);
    }

    #[test]
    fn volume_without_audio_is_inert() {
        let mut m = Model::new(4);
        m.tab = Tab::Volume;
        assert!(m.handle(Action::VolumeUp).is_empty());
        assert!(m.handle(Action::Right).is_empty());
        assert!(m.handle(Action::ToggleMute).is_empty());
    }

    #[test]
    fn info_panel_walks_tasks() {
        let mut m = model_with_tasks(3, 4);
        m.tab = Tab::Info;
        m.handle(Action::Right);
        m.handle(Action::Down);
        m.handle(Action::Right);
        assert_eq!(m.selected, 2);
        assert_eq!(m.handle(Action::Accept), vec![Effect::SwitchTo(3)]);
    }

    #[test]
    fn scroll_follows_selection() {
        let mut m = model_with_tasks(20, 4);
        m.selected = 13; // row 3
        m.ensure_visible(2);
        assert_eq!(m.scroll_row, 2);
        m.selected = 1;
        m.ensure_visible(2);
        assert_eq!(m.scroll_row, 0);
    }

    #[test]
    fn activate_task_by_index() {
        let mut m = model_with_tasks(3, 4);
        assert_eq!(m.handle(Action::ActivateTask(2)), vec![Effect::SwitchTo(3)]);
        assert_eq!(m.selected, 2);
        assert!(m.handle(Action::ActivateTask(7)).is_empty());
    }
}
