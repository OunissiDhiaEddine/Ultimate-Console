//! State shared between the agent's threads: the window thread (hotkeys,
//! suspend), the orchestrator thread and the IPC connection threads.
//!
//! Plain Rust, no Win32, so the orchestrator tests can use it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, MutexGuard, RwLock};

use uc_core::Config;
use uc_core::ipc::{IpcEvent, TvPowerState};
use uc_core::state::Mode;

/// What `GetStatus` reports, kept current by the orchestrator.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub mode: Mode,
    pub tv: TvPowerState,
    pub last_error: Option<String>,
}

#[derive(Default)]
pub struct Shared {
    status: Mutex<Status>,
    config: RwLock<Config>,
    subscribers: Mutex<Vec<Sender<IpcEvent>>>,
    /// Set by the panic hotkey so a running enter sequence stops at its next
    /// step and rolls back.
    abort: AtomicBool,
    /// A gaming-mode toggle is queued and not handled yet, so a second press
    /// before the first is handled is ignored instead of undoing it.
    toggle_pending: AtomicBool,
}

impl Shared {
    pub fn new(config: Config) -> Self {
        Self {
            config: RwLock::new(config),
            ..Self::default()
        }
    }

    pub fn status(&self) -> Status {
        lock(&self.status).clone()
    }

    pub fn mode(&self) -> Mode {
        lock(&self.status).mode
    }

    /// Changes the mode and tells subscribers.
    pub fn set_mode(&self, mode: Mode) {
        let changed = {
            let mut s = lock(&self.status);
            std::mem::replace(&mut s.mode, mode) != mode
        };
        if changed {
            tracing::info!(?mode, "mode changed");
            self.emit(IpcEvent::ModeChanged { mode });
        }
    }

    pub fn set_tv(&self, tv: TvPowerState) {
        let changed = {
            let mut s = lock(&self.status);
            std::mem::replace(&mut s.tv, tv) != tv
        };
        if changed {
            self.emit(IpcEvent::TvPowerChanged { state: tv });
        }
    }

    /// Records a user-visible problem: logged, kept for `GetStatus`, pushed
    /// to subscribers.
    pub fn report_error(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::error!("{message}");
        lock(&self.status).last_error = Some(message.clone());
        self.emit(IpcEvent::Error { message });
    }

    /// Resets the TV status when TV control is switched on or off.
    pub fn sync_tv_status(&self, config: &Config) {
        let configured = config.tv.enabled && config.tv.ip.is_some();
        let current = self.status().tv;
        if !configured {
            self.set_tv(TvPowerState::NotConfigured);
        } else if current == TvPowerState::NotConfigured {
            self.set_tv(TvPowerState::Unknown);
        }
    }

    pub fn config(&self) -> Config {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_config(&self, config: Config) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
    }

    /// A new event stream for one IPC connection.
    pub fn subscribe(&self) -> Receiver<IpcEvent> {
        let (tx, rx) = mpsc::channel();
        lock(&self.subscribers).push(tx);
        rx
    }

    /// Sends to every subscriber, dropping those that went away.
    pub fn emit(&self, event: IpcEvent) {
        lock(&self.subscribers).retain(|tx| tx.send(event.clone()).is_ok());
    }

    pub fn request_abort(&self) {
        self.abort.store(true, Ordering::SeqCst);
    }

    pub fn clear_abort(&self) {
        self.abort.store(false, Ordering::SeqCst);
    }

    pub fn abort_requested(&self) -> bool {
        self.abort.load(Ordering::SeqCst)
    }

    /// Marks a toggle as queued. False if one already is.
    pub fn try_queue_toggle(&self) -> bool {
        !self.toggle_pending.swap(true, Ordering::SeqCst)
    }

    pub fn toggle_handled(&self) {
        self.toggle_pending.store(false, Ordering::SeqCst);
    }
}

/// Locks, ignoring poisoning: a panic elsewhere must not take the agent down
/// while the user may be without Explorer.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_changes_are_pushed_once() {
        let s = Shared::new(Config::default());
        let rx = s.subscribe();
        s.set_mode(Mode::Entering);
        s.set_mode(Mode::Entering);
        s.set_mode(Mode::Gaming);
        let got: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            got,
            [
                IpcEvent::ModeChanged {
                    mode: Mode::Entering
                },
                IpcEvent::ModeChanged { mode: Mode::Gaming },
            ]
        );
    }

    #[test]
    fn dropped_subscribers_are_removed() {
        let s = Shared::new(Config::default());
        drop(s.subscribe());
        let rx = s.subscribe();
        s.report_error("boom");
        assert_eq!(lock(&s.subscribers).len(), 1);
        assert_eq!(rx.try_iter().count(), 1);
        assert_eq!(s.status().last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn toggle_is_queued_once() {
        let s = Shared::default();
        assert!(s.try_queue_toggle());
        assert!(!s.try_queue_toggle());
        s.toggle_handled();
        assert!(s.try_queue_toggle());
    }
}
