//! Ultimate Console agent. Stub created by WP-00; WP-08 builds the real thing.
//!
//! For now it only loads the config and state, logs what it found and exits,
//! which proves the `uc-core` plumbing works on the real machine.

// No console window in release builds: the agent runs headless at logon.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

use uc_core::logging::{self, LogOptions};
use uc_core::{Config, Paths, RuntimeState};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("uc-agent: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> uc_core::Result<()> {
    let paths = Paths::from_env()?;
    let config = Config::load(&paths.config_file())?;
    let _log_guard = logging::init(
        &paths.logs_dir(),
        &LogOptions {
            prefix: "agent",
            filter: &config.general.log_level,
            stderr: cfg!(debug_assertions),
        },
    )?;
    tracing::info!(version = uc_core::VERSION, "uc-agent starting (stub)");

    if let Err(e) = config.validate() {
        tracing::warn!("config has problems:\n{e}");
    }
    let state = RuntimeState::load(&paths.state_file())?;
    if state.needs_recovery() {
        tracing::warn!(mode = ?state.mode, "previous run did not end in desktop mode; recovery is WP-08");
    }
    tracing::info!("uc-agent stub exiting");
    Ok(())
}
