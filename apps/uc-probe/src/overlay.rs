//! `uc-probe overlay` (WP-07): run the overlay on the desktop, toggled by a
//! hotkey registered here the same way the agent does it.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use uc_core::logging::{self, LogOptions};
use uc_core::{Config, Hotkey, Paths};
use uc_overlay::{Backends, Overlay, Settings};

#[derive(Args)]
pub struct OverlayArgs {
    /// Shortcut that toggles the overlay. Defaults to `hotkeys.overlay` from
    /// the config (Ctrl+Alt+O).
    #[arg(long)]
    hotkey: Option<String>,
    /// Use fake audio and stats instead of the real devices.
    #[arg(long)]
    fake: bool,
    /// Don't open the overlay at start, wait for the hotkey.
    #[arg(long)]
    hidden: bool,
    /// Also take the media volume keys (as the agent does in gaming mode):
    /// they change the volume and show the volume popup. While this runs,
    /// Windows' own volume flyout won't appear.
    #[arg(long)]
    volume_keys: bool,
    /// Quit after this many seconds (default: run until Ctrl+C).
    #[arg(long)]
    seconds: Option<u64>,
}

pub fn run(args: OverlayArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let paths = Paths::from_env()?;
    let _guard = logging::init(
        &paths.logs_dir(),
        &LogOptions {
            prefix: "probe-overlay",
            filter: "info,uc_overlay=debug",
            stderr: true,
        },
    )?;
    let config = Config::load(&paths.config_file())?;
    let hotkey: Hotkey = match &args.hotkey {
        Some(s) => s.parse()?,
        None => config.hotkeys.overlay,
    };
    let backends = if args.fake {
        fake_backends()
    } else {
        Backends::real()
    };
    let overlay = Overlay::start_with(Settings::from(&config), backends)?;
    println!("overlay running; {hotkey} toggles it, Ctrl+C quits");
    if args.volume_keys {
        println!("volume keys show the volume popup");
    }
    if !args.hidden {
        overlay.show();
    }
    hotkey_loop(
        &overlay,
        hotkey,
        args.volume_keys,
        args.seconds.map(Duration::from_secs),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn fake_backends() -> Backends {
    use uc_audio::{AudioControl, FakeAudio};
    use uc_sysinfo::{FixedStats, ProcessStats, Snapshot, StatsSource, SystemStats};
    Backends {
        audio: Arc::new(|| {
            let a = FakeAudio::new();
            a.add_session(0, "System sounds", 1.0);
            a.add_session(4242, "game.exe", 0.8);
            Ok(Box::new(a) as Box<dyn AudioControl>)
        }),
        stats: Arc::new(|| {
            let gb = 1024 * 1024 * 1024;
            Box::new(FixedStats(Snapshot {
                system: SystemStats {
                    cpu_percent: Some(23.0),
                    memory_used_bytes: 9 * gb,
                    memory_total_bytes: 32 * gb,
                    gpu_percent: Some(61.0),
                },
                processes: vec![ProcessStats {
                    pid: 4242,
                    name: Some("game.exe".into()),
                    cpu_percent: Some(12.5),
                    working_set_bytes: 3 * gb,
                    private_bytes: 4 * gb,
                    gpu_percent: Some(58.0),
                    uptime: Duration::from_secs(4000),
                }],
            })) as Box<dyn StatsSource>
        }),
    }
}

#[cfg(windows)]
fn hotkey_loop(
    overlay: &Overlay,
    hotkey: Hotkey,
    volume_keys: bool,
    limit: Option<Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    use uc_overlay::VolumeKey;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMessageW, MSG, SetTimer, WM_HOTKEY, WM_TIMER,
    };

    let mods = HOT_KEY_MODIFIERS(hotkey.modifiers.to_win32()) | MOD_NOREPEAT;
    // SAFETY: thread hotkey (no window); unregistered below.
    unsafe { RegisterHotKey(None, 1, mods, u32::from(hotkey.key.vk())) }
        .map_err(|e| format!("RegisterHotKey({hotkey}) failed, is it in use? {e}"))?;
    if volume_keys {
        for key in VolumeKey::ALL {
            // SAFETY: thread hotkey (no window), id = the key code; unregistered below.
            if let Err(e) = unsafe {
                RegisterHotKey(
                    None,
                    i32::from(key.vk()),
                    HOT_KEY_MODIFIERS(0),
                    u32::from(key.vk()),
                )
            } {
                eprintln!("could not take {key:?}: {e}");
            }
        }
    }
    if let Some(d) = limit {
        let ms = d.as_millis().min(u128::from(u32::MAX)) as u32;
        // SAFETY: thread timer (no window); it ends the loop below.
        unsafe { SetTimer(None, 0, ms, None) };
    }
    let mut msg = MSG::default();
    // SAFETY: message loop on this thread; `msg` outlives each call.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        match msg.message {
            WM_HOTKEY => match VolumeKey::from_raw(msg.wParam.0) {
                Some(key) => overlay.volume_key(key),
                None => overlay.toggle(),
            },
            WM_TIMER => break,
            _ => {}
        }
    }
    // SAFETY: undoes the registrations above.
    unsafe {
        let _ = UnregisterHotKey(None, 1);
        for key in VolumeKey::ALL {
            let _ = UnregisterHotKey(None, i32::from(key.vk()));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn hotkey_loop(
    _overlay: &Overlay,
    _hotkey: Hotkey,
    _volume_keys: bool,
    _limit: Option<Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("the overlay probe only runs on Windows".into())
}
