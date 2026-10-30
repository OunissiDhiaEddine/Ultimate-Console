//! `uc-probe tv …` (WP-02): discover, pair, wake and sleep the Android TV.

use std::net::IpAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Subcommand};
use uc_core::{Config, Paths};

#[derive(Args)]
pub struct TvArgs {
    /// TV address; defaults to `tv.ip` from the config.
    #[arg(long, global = true)]
    ip: Option<IpAddr>,
    #[command(subcommand)]
    cmd: TvCmd,
}

#[derive(Subcommand)]
pub enum TvCmd {
    /// List TVs that answer mDNS on the local network.
    Discover {
        #[arg(long, default_value_t = 3)]
        secs: u64,
    },
    /// Connect and authorise this PC (accept the prompt on the TV), read the
    /// model and the MAC for Wake-on-LAN.
    Pair {
        ip: IpAddr,
        #[arg(long)]
        port: Option<u16>,
        /// How long to wait for "Allow" on the TV.
        #[arg(long, default_value_t = 60)]
        wait_secs: u64,
        /// Write ip, port, MAC and `enabled = true` into the config file.
        #[arg(long)]
        save: bool,
    },
    /// Wake-on-LAN, then ADB wake with retries, then the after-wake commands.
    Wake,
    /// Put the TV to sleep.
    Sleep,
    /// Sleep with the suspend time budget (`power.suspend_budget_ms`).
    SleepFast,
    /// Print the TV's power state.
    State,
    /// Run a shell command on the TV, e.g. `tv run getprop ro.build.version.release`.
    Run {
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
    /// Send one Wake-on-LAN packet to `tv.mac` (or `--mac`).
    Wol {
        #[arg(long)]
        mac: Option<String>,
    },
    /// Look up the MAC address of an IP in the ARP table.
    Mac { ip: IpAddr },
}

pub fn run(args: TvArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let paths = Paths::from_env()?;
    let _log = uc_core::logging::init(
        &paths.logs_dir(),
        &uc_core::logging::LogOptions {
            prefix: "probe",
            filter: "uc_tv=debug,info",
            stderr: true,
        },
    )?;
    let mut config = Config::load(&paths.config_file())?;
    let mut tv = config.tv.clone();
    if let Some(ip) = args.ip {
        tv.ip = Some(ip);
    }

    // Commands that need no ADB key.
    match &args.cmd {
        TvCmd::Discover { secs } => {
            let found = uc_tv::mdns::discover(Duration::from_secs(*secs))?;
            if found.is_empty() {
                println!("nothing found (is the PC on the same network as the TV?)");
            }
            for d in found {
                let adb = d
                    .adb_port
                    .map_or("ADB not advertised".to_string(), |p| format!("ADB on {p}"));
                println!(
                    "{:<16} {}  [{adb}; {}]",
                    d.ip,
                    d.name,
                    d.services.join(", ")
                );
            }
            return Ok(ExitCode::SUCCESS);
        }
        TvCmd::Mac { ip } => {
            println!("{}", uc_tv::arp::format_mac(uc_tv::arp::mac_for_ip(*ip)?));
            return Ok(ExitCode::SUCCESS);
        }
        TvCmd::Wol { mac } => {
            let mac = mac
                .as_ref()
                .or(tv.mac.as_ref())
                .ok_or(uc_tv::TvError::NoMac)?;
            let mac =
                uc_core::config::parse_mac(mac).ok_or("bad MAC, expected AA:BB:CC:DD:EE:FF")?;
            uc_tv::wol::send(mac, tv.wol_address, tv.wol_port, tv.ip)?;
            println!(
                "magic packet sent to {} port {}",
                tv.wol_address, tv.wol_port
            );
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }

    let key = Arc::new(uc_tv::AdbKey::load_or_create(&paths.adb_key_file())?);
    match args.cmd {
        TvCmd::Pair {
            ip,
            port,
            wait_secs,
            save,
        } => {
            tv.ip = Some(ip);
            if let Some(port) = port {
                tv.adb_port = port;
            }
            println!(
                "connecting; if the TV asks \"Allow USB debugging?\", tick \"Always allow\" and press Allow"
            );
            let mut c = uc_tv::TvController::new(tv.clone(), key);
            let info = c.pair(Duration::from_secs(wait_secs))?;
            println!("paired:  {} {}", info.manufacturer, info.model);
            println!("android: {}", info.android_version);
            match (&info.mac, &info.mac_error) {
                (Some(mac), _) => println!("mac:     {mac}"),
                (None, Some(e)) => println!("mac:     not found ({e})"),
                _ => {}
            }
            if save {
                config.tv = uc_core::config::Tv {
                    enabled: true,
                    mac: info.mac.or(tv.mac.clone()),
                    ..tv
                };
                config.save(&paths.config_file())?;
                println!("saved to {}", paths.config_file().display());
            }
        }
        TvCmd::Wake => {
            let r = uc_tv::TvController::new(tv, key).wake()?;
            println!(
                "awake after {} ms ({} connect attempts, {} WoL packets)",
                r.elapsed.as_millis(),
                r.connect_attempts,
                r.wol_packets
            );
            for (cmd, err) in &r.after_wake_failures {
                println!("  after-wake command {cmd:?} failed: {err}");
            }
        }
        TvCmd::Sleep => {
            uc_tv::TvController::new(tv, key).sleep()?;
            println!("sleep sent");
        }
        TvCmd::SleepFast => {
            let budget = Duration::from_millis(config.power.suspend_budget_ms as u64);
            let mut c = uc_tv::TvController::new(tv, key);
            // Like the agent in gaming mode: connection already open.
            c.connect()?;
            let start = std::time::Instant::now();
            c.sleep_within(budget)?;
            println!(
                "sleep sent in {} ms (budget {} ms)",
                start.elapsed().as_millis(),
                budget.as_millis()
            );
        }
        TvCmd::State => {
            println!("{}", uc_tv::TvController::new(tv, key).state()?);
        }
        TvCmd::Run { cmd } => {
            print!("{}", uc_tv::TvController::new(tv, key).run(&cmd.join(" "))?);
        }
        TvCmd::Discover { .. } | TvCmd::Mac { .. } | TvCmd::Wol { .. } => unreachable!(),
    }
    Ok(ExitCode::SUCCESS)
}
