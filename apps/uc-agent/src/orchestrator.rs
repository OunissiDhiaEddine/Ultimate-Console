//! The gaming-mode state machine (plan §3.1) and the gaming-only power
//! policy (§2.7).
//!
//! ```text
//! Desktop ──enter──► Entering ──ok──► Gaming ──BP closed / hotkey──► Exiting ──► Desktop
//!                       │fail                                            ▲
//!                       └──────────rollback──────────────────────────────┘
//!                     panic (any state): unconditional restore ─────────►┘
//! ```
//!
//! Every destructive step is recorded in `state.json` *before* it runs, and
//! every undo is idempotent, so a rollback, an exit, a panic and crash
//! recovery all do the same thing: undo the recorded steps, newest first.
//!
//! All OS work goes through [`Platform`], so this file is plain Rust and is
//! tested with a fake platform on every OS.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use uc_core::config::{self, DisplayId, Steam};
use uc_core::state::{EnterStep, Mode};
use uc_core::{Config, RuntimeState};

use crate::shared::Shared;

/// Result of one OS step. The error is a sentence for the log and the user.
pub type Step<T = ()> = Result<T, String>;

/// How long `exit_explorer` may wait for the shell to go (plan §2.3).
pub const EXPLORER_EXIT_TIMEOUT: Duration = Duration::from_secs(12);
/// After the watcher reports Big Picture closed, how long it may take to come
/// back (a game exiting, Steam recreating its window) before gaming mode ends.
pub const BP_REAPPEAR_GRACE: Duration = Duration::from_secs(6);

/// Everything the state machine needs from the OS. The real one is in
/// `platform.rs`; tests use a fake.
pub trait Platform {
    /// Current display layout, serialised for `state.json`.
    fn snapshot_topology(&mut self) -> Step<serde_json::Value>;
    fn restore_topology(&mut self, saved: &serde_json::Value) -> Step;
    /// Waits until the TV is available as a display.
    fn wait_display(&mut self, target: &DisplayId, timeout: Duration) -> Step;
    /// Only the TV active, with the configured mode and HDR.
    fn apply_display(&mut self, display: &config::Display) -> Step;

    fn default_audio(&mut self) -> Step<Option<String>>;
    fn set_default_audio(&mut self, id: &str) -> Step;

    fn shell_running(&mut self) -> bool;
    fn exit_explorer(&mut self) -> Step;
    fn start_explorer(&mut self) -> Step;

    fn launch_big_picture(&mut self, steam: &Steam) -> Step;
    /// Waits for the Big Picture window and brings it to the front. False if
    /// it did not show up in time.
    fn wait_big_picture(&mut self, steam: &Steam, timeout: Duration) -> Step<bool>;
    fn big_picture_open(&mut self, steam: &Steam) -> bool;
    /// After the watcher reported Big Picture closed: true only if it stays
    /// gone for `grace` and no Steam game is running. A game starting or
    /// exiting can hide or recreate the Big Picture window for a while.
    fn big_picture_gone(&mut self, steam: &Steam, grace: Duration) -> bool;
    fn focus_big_picture(&mut self, steam: &Steam);
    /// Leaves Big Picture; Steam keeps running on the desktop.
    fn close_big_picture(&mut self, steam: &Steam);
    /// Starts reporting when Big Picture closes (debounced, plan §2.8).
    fn watch_big_picture(&mut self, steam: &Steam) -> Step;
    fn unwatch_big_picture(&mut self);

    /// Starts waking the TV in the background (WoL + ADB, the slowest step).
    fn tv_wake_start(&mut self);
    /// Waits for a wake started by [`Platform::tv_wake_start`], if any.
    fn tv_wake_join(&mut self) -> Step;
    fn tv_wake(&mut self) -> Step;
    fn tv_sleep(&mut self) -> Step;
    /// Waits for a usable network after resume. True if it came up.
    fn wait_network(&mut self, timeout: Duration) -> bool;
}

/// Why gaming mode is being left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitTrigger {
    /// Hotkey or settings app: Big Picture is still open and gets closed.
    Request,
    /// Big Picture went away by itself; nothing to close.
    BigPictureClosed,
}

/// Why a request was refused before anything was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Another mode change is in progress, or the mode is wrong for it.
    Busy(String),
    /// A needed setting is missing.
    NotConfigured(String),
}

