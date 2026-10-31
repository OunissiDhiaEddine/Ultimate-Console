//! `uc-probe update …` (WP-10): the autostart value and a real update check. `check` only works from an installed copy
//! (`%LOCALAPPDATA%\UltimateConsole\current\uc-probe.exe`).

use std::process::ExitCode;

use clap::Subcommand;
use uc_core::{Config, Paths};
use uc_update::{Updater, autostart};

#[derive(Subcommand)]
pub enum UpdateCmd {
    /// Ask GitHub for a newer release.
    Check {
        /// Also download it (it is applied by the agent, not here).
        #[arg(long)]
        download: bool,
    },
    /// Show the HKCU Run value that starts the agent at logon.
    Autostart,
}

pub fn run(cmd: UpdateCmd) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let paths = Paths::from_env()?;
    match cmd {
        UpdateCmd::Check { download } => {
            let config = Config::load(&paths.config_file())?;
            let updater = Updater::new(&config.updates.repo)?;
            println!(
                "installed {}, checking {}",
                updater.current_version(),
                config.updates.repo
            );
            match updater.check()? {
                None => println!("up to date"),
                Some(u) => {
                    println!("update available: {}", u.version());
                    if download {
                        updater.download(&u)?;
                        println!(
                            "downloaded; the agent applies it next time you're in desktop mode"
                        );
                    }
                }
            }
        }
        UpdateCmd::Autostart => match autostart::registered_command() {
            Some(cmd) => println!("{}: {cmd}", autostart::RUN_KEY),
            None => {
                println!("not registered");
                return Ok(ExitCode::FAILURE);
            }
        },
    }
    Ok(ExitCode::SUCCESS)
}
