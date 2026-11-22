//! Answers the Quick Access Menu tab's calls.
//!
//! | Method            | Params            | Does |
//! |-------------------|-------------------|------|
//! | `state`           |                   | Everything the tab shows ([`state_json`]) |
//! | `setVolume`       | `level` 0..1      | Master volume |
//! | `setMuted`        | `muted`           | Master mute |
//! | `setAppVolume`    | `pid`, `level`    | One app's volume |
//! | `setAppMuted`     | `pid`, `muted`    | One app's mute |
//! | `setOutput`       | `id`              | Default output device |
//! | `switchTo`        | `hwnd`            | Bring a window to the front |
//! | `closeWindow`     | `hwnd`            | Ask a window to close |
//! | `endProcess`      | `pid`             | End a process |
//! | `rescanBatteries` |                   | Read the batteries again now |
//! | `leaveGaming`     |                   | Leave gaming mode ([`Host`]) |
//! | `log`             | `level`, `message`| Script diagnostics into the agent log |

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uc_audio::{AppSession, AudioControl, AudioDevice};
use uc_battery::{Device, Kind, Monitor};
use uc_overlay::model::{DisplayMode, Task};
use uc_sysinfo::{Snapshot, StatsSource, format_bytes};

/// Answers calls from the tab, on the bridge thread.
pub trait Handler {
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, String>;

    /// Called about once a second on the bridge thread.
    fn tick(&mut self) {}
}

/// Builds the handler on the bridge thread.
pub type HandlerFactory = Box<dyn FnOnce() -> Box<dyn Handler> + Send>;

/// What the agent tells the tab about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostStatus {
    /// In service-triggered gaming mode (offers "Leave gaming mode").
    pub gaming: bool,
    /// "Gaming", "Desktop" …
    pub mode: String,
    /// "Awake", "Asleep" …, or `None` when TV control is off.
    pub tv: Option<String>,
}

/// The agent's side: policy the crate doesn't decide.
pub trait Host: Send {
    fn status(&self) -> HostStatus;
    fn leave_gaming(&self) -> Result<(), String>;
}

/// Batteries are read only while the tab is being looked at: the monitor
/// stops this long after the last `state` call.
const BATTERY_IDLE_STOP: Duration = Duration::from_secs(60);

/// The real handler: Core Audio, the stats sampler, the battery monitor and
/// the desktop window helpers.
pub struct SystemHandler {
    host: Box<dyn Host>,
    /// Title of the Big Picture window, left out of the window list.
    bp_title: String,
    /// Built on first use and rebuilt after an error (devices come and go).
    audio: Option<Box<dyn AudioControl>>,
    stats: Box<dyn StatsSource>,
    battery: Option<Monitor>,
    scanner: uc_battery::Scanner,
    last_state: Option<Instant>,
}

impl SystemHandler {
    pub fn new(host: Box<dyn Host>, bp_title: impl Into<String>) -> Self {
        Self {
            host,
            bp_title: bp_title.into(),
            audio: None,
            stats: Box::new(uc_sysinfo::Sampler::new()),
            battery: None,
            scanner: uc_battery::real_scanner(),
            last_state: None,
        }
    }

    fn audio(&mut self) -> Result<&dyn AudioControl, String> {
        if self.audio.is_none() {
            let a = uc_audio::CoreAudio::new().map_err(|e| e.to_string())?;
            self.audio = Some(Box::new(a));
        }
        Ok(self.audio.as_deref().expect("set above"))
    }

    /// Runs an audio call; drops the client after an error so the next call
    /// starts fresh.
    fn with_audio<T>(
        &mut self,
        f: impl FnOnce(&dyn AudioControl) -> uc_audio::Result<T>,
    ) -> Result<T, String> {
        let r = f(self.audio()?).map_err(|e| e.to_string());
        if r.is_err() {
            self.audio = None;
        }
        r
    }

    fn start_battery(&mut self) {
        if self.battery.is_none() {
            match Monitor::start(self.scanner.clone(), uc_battery_interval(), || {}) {
                Ok(m) => self.battery = Some(m),
                Err(e) => tracing::warn!("battery monitor failed to start: {e}"),
            }
        }
    }

    fn state(&mut self) -> Value {
        self.last_state = Some(Instant::now());
        self.start_battery();
        let volume = self.with_audio(|a| Ok((a.volume()?, a.muted()?)));
        let apps = self.with_audio(|a| a.sessions()).unwrap_or_default();
        let outputs = self.with_audio(|a| a.outputs()).unwrap_or_default();
        let tasks: Vec<Task> = desktop::tasks()
            .into_iter()
            .filter(|t| t.title.trim() != self.bp_title)
            .collect();
        let pids: Vec<u32> = tasks.iter().map(|t| t.pid).collect();
        let snapshot = self.stats.sample(&pids);
        let batteries = self.battery.as_ref().and_then(Monitor::latest);
        state_json(&StateInputs {
            host: self.host.status(),
            volume,
            apps,
            outputs,
            tasks,
            snapshot,
            display: desktop::display_mode(),
            batteries,
        })
    }
}

