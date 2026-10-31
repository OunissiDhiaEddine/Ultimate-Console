//! Ultimate Console settings app (plan §2.10, WP-09).
//!
//! An egui window that edits the agent's config and runs its test actions
//! over the named pipe. It only runs while open.
//!
//! ```text
//! uc-settings                   normal use
//! uc-settings --mock            talk to a built-in fake agent (UI testing)
//! uc-settings --check-uninstall list what the app left behind (plan §2.12)
//! ```

// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod client;
mod keys;
mod mock;
mod pages;
mod system;

use std::process::ExitCode;
use std::sync::Arc;

use eframe::egui;
use uc_core::Paths;
use uc_core::logging::{self, LogOptions};

use crate::client::Connector;

const USAGE: &str = "usage: uc-settings [--mock | --check-uninstall]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mock = false;
    for arg in &args {
        match arg.as_str() {
            "--mock" => mock = true,
            // Must not create logs or anything else: it checks they're gone.
            "--check-uninstall" => {
                system::attach_console();
                return system::check_uninstall();
            }
            _ => {
                system::attach_console();
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let _log_guard = Paths::from_env().ok().and_then(|p| {
        logging::init(
            &p.logs_dir(),
            &LogOptions {
                prefix: "settings",
                filter: "info",
                stderr: cfg!(debug_assertions),
            },
        )
        .ok()
    });
    tracing::info!(version = uc_core::VERSION, mock, "uc-settings starting");

    let connector: Arc<dyn Connector> = match connector(mock) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("cannot set up the agent connection: {e}");
            eprintln!("uc-settings: {e}");
            return ExitCode::FAILURE;
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Ultimate Console Settings")
            .with_inner_size([900.0, 640.0])
            .with_min_inner_size([640.0, 420.0]),
        ..Default::default()
    };
    match eframe::run_native(
        "Ultimate Console Settings",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, connector, mock)))),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("UI failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn connector(mock: bool) -> std::io::Result<Arc<dyn Connector>> {
    if mock {
        return Ok(Arc::new(mock::MockConnector::new(mock::MockAgent::new())));
    }
    #[cfg(windows)]
    return Ok(Arc::new(system::PipeConnector::for_current_user()?));
    #[cfg(not(windows))]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the agent's named pipe only exists on Windows; use --mock",
    ))
}