/// Sleep the TV on suspend? Only in service-triggered gaming mode (R8).
pub fn tv_sleep_on_suspend(mode: Mode, config: &Config) -> bool {
    mode == Mode::Gaming && config.tv.enabled && config.power.tv_sleep_on_suspend
}

/// Wake the TV on resume? Same rule as [`tv_sleep_on_suspend`].
pub fn tv_wake_on_resume(mode: Mode, config: &Config) -> bool {
    mode == Mode::Gaming && config.tv.enabled && config.power.tv_wake_on_resume
}

/// May the overlay hotkey show the overlay now?
pub fn overlay_allowed(mode: Mode, config: &Config) -> bool {
    mode == Mode::Gaming || config.overlay.in_desktop_mode
}

/// Whether the agent takes over the volume keys: only in gaming mode, where
/// Explorer (which normally handles them) isn't running.
pub fn volume_keys_active(mode: Mode, config: &Config) -> bool {
    mode == Mode::Gaming && config.overlay.volume_keys
}

pub struct Orchestrator<P: Platform> {
    platform: P,
    config: Config,
    state: RuntimeState,
    state_file: PathBuf,
    shared: Arc<Shared>,
}

impl<P: Platform> Orchestrator<P> {
    pub fn new(platform: P, config: Config, state_file: PathBuf, shared: Arc<Shared>) -> Self {
        Self {
            platform,
            config,
            state: RuntimeState::default(),
            state_file,
            shared,
        }
    }

