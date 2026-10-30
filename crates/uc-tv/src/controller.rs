//! [`TvController`]: wake, sleep, state, run and pair, with the timeouts and
//! retry policy from `config.tv`. Holds one persistent ADB connection so the
//! sleep command on suspend goes out immediately (plan §2.6, §2.7).
//!
//! The controller has no policy: it never decides *whether* to wake or sleep
//! the TV. The agent does (gaming mode only).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use uc_core::config::{Tv as TvConfig, parse_mac};

use crate::adb::{AdbConnection, AdbKey, ConnectOptions};
use crate::{Result, TvError, arp, wol};

/// Time allowed for one shell command (`dumpsys` can be slow on a busy TV).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Time allowed to check that a held connection still works.
const PING_TIMEOUT: Duration = Duration::from_secs(2);
/// How often to repeat the WoL packet while waiting for the TV.
const WOL_REPEAT: Duration = Duration::from_secs(3);
/// Backoff between connection attempts while the TV wakes up.
const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(2);

pub const CMD_WAKE: &str = "input keyevent KEYCODE_WAKEUP";
pub const CMD_SLEEP: &str = "input keyevent KEYCODE_SLEEP";
pub const CMD_STATE: &str = "dumpsys power | grep mWakefulness=";

/// Power state reported by the TV (`mWakefulness`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TvState {
    Awake,
    Asleep,
    Dozing,
    Dreaming,
    /// No answer on the network: off, in deep sleep, or not connected.
    Unreachable,
    /// Some other or unparseable answer, kept as-is.
    Unknown(String),
}

impl TvState {
    /// Parses `dumpsys power` output.
    pub fn parse(output: &str) -> Self {
        let Some(value) = output
            .lines()
            .find_map(|l| l.trim().strip_prefix("mWakefulness="))
            .map(str::trim)
        else {
            return Self::Unknown(output.trim().chars().take(200).collect());
        };
        match value {
            "Awake" => Self::Awake,
            "Asleep" => Self::Asleep,
            "Dozing" => Self::Dozing,
            "Dreaming" => Self::Dreaming,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn is_on(&self) -> bool {
        matches!(self, Self::Awake | Self::Dreaming)
    }
}

impl std::fmt::Display for TvState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Awake => f.write_str("awake"),
            Self::Asleep => f.write_str("asleep"),
            Self::Dozing => f.write_str("dozing"),
            Self::Dreaming => f.write_str("dreaming (screensaver)"),
            Self::Unreachable => f.write_str("unreachable"),
            Self::Unknown(s) => write!(f, "unknown ({s})"),
        }
    }
}

/// What [`TvController::wake`] did, for logs and the settings app.
#[derive(Debug, Clone, Default)]
pub struct WakeReport {
    /// Magic packets sent (0 if WoL is off or no MAC is known).
    pub wol_packets: u32,
    /// ADB connection attempts until one worked.
    pub connect_attempts: u32,
    pub elapsed: Duration,
    /// `after_wake_commands` that failed, with the error. They don't fail the wake.
    pub after_wake_failures: Vec<(String, String)>,
}

/// What [`TvController::pair`] learned about the TV.
#[derive(Debug, Clone, Default)]
pub struct PairInfo {
    pub addr: Option<SocketAddr>,
    pub manufacturer: String,
    pub model: String,
    pub android_version: String,
    /// For `config.tv.mac`, as `AA:BB:CC:DD:EE:FF`.
    pub mac: Option<String>,
    /// Why the MAC could not be found, if it wasn't.
    pub mac_error: Option<String>,
}

pub struct TvController {
    config: TvConfig,
    key: Arc<AdbKey>,
    conn: Option<AdbConnection>,
}

impl TvController {
    pub fn new(config: TvConfig, key: Arc<AdbKey>) -> Self {
        Self {
            config,
            key,
            conn: None,
        }
    }

    pub fn config(&self) -> &TvConfig {
        &self.config
    }

    /// Applies a new config (hot reload). Drops the connection if the TV's
    /// address changed.
    pub fn set_config(&mut self, config: TvConfig) {
        if (config.ip, config.adb_port) != (self.config.ip, self.config.adb_port) {
            self.disconnect();
        }
        self.config = config;
    }