fn uc_battery_interval() -> Duration {
    uc_overlay::BATTERY_INTERVAL
}

fn param<'a>(params: &'a Value, name: &str) -> Result<&'a Value, String> {
    params
        .get(name)
        .ok_or_else(|| format!("missing parameter {name:?}"))
}

fn level(params: &Value) -> Result<f32, String> {
    let v = param(params, "level")?
        .as_f64()
        .ok_or("level must be a number")?;
    Ok((v as f32).clamp(0.0, 1.0))
}

fn flag(params: &Value, name: &str) -> Result<bool, String> {
    param(params, name)?
        .as_bool()
        .ok_or_else(|| format!("{name} must be true or false"))
}

fn pid(params: &Value) -> Result<u32, String> {
    param(params, "pid")?
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .ok_or_else(|| "pid must be a process id".to_owned())
}

fn hwnd(params: &Value) -> Result<isize, String> {
    param(params, "hwnd")?
        .as_i64()
        .and_then(|h| isize::try_from(h).ok())
        .filter(|&h| h != 0)
        .ok_or_else(|| "hwnd must be a window handle".to_owned())
}

/// Only windows that are in the task list now may be switched to, closed
/// or ended: the page can't aim at arbitrary windows or processes.
fn known_task(hwnd: isize) -> Result<Task, String> {
    desktop::tasks()
        .into_iter()
        .find(|t| t.hwnd == hwnd)
        .ok_or_else(|| "that window is gone".to_owned())
}

impl Handler for SystemHandler {
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        match method {
            "state" => Ok(self.state()),
            "setVolume" => {
                let l = level(params)?;
                self.with_audio(|a| a.set_volume(l))?;
                Ok(Value::Null)
            }
            "setMuted" => {
                let m = flag(params, "muted")?;
                self.with_audio(|a| a.set_muted(m))?;
                Ok(Value::Null)
            }
            "setAppVolume" => {
                let (p, l) = (pid(params)?, level(params)?);
                self.with_audio(|a| a.set_app_volume(p, l))?;
                Ok(Value::Null)
            }
            "setAppMuted" => {
                let (p, m) = (pid(params)?, flag(params, "muted")?);
                self.with_audio(|a| a.set_app_muted(p, m))?;
                Ok(Value::Null)
            }
            "setOutput" => {
                let id = param(params, "id")?
                    .as_str()
                    .ok_or("id must be a string")?
                    .to_owned();
                self.with_audio(|a| a.set_default_output(&id))?;
                Ok(Value::Null)
            }
            "switchTo" => {
                let t = known_task(hwnd(params)?)?;
                tracing::info!(title = %t.title, "Steam menu: switching to window");
                desktop::switch_to(t.hwnd);
                Ok(Value::Null)
            }
            "closeWindow" => {
                let t = known_task(hwnd(params)?)?;
                tracing::info!(title = %t.title, "Steam menu: closing window");
                desktop::close(t.hwnd);
                Ok(Value::Null)
            }
            "endProcess" => {
                let p = pid(params)?;
                if !desktop::tasks().iter().any(|t| t.pid == p) {
                    return Err("that program is gone".into());
                }
                tracing::info!(pid = p, "Steam menu: ending process");
                desktop::kill(p);
                Ok(Value::Null)
            }
            "rescanBatteries" => {
                // A fresh monitor scans at once (and again after its warm-up).
                self.battery = None;
                self.start_battery();
                Ok(Value::Null)
            }
            "leaveGaming" => {
                tracing::info!("Steam menu: leave gaming mode");
                self.host.leave_gaming().map(|()| Value::Null)
            }
            "log" => {
                let level = params
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("info");
                let message = params.get("message").and_then(Value::as_str).unwrap_or("");
                match level {
                    "error" | "warn" => tracing::warn!("Steam menu script: {message}"),
                    _ => tracing::info!("Steam menu script: {message}"),
                }
                Ok(Value::Null)
            }
            other => Err(format!("unknown method {other:?}")),
        }
    }

    fn tick(&mut self) {
        if self.battery.is_some()
            && self
                .last_state
                .is_none_or(|t| t.elapsed() >= BATTERY_IDLE_STOP)
        {
            self.battery = None;
        }
    }
}

