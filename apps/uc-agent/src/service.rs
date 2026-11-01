//! The orchestrator thread: runs every mode change, applies config changes
//! (from the settings app or from editing `config.toml` by hand), and keeps
//! the TV connection warm in gaming mode.
//!
//! Everything that can take seconds happens here, one thing at a time, so
//! the window thread (hotkeys, suspend) and the IPC threads never wait on it.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};

use uc_core::ipc::{ErrorCode, IpcEvent, IpcResponse};
use uc_core::state::Mode;
use uc_core::{Config, RuntimeState};

use crate::orchestrator::{ExitTrigger, Orchestrator, overlay_allowed, volume_keys_active};
use crate::overlay::OverlayHost;
use crate::platform::{RealPlatform, SharedTv};
use crate::shared::Shared;
use crate::updates::Updates;
use crate::window::WindowHandle;
use crate::{autostart, hotkeys};

/// How often the loop wakes when idle: config file check, keepalive.
const TICK: Duration = Duration::from_secs(2);
/// How often the held ADB connection is checked in gaming mode.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(30);

/// Work for the orchestrator thread.
pub enum Command {
    /// Gaming-mode hotkey, with the mode shown when it was pressed.
    Toggle {
        pressed_in: Mode,
    },
    /// Panic hotkey or settings app.
    Panic,
    Enter,
    Exit,
    BigPictureClosed,
    Resumed,
    /// Windows reported a display change (WM_DISPLAYCHANGE) in gaming mode.
    DisplayChanged,
    SetConfig(Box<Config>, Sender<IpcResponse>),
    /// Restore desktop, answer, then stop (uninstaller, settings app).
    Shutdown(Sender<IpcResponse>),
}

pub struct Service {
    orch: Orchestrator<RealPlatform>,
    shared: Arc<Shared>,
    tv: SharedTv,
    window: WindowHandle,
    overlay: Arc<OverlayHost>,
    updates: Arc<Updates>,
    config_file: PathBuf,
    config_stamp: Option<SystemTime>,
    last_keepalive: Instant,
    /// What the window was last told about the volume keys.
    volume_keys: bool,
}

