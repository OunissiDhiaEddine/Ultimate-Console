//! Hosts the Ultimate Console tab in Steam's Quick Access Menu
//! (`uc-steamui`), which replaces the full-screen overlay as the agent's
//! menu, and keeps Steam's DevTools switch file in step with the config.
//!
//! The agent decides policy here: what the tab may do (leave gaming mode
//! only in gaming mode) and whether the switch file should exist.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use uc_core::Config;
use uc_core::state::Mode;
use uc_steamui::flag::{self, Ensured};
use uc_steamui::{Handler, Host, HostStatus, Settings, SteamMenu, SystemHandler};

use crate::service::Command;
use crate::shared::{Shared, lock};

/// How long the overlay hotkey waits to hear that Steam opened the menu
/// before falling back to the full-screen overlay.
const OPEN_WAIT: Duration = Duration::from_millis(600);

pub struct SteamMenuHost {
    /// `None` if the bridge thread could not start.
    menu: Option<SteamMenu>,
    /// `Paths::steam_debug_marker_file`.
    marker: PathBuf,
}

impl SteamMenuHost {
    /// Starts the bridge (idle when turned off) and sets up the switch file.
    pub fn start(
        config: &Config,
        shared: Arc<Shared>,
        commands: Sender<Command>,
        marker: PathBuf,
    ) -> Self {
        let bp_title = config.steam.bp_window_title.clone();
        let host = AgentHost {
            shared: shared.clone(),
            commands: Mutex::new(commands),
        };
        let factory: uc_steamui::handler::HandlerFactory = Box::new(move || {
            Box::new(SystemHandler::new(Box::new(host), bp_title)) as Box<dyn Handler>
        });
        let menu = match SteamMenu::start(Settings::from(config), uc_core::VERSION, factory) {
            Ok(m) => Some(m),
            Err(e) => {
                shared.report_error(format!("Steam menu unavailable: {e}"));
                None
            }
        };
        let this = Self { menu, marker };
        this.sync_debug_file(config);
        this
    }

    /// Config changed (settings app or config.toml).
    pub fn set_config(&self, old: &Config, new: &Config) {
        if let Some(m) = &self.menu {
            m.set_settings(Settings::from(new));
        }
        if old.steam_menu != new.steam_menu || old.steam.exe_path != new.steam.exe_path {
            self.sync_debug_file(new);
        }
    }

    /// One line for the settings app.
    pub fn status_text(&self) -> String {
        match &self.menu {
            Some(m) => m.status().text(),
            None => "Unavailable (see the agent log)".into(),
        }
    }

    /// Opens the Quick Access Menu on our tab. False if it can't now (not
    /// injected, or Steam's menu store wasn't found).
    pub fn open(&self) -> bool {
        self.menu
            .as_ref()
            .is_some_and(|m| m.is_ready() && m.open(OPEN_WAIT))
    }

    /// Tells an open tab to re-read (after a volume key).
    pub fn refresh(&self) {
        if let Some(m) = &self.menu {
            m.refresh();
        }
    }

    /// Creates Steam's `.cef-enable-remote-debugging` when the menu is on
    /// and the agent manages it; removes it (only if the agent made it)
    /// otherwise.
    fn sync_debug_file(&self, config: &Config) {
        let menu = &config.steam_menu;
        if !(menu.enabled && menu.manage_debug_file) {
            if let Err(e) = flag::remove_ours(&self.marker) {
                tracing::warn!("could not remove Steam's DevTools switch: {e}");
            }
            return;
        }
        let exe = match uc_shell::steam::find_steam_exe(config.steam.exe_path.as_deref()) {
            Ok(exe) => exe,
            Err(e) => {
                tracing::info!("Steam menu: Steam not found yet ({e})");
                return;
            }
        };
        let Some(dir) = exe.parent() else { return };
        match flag::ensure(dir, &self.marker) {
            Ok(Ensured::Created) => {
                if uc_shell::steam::steam_running().unwrap_or(false) {
                    tracing::info!("Steam menu: restart Steam once so it opens its DevTools port");
                }
            }
            Ok(Ensured::AlreadyThere) => {}
            Err(e) => {
                tracing::warn!(dir = %dir.display(), "could not create Steam's DevTools switch: {e}")
            }
        }
    }
}

/// What the tab may ask of the agent.
struct AgentHost {
    shared: Arc<Shared>,
    /// `Sender` is not `Sync`; the bridge thread is the only user anyway.
    commands: Mutex<Sender<Command>>,
}

impl Host for AgentHost {
    fn status(&self) -> HostStatus {
        let s = self.shared.status();
        let config = self.shared.config();
        HostStatus {
            gaming: s.mode == Mode::Gaming,
            mode: match s.mode {
                Mode::Desktop => "Desktop",
                Mode::Entering => "Starting gaming mode",
                Mode::Gaming => "Gaming mode",
                Mode::Exiting => "Leaving gaming mode",
            }
            .into(),
            tv: config.tv.enabled.then(|| format!("{:?}", s.tv)),
        }
    }

    fn leave_gaming(&self) -> Result<(), String> {
        if self.shared.mode() != Mode::Gaming {
            return Err("not in gaming mode".into());
        }
        lock(&self.commands)
            .send(Command::Exit)
            .map_err(|_| "the agent is shutting down".to_owned())
    }
}
