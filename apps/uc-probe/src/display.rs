//! `uc-probe display ...` (WP-01).

use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use clap::{Args, Subcommand};
use uc_display::{DisplayId, DisplayMode, Topology, identity};

/// File `switch` writes the old layout to, in the current directory.
const BEFORE_SWITCH: &str = "display-before-switch.json";

#[derive(Subcommand)]
pub enum DisplayCmd {
    /// List connected displays with their number, identity and modes.
    List {
        /// Print the JSON the agent sends to the settings app.
        #[arg(long)]
        json: bool,
    },
    /// Print the current layout as JSON (what goes into state.json).
    Snapshot {
        /// Write it to a file instead of printing.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Make one display the only (primary) one. Saves the old layout to
    /// `display-before-switch.json` first and reverts after a few seconds
    /// unless you press Enter.
    Switch {
        #[command(flatten)]
        target: Target,
        /// Resolution width to set, e.g. 3840 (needs --height).
        #[arg(long, requires = "height")]
        width: Option<u32>,
        /// Resolution height to set, e.g. 2160 (needs --width).
        #[arg(long, requires = "width")]
        height: Option<u32>,
        /// Refresh rate in Hz, e.g. 60 or 59.94 (needs --width/--height).
        #[arg(long, requires = "width")]
        refresh: Option<f64>,
        /// Turn HDR on.
        #[arg(long)]
        hdr: bool,
        /// Wait up to this many seconds for the display to become available.
        #[arg(long, default_value_t = 0)]
        wait: u64,
        /// Seconds before reverting unless Enter is pressed. 0 keeps the new
        /// layout without asking.
        #[arg(long, default_value_t = 15)]
        revert_after: u64,
    },
    /// Put back a layout saved by `snapshot --out` or `switch`.
    Restore { file: PathBuf },
    /// Wait until a display is connected and available.
    Wait {
        #[command(flatten)]
        target: Target,
        #[arg(long, default_value_t = 15)]
        timeout: u64,
    },
    /// Show each active display's number on it.
    Identify {
        #[arg(long, default_value_t = 3)]
        secs: u64,
    },
}

#[derive(Args)]
pub struct Target {
    /// Number from `display list`, a monitor device path (`\\?\DISPLAY#...`)
    /// or a friendly name (e.g. "IRIS TV").
    id: String,
}

impl Target {
    fn resolve(&self) -> Result<DisplayId, Box<dyn std::error::Error>> {
        if let Ok(n) = self.id.parse::<u32>() {
            return uc_display::list()?
                .into_iter()
                .find(|d| d.index == n)
                .map(|d| d.id)
                .ok_or_else(|| format!("no display number {n}; see `display list`").into());
        }
        Ok(if self.id.starts_with(r"\\?\") {
            DisplayId {
                device_path: self.id.clone(),
                ..DisplayId::default()
            }
        } else {
            DisplayId {
                friendly_name: self.id.clone(),
                ..DisplayId::default()
            }
        })
    }
}

pub fn run(cmd: DisplayCmd) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match cmd {
        DisplayCmd::List { json } => {
            let displays = uc_display::list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&displays)?);
                return Ok(ExitCode::SUCCESS);
            }
            for d in &displays {
                let mut flags = Vec::new();
                if d.active {
                    flags.push("active");
                }
                if d.primary {
                    flags.push("primary");
                }
                if d.hdr_supported {
                    flags.push("HDR");
                }
                println!(
                    "{}. {}  [{}]",
                    d.index,
                    identity::describe(&d.id),
                    flags.join(", ")
                );
                if let Some(m) = &d.current_mode {
                    println!("   mode:  {}", mode_label(m));
                }
                println!("   path:  {}", d.id.device_path);
                if !d.modes.is_empty() {
                    let shown: Vec<_> = d.modes.iter().take(8).map(mode_label).collect();
                    let more = d.modes.len().saturating_sub(shown.len());
                    let tail = if more > 0 {
                        format!(" (+{more} more)")
                    } else {
                        String::new()
                    };
                    println!("   modes: {}{tail}", shown.join(", "));
                }
            }
        }
        DisplayCmd::Snapshot { out } => {
            let t = uc_display::snapshot()?;
            match out {
                Some(path) => {
                    std::fs::write(&path, t.to_json())?;
                    println!("saved {} display(s) to {}", t.paths.len(), path.display());
                    for line in t.summary() {
                        println!("  {line}");
                    }
                }
                None => println!("{}", t.to_json()),
            }
        }
        DisplayCmd::Switch {
            target,
            width,
            height,
            refresh,
            hdr,
            wait,
            revert_after,
        } => {
            let id = target.resolve()?;
            let id = if wait > 0 {
                println!("waiting up to {wait} s for {} ...", identity::describe(&id));
                uc_display::wait_available(&id, Duration::from_secs(wait))?
            } else {
                id
            };

            let before = uc_display::snapshot()?;
            std::fs::write(BEFORE_SWITCH, before.to_json())?;
            println!("old layout saved to {BEFORE_SWITCH}");

            let mode = width.zip(height).map(|(width, height)| DisplayMode {
                width,
                height,
                refresh_hz: refresh,
            });
            let outcome = uc_display::apply_single(&id, mode.as_ref(), hdr)?;
            println!("switched to {}", identity::describe(&outcome.display));
            for w in &outcome.warnings {
                println!("  warning: {w}");
            }

            if revert_after > 0 && !confirmed_within(Duration::from_secs(revert_after)) {
                let how = uc_display::restore(&before)?;
                println!("reverted ({how:?})");
            }
        }
        DisplayCmd::Restore { file } => {
            let t = Topology::from_json(&std::fs::read_to_string(&file)?)?;
            let how = uc_display::restore(&t)?;
            println!("restored ({how:?})");
            for line in t.summary() {
                println!("  {line}");
            }
        }
        DisplayCmd::Wait { target, timeout } => {
            let id = target.resolve()?;
            let found = uc_display::wait_available(&id, Duration::from_secs(timeout))?;
            println!("available: {}", identity::describe(&found));
        }
        DisplayCmd::Identify { secs } => uc_display::identify(Duration::from_secs(secs))?,
    }
    Ok(ExitCode::SUCCESS)
}

fn mode_label(m: &DisplayMode) -> String {
    match m.refresh_hz {
        Some(hz) => format!("{}x{} @ {hz} Hz", m.width, m.height),
        None => format!("{}x{}", m.width, m.height),
    }
}

/// Like Windows' "Keep these display settings?": true if Enter is pressed in time.
fn confirmed_within(timeout: Duration) -> bool {
    println!(
        "press Enter within {} s to keep this layout, otherwise it reverts",
        timeout.as_secs()
    );
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_ok() {
            let _ = tx.send(());
        }
    });
    rx.recv_timeout(timeout).is_ok()
}