/// Everything [`state_json`] turns into the tab's data.
#[derive(Debug, Clone)]
pub struct StateInputs {
    pub host: HostStatus,
    /// Master `(level, muted)`, or why audio is unavailable.
    pub volume: Result<(f32, bool), String>,
    pub apps: Vec<AppSession>,
    pub outputs: Vec<AudioDevice>,
    pub tasks: Vec<Task>,
    pub snapshot: Snapshot,
    pub display: Option<DisplayMode>,
    /// `None` until the first battery read finishes.
    pub batteries: Option<Vec<Device>>,
}

impl Default for StateInputs {
    fn default() -> Self {
        Self {
            host: HostStatus::default(),
            volume: Err("not read".into()),
            apps: Vec::new(),
            outputs: Vec::new(),
            tasks: Vec::new(),
            snapshot: Snapshot::default(),
            display: None,
            batteries: None,
        }
    }
}

fn percent(p: Option<f32>) -> Value {
    p.map_or(Value::Null, |p| Value::from(format!("{:.0}%", p)))
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Controller => "controller",
        Kind::Headset => "headset",
        Kind::Mouse => "mouse",
        Kind::Keyboard => "keyboard",
        Kind::Other => "other",
    }
}

/// The `state` answer. Display strings are made here so the script stays
/// simple.
pub fn state_json(i: &StateInputs) -> Value {
    let (volume, volume_error) = match &i.volume {
        Ok((level, muted)) => (json!({ "level": level, "muted": muted }), Value::Null),
        Err(e) => (Value::Null, Value::from(e.as_str())),
    };
    let apps: Vec<Value> = i
        .apps
        .iter()
        .map(|a| {
            let name = if a.pid == 0 {
                "System sounds".to_owned()
            } else {
                a.name.trim_end_matches(".exe").to_owned()
            };
            json!({ "pid": a.pid, "name": name, "level": a.volume, "muted": a.muted })
        })
        .collect();
    let outputs: Vec<Value> = i
        .outputs
        .iter()
        .map(|d| json!({ "id": d.id, "name": d.name, "default": d.is_default }))
        .collect();
    let tasks: Vec<Value> = i
        .tasks
        .iter()
        .map(|t| {
            let stats = i.snapshot.process(t.pid);
            let mut detail = vec![
                stats
                    .and_then(|s| s.name.clone())
                    .unwrap_or_else(|| format!("pid {}", t.pid)),
            ];
            if let Some(s) = stats {
                if let Some(c) = s.cpu_percent {
                    detail.push(format!("CPU {c:.0}%"));
                }
                if let Some(g) = s.gpu_percent {
                    detail.push(format!("GPU {g:.0}%"));
                }
                detail.push(format_bytes(s.working_set_bytes));
            }
            json!({ "hwnd": t.hwnd, "pid": t.pid, "title": t.title, "detail": detail.join(" · ") })
        })
        .collect();
    let sys = &i.snapshot.system;
    let ram = (sys.memory_total_bytes > 0).then(|| {
        format!(
            "{} of {}",
            format_bytes(sys.memory_used_bytes),
            format_bytes(sys.memory_total_bytes)
        )
    });
    let display = i
        .display
        .map(|d| format!("{} · {}", d.resolution_text(), d.refresh_text()));
    let batteries = i.batteries.as_ref().map(|list| {
        list.iter()
            .map(|d| {
                json!({
                    "name": d.name,
                    "kind": kind_name(d.kind),
                    "level": d.level_text(),
                    "power": d.power_text(),
                    "fraction": d.fraction(),
                    "low": d.is_low(),
                })
            })
            .collect::<Vec<_>>()
    });
    json!({
        "host": {
            "gaming": i.host.gaming,
            "mode": i.host.mode,
            "tv": i.host.tv,
        },
        "volume": volume,
        "volumeError": volume_error,
        "apps": apps,
        "outputs": outputs,
        "tasks": tasks,
        "system": {
            "cpu": percent(sys.cpu_percent),
            "gpu": percent(sys.gpu_percent),
            "ram": ram,
        },
        "display": display,
        "batteries": batteries,
    })
}

/// The desktop helpers, as plain integers so the rest stays portable.
#[cfg(windows)]
mod desktop {
    use uc_overlay::desktop as sys;
    use uc_overlay::model::{DisplayMode, Task};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    fn hwnd(h: isize) -> HWND {
        HWND(h as *mut _)
    }

    pub fn tasks() -> Vec<Task> {
        sys::list_tasks(HWND::default())
    }

    pub fn switch_to(h: isize) {
        sys::switch_to(hwnd(h));
    }

    pub fn close(h: isize) {
        sys::close_window(hwnd(h));
    }