    pub fn addr(&self) -> Result<SocketAddr> {
        let ip = self.config.ip.ok_or(TvError::NoAddress)?;
        Ok(SocketAddr::new(ip, self.config.adb_port))
    }

    pub fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    /// Drops the held connection (leaving gaming mode).
    pub fn disconnect(&mut self) {
        if self.conn.take().is_some() {
            tracing::debug!("ADB connection closed");
        }
    }

    fn connect_options(&self) -> ConnectOptions {
        let connect_timeout = Duration::from_millis(self.config.connect_timeout_ms.max(1) as u64);
        ConnectOptions {
            connect_timeout,
            io_timeout: connect_timeout.max(Duration::from_secs(3)),
            pairing_wait: None,
        }
    }

    /// Opens the persistent connection if there isn't one.
    pub fn connect(&mut self) -> Result<&mut AdbConnection> {
        if self.conn.is_none() {
            let addr = self.addr()?;
            let conn = AdbConnection::connect(addr, &self.key, &self.connect_options())?;
            self.conn = Some(conn);
        }
        Ok(self.conn.as_mut().expect("just connected"))
    }

    /// Runs a shell command on the TV. A held connection that turns out to be
    /// dead is replaced once.
    pub fn run(&mut self, cmd: &str) -> Result<String> {
        self.run_with_timeout(cmd, COMMAND_TIMEOUT)
    }

    fn run_with_timeout(&mut self, cmd: &str, timeout: Duration) -> Result<String> {
        let reused = self.conn.is_some();
        match self.connect()?.shell(cmd, timeout) {
            Ok(out) => Ok(out),
            Err(e) => {
                self.disconnect();
                if !(reused && e.is_retryable()) {
                    return Err(e);
                }
                tracing::debug!(error = %e, "held ADB connection is dead, reconnecting");
                let result = self.connect()?.shell(cmd, timeout);
                if result.is_err() {
                    self.disconnect();
                }
                result
            }
        }
    }

    /// Checks the held connection (if any) and drops it if it no longer
    /// answers, then reconnects if needed. Call it every half minute or so in
    /// gaming mode, so a sleep command on suspend doesn't land on a dead
    /// socket. An error means no working connection is held now.
    pub fn keepalive(&mut self) -> Result<()> {
        if !self.ping_held(PING_TIMEOUT) {
            self.connect()?;
        }
        Ok(())
    }

    /// True if a held connection answered a trivial command in time.
    fn ping_held(&mut self, timeout: Duration) -> bool {
        let Some(conn) = self.conn.as_mut() else {
            return false;
        };
        match conn.shell("echo uc", timeout) {
            Ok(out) if out.trim() == "uc" => true,
            other => {
                tracing::debug!(result = ?other, "held ADB connection failed ping");
                self.disconnect();
                false
            }
        }
    }

    /// Current power state. `Ok(Unreachable)` if the TV does not answer on the
    /// network; an error only for problems that need the user (not paired,
    /// no IP set).
    pub fn state(&mut self) -> Result<TvState> {
        match self.run(CMD_STATE) {
            Ok(out) => Ok(TvState::parse(&out)),
            Err(e) if e.is_retryable() => {
                tracing::debug!(error = %e, "TV unreachable");
                Ok(TvState::Unreachable)
            }
            Err(e) => Err(e),
        }
    }

