//! The real [`Platform`]: `uc-display`, `uc-shell`, `uc-audio`, `uc-tv` and
//! `uc-power` behind the orchestrator's trait. Runs on the orchestrator thread.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use uc_audio::{AudioControl, CoreAudio};
use uc_core::config::{self, DisplayId, Steam};
use uc_core::ipc::TvPowerState;
use uc_display::Topology;
use uc_shell::{BigPictureWatcher, BpEvent, WindowMatcher, explorer, steam};
use uc_tv::{AdbKey, TvController, TvState};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
};

use crate::orchestrator::{EXPLORER_EXIT_TIMEOUT, Platform, Step};
use crate::service::Command;
use crate::shared::{Shared, lock};

/// How long `start_explorer` waits for the taskbar.
const EXPLORER_START_WAIT: Duration = Duration::from_secs(10);

/// The TV, shared with the window thread (suspend) and the IPC threads
/// (test buttons, pairing).
pub type SharedTv = Arc<Tv>;

pub struct Tv {
    /// `None` while the TV has no IP set.
    pub controller: Mutex<Option<TvController>>,
    key: Mutex<Option<Arc<AdbKey>>>,
    key_path: PathBuf,
}

impl Tv {
    pub fn new(key_path: PathBuf) -> Self {
        Self {
            controller: Mutex::new(None),
            key: Mutex::new(None),
            key_path,
        }
    }

    /// The PC's ADB key, loaded (or created) on first use.
    pub fn key(&self) -> Step<Arc<AdbKey>> {
        let mut key = lock(&self.key);
        if key.is_none() {
            let loaded = AdbKey::load_or_create(&self.key_path).map_err(|e| e.to_string())?;
            *key = Some(Arc::new(loaded));
        }
        Ok(key.clone().expect("set above"))
    }

    /// Follows `config.tv`: builds, updates or drops the controller.
    pub fn sync(&self, config: &config::Tv) -> Step {
        if config.ip.is_none() {
            *lock(&self.controller) = None;
            return Ok(());
        }
        let key = self.key()?;
        let mut controller = lock(&self.controller);
        match controller.as_mut() {
            Some(tv) => tv.set_config(config.clone()),
            None => *controller = Some(TvController::new(config.clone(), key)),
        }
        Ok(())
    }

    /// Runs `f` on the controller.
    pub fn with<T>(&self, f: impl FnOnce(&mut TvController) -> uc_tv::Result<T>) -> Step<T> {
        let mut guard = lock(&self.controller);
        let tv = guard
            .as_mut()
            .ok_or_else(|| "the TV's IP address is not set".to_string())?;
        f(tv).map_err(|e| e.to_string())
    }
}

pub struct RealPlatform {
    shared: Arc<Shared>,
    tv: SharedTv,
    commands: Sender<Command>,
    /// Created on first use: COM objects stay on the thread that made them.
    audio: Option<CoreAudio>,
    watcher: Option<BigPictureWatcher>,
    wake: Option<JoinHandle<Step>>,
}

impl RealPlatform {
    pub fn new(shared: Arc<Shared>, tv: SharedTv, commands: Sender<Command>) -> Self {
        Self {
            shared,
            tv,
            commands,
            audio: None,
            watcher: None,
            wake: None,
        }
    }

    fn audio(&mut self) -> Step<&CoreAudio> {
        if self.audio.is_none() {
            self.audio = Some(CoreAudio::new().map_err(|e| e.to_string())?);
        }
        Ok(self.audio.as_ref().expect("set above"))
    }

    /// Keeps the held ADB connection warm so the suspend handler can send
    /// the sleep command at once (plan §2.6). Gaming mode only.
    pub fn tv_keepalive(&mut self) {
        if let Err(e) = self.tv.with(|tv| tv.keepalive()) {
            tracing::debug!("TV keepalive: {e}");
        }
    }
}

/// `TvState` as the settings app shows it.
pub fn tv_power_state(state: &TvState) -> TvPowerState {
    match state {
        TvState::Awake | TvState::Dreaming => TvPowerState::Awake,
        TvState::Asleep | TvState::Dozing => TvPowerState::Asleep,
        TvState::Unreachable => TvPowerState::Unreachable,
        TvState::Unknown(_) => TvPowerState::Unknown,
    }
}

fn find_window(steam: &Steam) -> Option<HWND> {
    steam::find_big_picture(&WindowMatcher::from_config(steam))
        .ok()
        .flatten()
        .map(|w| HWND(w.hwnd as *mut _))
}

fn focus(hwnd: HWND) {
    // SAFETY: plain FFI calls; a stale handle just makes them fail. The agent
    // started Big Picture (or holds the foreground from the hotkey), so
    // Windows normally lets it take the foreground.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if !SetForegroundWindow(hwnd).as_bool() {
            tracing::debug!("SetForegroundWindow on Big Picture was refused");
        }
    }
}

impl Platform for RealPlatform {
    fn snapshot_topology(&mut self) -> Step<serde_json::Value> {
        uc_display::snapshot()
            .map(|t| t.to_value())
            .map_err(|e| e.to_string())
    }

