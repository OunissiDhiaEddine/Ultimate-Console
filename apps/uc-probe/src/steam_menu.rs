//! `uc-probe steam-menu`: the Ultimate Console tab in Steam's Quick Access
//! Menu, without the agent.
//!
//! - `targets`: is Steam's DevTools port open, and what does it list?
//! - `run`: inject the tab and answer it (real volume, windows, batteries),
//!   printing what the script found. Leave the agent stopped meanwhile, or
//!   both will answer.
//! - `eval`: evaluate JavaScript in Steam's shared UI context.
//! - `switch on|off`: create or remove Steam's `.cef-enable-remote-debugging`.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Subcommand;
use uc_core::logging::{self, LogOptions};
use uc_core::{Config, Paths};
use uc_steamui::handler::HandlerFactory;
use uc_steamui::{Handler, Host, HostStatus, Settings, SteamMenu, SystemHandler, cdp, flag};

#[derive(Subcommand)]
pub enum SteamMenuCmd {
    /// List the pages on Steam's DevTools port and say which one we'd use.
    Targets,
    /// Inject the tab and serve it until Ctrl+C (or --seconds).
    Run {
        /// Quit after this many seconds.
        #[arg(long)]
        seconds: Option<u64>,
        /// Open the Quick Access Menu on the tab once it's ready.
        #[arg(long)]
        open: bool,
    },
    /// Evaluate a JavaScript expression in Steam's UI, e.g.
    /// `eval "window.__uc && window.__uc.health()"`.
    Eval { expression: String },
    /// Create (`on`) or remove (`off`, only if made by Ultimate Console)
    /// Steam's DevTools switch file. Restart Steam afterwards.
    Switch { action: OnOff },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

/// The probe isn't the agent: no gaming mode to leave.
struct ProbeHost;

impl Host for ProbeHost {
    fn status(&self) -> HostStatus {
        HostStatus {
            gaming: false,
            mode: "uc-probe (no agent)".into(),
            tv: None,
        }
    }
    fn leave_gaming(&self) -> Result<(), String> {
        Err("uc-probe can't leave gaming mode; use the agent".into())
    }
}

pub fn run(cmd: SteamMenuCmd) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let paths = Paths::from_env()?;
    let config = Config::load(&paths.config_file())?;
    let port = config.steam_menu.port;
    match cmd {
        SteamMenuCmd::Targets => {
            let targets = cdp::list_targets(port, Duration::from_secs(2))?;
            for t in &targets {
                let ws = if t.ws_path().is_some() {
                    ""
                } else {
                    "  (in use by another debugger)"
                };
                println!("{:<8} {:<34} {}{ws}", t.kind, t.title, t.url);
            }
            match cdp::shared_context(&targets) {
                Some(t) => println!("\nwould inject into: {} ({})", t.title, t.id),
                None => println!(
                    "\nno usable {} target: Steam's UI isn't up, or another debugger holds it",
                    cdp::SHARED_JS_CONTEXT
                ),
            }
        }
        SteamMenuCmd::Run { seconds, open } => {
            let _guard = logging::init(
                &paths.logs_dir(),
                &LogOptions {
                    prefix: "probe-steam-menu",
                    filter: "info,uc_steamui=debug",
                    stderr: true,
                },
            )?;
            let bp_title = config.steam.bp_window_title.clone();
            let factory: HandlerFactory = Box::new(move || {
                Box::new(SystemHandler::new(Box::new(ProbeHost), bp_title)) as Box<dyn Handler>
            });
            let menu = SteamMenu::start(
                Settings {
                    enabled: true,
                    port,
                },
                uc_core::VERSION,
                factory,
            )?;
            println!("serving the Steam menu on port {port}; Ctrl+C quits");
            let start = Instant::now();
            let mut last = String::new();
            let mut opened = false;
            loop {
                let status = menu.status();
                let text = status.text();
                if text != last {
                    println!("status: {text}");
                    if let uc_steamui::Status::Connected(h) = &status {
                        println!("  found: {:?}", h.found);
                        for p in &h.problems {
                            println!("  problem: {p}");
                        }
                    }
                    last = text;
                }
                if open && !opened && status.is_ready() {
                    opened = true;
                    println!(
                        "open the Quick Access Menu on the tab: {}",
                        menu.open(Duration::from_secs(2))
                    );
                }
                if seconds.is_some_and(|s| start.elapsed() >= Duration::from_secs(s)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        SteamMenuCmd::Eval { expression } => {
            let targets = cdp::list_targets(port, Duration::from_secs(2))?;
            let target =
                cdp::shared_context(&targets).ok_or("Steam's shared UI context isn't available")?;
            let mut session = cdp::Session::connect(port, target, Duration::from_secs(5))?;
            let value = session.evaluate(&expression, Duration::from_secs(10))?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        SteamMenuCmd::Switch { action } => {
            let marker = paths.steam_debug_marker_file();
            match action {
                OnOff::On => {
                    let exe = steam_exe(&config)?;
                    let dir = exe.parent().ok_or("steam.exe has no folder")?;
                    match flag::ensure(dir, &marker)? {
                        flag::Ensured::Created => {
                            println!("created {}", flag::path_in(dir).display());
                            println!("restart Steam so it opens the DevTools port");
                        }
                        flag::Ensured::AlreadyThere => {
                            println!("already there: {}", flag::path_in(dir).display());
                        }
                    }
                }
                OnOff::Off => {
                    if flag::remove_ours(&marker)? {
                        println!("removed; restart Steam to close the port");
                    } else {
                        println!("nothing removed (not made by Ultimate Console)");
                    }
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(windows)]
fn steam_exe(config: &Config) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    Ok(uc_shell::steam::find_steam_exe(
        config.steam.exe_path.as_deref(),
    )?)
}

#[cfg(not(windows))]
fn steam_exe(config: &Config) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    config
        .steam
        .exe_path
        .clone()
        .ok_or_else(|| "set steam.exe_path (Steam is only found automatically on Windows)".into())
}
