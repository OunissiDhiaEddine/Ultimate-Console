//! Ultimate Console agent (WP-08): the one background process (plan §3).
//!
//! ```text
//! main thread           hidden window: hotkeys, suspend/resume, overlay hotkey
//! orchestrator thread   mode changes, config reload, TV keepalive (service.rs)
//! IPC threads           named pipe for uc-settings and the uninstaller (ipc.rs)
//! overlay thread        owned by uc-overlay (WP-07)
//! ```
//!
//! Command line:
//! - `--autostart on|off`: add or remove the HKCU Run entry, then exit
//!   (for the installer's hooks).
//! - `--restarted`: passed by Windows when it restarts the agent after a crash.

// No console window in release builds: the agent runs headless at logon.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Elsewhere only the pure modules build, for their tests.
#![cfg_attr(not(windows), allow(dead_code))]

mod hotkeys;
mod orchestrator;
mod overlay;
mod shared;
mod updates;

#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod ipc;
#[cfg(windows)]
mod platform;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod sys;
#[cfg(windows)]
mod window;

use std::process::ExitCode;

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("uc-agent only runs on Windows");
    ExitCode::FAILURE
}

#[cfg(windows)]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["--autostart", "on"] => autostart::set(true).map_err(|e| e.to_string()),
        ["--autostart", "off"] => autostart::set(false).map_err(|e| e.to_string()),
        [] | ["--restarted"] => run(!args.is_empty()),
        _ => Err(format!(
            "unknown arguments {args:?}; use --autostart on|off or no arguments"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("uc-agent: {e}");
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn run(restarted: bool) -> Result<(), String> {
    use std::sync::{Arc, mpsc};

    use uc_core::logging::{self, LogOptions};
    use uc_core::{Config, Paths};

    use orchestrator::Orchestrator;
    use overlay::OverlayHost;
    use platform::{RealPlatform, Tv};
    use service::Service;
    use shared::Shared;
    use window::Window;

    let paths = Paths::from_env().map_err(|e| e.to_string())?;
    let (config, config_error) = match Config::load(&paths.config_file()) {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e.to_string())),
    };
    let _log_guard = logging::init(
        &paths.logs_dir(),
        &LogOptions {
            prefix: "agent",
            filter: &config.general.log_level,
            stderr: cfg!(debug_assertions),
        },
    )
    .map_err(|e| e.to_string())?;
    std::panic::set_hook(Box::new(|info| tracing::error!("panic: {info}")));

    let Some(_instance) = sys::single_instance().map_err(|e| e.to_string())? else {
        tracing::info!("another agent is already running; exiting");
        return Ok(());
    };
    tracing::info!(version = uc_core::VERSION, restarted, "uc-agent starting");
    sys::register_restart();

    let shared = Arc::new(Shared::new(config.clone()));
    if let Some(e) = config_error {
        shared.report_error(format!(
            "config.toml could not be read, using defaults: {e}"
        ));
    } else if let Err(e) = config.validate() {
        shared.report_error(format!("config.toml has problems: {e}"));
    }
    if !config.general.start_with_windows
        && let Err(e) = autostart::set(false)
    {
        tracing::warn!("could not remove autostart: {e}");
    }

    let tv = Arc::new(Tv::new(paths.adb_key_file()));
    if let Err(e) = tv.sync(&config.tv) {
        shared.report_error(format!("TV: {e}"));
    }
    shared.sync_tv_status(&config);

    let updates = updates::Updates::new(shared.clone());
    let (commands, inbox) = mpsc::channel();
    let overlay = Arc::new(OverlayHost::start(&config));
    let window = Window::create(
        shared.clone(),
        tv.clone(),
        commands.clone(),
        overlay.clone(),
    )
    .map_err(|e| format!("could not create the agent window: {e}"))?;
    let handle = window.handle();
    for (action, e) in handle.apply_hotkeys(&config.hotkeys) {
        shared.report_error(format!("hotkeys.{}: {e}", hotkeys::name(action)));
    }

    if let Err(e) = ipc::serve(ipc::Handler {
        shared: shared.clone(),
        tv: tv.clone(),
        commands: commands.clone(),
        window: handle,
        updates: updates.clone(),
    }) {
        shared.report_error(format!("settings app connection unavailable: {e}"));
    }

    // Built on its own thread: the platform holds COM objects tied to it.
    std::thread::Builder::new()
        .name("uc-orchestrator".into())
        .spawn(move || {
            let platform = RealPlatform::new(shared.clone(), tv.clone(), commands);
            let orch = Orchestrator::new(platform, config, paths.state_file(), shared.clone());
            Service::new(
                orch,
                shared,
                tv,
                handle,
                overlay,
                updates,
                paths.config_file(),
            )
            .run(paths.state_file(), inbox);
        })
        .map_err(|e| e.to_string())?;

    window.run();
    tracing::info!("uc-agent stopped");
    Ok(())
}
