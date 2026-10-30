//! `uc-probe power …` (WP-05): watch suspend/resume/display events and measure
//! the suspend budget and how long the network takes to come back.

use std::net::Ipv4Addr;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use clap::Subcommand;
use uc_core::Paths;
use uc_core::logging::{self, LogOptions};
use uc_power::{AdapterInfo, PowerEvent, PowerWatcher, network_ready, network_snapshot};

#[derive(Subcommand)]
pub enum PowerCmd {
    /// Log power events with timestamps and network state until Ctrl+C.
    /// Sleep and wake the PC while it runs.
    Watch {
        /// Keep the suspend handler busy this long (logging every 100 ms) to
        /// see how long Windows waits before it really sleeps.
        #[arg(long, default_value_t = 0)]
        hold_suspend_ms: u64,
        /// The TV's IP: "network ready" then means "on the TV's subnet".
        #[arg(long)]
        tv_ip: Option<Ipv4Addr>,
        /// How long to follow the network after a resume.
        #[arg(long, default_value_t = 30)]
        net_timeout_s: u64,
    },
    /// Print the network adapters and whether the network counts as ready.
    Net {
        #[arg(long)]
        tv_ip: Option<Ipv4Addr>,
    },
}

pub fn run(cmd: PowerCmd) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match cmd {
        PowerCmd::Net { tv_ip } => {
            let adapters = network_snapshot()?;
            for a in &adapters {
                println!("{a}");
            }
            let ready = network_ready(&adapters, tv_ip);
            println!("network ready: {ready}");
            Ok(if ready {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        PowerCmd::Watch {
            hold_suspend_ms,
            tv_ip,
            net_timeout_s,
        } => watch(
            Duration::from_millis(hold_suspend_ms),
            tv_ip,
            Duration::from_secs(net_timeout_s),
        ),
    }
}

fn watch(
    hold: Duration,
    tv_ip: Option<Ipv4Addr>,
    net_timeout: Duration,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let logs = Paths::from_env()?.logs_dir();
    let _guard = logging::init(
        &logs,
        &LogOptions {
            prefix: "probe-power",
            // Debug for uc-power shows repeated events the filter dropped.
            filter: "info,uc_power=debug",
            stderr: true,
        },
    )?;
    tracing::info!(log_dir = %logs.display(), "watching power events, Ctrl+C to stop");
    log_network("at start", tv_ip);

    let (tx, rx) = mpsc::channel::<(PowerEvent, Instant)>();
    let _watcher = PowerWatcher::start(move |event| {
        let at = Instant::now();
        if event == PowerEvent::Suspend {
            // Runs inside the suspend handler, so it is what the agent's
            // TV-sleep would get.
            log_network("at suspend", tv_ip);
            hold_suspend(hold);
        }
        let _ = tx.send((event, at));
    })?;

    for (event, at) in rx {
        if event == PowerEvent::ResumeAutomatic {
            follow_network(at, tv_ip, net_timeout);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Logs a heartbeat every 100 ms for `hold`. After resume, a jump in the wall
/// clock between two heartbeats shows when Windows stopped waiting.
fn hold_suspend(hold: Duration) {
    if hold.is_zero() {
        return;
    }
    let start = Instant::now();
    let wall = SystemTime::now();
    while start.elapsed() < hold {
        std::thread::sleep(Duration::from_millis(100));
        let wall_ms = wall.elapsed().map(|d| d.as_millis()).unwrap_or(0);
        tracing::info!(
            "suspend handler alive: +{} ms (wall clock +{wall_ms} ms)",
            start.elapsed().as_millis()
        );
    }
    tracing::info!("suspend handler returning");
}

/// After resume: logs every change in the adapters until the network is ready
/// or `timeout` passes, with the time since the resume event.
fn follow_network(resumed: Instant, tv_ip: Option<Ipv4Addr>, timeout: Duration) {
    let mut last: Option<Vec<AdapterInfo>> = None;
    loop {
        let ms = resumed.elapsed().as_millis();
        match network_snapshot() {
            Ok(adapters) => {
                if last.as_ref() != Some(&adapters) {
                    for a in &adapters {
                        tracing::info!("+{ms} ms  {a}");
                    }
                }
                if network_ready(&adapters, tv_ip) {
                    tracing::info!("network ready {ms} ms after resume");
                    return;
                }
                last = Some(adapters);
            }
            Err(e) => tracing::warn!("+{ms} ms  adapter read failed: {e}"),
        }
        if resumed.elapsed() >= timeout {
            tracing::warn!("network not ready {ms} ms after resume, giving up");
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn log_network(when: &str, tv_ip: Option<Ipv4Addr>) {
    match network_snapshot() {
        Ok(adapters) => {
            for a in &adapters {
                tracing::info!("network {when}: {a}");
            }
            tracing::info!(
                "network {when}: ready = {}",
                network_ready(&adapters, tv_ip)
            );
        }
        Err(e) => tracing::warn!("network {when}: adapter read failed: {e}"),
    }
}