fn modified(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Service {
    pub fn new(
        orch: Orchestrator<RealPlatform>,
        shared: Arc<Shared>,
        tv: SharedTv,
        window: WindowHandle,
        overlay: Arc<OverlayHost>,
        updates: Arc<Updates>,
        config_file: PathBuf,
    ) -> Self {
        let config_stamp = modified(&config_file);
        Self {
            orch,
            shared,
            tv,
            window,
            overlay,
            updates,
            config_file,
            config_stamp,
            last_keepalive: Instant::now(),
            volume_keys: false,
        }
    }

    /// Crash recovery first, then commands until shutdown.
    pub fn run(mut self, state_file: PathBuf, commands: Receiver<Command>) {
        let loaded = RuntimeState::load(&state_file).map_err(|e| e.to_string());
        self.guarded("recovery", |s| s.orch.recover(loaded));
        loop {
            match commands.recv_timeout(TICK) {
                Ok(Command::Shutdown(reply)) => {
                    self.guarded("shutdown", |s| s.orch.restore_for_shutdown());
                    tracing::info!("shutting down");
                    // The IPC thread writes the answer, then closes the window.
                    if reply.send(IpcResponse::Ok).is_err() {
                        self.window.close();
                    }
                    return;
                }
                Ok(cmd) => self.guarded("command", |s| s.handle(cmd)),
                Err(RecvTimeoutError::Timeout) => {
                    self.guarded("tick", Self::tick);
                    if self.apply_update() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if !overlay_allowed(self.orch.mode(), self.orch.config()) {
                self.overlay.hide();
            }
            let volume_keys = volume_keys_active(self.orch.mode(), self.orch.config());
            if volume_keys != self.volume_keys {
                self.volume_keys = volume_keys;
                self.window.set_volume_keys(volume_keys);
            }
        }
    }

    /// Runs `f`, logging a panic instead of losing the orchestrator thread.
    fn guarded(&mut self, what: &str, f: impl FnOnce(&mut Self)) {
        if catch_unwind(AssertUnwindSafe(|| f(self))).is_err() {
            self.shared
                .report_error(format!("internal error during {what}; see the log"));
        }
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Toggle { pressed_in } => {
                match toggle_action(pressed_in, self.orch.mode()) {
                    Some(Mode::Gaming) => self.enter(),
                    Some(_) => self.exit(),
                    None => tracing::info!(
                        ?pressed_in,
                        now = ?self.orch.mode(),
                        "toggle ignored: the mode changed since it was pressed"
                    ),
                }
                self.shared.toggle_handled();
            }
            Command::Enter => self.enter(),
            Command::Exit => self.exit(),
            Command::Panic => {
                self.orch.panic_restore();
                self.shared.clear_abort();
            }
            Command::BigPictureClosed => self.orch.on_big_picture_closed(),
            Command::Resumed => self.orch.on_resume(),
            Command::DisplayChanged => self.orch.on_display_changed(),
            Command::SetConfig(config, reply) => {
                let _ = reply.send(self.set_config(*config));
            }
            Command::Shutdown(_) => unreachable!("handled in run"),
        }
    }

    fn enter(&mut self) {
        if let Err(e) = self.orch.enter() {
            tracing::info!("enter refused: {e:?}");
        }
    }

    fn exit(&mut self) {
        if let Err(e) = self.orch.exit(ExitTrigger::Request) {
            tracing::info!("exit refused: {e:?}");
        }
    }

    fn tick(&mut self) {
        let stamp = modified(&self.config_file);
        if stamp != self.config_stamp {
            self.config_stamp = stamp;
            self.reload_from_file();
        }
        if self.orch.mode() == Mode::Gaming
            && self.orch.config().tv.enabled
            && self.last_keepalive.elapsed() >= KEEPALIVE_EVERY
        {
            self.last_keepalive = Instant::now();
            self.orch.platform_mut().tv_keepalive();
        }
        self.orch.recheck_big_picture();
        self.updates.tick();
    }

    /// Hands a downloaded update to Velopack and closes the agent, only in
    /// desktop mode. `Update.exe` applies it once this process has exited and
    /// starts the new agent. Returns true when the agent is stopping.
    fn apply_update(&mut self) -> bool {
        let Some(updater) = self.updates.take_ready(self.orch.mode()) else {
            return false;
        };
        let Some(asset) = updater.downloaded() else {
            return false;
        };
        match updater.apply_on_exit(&asset) {
            Ok(()) => {
                tracing::info!(version = %asset.Version, "exiting to apply the update");
                self.window.close();
                true
            }
            Err(e) => {
                self.shared
                    .report_error(format!("could not install update {}: {e}", asset.Version));
                false
            }
        }
    }

    /// `SetConfig` from the settings app: validate, register the hotkeys,
    /// save, apply.
    fn set_config(&mut self, new: Config) -> IpcResponse {
        if let Err(e) = new.validate() {
            return IpcResponse::error(ErrorCode::InvalidConfig, e.to_string());
        }
        let old = self.orch.config().clone();
        let errors = self.window.apply_hotkeys(&new.hotkeys);
        if !errors.is_empty() {
            self.window.apply_hotkeys(&old.hotkeys);
            let msg: Vec<_> = errors
                .iter()
                .map(|(a, e)| format!("hotkeys.{}: {e}", hotkeys::name(*a)))
                .collect();
            return IpcResponse::error(ErrorCode::InvalidConfig, msg.join("\n"));
        }
        if let Err(e) = new.save(&self.config_file) {
            self.window.apply_hotkeys(&old.hotkeys);
            return IpcResponse::error(ErrorCode::Failed, e.to_string());
        }
        self.config_stamp = modified(&self.config_file);
        self.apply_config(&old, new);
        IpcResponse::Ok
    }

    /// `config.toml` changed on disk. A broken file is reported and ignored.
    fn reload_from_file(&mut self) {
        let new = match Config::load(&self.config_file) {
            Ok(c) => c,
            Err(e) => {
                self.shared
                    .report_error(format!("config.toml not reloaded: {e}"));
                return;
            }
        };
        if &new == self.orch.config() {
            return;
        }
        if let Err(e) = new.validate() {
            self.shared
                .report_error(format!("config.toml not reloaded: {e}"));
            return;
        }
        tracing::info!("config.toml changed; reloading");
        for (action, e) in self.window.apply_hotkeys(&new.hotkeys) {
            self.shared.report_error(format!(
                "hotkeys.{}: {e} (keeping the previous shortcut)",
                hotkeys::name(action)
            ));
        }
        let old = self.orch.config().clone();
        self.apply_config(&old, new);
    }

    fn apply_config(&mut self, old: &Config, new: Config) {
        if new.tv != old.tv {
            if let Err(e) = self.tv.sync(&new.tv) {
                self.shared.report_error(format!("TV: {e}"));
            }
            self.shared.sync_tv_status(&new);
        }
        if new.general.start_with_windows != old.general.start_with_windows
            && let Err(e) = autostart::set(new.general.start_with_windows)
        {
            self.shared
                .report_error(format!("could not change start with Windows: {e}"));
        }
        self.overlay.set_config(&new);
        self.shared.set_config(new.clone());
        self.orch.set_config(new);
        self.shared.emit(IpcEvent::ConfigReloaded);
    }
}

/// What a gaming-mode press does once the orchestrator gets to it: the mode
/// to go to, or `None` if the mode changed since the press. A press while
/// gaming mode is leaving on its own (Big Picture closed) must not start it
/// again a moment later, and a press mid-change does nothing.
pub fn toggle_action(pressed_in: Mode, now: Mode) -> Option<Mode> {
    match (pressed_in, now) {
        (Mode::Desktop, Mode::Desktop) => Some(Mode::Gaming),
        (Mode::Gaming, Mode::Gaming) => Some(Mode::Desktop),
        _ => None,
    }
}

#[cfg(test)]
mod toggle_tests {
    use super::*;

    #[test]
    fn toggle_acts_only_if_the_mode_is_unchanged() {
        assert_eq!(
            toggle_action(Mode::Desktop, Mode::Desktop),
            Some(Mode::Gaming)
        );
        assert_eq!(
            toggle_action(Mode::Gaming, Mode::Gaming),
            Some(Mode::Desktop)
        );
        // Pressed while gaming mode was leaving by itself: no re-entry.
        assert_eq!(toggle_action(Mode::Gaming, Mode::Desktop), None);
        assert_eq!(toggle_action(Mode::Exiting, Mode::Desktop), None);
        assert_eq!(toggle_action(Mode::Entering, Mode::Gaming), None);
        assert_eq!(toggle_action(Mode::Desktop, Mode::Gaming), None);
    }
}