    /// Wakes the TV: Wake-on-LAN first (if on and the MAC is known), then
    /// keep trying ADB with backoff for up to `wake_retry_secs` (repeating the
    /// magic packet every few seconds), then `KEYCODE_WAKEUP` and the
    /// `after_wake_commands`. Safe to call when the TV is already on.
    pub fn wake(&mut self) -> Result<WakeReport> {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(self.config.wake_retry_secs as u64);
        let mut report = WakeReport::default();
        self.addr()?;

        let mac = self.wol_mac();
        let (wol_address, wol_port, tv_ip) = (
            self.config.wol_address,
            self.config.wol_port,
            self.config.ip,
        );
        let send_wol = |report: &mut WakeReport| {
            if let Some(mac) = mac {
                match wol::send(mac, wol_address, wol_port, tv_ip) {
                    Ok(()) => report.wol_packets += 1,
                    Err(e) => tracing::warn!(error = %e, "Wake-on-LAN send failed"),
                }
            }
        };
        send_wol(&mut report);
        let mut next_wol = start + WOL_REPEAT;

        // A connection held from before may be stale (TV slept, Wi-Fi dropped).
        self.ping_held(PING_TIMEOUT);

        let mut backoff = BACKOFF_START;
        loop {
            report.connect_attempts += 1;
            match self.connect() {
                Ok(_) => break,
                Err(e) if e.is_retryable() && Instant::now() < deadline => {
                    tracing::debug!(attempt = report.connect_attempts, error = %e, "TV not up yet");
                    let left = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(backoff.min(left));
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    if Instant::now() >= next_wol {
                        send_wol(&mut report);
                        next_wol += WOL_REPEAT;
                    }
                }
                Err(e) => {
                    tracing::warn!(attempts = report.connect_attempts, error = %e, "TV did not come up");
                    return Err(e);
                }
            }
        }

        self.run(CMD_WAKE)?;
        for cmd in self.config.after_wake_commands.clone() {
            if let Err(e) = self.run(&cmd) {
                tracing::warn!(%cmd, error = %e, "after-wake command failed");
                report.after_wake_failures.push((cmd, e.to_string()));
            }
        }
        report.elapsed = start.elapsed();
        tracing::info!(
            elapsed_ms = report.elapsed.as_millis() as u64,
            attempts = report.connect_attempts,
            wol = report.wol_packets,
            "TV woken"
        );
        Ok(report)
    }

    /// The MAC to use for WoL, if WoL is on and the MAC is set and valid.
    fn wol_mac(&self) -> Option<[u8; 6]> {
        if !self.config.wol_enabled {
            return None;
        }
        let mac = self.config.mac.as_deref().and_then(parse_mac);
        if mac.is_none() {
            tracing::warn!("Wake-on-LAN is on but no MAC is set; pair the TV to fill it in");
        }
        mac
    }

    /// Sends one magic packet (for testing WoL on its own).
    pub fn send_wol(&self) -> Result<()> {
        let mac = self
            .config
            .mac
            .as_deref()
            .and_then(parse_mac)
            .ok_or(TvError::NoMac)?;
        wol::send(
            mac,
            self.config.wol_address,
            self.config.wol_port,
            self.config.ip,
        )?;
        Ok(())
    }

    /// Puts the TV to sleep (`KEYCODE_SLEEP`, which does nothing if it
    /// already sleeps).
    pub fn sleep(&mut self) -> Result<()> {
        self.run(CMD_SLEEP)?;
        tracing::info!("TV sent to sleep");
        Ok(())
    }

