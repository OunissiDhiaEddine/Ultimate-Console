//! `uc-probe shell …`: WP-03 (Explorer and Steam Big Picture).

use clap::Subcommand;

#[derive(Subcommand)]
pub enum ShellCmd {
    /// Show Explorer, Steam and Big Picture state.
    Status,
    /// Ask Explorer to exit (no sign-out). Ctrl+Shift+Esc → Run → explorer brings it back.
    KillExplorer {
        /// How long Explorer gets to exit.
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
        /// Afterwards, watch this long for Windows restarting Explorer by
        /// itself (the spike in docs/spikes/shell.md).
        #[arg(long, default_value_t = 15)]
        watch_secs: u64,
        /// Start Explorer again at the end.
        #[arg(long)]
        restore: bool,
    },
    /// Start Explorer if it isn't running.
    StartExplorer {
        #[arg(long, default_value_t = 10)]
        wait_secs: u64,
    },
    /// Open Steam Big Picture and wait for its window.
    BpLaunch,
    /// Ask Steam to leave Big Picture.
    BpClose,
    /// List the Big Picture match and every window that looks like it.
    BpFind,
    /// Print Big Picture appear/close events until Ctrl+C.
    BpWatch,
}

#[cfg(not(windows))]
pub fn run(_cmd: ShellCmd) -> Result<(), Box<dyn std::error::Error>> {
    Err("shell commands only work on Windows".into())
}

#[cfg(windows)]
pub fn run(cmd: ShellCmd) -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use uc_core::{Config, Paths};
    use uc_shell::explorer;
    use uc_shell::{BigPictureWatcher, BpEvent, WindowMatcher, steam};

    let config = Config::load(&Paths::from_env()?.config_file())?;
    let matcher = WindowMatcher::from_config(&config.steam);

    match cmd {
        ShellCmd::Status => {
            println!("shell (taskbar) running: {}", explorer::shell_running());
            println!("explorer.exe pids:       {:?}", explorer::explorer_pids()?);
            match steam::find_steam_exe(config.steam.exe_path.as_deref()) {
                Ok(p) => println!("steam.exe:               {}", p.display()),
                Err(e) => println!("steam.exe:               {e}"),
            }
            println!("steam running:           {}", steam::steam_running()?);
            match steam::find_big_picture(&matcher)? {
                Some(w) => println!("Big Picture window:      {w:?}"),
                None => println!("Big Picture window:      none"),
            }
        }
        ShellCmd::KillExplorer {
            timeout_secs,
            watch_secs,
            restore,
        } => {
            println!("Escape hatch: Ctrl+Shift+Esc → Run new task → explorer");
            let report = explorer::exit_explorer(Duration::from_secs(timeout_secs))?;
            println!("{report:#?}");
            if watch_secs > 0 {
                println!("watching {watch_secs} s for Explorer coming back by itself…");
                let start = Instant::now();
                if explorer::wait_for_shell(Duration::from_secs(watch_secs)) {
                    println!(
                        "RESULT: Explorer restarted by itself after {:.1} s",
                        start.elapsed().as_secs_f32()
                    );
                } else {
                    let pids = explorer::explorer_pids()?;
                    println!(
                        "RESULT: no taskbar after {watch_secs} s (explorer.exe pids: {pids:?})"
                    );
                }
            }
            if restore {
                print_start(explorer::start_explorer(Duration::from_secs(10))?);
            }
        }
        ShellCmd::StartExplorer { wait_secs } => {
            print_start(explorer::start_explorer(Duration::from_secs(wait_secs))?);
        }
        ShellCmd::BpLaunch => {
            let start = Instant::now();
            let method = steam::launch_big_picture(&config.steam)?;
            println!("launched: {method:?}");
            let timeout = Duration::from_secs(config.steam.bp_launch_timeout_secs.into());
            match steam::wait_for_big_picture(&matcher, timeout)? {
                Some(w) => println!(
                    "Big Picture up after {:.1} s: {w:?}",
                    start.elapsed().as_secs_f32()
                ),
                None => {
                    println!("no Big Picture window within {timeout:?}; run `shell bp-find`");
                    return Err("Big Picture window not found".into());
                }
            }
        }
        ShellCmd::BpClose => {
            if steam::close_big_picture(&config.steam)? {
                println!("close request sent");
            } else {
                println!("Steam is not running");
            }
        }
        ShellCmd::BpFind => {
            println!(
                "matcher: class {:?}, title {:?}",
                matcher.class, matcher.title
            );
            let windows = uc_shell::list_windows()?;
            let names: HashMap<u32, String> = uc_shell::list_processes()?
                .into_iter()
                .map(|p| (p.pid, p.name))
                .collect();
            match matcher.find(&windows) {
                Some(w) => println!("MATCH: {w:?}"),
                None => println!("MATCH: none"),
            }
            println!(
                "candidates (class matches, or title mentions Big Picture, or Steam's own windows):"
            );
            for w in &windows {
                let process = names.get(&w.pid).map(String::as_str).unwrap_or("?");
                let steamish = process.to_ascii_lowercase().starts_with("steam");
                if matcher.class_matches(&w.class)
                    || w.title.to_lowercase().contains("big picture")
                    || (steamish && w.visible)
                {
                    println!(
                        "  {:>10x}  {process:<20} visible={:<5} class={:?} title={:?}",
                        w.hwnd, w.visible, w.class, w.title
                    );
                }
            }
        }
        ShellCmd::BpWatch => {
            let debounce = Duration::from_millis(config.steam.bp_close_debounce_ms.into());
            let start = Instant::now();
            let (tx, rx) = std::sync::mpsc::channel();
            let _watcher = BigPictureWatcher::start(matcher, debounce, move |e| {
                let _ = tx.send(e);
            })?;
            println!("watching (debounce {debounce:?}); Ctrl+C to stop");
            for event in rx {
                let t = start.elapsed().as_secs_f32();
                match event {
                    BpEvent::Appeared(w) => println!("[{t:>7.1} s] APPEARED {w:?}"),
                    BpEvent::Closed => println!("[{t:>7.1} s] CLOSED"),
                }
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn print_start(report: uc_shell::explorer::StartReport) {
    use uc_shell::explorer::StartReport;
    match report {
        StartReport::AlreadyRunning => println!("Explorer was already running"),
        StartReport::BecameShell { elapsed } => {
            println!("an existing explorer.exe became the shell after {elapsed:?}")
        }
        StartReport::Launched {
            pid,
            shell_ready,
            elapsed,
        } => println!(
            "started explorer.exe (pid {pid}); taskbar up: {shell_ready} after {elapsed:?}"
        ),
    }
}
