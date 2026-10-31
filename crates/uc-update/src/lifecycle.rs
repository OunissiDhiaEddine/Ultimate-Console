//! Velopack install/update/uninstall hooks, run by `uc-settings.exe` (the
//! package's main exe) as the very first thing in `main` (plan §2.12).
//!
//! Velopack calls the main exe with a hook argument after install, after
//! each update and before uninstall; [`run`] handles those and exits the
//! process. On a normal start it returns what the app should do next.
//!
//! | Event        | What happens                                                    |
//! |--------------|-----------------------------------------------------------------|
//! | install      | register autostart (if `general.start_with_windows`)            |
//! | first run    | [`Startup::FirstRun`]: start the agent, then show settings      |
//! | update       | re-register autostart (same path, keeps it in step with config) |
//! | restart after update | [`Startup::StartAgentOnly`] (from [`crate::START_AGENT_ARG`]) |
//! | uninstall    | stop the agent (it restores desktop first), remove autostart, delete config/state/keys/token/logs; Velopack removes the rest |

use std::cell::Cell;
use std::path::PathBuf;

use uc_core::{Config, Paths};
use velopack::VelopackApp;

use crate::{AGENT_EXE, START_AGENT_ARG, autostart, uninstall};

/// What `main` should do after [`run`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Startup {
    /// Normal start: show the settings window.
    Normal,
    /// First start right after install: start the agent, then show settings.
    FirstRun,
    /// Restarted by `Update.exe` after an update: start the agent and exit
    /// without a window.
    StartAgentOnly,
}

/// Runs the Velopack hooks. `stop_agent` must ask a running agent to restore
/// desktop mode and exit (IPC `Shutdown`), and return once it has gone or
/// after a short timeout; it is only called before uninstall.
///
/// Returns only on a normal start; a hook invocation exits the process.
pub fn run(stop_agent: impl FnOnce()) -> Startup {
    let first_run = Cell::new(false);
    // Never apply a downloaded update just because settings was opened: that
    // would kill the agent, maybe in the middle of a game. The agent applies
    // updates itself when it's back in desktop mode.
    let app = VelopackApp::build()
        .set_auto_apply_on_startup(false)
        .on_first_run(|_| first_run.set(true));
    // Velopack only has install/update/uninstall hooks on Windows. The hard
    // limit is 15 s (update) / 30 s (install, uninstall) before it kills us.
    #[cfg(windows)]
    let app = app
        .on_after_install_fast_callback(|v| sync_autostart("install", &v))
        .on_after_update_fast_callback(|v| sync_autostart("update", &v))
        .on_before_uninstall_fast_callback(|v| {
            tracing::info!(version = %v, "uninstalling");
            stop_agent();
            remove_everything();
        });
    #[cfg(not(windows))]
    drop(stop_agent);
    let mut app = app;
    app.run();

    if std::env::args().skip(1).any(|a| a == START_AGENT_ARG) {
        Startup::StartAgentOnly
    } else if first_run.get() {
        Startup::FirstRun
    } else {
        Startup::Normal
    }
}

/// `uc-agent.exe` next to the running exe.
pub fn agent_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(AGENT_EXE))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn sync_autostart(event: &str, version: &dyn std::fmt::Display) {
    tracing::info!(%version, "{event} hook");
    let wanted = Paths::from_env()
        .and_then(|p| Config::load(&p.config_file()))
        .map(|c| c.general.start_with_windows)
        .unwrap_or(true);
    let result = match (wanted, agent_path()) {
        (true, Some(agent)) => autostart::register(&agent),
        (true, None) => return tracing::warn!("can't find the agent exe; autostart not set"),
        (false, _) => autostart::unregister(),
    };
    if let Err(e) = result {
        tracing::warn!("autostart: {e}");
    }
}

/// Uninstall steps 3 and 4 of plan §2.12. Best effort: an uninstall must
/// never stop half way.
pub fn remove_everything() {
    if let Err(e) = autostart::unregister() {
        tracing::warn!("autostart: {e}");
    }
    match Paths::from_env() {
        Ok(paths) => {
            uninstall::remove_app_data(&paths);
        }
        Err(e) => tracing::warn!("can't resolve app folders: {e}"),
    }
}