    /// Sleep for the suspend handler: never takes longer than `budget`.
    /// Uses the held connection if there is one (the fast path the agent
    /// keeps warm with [`keepalive`](Self::keepalive)); otherwise tries a
    /// fresh connection within what is left of the budget.
    pub fn sleep_within(&mut self, budget: Duration) -> Result<()> {
        let start = Instant::now();
        let left = || budget.saturating_sub(start.elapsed());
        let mut result = Err(TvError::Timeout("the suspend budget"));
        // Second try only if the held connection was dead and time is left.
        for _ in 0..2 {
            if left().is_zero() {
                break;
            }
            let reused = self.conn.is_some();
            result = self.sleep_once(left());
            if result.is_ok() || !reused {
                break;
            }
            self.disconnect();
        }
        let elapsed_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok(()) => {
                tracing::info!(elapsed_ms, "TV sent to sleep (suspend)");
                Ok(())
            }
            Err(e) => {
                self.disconnect();
                tracing::warn!(elapsed_ms, error = %e, "TV sleep on suspend failed");
                Err(e)
            }
        }
    }

    fn sleep_once(&mut self, budget: Duration) -> Result<()> {
        let start = Instant::now();
        if self.conn.is_none() {
            let addr = self.addr()?;
            let mut opts = self.connect_options();
            opts.connect_timeout = opts.connect_timeout.min(budget);
            opts.io_timeout = budget;
            self.conn = Some(AdbConnection::connect(addr, &self.key, &opts)?);
        }
        let left = budget.saturating_sub(start.elapsed());
        if left.is_zero() {
            return Err(TvError::Timeout("the suspend budget"));
        }
        let conn = self.conn.as_mut().expect("connected above");
        conn.shell(CMD_SLEEP, left).map(drop)
    }

    /// Pairs with the TV at the configured address: connects, and if the
    /// TV doesn't know this PC yet, waits up to `wait` for the user to pick
    /// "Allow" (and "Always allow") on the TV. Then reads the model and
    /// looks up the MAC for Wake-on-LAN. Saving the result to the config is
    /// the caller's job. Keeps the connection open.
    pub fn pair(&mut self, wait: Duration) -> Result<PairInfo> {
        let addr = self.addr()?;
        self.disconnect();
        let opts = ConnectOptions {
            pairing_wait: Some(wait),
            ..self.connect_options()
        };
        self.conn = Some(AdbConnection::connect(addr, &self.key, &opts)?);

        let mut info = PairInfo {
            addr: Some(addr),
            ..PairInfo::default()
        };
        let mut prop = |name: &str| -> Result<String> {
            Ok(self.run(&format!("getprop {name}"))?.trim().to_string())
        };
        info.manufacturer = prop("ro.product.manufacturer")?;
        info.model = prop("ro.product.model")?;
        info.android_version = prop("ro.build.version.release")?;

        match arp::mac_for_ip(addr.ip()) {
            Ok(mac) => info.mac = Some(arp::format_mac(mac)),
            Err(e) => info.mac_error = Some(e.to_string()),
        }
        tracing::info!(?info, "TV paired");
        Ok(info)
    }
}