    fn restore_topology(&mut self, saved: &serde_json::Value) -> Step {
        let topology = Topology::from_value(saved).map_err(|e| e.to_string())?;
        let outcome = uc_display::restore(&topology).map_err(|e| e.to_string())?;
        tracing::info!(?outcome, "display layout restored");
        Ok(())
    }

    fn wait_display(&mut self, target: &DisplayId, timeout: Duration) -> Step {
        uc_display::wait_available(target, timeout)
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn apply_display(&mut self, display: &config::Display) -> Step {
        let target = display
            .target
            .as_ref()
            .ok_or_else(|| "no TV display is configured".to_string())?;
        let outcome = uc_display::apply_single(target, display.mode.as_ref(), display.hdr)
            .map_err(|e| e.to_string())?;
        for w in &outcome.warnings {
            tracing::warn!("display: {w}");
        }
        Ok(())
    }

    fn default_audio(&mut self) -> Step<Option<String>> {
        let device = self.audio()?.default_output().map_err(|e| e.to_string())?;
        Ok(device.map(|d| d.id))
    }

    fn set_default_audio(&mut self, id: &str) -> Step {
        self.audio()?
            .set_default_output(id)
            .map_err(|e| e.to_string())
    }

    fn shell_running(&mut self) -> bool {
        explorer::shell_running()
    }

    fn exit_explorer(&mut self) -> Step {
        explorer::exit_explorer(EXPLORER_EXIT_TIMEOUT)
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn start_explorer(&mut self) -> Step {
        let report = explorer::start_explorer(EXPLORER_START_WAIT).map_err(|e| e.to_string())?;
        tracing::info!(?report, "Explorer start");
        Ok(())
    }

    fn launch_big_picture(&mut self, steam: &Steam) -> Step {
        steam::launch_big_picture(steam)
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn wait_big_picture(&mut self, steam: &Steam, timeout: Duration) -> Step<bool> {
        let found = steam::wait_for_big_picture(&WindowMatcher::from_config(steam), timeout)
            .map_err(|e| e.to_string())?;
        if let Some(w) = &found {
            focus(HWND(w.hwnd as *mut _));
        }
        Ok(found.is_some())
    }

    fn big_picture_open(&mut self, steam: &Steam) -> bool {
        find_window(steam).is_some()
    }

    fn focus_big_picture(&mut self, steam: &Steam) {
        if let Some(hwnd) = find_window(steam) {
            focus(hwnd);
        }
    }

    fn close_big_picture(&mut self, steam: &Steam) {
        if let Err(e) = steam::close_big_picture(steam) {
            tracing::warn!("could not close Big Picture: {e}");
        }
    }

    fn watch_big_picture(&mut self, steam: &Steam) -> Step {
        self.unwatch_big_picture();
        let commands = self.commands.clone();
        let watcher = BigPictureWatcher::start(
            WindowMatcher::from_config(steam),
            Duration::from_millis(steam.bp_close_debounce_ms.into()),
            move |event| {
                tracing::info!(?event, "Big Picture");
                if event == BpEvent::Closed {
                    let _ = commands.send(Command::BigPictureClosed);
                }
            },
        )
        .map_err(|e| e.to_string())?;
        self.watcher = Some(watcher);
        Ok(())
    }

    fn unwatch_big_picture(&mut self) {
        if let Some(w) = self.watcher.take() {
            w.stop();
        }
    }

    fn tv_wake_start(&mut self) {
        let _ = self.tv_wake_join();
        let tv = self.tv.clone();
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name("uc-tv-wake".into())
            .spawn(move || {
                let result = tv.with(|tv| tv.wake());
                shared.set_tv(match result {
                    Ok(_) => TvPowerState::Awake,
                    Err(_) => TvPowerState::Unreachable,
                });
                result.map(drop)
            });
        match spawned {
            Ok(handle) => self.wake = Some(handle),
            Err(e) => tracing::warn!("could not start the TV wake thread: {e}"),
        }
    }

    fn tv_wake_join(&mut self) -> Step {
        match self.wake.take() {
            None => Ok(()),
            Some(h) => h
                .join()
                .unwrap_or_else(|_| Err("the TV wake thread panicked".into())),
        }
    }

    fn tv_wake(&mut self) -> Step {
        let result = self.tv.with(|tv| tv.wake()).map(drop);
        self.shared.set_tv(if result.is_ok() {
            TvPowerState::Awake
        } else {
            TvPowerState::Unreachable
        });
        result
    }

    fn tv_sleep(&mut self) -> Step {
        self.tv.with(|tv| tv.sleep())?;
        self.shared.set_tv(TvPowerState::Asleep);
        Ok(())
    }

    fn wait_network(&mut self, timeout: Duration) -> bool {
        let target = self.shared.config().tv.ip.and_then(|ip| match ip {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        });
        match uc_power::wait_for_network(target, timeout) {
            Ok(wait) => {
                tracing::info!(?wait, "network wait after resume");
                wait.ready
            }
            Err(e) => {
                tracing::warn!("network wait failed: {e}");
                false
            }
        }
    }
}
