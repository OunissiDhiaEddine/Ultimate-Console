//! `uc-probe`: exercise each crate on its own, on the real PC (plan §6).
//!
//! Each work package adds its own subcommand group here
//! (`display`, `tv`, `shell`, `audio`, `power`, `sysinfo`, `overlay`).
//! WP-00 provides the `uc-core` ones: `paths`, `config`, `state`, `hotkey`.
//!
//! Set `UC_HOME` to point every file at a scratch folder instead of the real
//! `%APPDATA%`/`%LOCALAPPDATA%` ones.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use uc_core::{Config, Hotkey, Paths, RuntimeState};

mod shell;

#[derive(Parser)]
#[command(
    name = "uc-probe",
    version,
    about = "Test Ultimate Console pieces in isolation"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print where config, state and logs live.
    Paths,
    /// Inspect the config file.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Inspect or reset the crash-recovery state.
    #[command(subcommand)]
    State(StateCmd),
    /// Check a shortcut against the built-in conflict rules, e.g. `hotkey "Ctrl+Alt+G"`.
    Hotkey { combo: String },
    /// Explorer exit/start and Steam Big Picture (WP-03).
    #[command(subcommand)]
    Shell(shell::ShellCmd),
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the effective config (file merged over defaults).
    Show,
    /// Validate the config file.
    Validate,
    /// Print the default config.
    Default,
}

#[derive(Subcommand)]
enum StateCmd {
    /// Print state.json.
    Show,
    /// Delete state.json (back to desktop mode). Does not restore anything.
    Clear,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match cli.command {
        Command::Paths => {
            let p = Paths::from_env()?;
            println!("config: {}", p.config_file().display());
            println!("state:  {}", p.state_file().display());
            println!("logs:   {}", p.logs_dir().display());
            println!("adbkey: {}", p.adb_key_file().display());
        }
        Command::Config(cmd) => {
            let p = Paths::from_env()?;
            match cmd {
                ConfigCmd::Show => print!("{}", Config::load(&p.config_file())?.to_toml()?),
                ConfigCmd::Default => print!("{}", Config::default().to_toml()?),
                ConfigCmd::Validate => {
                    Config::load(&p.config_file())?.validate()?;
                    println!("config OK");
                }
            }
        }
        Command::State(cmd) => {
            let path = Paths::from_env()?.state_file();
            match cmd {
                StateCmd::Show => {
                    let s = RuntimeState::load(&path)?;
                    println!("{}", serde_json::to_string_pretty(&s)?);
                }
                StateCmd::Clear => {
                    RuntimeState::clear(&path)?;
                    println!("state cleared");
                }
            }
        }
        Command::Hotkey { combo } => {
            let hotkey: Hotkey = combo.parse()?;
            let check = hotkey.check();
            println!(
                "{hotkey}  (vk {:#04X}, modifiers {:#x})",
                hotkey.key.vk(),
                hotkey.modifiers.to_win32()
            );
            for e in &check.errors {
                println!("  error:   {e}");
            }
            for w in &check.warnings {
                println!("  warning: {w}");
            }
            if check.needs_altgr_check {
                println!(
                    "  note:    Ctrl+Alt = AltGr on some layouts; the agent checks installed layouts"
                );
            }
            if !check.is_ok() {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Shell(cmd) => shell::run(cmd)?,
    }
    Ok(ExitCode::SUCCESS)
}
