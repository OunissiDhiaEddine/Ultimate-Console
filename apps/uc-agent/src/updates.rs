//! Updates from the private GitHub Releases (WP-10, plan §2.12).
//!
//! - `CheckForUpdates` from the settings app checks right away; with
//!   `updates.auto_check` the orchestrator's tick also checks a minute after
//!   start (network up after logon) and then daily, retrying hourly on
//!   failure. Checks and downloads run on their own thread, never on the
//!   orchestrator thread.
//! - A found update is downloaded at once, but only applied by
//!   [`Updates::take_ready`] when the orchestrator is in desktop mode: the
//!   agent then exits and Velopack restarts it on the new version.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uc_core::Paths;
use uc_core::ipc::{ErrorCode, IpcEvent, IpcResponse, UpdateStatus};
use uc_core::state::Mode;
use uc_update::{Error, TokenStore, Updater, updater};

use crate::shared::{Shared, lock};

/// First automatic check after the agent starts.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(60);
/// Next automatic check after a failed one.
const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

pub struct Updates {
    shared: Arc<Shared>,
    token: TokenStore,
    /// When the next automatic check is due.
    next_check: Mutex<Instant>,
    /// A check (and download) is running.
    busy: AtomicBool,
    /// An update is downloaded and waiting for desktop mode.
    ready: AtomicBool,
}

impl Updates {
    pub fn new(shared: Arc<Shared>, paths: &Paths) -> Arc<Self> {
        let updates = Arc::new(Self {
            shared,
            token: TokenStore::for_paths(paths),
            next_check: Mutex::new(Instant::now() + FIRST_CHECK_AFTER),
            busy: AtomicBool::new(false),
            ready: AtomicBool::new(false),
        });
        // Downloaded by an earlier run but not applied yet.
        if let Ok(u) = Updater::new(&updates.shared.config().updates.repo, None)
            && u.downloaded().is_some()
        {
            updates.ready.store(true, Ordering::SeqCst);
        }
        updates
    }

    pub fn token_set(&self) -> bool {
        self.token.is_set()
    }

    pub fn set_token(&self, token: &str) -> IpcResponse {
        match self.token.save(token) {
            Ok(()) => IpcResponse::Ok,
            Err(e @ Error::InvalidToken(_)) => {
                IpcResponse::error(ErrorCode::InvalidRequest, e.to_string())
            }
            Err(e) => IpcResponse::error(ErrorCode::Failed, e.to_string()),
        }
    }

    pub fn clear_token(&self) -> IpcResponse {
        match self.token.clear() {
            Ok(()) => IpcResponse::Ok,
            Err(e) => IpcResponse::error(ErrorCode::Failed, e.to_string()),
        }
    }

    /// "Check now" from the settings app. Blocks the calling IPC thread
    /// until the check (and the download, if any) is done.
    pub fn check_now(&self) -> IpcResponse {
        if self.busy.swap(true, Ordering::SeqCst) {
            return IpcResponse::error(ErrorCode::Busy, "an update check is already running");
        }
        let status = self.check_and_download();
        self.busy.store(false, Ordering::SeqCst);
        IpcResponse::Update(status)
    }

    /// Called on every orchestrator tick: starts a background check when one
    /// is due.
    pub fn tick(self: &Arc<Self>) {
        if !self.shared.config().updates.auto_check || !self.token.is_set() {
            return;
        }
        {
            let mut next = lock(&self.next_check);
            if Instant::now() < *next {
                return;
            }
            // Pushed back now so the following ticks don't start another.
            *next = Instant::now() + RETRY_AFTER;
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        let spawned = std::thread::Builder::new()
            .name("uc-update-check".into())
            .spawn(move || {
                let status = this.check_and_download();
                if !matches!(status, UpdateStatus::Failed { .. }) {
                    *lock(&this.next_check) = Instant::now() + updater::CHECK_INTERVAL;
                }
                this.busy.store(false, Ordering::SeqCst);
            });
        if spawned.is_err() {
            self.busy.store(false, Ordering::SeqCst);
        }
    }

    /// The downloaded update to apply now, if there is one and `mode` allows
    /// it. The caller must then exit the process (see
    /// [`Updater::apply_on_exit`]).
    pub fn take_ready(&self, mode: Mode) -> Option<Updater> {
        if !apply_allowed(
            mode,
            self.ready.load(Ordering::SeqCst),
            self.busy.load(Ordering::SeqCst),
        ) {
            return None;
        }
        self.ready.store(false, Ordering::SeqCst);
        Updater::new(&self.shared.config().updates.repo, None).ok()
    }

    fn check_and_download(&self) -> UpdateStatus {
        let token = match self.token.load() {
            Ok(Some(t)) => t,
            Ok(None) => return UpdateStatus::TokenNeeded,
            Err(e) => return failed(e),
        };
        let updater = match Updater::new(&self.shared.config().updates.repo, Some(token)) {
            Ok(u) => u,
            Err(e) => return failed(e),
        };
        let found = updater.check();
        let status = updater::status_for(&updater.current_version(), &found);
        if let Ok(Some(update)) = found {
            self.shared.emit(IpcEvent::UpdateAvailable {
                version: update.version().to_string(),
            });
            match updater.download(&update) {
                Ok(()) => self.ready.store(true, Ordering::SeqCst),
                Err(e) => return failed(e),
            }
        }
        status
    }
}

fn failed(e: Error) -> UpdateStatus {
    tracing::warn!("update check failed: {e}");
    UpdateStatus::Failed {
        message: e.to_string(),
    }
}

/// Updates restart the agent, so only in plain desktop mode, never during a
/// mode change or a game, and not while a download is still being written.
fn apply_allowed(mode: Mode, ready: bool, busy: bool) -> bool {
    ready && !busy && mode == Mode::Desktop
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_only_in_desktop_mode() {
        assert!(apply_allowed(Mode::Desktop, true, false));
        assert!(!apply_allowed(Mode::Desktop, false, false));
        assert!(!apply_allowed(Mode::Desktop, true, true));
        for mode in [Mode::Entering, Mode::Gaming, Mode::Exiting] {
            assert!(!apply_allowed(mode, true, false), "{mode:?}");
        }
    }

    #[test]
    fn token_requests_without_install() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("c"), dir.path().join("d"));
        let updates = Updates::new(Arc::new(Shared::new(Default::default())), &paths);
        assert!(!updates.token_set());
        assert!(matches!(
            updates.set_token("not a token"),
            IpcResponse::Error {
                code: ErrorCode::InvalidRequest,
                ..
            }
        ));
        assert_eq!(updates.clear_token(), IpcResponse::Ok);
        assert_eq!(
            updates.check_now(),
            IpcResponse::Update(UpdateStatus::TokenNeeded)
        );
        assert!(updates.take_ready(Mode::Desktop).is_none());
    }
}
