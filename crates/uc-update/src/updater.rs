//! Updates from this repo's private GitHub Releases (plan §2.12, WP-10).
//!
//! The agent owns the [`Updater`] and the policy: check daily (and on
//! "Check now" from the settings app), download in the background, and call
//! [`Updater::apply_on_exit`] only when back in desktop mode. Velopack's
//! `Update.exe` then waits for the agent to exit, swaps `current\`, and
//! restarts the main exe (`uc-settings.exe`) with [`crate::START_AGENT_ARG`],
//! which starts the new agent without opening a window.

use std::time::Duration;

use uc_core::ipc::UpdateStatus;
use velopack::sources::GithubSource;
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackAsset};

use crate::{Error, Result};

/// How often the agent checks when `updates.auto_check` is on.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// A newer release found by [`Updater::check`].
#[derive(Debug, Clone)]
pub struct PendingUpdate {
    info: UpdateInfo,
}

impl PendingUpdate {
    pub fn version(&self) -> &str {
        &self.info.TargetFullRelease.Version
    }
}

/// Velopack update manager for an installed copy.
pub struct Updater {
    manager: UpdateManager,
}

impl Updater {
    /// `repo` is `config.updates.repo` (`"owner/name"`); `token` is the
    /// decrypted [`crate::TokenStore`] token. Without a token a private repo
    /// can't be read, so callers should report [`UpdateStatus::TokenNeeded`]
    /// instead of calling this.
    ///
    /// Fails with [`Error::NotInstalled`] in a dev or CI build.
    pub fn new(repo: &str, token: Option<String>) -> Result<Self> {
        let source = GithubSource::new(&repo_url(repo)?, token, false);
        let manager = UpdateManager::new(source, None, None).map_err(|e| match e {
            velopack::Error::NotInstalled(_) => Error::NotInstalled,
            other => Error::Velopack(other),
        })?;
        Ok(Self { manager })
    }

    pub fn current_version(&self) -> String {
        self.manager.get_current_version_as_string()
    }

    /// Asks GitHub for a newer release. `Ok(None)` when up to date.
    pub fn check(&self) -> Result<Option<PendingUpdate>> {
        match self.manager.check_for_updates()? {
            UpdateCheck::UpdateAvailable(info) => {
                tracing::info!(
                    version = %info.TargetFullRelease.Version,
                    "update available"
                );
                Ok(Some(PendingUpdate { info: *info }))
            }
            UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty => Ok(None),
        }
    }

    /// Downloads (and verifies) the update into Velopack's `packages\`
    /// folder. Blocking; run it off the agent's main thread.
    pub fn download(&self, update: &PendingUpdate) -> Result<()> {
        self.manager.download_updates(&update.info, None)?;
        tracing::info!(version = update.version(), "update downloaded");
        Ok(())
    }

    /// An update that has been downloaded and is waiting to be applied.
    pub fn downloaded(&self) -> Option<VelopackAsset> {
        self.manager.get_update_pending_restart()
    }

    /// Hands `asset` to `Update.exe`, which waits for this process to exit,
    /// applies it silently and restarts the app (agent included). The caller
    /// must restore desktop mode first and exit within 60 seconds.
    pub fn apply_on_exit(&self, asset: &VelopackAsset) -> Result<()> {
        tracing::info!(version = %asset.Version, "applying update after exit");
        self.manager
            .wait_exit_then_apply_updates(asset, true, true, [crate::START_AGENT_ARG])?;
        Ok(())
    }
}

/// `"owner/name"` → `https://github.com/owner/name`.
pub fn repo_url(repo: &str) -> Result<String> {
    let repo = repo.trim().trim_matches('/');
    let valid_part = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    match repo.split_once('/') {
        Some((owner, name)) if valid_part(owner) && valid_part(name) => {
            Ok(format!("https://github.com/{owner}/{name}"))
        }
        _ => Err(Error::InvalidRepo(repo.to_string())),
    }
}

/// The IPC answer for a check result.
pub fn status_for(current_version: &str, result: &Result<Option<PendingUpdate>>) -> UpdateStatus {
    match result {
        Ok(None) => UpdateStatus::UpToDate {
            version: current_version.to_string(),
        },
        Ok(Some(u)) => UpdateStatus::Available {
            version: u.version().to_string(),
        },
        Err(e) => UpdateStatus::Failed {
            message: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_urls() {
        assert_eq!(
            repo_url("OunissiDhiaEddine/Ultimate-Console").unwrap(),
            "https://github.com/OunissiDhiaEddine/Ultimate-Console"
        );
        assert_eq!(repo_url(" /a/b.c/ ").unwrap(), "https://github.com/a/b.c");
        for bad in ["", "noslash", "a/", "/b", "a/b/c", "a b/c", "a/b?x=1"] {
            assert!(repo_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn not_installed_in_tests() {
        assert!(matches!(
            Updater::new("a/b", None),
            Err(Error::NotInstalled)
        ));
    }

    #[test]
    fn statuses() {
        assert_eq!(
            status_for("0.1.0", &Ok(None)),
            UpdateStatus::UpToDate {
                version: "0.1.0".into()
            }
        );
        assert!(matches!(
            status_for("0.1.0", &Err(Error::NotInstalled)),
            UpdateStatus::Failed { .. }
        ));
    }
}