    pub fn kill(pid: u32) {
        sys::kill_process(pid);
    }

    /// Mode of the display Big Picture (the foreground window) is on.
    pub fn display_mode() -> Option<DisplayMode> {
        // SAFETY: plain FFI call with no arguments.
        let fg = unsafe { GetForegroundWindow() };
        sys::display_mode(fg)
    }
}

#[cfg(not(windows))]
mod desktop {
    use uc_overlay::model::{DisplayMode, Task};

    pub fn tasks() -> Vec<Task> {
        Vec::new()
    }
    pub fn switch_to(_: isize) {}
    pub fn close(_: isize) {}
    pub fn kill(_: u32) {}
    pub fn display_mode() -> Option<DisplayMode> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uc_battery::{Level, Power, Source};
    use uc_sysinfo::{ProcessStats, SystemStats};

    #[test]
    fn state_has_what_the_tab_reads() {
        let v = state_json(&StateInputs {
            host: HostStatus {
                gaming: true,
                mode: "Gaming".into(),
                tv: Some("Awake".into()),
            },
            volume: Ok((0.45, false)),
            apps: vec![
                AppSession {
                    pid: 0,
                    name: String::new(),
                    volume: 1.0,
                    muted: false,
                },
                AppSession {
                    pid: 42,
                    name: "eldenring.exe".into(),
                    volume: 0.5,
                    muted: true,
                },
            ],
            outputs: vec![AudioDevice {
                id: "{0}.{abc}".into(),
                name: "TV".into(),
                is_default: true,
            }],
            tasks: vec![Task {
                hwnd: 0x1234,
                pid: 42,
                title: "ELDEN RING".into(),
            }],
            snapshot: Snapshot {
                system: SystemStats {
                    cpu_percent: Some(12.4),
                    memory_used_bytes: 8 << 30,
                    memory_total_bytes: 32 << 30,
                    gpu_percent: None,
                },
                processes: vec![ProcessStats {
                    pid: 42,
                    name: Some("eldenring.exe".into()),
                    cpu_percent: Some(30.0),
                    working_set_bytes: 4 << 30,
                    private_bytes: 0,
                    gpu_percent: Some(97.0),
                    uptime: Duration::from_secs(60),
                }],
            },
            display: Some(DisplayMode {
                width: 3840,
                height: 2160,
                refresh_hz: 120,
            }),
            batteries: Some(vec![Device {
                name: "Xbox Controller".into(),
                kind: Kind::Controller,
                level: Some(Level::Percent(15)),
                power: Power::Discharging,
                source: Source::XInput,
            }]),
        });
        assert_eq!(v["host"]["gaming"], true);
        assert_eq!(v["host"]["tv"], "Awake");
        assert!((v["volume"]["level"].as_f64().unwrap() - 0.45).abs() < 1e-6);
        assert_eq!(v["volumeError"], Value::Null);
        assert_eq!(v["apps"][0]["name"], "System sounds");
        assert_eq!(v["apps"][1]["name"], "eldenring");
        assert_eq!(v["outputs"][0]["default"], true);
        assert_eq!(v["tasks"][0]["hwnd"], 0x1234);
        let detail = v["tasks"][0]["detail"].as_str().unwrap();
        assert!(
            detail.starts_with("eldenring.exe · CPU 30% · GPU 97% · "),
            "{detail}"
        );
        assert_eq!(v["system"]["cpu"], "12%");
        assert_eq!(v["system"]["gpu"], Value::Null);
        assert!(v["system"]["ram"].as_str().unwrap().contains(" of "));
        assert_eq!(v["display"], "3840 × 2160 · 120 Hz");
        assert_eq!(v["batteries"][0]["level"], "15%");
        assert_eq!(v["batteries"][0]["low"], true);
    }

    #[test]
    fn no_audio_and_no_batteries_yet() {
        let v = state_json(&StateInputs {
            volume: Err("no device".into()),
            ..StateInputs::default()
        });
        assert_eq!(v["volume"], Value::Null);
        assert_eq!(v["volumeError"], "no device");
        assert_eq!(v["batteries"], Value::Null);
        assert_eq!(v["tasks"], json!([]));
    }

    #[test]
    fn parameters_are_checked() {
        assert_eq!(level(&json!({"level": 1.7})).unwrap(), 1.0);
        assert!(level(&json!({})).is_err());
        assert!(pid(&json!({"pid": -1})).is_err());
        assert!(hwnd(&json!({"hwnd": 0})).is_err());
        assert_eq!(hwnd(&json!({"hwnd": 4660})).unwrap(), 4660);
        assert!(flag(&json!({"muted": "yes"}), "muted").is_err());
    }
}