    pub fn mode(&self) -> Mode {
        self.state.mode
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn platform_mut(&mut self) -> &mut P {
        &mut self.platform
    }

    /// Takes a new config. It applies from the next step on; a running gaming
    /// session keeps its display and audio until it exits.
    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    /// Startup: picks up after a crash or an unclean shutdown (plan §2.3).
    /// `loaded` is `state.json` as read from disk.
    pub fn recover(&mut self, loaded: Result<RuntimeState, String>) {
        let state = match loaded {
            Ok(s) if !s.needs_recovery() => return,
            Ok(s) => s,
            Err(e) => {
                // Nothing to go on but the shell: bring Explorer back.
                tracing::warn!("state file unreadable ({e}); restoring the desktop");
                RuntimeState {
                    mode: Mode::Exiting,
                    steps_done: vec![EnterStep::ExplorerExited],
                    ..RuntimeState::default()
                }
            }
        };
        let adopt = state.mode == Mode::Gaming
            && !self.platform.shell_running()
            && self.platform.big_picture_open(&self.config.steam);
        self.state = state;
        if adopt {
            tracing::info!("agent restarted during gaming mode; carrying on");
            self.shared.set_mode(Mode::Gaming);
            if let Err(e) = self.platform.watch_big_picture(&self.config.steam) {
                self.shared
                    .report_error(format!("cannot watch Big Picture: {e}"));
            }
            return;
        }
        tracing::warn!(mode = ?self.state.mode, steps = ?self.state.steps_done, "previous run did not end in desktop mode; restoring");
        self.shared.set_mode(self.state.mode);
        let errors = self.undo(true);
        self.finish_restore(errors, "Restore after restart");
    }

    /// Enters gaming mode (plan §3.1). On failure everything done so far is
    /// undone and the error is reported. Returns `Err` only for refusals.
    pub fn enter(&mut self) -> Result<(), Refused> {
        if self.state.mode != Mode::Desktop {
            return Err(Refused::Busy(format!(
                "cannot enter gaming mode while {:?}",
                self.state.mode
            )));
        }
        let Some(target) = self.config.display.target.clone() else {
            let msg = "pick the TV in the settings app (Display page) first";
            self.shared.report_error(format!("Gaming mode: {msg}"));
            return Err(Refused::NotConfigured(msg.into()));
        };
        self.shared.clear_abort();
        self.state = RuntimeState::entering();
        self.shared.set_mode(Mode::Entering);
        tracing::info!("entering gaming mode");

        match self.enter_steps(&target) {
            Ok(()) => {
                self.state.mode = Mode::Gaming;
                if let Err(e) = self.save() {
                    tracing::warn!("{e}");
                }
                self.shared.set_mode(Mode::Gaming);
                if let Err(e) = self.platform.watch_big_picture(&self.config.steam) {
                    // Gaming mode still works; leaving needs the hotkey.
                    self.shared.report_error(format!(
                        "cannot watch Big Picture ({e}); use the hotkey to leave gaming mode"
                    ));
                }
                tracing::info!("gaming mode on");
            }
            Err(e) => {
                tracing::warn!("entering gaming mode failed: {e}; rolling back");
                self.state.mode = Mode::Exiting;
                let _ = self.save();
                self.shared.set_mode(Mode::Exiting);
                let mut errors = self.undo(true);
                errors.insert(0, e);
                self.finish_restore(errors, "Gaming mode cancelled");
            }
        }
        Ok(())
    }

    fn enter_steps(&mut self, target: &DisplayId) -> Step {
        let cfg = self.config.clone();

        // 1. Save what we are about to change.
        let topology = self
            .platform
            .snapshot_topology()
            .map_err(|e| format!("could not read the display layout: {e}"))?;
        self.state.saved_topology = Some(topology);
        let audio_target = cfg
            .audio
            .device_id
            .clone()
            .filter(|_| cfg.audio.switch_default);
        if audio_target.is_some() {
            match self.platform.default_audio() {
                Ok(id) => self.state.saved_audio_device = id,
                Err(e) => tracing::warn!("could not read the default audio device: {e}"),
            }
        }
        self.save()?;

        // 2. TV first: it is the slowest.
        if cfg.tv.enabled {
            self.mark(EnterStep::TvWoken)?;
            self.platform.tv_wake_start();
        }

        // 3. Explorer.
        self.check_abort()?;
        self.mark(EnterStep::ExplorerExited)?;
        self.platform.exit_explorer()?;

        // 4. Display.
        self.check_abort()?;
        let timeout = Duration::from_secs(cfg.display.wait_timeout_secs.into());
        self.platform
            .wait_display(target, timeout)
            .map_err(|e| format!("the TV never showed up as a display: {e}"))?;
        self.check_abort()?;
        self.mark(EnterStep::TopologyApplied)?;
        self.platform
            .apply_display(&cfg.display)
            .map_err(|e| format!("could not switch to the TV: {e}"))?;

        // 5. Audio (optional, so a failure is only a warning).
        if let Some(id) = audio_target {
            self.check_abort()?;
            self.mark(EnterStep::AudioSwitched)?;
            if let Err(e) = self.platform.set_default_audio(&id) {
                self.shared
                    .report_error(format!("could not switch the audio device: {e}"));
            }
        }

        // 6. Big Picture.
        self.check_abort()?;
        self.mark(EnterStep::BigPictureLaunched)?;
        self.platform
            .launch_big_picture(&cfg.steam)
            .map_err(|e| format!("could not start Big Picture: {e}"))?;
        let timeout = Duration::from_secs(cfg.steam.bp_launch_timeout_secs.into());
        if !self.platform.wait_big_picture(&cfg.steam, timeout)? {
            return Err(format!("Big Picture did not open within {timeout:?}"));
        }

        // 7. The TV wake should be done by now; a failure is not fatal since
        // the display is already up.
        if cfg.tv.enabled
            && let Err(e) = self.platform.tv_wake_join()
        {
            tracing::warn!("TV wake reported: {e}");
        }
        self.check_abort()
    }

    /// Leaves gaming mode. Refused unless in gaming mode.
    pub fn exit(&mut self, trigger: ExitTrigger) -> Result<(), Refused> {
        if self.state.mode != Mode::Gaming {
            return Err(Refused::Busy(format!(
                "not in gaming mode ({:?})",
                self.state.mode
            )));
        }
        tracing::info!(?trigger, "leaving gaming mode");
        self.state.mode = Mode::Exiting;
        let _ = self.save();
        self.shared.set_mode(Mode::Exiting);
        self.platform.unwatch_big_picture();
        let errors = self.undo(trigger == ExitTrigger::Request);
        self.finish_restore(errors, "Leaving gaming mode");
        Ok(())
    }

    /// Panic hotkey: restores whatever was recorded, and always makes sure
    /// Explorer is running, whatever the mode.
    pub fn panic_restore(&mut self) {
        tracing::warn!(mode = ?self.state.mode, "panic restore");
        self.platform.unwatch_big_picture();
        if self.state.mode == Mode::Desktop {
            self.state = RuntimeState::default();
        }
        self.state.mode = Mode::Exiting;
        self.state.mark_done(EnterStep::ExplorerExited);
        let _ = self.save();
        self.shared.set_mode(Mode::Exiting);
        let errors = self.undo(true);
        self.finish_restore(errors, "Panic restore");
    }

    /// Leaves whatever mode we are in, for shutdown and uninstall.
    pub fn restore_for_shutdown(&mut self) {
        match self.state.mode {
            Mode::Desktop => {}
            Mode::Gaming => {
                let _ = self.exit(ExitTrigger::Request);
            }
            Mode::Entering | Mode::Exiting => self.panic_restore(),
        }
    }

    /// Big Picture has been gone for the whole debounce.
    pub fn on_big_picture_closed(&mut self) {
        if self.state.mode != Mode::Gaming {
            return;
        }
        let steam = self.config.steam.clone();
        if !self.platform.big_picture_gone(&steam, BP_REAPPEAR_GRACE) {
            // The watcher keeps running and reports the next close.
            tracing::info!("Big Picture is back or a game is running; staying in gaming mode");
            return;
        }
        let _ = self.exit(ExitTrigger::BigPictureClosed);
    }

    /// After resume in gaming mode: network, TV, display, focus (plan §2.7).
    pub fn on_resume(&mut self) {
        if self.state.mode != Mode::Gaming {
            return;
        }
        let cfg = self.config.clone();
        tracing::info!("resumed in gaming mode");
        if tv_wake_on_resume(self.state.mode, &cfg) {
            let wait = Duration::from_secs(cfg.power.network_wait_secs.into());
            if !self.platform.wait_network(wait) {
                tracing::warn!(
                    ?wait,
                    "network not ready after resume; trying the TV anyway"
                );
            }
            if let Err(e) = self.platform.tv_wake() {
                self.shared
                    .report_error(format!("could not wake the TV after resume: {e}"));
            }
        }
        // Windows sometimes turns the other monitors back on after resume.
        if let Some(target) = &cfg.display.target {
            let timeout = Duration::from_secs(cfg.display.wait_timeout_secs.into());
            let result = self
                .platform
                .wait_display(target, timeout)
                .and_then(|()| self.platform.apply_display(&cfg.display));
            if let Err(e) = result {
                self.shared
                    .report_error(format!("could not switch back to the TV after resume: {e}"));
            }
        }
        self.platform.focus_big_picture(&cfg.steam);
    }

    /// Undoes the recorded steps, newest first. Best effort: every undo is
    /// tried, and the errors are returned.
    fn undo(&mut self, close_big_picture: bool) -> Vec<String> {
        let mut errors = Vec::new();
        let steps: Vec<_> = self.state.undo_order().collect();
        for step in steps {
            let result = match step {
                EnterStep::BigPictureLaunched => {
                    if close_big_picture {
                        self.platform.close_big_picture(&self.config.steam);
                    }
                    Ok(())
                }
                EnterStep::AudioSwitched => match self.state.saved_audio_device.clone() {
                    Some(id) => self
                        .platform
                        .set_default_audio(&id)
                        .map_err(|e| format!("could not restore the audio device: {e}")),
                    None => Ok(()),
                },
                EnterStep::TopologyApplied => match self.state.saved_topology.clone() {
                    Some(t) => self
                        .platform
                        .restore_topology(&t)
                        .map_err(|e| format!("could not restore the display layout: {e}")),
                    None => Ok(()),
                },
                EnterStep::ExplorerExited => self
                    .platform
                    .start_explorer()
                    .map_err(|e| format!("could not start Explorer: {e}")),
                EnterStep::TvWoken => {
                    let _ = self.platform.tv_wake_join();
                    self.platform
                        .tv_sleep()
                        .map_err(|e| format!("could not put the TV to sleep: {e}"))
                }
            };
            match result {
                Ok(()) => tracing::info!(?step, "undone"),
                Err(e) => {
                    tracing::warn!(?step, "{e}");
                    errors.push(e);
                }
            }
        }
        errors
    }

    /// Back to desktop mode after an undo, whatever happened.
    fn finish_restore(&mut self, errors: Vec<String>, what: &str) {
        self.state = RuntimeState::default();
        if let Err(e) = RuntimeState::clear(&self.state_file) {
            tracing::warn!("could not delete the state file: {e}");
        }
        self.shared.set_mode(Mode::Desktop);
        if !errors.is_empty() {
            self.shared
                .report_error(format!("{what}: {}", errors.join("; ")));
        }
    }

    fn mark(&mut self, step: EnterStep) -> Step {
        self.state.mark_done(step);
        self.save()
    }

    fn save(&self) -> Step {
        self.state
            .save(&self.state_file)
            .map_err(|e| format!("could not write the state file: {e}"))
    }

    fn check_abort(&self) -> Step {
        if self.shared.abort_requested() {
            Err("cancelled by the panic hotkey".into())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashSet;
    use uc_core::ipc::IpcEvent;

    /// Records every call; `fail` makes the named call fail.
    #[derive(Default)]
    pub struct Fake {
        pub calls: Vec<String>,
        pub fail: HashSet<&'static str>,
        pub shell: bool,
        pub bp_open: bool,
        /// Set the shared abort flag when this call happens.
        pub abort_on: Option<(&'static str, Arc<Shared>)>,
    }

    impl Fake {
        fn call(&mut self, name: &'static str) -> Step {
            self.calls.push(name.to_string());
            if let Some((on, shared)) = &self.abort_on
                && *on == name
            {
                shared.request_abort();
            }
            if self.fail.contains(name) {
                Err(format!("{name} failed"))
            } else {
                Ok(())
            }
        }
    }

    impl Platform for Fake {
        fn snapshot_topology(&mut self) -> Step<serde_json::Value> {
            self.call("snapshot_topology")?;
            Ok(serde_json::json!({"layout": "desk"}))
        }
        fn restore_topology(&mut self, saved: &serde_json::Value) -> Step {
            assert_eq!(saved, &serde_json::json!({"layout": "desk"}));
            self.call("restore_topology")
        }
        fn wait_display(&mut self, _: &DisplayId, _: Duration) -> Step {
            self.call("wait_display")
        }
        fn apply_display(&mut self, _: &config::Display) -> Step {
            self.call("apply_display")
        }
        fn default_audio(&mut self) -> Step<Option<String>> {
            self.call("default_audio")?;
            Ok(Some("speakers".into()))
        }
        fn set_default_audio(&mut self, id: &str) -> Step {
            self.calls.push(format!("set_default_audio {id}"));
            Ok(())
        }
        fn shell_running(&mut self) -> bool {
            self.shell
        }
        fn exit_explorer(&mut self) -> Step {
            self.call("exit_explorer")
        }
        fn start_explorer(&mut self) -> Step {
            self.call("start_explorer")
        }
        fn launch_big_picture(&mut self, _: &Steam) -> Step {
            self.call("launch_big_picture")
        }
        fn wait_big_picture(&mut self, _: &Steam, _: Duration) -> Step<bool> {
            self.call("wait_big_picture")?;
            Ok(!self.fail.contains("bp_never_opens"))
        }
        fn big_picture_open(&mut self, _: &Steam) -> bool {
            self.bp_open
        }
        fn big_picture_gone(&mut self, _: &Steam, _: Duration) -> bool {
            self.calls.push("big_picture_gone".into());
            !self.bp_open
        }
        fn focus_big_picture(&mut self, _: &Steam) {
            let _ = self.call("focus_big_picture");
        }
        fn close_big_picture(&mut self, _: &Steam) {
            let _ = self.call("close_big_picture");
        }
        fn watch_big_picture(&mut self, _: &Steam) -> Step {
            self.call("watch_big_picture")
        }
        fn unwatch_big_picture(&mut self) {
            let _ = self.call("unwatch_big_picture");
        }
        fn tv_wake_start(&mut self) {
            let _ = self.call("tv_wake_start");
        }
        fn tv_wake_join(&mut self) -> Step {
            self.call("tv_wake_join")
        }
        fn tv_wake(&mut self) -> Step {
            self.call("tv_wake")
        }
        fn tv_sleep(&mut self) -> Step {
            self.call("tv_sleep")
        }
        fn wait_network(&mut self, _: Duration) -> bool {
            let _ = self.call("wait_network");
            true
        }
    }

    struct Rig {
        orch: Orchestrator<Fake>,
        shared: Arc<Shared>,
        _dir: tempfile::TempDir,
        state_file: PathBuf,
    }

    fn configured() -> Config {
        let mut c = Config::default();
        c.display.target = Some(DisplayId {
            device_path: "tv".into(),
            ..DisplayId::default()
        });
        c.tv.enabled = true;
        c.tv.ip = Some("192.168.1.50".parse().unwrap());
        c
    }

    fn rig(config: Config, fake: Fake) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join("state.json");
        let shared = Arc::new(Shared::new(config.clone()));
        let orch = Orchestrator::new(fake, config, state_file.clone(), shared.clone());
        Rig {
            orch,
            shared,
            _dir: dir,
            state_file,
        }
    }

    impl Rig {
        fn calls(&mut self) -> Vec<String> {
            std::mem::take(&mut self.orch.platform_mut().calls)
        }
        fn on_disk(&self) -> RuntimeState {
            RuntimeState::load(&self.state_file).unwrap()
        }
    }

    const ENTER: [&str; 9] = [
        "snapshot_topology",
        "tv_wake_start",
        "exit_explorer",
        "wait_display",
        "apply_display",
        "launch_big_picture",
        "wait_big_picture",
        "tv_wake_join",
        "watch_big_picture",
    ];

    #[test]
    fn enter_then_exit_by_hotkey() {
        let mut r = rig(configured(), Fake::default());
        let events = r.shared.subscribe();
        r.orch.enter().unwrap();
        assert_eq!(r.orch.mode(), Mode::Gaming);
        assert_eq!(r.calls(), ENTER);
        let disk = r.on_disk();
        assert_eq!(disk.mode, Mode::Gaming);
        assert_eq!(
            disk.steps_done,
            [
                EnterStep::TvWoken,
                EnterStep::ExplorerExited,
                EnterStep::TopologyApplied,
                EnterStep::BigPictureLaunched
            ]
        );
        assert!(disk.saved_topology.is_some());

        r.orch.exit(ExitTrigger::Request).unwrap();
        assert_eq!(
            r.calls(),
            [
                "unwatch_big_picture",
                "close_big_picture",
                "restore_topology",
                "start_explorer",
                "tv_wake_join",
                "tv_sleep"
            ]
        );
        assert_eq!(r.orch.mode(), Mode::Desktop);
        assert!(!r.state_file.exists());
        let modes: Vec<_> = events
            .try_iter()
            .filter_map(|e| match e {
                IpcEvent::ModeChanged { mode } => Some(mode),
                _ => None,
            })
            .collect();
        assert_eq!(
            modes,
            [Mode::Entering, Mode::Gaming, Mode::Exiting, Mode::Desktop]
        );
        assert_eq!(r.shared.status().last_error, None);
    }

    #[test]
    fn big_picture_closing_exits_without_closing_it_again() {
        let mut r = rig(configured(), Fake::default());
        r.orch.enter().unwrap();
        r.calls();
        r.orch.on_big_picture_closed();
        let calls = r.calls();
        assert!(
            !calls.contains(&"close_big_picture".to_string()),
            "{calls:?}"
        );
        assert!(calls.contains(&"start_explorer".to_string()));
        assert_eq!(r.orch.mode(), Mode::Desktop);
        // Ignored in desktop mode.
        r.orch.on_big_picture_closed();
        assert!(r.calls().is_empty());
    }

    #[test]
    fn big_picture_coming_back_keeps_gaming_mode() {
        let mut r = rig(configured(), Fake::default());
        r.orch.enter().unwrap();
        r.calls();
        r.orch.platform_mut().bp_open = true;
        r.orch.on_big_picture_closed();
        assert_eq!(r.calls(), vec!["big_picture_gone".to_string()]);
        assert_eq!(r.orch.mode(), Mode::Gaming);
    }

    #[test]
    fn audio_is_switched_and_restored() {
        let mut c = configured();
        c.audio.switch_default = true;
        c.audio.device_id = Some("tv-hdmi".into());
        let mut r = rig(c, Fake::default());
        r.orch.enter().unwrap();
        let calls = r.calls();
        assert!(calls.contains(&"default_audio".to_string()));
        assert!(calls.contains(&"set_default_audio tv-hdmi".to_string()));
        assert_eq!(r.on_disk().saved_audio_device.as_deref(), Some("speakers"));
        r.orch.exit(ExitTrigger::Request).unwrap();
        assert!(
            r.calls()
                .contains(&"set_default_audio speakers".to_string())
        );
    }

    #[test]
    fn display_failure_rolls_back_what_was_done() {
        let fake = Fake {
            fail: ["wait_display"].into(),
            ..Fake::default()
        };
        let mut r = rig(configured(), fake);
        r.orch.enter().unwrap();
        assert_eq!(
            r.calls(),
            [
                "snapshot_topology",
                "tv_wake_start",
                "exit_explorer",
                "wait_display",
                // rollback: topology was never applied, so it isn't restored
                "start_explorer",
                "tv_wake_join",
                "tv_sleep"
            ]
        );
        assert_eq!(r.orch.mode(), Mode::Desktop);
        assert!(!r.state_file.exists());
        let err = r.shared.status().last_error.unwrap();
        assert!(err.contains("never showed up"), "{err}");
    }

    #[test]
    fn big_picture_timeout_rolls_back_everything() {
        let fake = Fake {
            fail: ["bp_never_opens"].into(),
            ..Fake::default()
        };
        let mut r = rig(configured(), fake);
        r.orch.enter().unwrap();
        let calls = r.calls();
        let rollback = &calls[calls.iter().position(|c| c == "wait_big_picture").unwrap() + 1..];
        assert_eq!(
            rollback,
            [
                "close_big_picture",
                "restore_topology",
                "start_explorer",
                "tv_wake_join",
                "tv_sleep"
            ]
        );
        assert_eq!(r.orch.mode(), Mode::Desktop);
    }

    #[test]
    fn explorer_refusing_to_exit_cancels_cleanly() {
        let mut c = configured();
        c.tv.enabled = false;
        let fake = Fake {
            fail: ["exit_explorer"].into(),
            ..Fake::default()
        };
        let mut r = rig(c, fake);
        r.orch.enter().unwrap();
        assert_eq!(
            r.calls(),
            ["snapshot_topology", "exit_explorer", "start_explorer"]
        );
        assert_eq!(r.orch.mode(), Mode::Desktop);
    }

    #[test]
    fn panic_during_enter_stops_at_next_step() {
        let mut r = rig(configured(), Fake::default());
        let shared = r.shared.clone();
        r.orch.platform_mut().abort_on = Some(("exit_explorer", shared));
        r.orch.enter().unwrap();
        let calls = r.calls();
        assert!(!calls.contains(&"wait_display".to_string()), "{calls:?}");
        assert!(calls.contains(&"start_explorer".to_string()));
        assert_eq!(r.orch.mode(), Mode::Desktop);
        // The panic command that follows still makes sure Explorer runs.
        r.orch.panic_restore();
        assert_eq!(r.calls(), ["unwatch_big_picture", "start_explorer"]);
    }

    #[test]
    fn enter_needs_a_target_display_and_desktop_mode() {
        let mut r = rig(Config::default(), Fake::default());
        assert!(matches!(r.orch.enter(), Err(Refused::NotConfigured(_))));
        assert!(r.calls().is_empty());

        let mut r = rig(configured(), Fake::default());
        r.orch.enter().unwrap();
        assert!(matches!(r.orch.enter(), Err(Refused::Busy(_))));
        r.orch.exit(ExitTrigger::Request).unwrap();
        assert!(matches!(
            r.orch.exit(ExitTrigger::Request),
            Err(Refused::Busy(_))
        ));
    }

    #[test]
    fn undo_keeps_going_after_a_failure() {
        let fake = Fake {
            fail: ["restore_topology"].into(),
            ..Fake::default()
        };
        let mut r = rig(configured(), fake);
        r.orch.enter().unwrap();
        r.calls();
        r.orch.exit(ExitTrigger::Request).unwrap();
        assert!(r.calls().contains(&"start_explorer".to_string()));
        assert_eq!(r.orch.mode(), Mode::Desktop);
        let err = r.shared.status().last_error.unwrap();
        assert!(err.contains("display layout"), "{err}");
    }

    fn crashed_state(mode: Mode) -> RuntimeState {
        RuntimeState {
            mode,
            saved_topology: Some(serde_json::json!({"layout": "desk"})),
            steps_done: vec![
                EnterStep::TvWoken,
                EnterStep::ExplorerExited,
                EnterStep::TopologyApplied,
                EnterStep::BigPictureLaunched,
            ],
            ..RuntimeState::default()
        }
    }

    #[test]
    fn recovery_restores_when_big_picture_is_gone() {
        let mut r = rig(configured(), Fake::default());
        r.orch.recover(Ok(crashed_state(Mode::Gaming)));
        assert_eq!(
            r.calls(),
            [
                "close_big_picture",
                "restore_topology",
                "start_explorer",
                "tv_wake_join",
                "tv_sleep"
            ]
        );
        assert_eq!(r.orch.mode(), Mode::Desktop);
    }

    #[test]
    fn recovery_adopts_a_live_gaming_session() {
        let fake = Fake {
            bp_open: true,
            ..Fake::default()
        };
        let mut r = rig(configured(), fake);
        r.orch.recover(Ok(crashed_state(Mode::Gaming)));
        assert_eq!(r.calls(), ["watch_big_picture"]);
        assert_eq!(r.orch.mode(), Mode::Gaming);
        assert_eq!(r.shared.mode(), Mode::Gaming);
    }

    #[test]
    fn recovery_does_not_adopt_after_a_reboot() {
        // Explorer is the shell again (fresh logon) and Steam opened in Big
        // Picture by itself: that is not our gaming mode.
        let fake = Fake {
            bp_open: true,
            shell: true,
            ..Fake::default()
        };
        let mut r = rig(configured(), fake);
        r.orch.recover(Ok(crashed_state(Mode::Gaming)));
        assert!(r.calls().contains(&"restore_topology".to_string()));
        assert_eq!(r.orch.mode(), Mode::Desktop);
    }

    #[test]
    fn recovery_from_entering_and_from_a_corrupt_file() {
        let mut r = rig(configured(), Fake::default());
        r.orch.recover(Ok(crashed_state(Mode::Entering)));
        assert!(r.calls().contains(&"start_explorer".to_string()));

        r.orch.recover(Err("bad json".into()));
        assert_eq!(r.calls(), ["start_explorer"]);
        assert_eq!(r.orch.mode(), Mode::Desktop);

        r.orch.recover(Ok(RuntimeState::default()));
        assert!(r.calls().is_empty());
    }

    #[test]
    fn panic_in_desktop_mode_only_ensures_explorer() {
        let mut r = rig(configured(), Fake::default());
        r.orch.panic_restore();
        assert_eq!(r.calls(), ["unwatch_big_picture", "start_explorer"]);
        assert_eq!(r.orch.mode(), Mode::Desktop);
    }

    #[test]
    fn resume_wakes_tv_and_reasserts_display_only_in_gaming_mode() {
        let mut r = rig(configured(), Fake::default());
        r.orch.on_resume();
        assert!(r.calls().is_empty());

        r.orch.enter().unwrap();
        r.calls();
        r.orch.on_resume();
        assert_eq!(
            r.calls(),
            [
                "wait_network",
                "tv_wake",
                "wait_display",
                "apply_display",
                "focus_big_picture"
            ]
        );
    }

    #[test]
    fn power_policy_is_gaming_only() {
        let c = configured();
        assert!(tv_sleep_on_suspend(Mode::Gaming, &c));
        assert!(tv_wake_on_resume(Mode::Gaming, &c));
        for mode in [Mode::Desktop, Mode::Entering, Mode::Exiting] {
            assert!(!tv_sleep_on_suspend(mode, &c));
            assert!(!tv_wake_on_resume(mode, &c));
        }
        let mut off = c.clone();
        off.power.tv_sleep_on_suspend = false;
        off.power.tv_wake_on_resume = false;
        assert!(!tv_sleep_on_suspend(Mode::Gaming, &off));
        assert!(!tv_wake_on_resume(Mode::Gaming, &off));
        let mut no_tv = c;
        no_tv.tv.enabled = false;
        assert!(!tv_sleep_on_suspend(Mode::Gaming, &no_tv));
    }

    #[test]
    fn overlay_policy() {
        let mut c = Config::default();
        assert!(overlay_allowed(Mode::Gaming, &c));
        assert!(!overlay_allowed(Mode::Desktop, &c));
        c.overlay.in_desktop_mode = true;
        assert!(overlay_allowed(Mode::Desktop, &c));
    }

    #[test]
    fn volume_keys_only_in_gaming_mode() {
        let mut c = Config::default();
        assert!(volume_keys_active(Mode::Gaming, &c));
        for mode in [Mode::Desktop, Mode::Entering, Mode::Exiting] {
            assert!(!volume_keys_active(mode, &c), "{mode:?}");
        }
        // Even with the overlay allowed on the desktop: Explorer has them there.
        c.overlay.in_desktop_mode = true;
        assert!(!volume_keys_active(Mode::Desktop, &c));
        c.overlay.volume_keys = false;
        assert!(!volume_keys_active(Mode::Gaming, &c));
    }
}
