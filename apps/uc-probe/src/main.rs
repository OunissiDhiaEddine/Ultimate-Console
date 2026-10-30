//! `uc-probe`: exercise each crate on its own, on the real PC (plan §6).
//!
//! Each work package adds its own subcommand group here
//! (`display`, `tv`, `shell`, `audio`, `power`, `sysinfo`, `overlay`).
//! WP-00 provides the `uc-core` ones: `paths`, `config`, `state`, `hotkey`.
//! WP-01 adds `display`.
//!
//! Set `UC_HOME` to point every file at a scratch folder instead of the real
//! `%APPDATA%`/`%LOCALAPPDATA%` ones.

mod display;
mod power;

use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use uc_audio::{AudioControl, CoreAudio, level_to_percent, percent_to_level};
use uc_core::{Config, Hotkey, Paths, RuntimeState};
use uc_sysinfo::{Sampler, StatsSource, format_bytes, format_uptime};

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
    /// Audio devices and volume (WP-04).
    #[command(subcommand)]
    Audio(AudioCmd),
    /// List, snapshot, switch and restore displays (WP-01).
    #[command(subcommand)]
    Display(display::DisplayCmd),
    /// Explorer exit/start and Steam Big Picture (WP-03).
    #[command(subcommand)]
    Shell(shell::ShellCmd),
    /// Sample CPU/RAM/GPU for the system and the given PIDs, like the overlay does.
    Sysinfo {
        /// Process IDs to watch (see Task Manager's Details tab). None = system only.
        pids: Vec<u32>,
        /// Number of samples to print.
        #[arg(long, default_value_t = 5)]
        count: u32,
        /// Milliseconds between samples.
        #[arg(long, default_value_t = 1000)]
        interval: u64,
    },
    /// Suspend/resume, display-state and network-ready checks (WP-05).
    #[command(subcommand)]
    Power(power::PowerCmd),
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

#[derive(Subcommand)]
enum AudioCmd {
    /// List active output devices; `*` marks the default.
    List,
    /// Make a device the default output (all roles). Takes an id from `list`.
    SetDefault { id: String },
    /// Show the master volume, or set it to PERCENT (0-100).
    Volume {
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: Option<u8>,
    },
    /// Show the master mute, or change it.
    Mute { action: Option<Switch> },
    /// List apps with an audio session on the default output.
    Apps,
    /// Set the volume (0-100) of every session of process PID.
    AppVolume {
        pid: u32,
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
    },
    /// Mute or unmute process PID.
    AppMute { pid: u32, action: Switch },
}

#[derive(Clone, Copy, ValueEnum)]
enum Switch {
    On,
    Off,
    Toggle,
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
        Command::Power(cmd) => return power::run(cmd),
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
        Command::Audio(cmd) => audio(cmd)?,
        Command::Display(cmd) => return display::run(cmd),
        Command::Shell(cmd) => shell::run(cmd)?,
        Command::Sysinfo {
            pids,
            count,
            interval,
        } => sysinfo(&pids, count, interval),
    }
    Ok(ExitCode::SUCCESS)
}

fn audio(cmd: AudioCmd) -> Result<(), Box<dyn std::error::Error>> {
    let audio = CoreAudio::new()?;
    let on_off = |m: bool| if m { "on" } else { "off" };
    match cmd {
        AudioCmd::List => {
            for d in audio.outputs()? {
                let mark = if d.is_default { '*' } else { ' ' };
                println!("{mark} {}\n    {}", d.name, d.id);
            }
        }
        AudioCmd::SetDefault { id } => {
            audio.set_default_output(&id)?;
            if let Some(d) = audio.default_output()? {
                println!("default output: {} ({})", d.name, d.id);
            }
        }
        AudioCmd::Volume { percent } => {
            if let Some(p) = percent {
                audio.set_volume(percent_to_level(p))?;
            }
            println!(
                "volume {}%  mute {}",
                level_to_percent(audio.volume()?),
                on_off(audio.muted()?)
            );
        }
        AudioCmd::Mute { action } => {
            let muted = match action {
                None => audio.muted()?,
                Some(Switch::On) => set_mute(&audio, true)?,
                Some(Switch::Off) => set_mute(&audio, false)?,
                Some(Switch::Toggle) => audio.toggle_mute()?,
            };
            println!("mute {}", on_off(muted));
        }
        AudioCmd::Apps => {
            for s in audio.sessions()? {
                println!(
                    "{:>7}  {:>3}%  mute {:<3}  {}",
                    s.pid,
                    level_to_percent(s.volume),
                    on_off(s.muted),
                    s.name
                );
            }
        }
        AudioCmd::AppVolume { pid, percent } => {
            audio.set_app_volume(pid, percent_to_level(percent))?;
            println!("process {pid}: volume {percent}%");
        }
        AudioCmd::AppMute { pid, action } => {
            let muted = match action {
                Switch::On => true,
                Switch::Off => false,
                Switch::Toggle => {
                    !audio
                        .app_session(pid)?
                        .ok_or(uc_audio::Error::NoSession(pid))?
                        .muted
                }
            };
            audio.set_app_muted(pid, muted)?;
            println!("process {pid}: mute {}", on_off(muted));
        }
    }
    Ok(())
}

fn set_mute(audio: &CoreAudio, muted: bool) -> uc_audio::Result<bool> {
    audio.set_muted(muted)?;
    Ok(muted)
}

fn sysinfo(pids: &[u32], count: u32, interval_ms: u64) {
    let pct = |p: Option<f32>| p.map_or_else(|| "  -- ".to_owned(), |v| format!("{v:4.1}%"));
    let mut sampler = Sampler::new();
    for i in 0..count {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        }
        let started = std::time::Instant::now();
        let snap = sampler.sample(pids);
        let took = started.elapsed();
        let s = &snap.system;
        println!(
            "[{i}] system  cpu {}  gpu {}  ram {} / {}  (sample took {} ms)",
            pct(s.cpu_percent),
            pct(s.gpu_percent),
            format_bytes(s.memory_used_bytes),
            format_bytes(s.memory_total_bytes),
            took.as_millis()
        );
        for &pid in pids {
            match snap.process(pid) {
                Some(p) => println!(
                    "    {pid:>6} {:<24} cpu {}  gpu {}  ws {}  private {}  up {}",
                    p.name.as_deref().unwrap_or("?"),
                    pct(p.cpu_percent),
                    pct(p.gpu_percent),
                    format_bytes(p.working_set_bytes),
                    format_bytes(p.private_bytes),
                    format_uptime(p.uptime)
                ),
                None => println!("    {pid:>6} not readable (exited or access denied)"),
            }
        }
    }
}