/// Convenience for the probe and settings app: the IP to use, from an
/// explicit override or the config.
pub fn pick_ip(override_ip: Option<IpAddr>, config: &TvConfig) -> Result<IpAddr> {
    override_ip.or(config.ip).ok_or(TvError::NoAddress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::client::tests::{FakeTv, Trust, echo};
    use crate::adb::key::tests::test_key;

    /// Shares the generated test key.
    fn key() -> Arc<AdbKey> {
        Arc::new(AdbKey::from_pem(&test_key().to_pem().unwrap()).unwrap())
    }

    fn config_for(addr: SocketAddr) -> TvConfig {
        TvConfig {
            enabled: true,
            ip: Some(addr.ip()),
            adb_port: addr.port(),
            // Point WoL at a harmless local port.
            wol_address: IpAddr::from([127, 0, 0, 1]),
            wol_port: 9,
            wake_retry_secs: 2,
            connect_timeout_ms: 500,
            ..TvConfig::default()
        }
    }

    fn tv_answers(cmd: &str) -> String {
        match cmd {
            CMD_STATE => "  mWakefulness=Asleep\n".into(),
            "echo uc" => "uc\n".into(),
            c if c.starts_with("getprop") => match c {
                "getprop ro.product.manufacturer" => "IRIS\n".into(),
                "getprop ro.product.model" => "IRIS 55 Android TV\n".into(),
                _ => "11\n".into(),
            },
            _ => String::new(),
        }
    }

    #[test]
    fn state_parsing() {
        assert_eq!(TvState::parse("  mWakefulness=Awake\n"), TvState::Awake);
        assert_eq!(TvState::parse("mWakefulness=Asleep"), TvState::Asleep);
        assert_eq!(TvState::parse("mWakefulness=Dozing\r"), TvState::Dozing);
        assert_eq!(
            TvState::parse("mWakefulness=Odd"),
            TvState::Unknown("Odd".into())
        );
        assert!(matches!(
            TvState::parse("grep: not found"),
            TvState::Unknown(_)
        ));
        assert!(TvState::Dreaming.is_on());
        assert!(!TvState::Unreachable.is_on());
    }

    #[test]
    fn wake_sleep_state_against_fake_tv() {
        let tv = FakeTv::start(Trust::Known, tv_answers);
        let mut cfg = config_for(tv.addr);
        cfg.mac = Some("AA:BB:CC:DD:EE:FF".into());
        cfg.after_wake_commands = vec!["am start -a android.intent.action.MAIN".into()];
        let mut c = TvController::new(cfg, key());

        let report = c.wake().unwrap();
        assert_eq!(report.connect_attempts, 1);
        assert_eq!(report.wol_packets, 1);
        assert!(report.after_wake_failures.is_empty());
        assert_eq!(c.state().unwrap(), TvState::Asleep);
        c.sleep().unwrap();
        c.keepalive().unwrap();
        c.sleep_within(Duration::from_millis(1500)).unwrap();
        assert!(c.is_connected());

        assert_eq!(
            tv.commands(),
            [
                CMD_WAKE,
                "am start -a android.intent.action.MAIN",
                CMD_STATE,
                CMD_SLEEP,
                "echo uc",
                CMD_SLEEP,
            ]
        );
    }

    #[test]
    fn wake_retries_until_tv_comes_up() {
        // Reserve a port, start the TV on it only after a delay.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let mut c = TvController::new(config_for(addr), key());
        let starter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            let listener = std::net::TcpListener::bind(addr).unwrap();
            FakeTv::start_on(listener, Trust::Known, tv_answers)
        });
        let report = c.wake().unwrap();
        assert!(report.connect_attempts > 1, "{report:?}");
        assert_eq!(report.wol_packets, 0, "no MAC configured");
        c.disconnect();
        starter.join().unwrap();
    }

    #[test]
    fn unreachable_and_unpaired() {
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut c = TvController::new(config_for(addr), key());
        assert_eq!(c.state().unwrap(), TvState::Unreachable);
        let start = Instant::now();
        assert!(c.wake().unwrap_err().is_retryable());
        assert!(start.elapsed() < Duration::from_secs(5));

        let tv = FakeTv::start(Trust::AcceptOnPrompt, echo);
        let mut c = TvController::new(config_for(tv.addr), key());
        // Not paired: an error for the user, not "unreachable", and no retry loop.
        assert!(matches!(c.state(), Err(TvError::NotAuthorized)));
        assert!(matches!(c.wake(), Err(TvError::NotAuthorized)));

        let mut none = TvController::new(TvConfig::default(), key());
        assert!(matches!(none.wake(), Err(TvError::NoAddress)));
        assert!(matches!(none.send_wol(), Err(TvError::NoMac)));
    }

    #[test]
    fn pairing_reads_device_info() {
        let tv = FakeTv::start(Trust::AcceptOnPrompt, tv_answers);
        let mut c = TvController::new(config_for(tv.addr), key());
        let info = c.pair(Duration::from_secs(5)).unwrap();
        assert_eq!(info.manufacturer, "IRIS");
        assert_eq!(info.model, "IRIS 55 Android TV");
        assert_eq!(info.android_version, "11");
        // Loopback has no MAC; the lookup failure is reported, not fatal.
        assert!(info.mac.is_some() || info.mac_error.is_some());
        assert!(c.is_connected());
    }

    #[test]
    fn dead_held_connection_is_replaced() {
        let tv = FakeTv::start(Trust::Known, tv_answers);
        let mut c = TvController::new(config_for(tv.addr), key());
        c.connect().unwrap().kill();
        c.sleep().unwrap();
        assert!(c.is_connected());
        assert_eq!(tv.commands(), [CMD_SLEEP]);

        // The suspend path replaces a dead held connection within the budget.
        c.conn.as_ref().unwrap().kill();
        c.sleep_within(Duration::from_millis(1500)).unwrap();
        assert_eq!(tv.commands(), [CMD_SLEEP, CMD_SLEEP]);

        // With nobody listening it gives up inside the budget.
        let start = Instant::now();
        let gone = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut d = TvController::new(config_for(gone), key());
        assert!(d.sleep_within(Duration::from_millis(500)).is_err());
        assert!(!d.is_connected());
        assert!(start.elapsed() < Duration::from_secs(2));

        let mut cfg = c.config().clone();
        cfg.adb_port += 1;
        c.set_config(cfg);
        assert!(!c.is_connected());
    }

    #[test]
    fn ip_override() {
        let cfg = TvConfig::default();
        assert!(matches!(pick_ip(None, &cfg), Err(TvError::NoAddress)));
        let ip: IpAddr = "192.168.1.5".parse().unwrap();
        assert_eq!(pick_ip(Some(ip), &cfg).unwrap(), ip);
    }
}
