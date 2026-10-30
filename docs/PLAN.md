# Ultimate Console — Architecture & Implementation Plan

Status: **draft for review** · Owner: Dhia · Last updated: 2026-10-30

Goal: a GameScope-like "console mode" on Windows. One hotkey turns the PC into a
couch console (no Explorer, Steam Big Picture as the shell, TV as the only
display, TV woken over the network) and the same hotkey (or exiting Big
Picture) puts everything back, without signing out. One small background
process does all of it, including an integrated overlay. A separate settings
app configures it, and a proper installer/updater/uninstaller manages it.

---

## 1. Requirements (from the brief)

| # | Requirement | Notes |
|---|---|---|
| R1 | Global hotkey enters "gaming mode" | Configurable; same or second hotkey exits |
| R2 | Close the Explorer shell **without signing out** | Taskbar, Start, notifications all gone |
| R3 | Launch Steam Big Picture as the shell | `steam.exe -gamepadui` |
| R4 | Switch to a chosen display (the TV) | Other monitors disabled while in gaming mode |
| R5 | Wake the Android TV | ADB over network, Wake-on-LAN fallback |
| R6 | Overlay (hotkey): volume, task switcher (Win+Tab-like), resource info for the selected task | Part of the same process, not a separate service |
| R7 | Exiting Big Picture restores: original display layout, Explorer, TV off | |
| R8 | Sleep while in **service-triggered** gaming mode → TV sleeps; resume → TV wakes | Must do nothing in desktop mode or in Big Picture opened manually |
| R9 | Shutdown → TV off | **Out of scope for now** |
| R10 | Settings app: hotkeys, target display, TV pairing/test, etc. | |
| R11 | Easy updates; uninstall removes **everything** | |
| R12 | Lightweight: one background process, near-zero idle cost | |

---

## 2. Key decisions

### 2.1 Language and stack: Rust

- **Rust + `windows` crate (windows-rs)** for everything. No runtime, a single
  small exe, idle RAM in the low single-digit MB, direct access to every Win32
  API we need (CCD display config, Core Audio, DWM thumbnails, power
  notifications, Direct2D).
- C#/.NET was considered: faster to write UI in, but WPF/WinForms can't be
  NativeAOT-compiled, so the always-running agent would carry the .NET runtime
  (~40–80 MB RAM). That conflicts with R12.
- Build: Cargo workspace, target `x86_64-pc-windows-msvc`. CI on GitHub
  Actions `windows-latest`. Cloud threads can type-check/cross-build with
  `cargo xwin`, but **real behaviour can only be tested on Dhia's PC**, so
  every module ships a small CLI probe (see §6) to test it in isolation.

### 2.2 "Service" = a per-user background agent, not a Windows Service

A real Windows Service runs in Session 0 and **cannot** show UI, register
hotkeys, see the user's windows, or start Explorer/Steam in the user session.
So the "service" is a **headless user-session process** (`uc-agent.exe`)
started at logon. It has no window except the (hidden until needed) overlay
and an invisible message-only window for hotkeys/power events.

- Autostart: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
- **Runs non-elevated (medium integrity).** Every core feature works without
  admin (see 2.3). This also means Explorer and Steam get relaunched with the
  normal user token, which avoids the classic "elevated Explorer" bugs.
- Known limitation of non-elevated: the overlay can't focus or read stats of
  **elevated** game windows (UIPI). Rare for games. Optional stretch: a
  `uiAccess` signed build (WP-11).

### 2.3 Shell swap without sign-out

Primary method (no admin, no registry changes):

1. Ask Explorer to exit cleanly: `PostMessage(FindWindow("Shell_TrayWnd"), 0x5B4, 0, 0)`.
   This is the same message as "Exit Explorer" (Ctrl+Shift+right-click on the
   taskbar). On a *clean* exit Winlogon does **not** auto-restart the shell.
2. Wait for all `explorer.exe` processes of this session to exit (timeout 5 s).
   If any remain (e.g. separate-process folder windows), terminate those.
3. Launch Steam: if running, `steam://open/bigpicture`; else `steam.exe -gamepadui`.

Fallback (only if the spike shows Winlogon restarts Explorer on a given
Windows build): temporarily set
`HKLM\...\Winlogon\AutoRestartShell = 0` around the kill. Needs admin, so it
would be done by a one-time elevated helper task created by the installer.
**WP-03 starts with a spike on Win11 24H2/25H2 to decide.**

Restore: start `explorer.exe` with the agent's (normal) token. Explorer
detects there's no shell and becomes the shell (taskbar etc.).

Safety nets (very important, otherwise a crash leaves the user with a black
screen):
- Persist `state.json` (`{mode: "gaming", saved_topology, audio_device, started_at}`)
  **before** killing Explorer.
- On agent start: if `state.json` says gaming but there's no Big Picture
  window → run the full restore.
- **Panic hotkey** (default `Ctrl+Alt+Shift+R`): unconditional restore.
- `RegisterApplicationRestart` so Windows relaunches the agent after a crash.
- Ctrl+Shift+Esc (Task Manager → Run → explorer) is always the manual escape;
  documented in README.

### 2.4 Display switching

Win32 **CCD API** (`QueryDisplayConfig` / `SetDisplayConfig`):
- Snapshot the full current topology (paths + modes) into `state.json` before
  switching.
- Target display is stored by a **stable identity**: monitor device path
  (`DISPLAYCONFIG_TARGET_DEVICE_NAME.monitorDevicePath`, EDID-based) plus
  friendly name and connector as fallbacks. GPU target ids change across
  driver updates; device paths mostly don't.
- Gaming mode topology: only the TV path active, set as primary, optional
  preferred resolution/refresh, optional HDR on
  (`DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE`).
- **Order matters:** an HDMI sink that's off may not be "available". So: wake
  TV → poll `QueryDisplayConfig(QDC_ALL_PATHS)` until the TV target is
  available (timeout ~15 s, configurable) → switch. If it never appears, abort
  gaming mode cleanly and show a toast/log.
- Exit: restore the saved topology (`SDC_USE_SUPPLIED_DISPLAY_CONFIG`), fall
  back to `SDC_TOPOLOGY_EXTEND`/`SDC_USE_DATABASE_CURRENT` if the exact restore fails.

### 2.5 Audio

Core Audio (`IMMDeviceEnumerator`, `IAudioEndpointVolume`,
`IAudioSessionManager2`):
- Optional: switch default output to the TV's HDMI audio endpoint on entry,
  restore on exit (uses the undocumented-but-stable `IPolicyConfig`
  interface, same as every audio switcher).
- Overlay: master volume + mute, and per-app volume for the selected task.

### 2.6 Android TV control

Native **ADB-over-TCP client inside the agent** (no bundled `adb.exe`, which
would spawn its own background server, against R12). Candidate crate:
`adb_client` (supports TCP transport + RSA auth); otherwise implement the
small subset of the ADB protocol we need (CNXN/AUTH/OPEN/WRTE, `shell:`).

- Setup (settings app): user enables *Developer options → Network debugging*
  on the TV, enters IP (or picks from mDNS `_adb._tcp` discovery), presses
  "Pair"; TV shows "Allow USB debugging?" → "Always allow". Agent's RSA key
  lives in `%LOCALAPPDATA%\UltimateConsole\adbkey`.
- Commands:
  - Wake: `input keyevent KEYCODE_WAKEUP` (224)
  - Sleep: `input keyevent KEYCODE_SLEEP` (223)
  - State: `dumpsys power | grep mWakefulness` (to make wake/sleep idempotent)
  - Optional user-defined "after wake" commands (e.g. switch TV input / launch
    an HDMI passthrough intent; vendor-specific, so free-form list).
- **Wake-on-LAN fallback**: many Android TVs drop Wi-Fi/Ethernet in deep
  sleep, making ADB unreachable. Before ADB wake, send a WoL magic packet to the
  TV's MAC (captured automatically during pairing via the ARP table), then
  retry ADB connect with backoff (up to ~20 s). Settings app warns if the TV
  model needs "Network standby"/"Wake on Wi-Fi" enabled.
- Keep a persistent ADB connection while in gaming mode (with keepalive) so
  the sleep command is instant (matters for the suspend time budget, §2.7).
- HDMI-CEC is not possible from normal PC GPUs; noted as a future option via a
  Pulse-Eight USB-CEC adapter (libCEC). Not in scope.

### 2.7 Sleep / resume hooks (gaming mode only)

- `RegisterSuspendResumeNotification(DEVICE_NOTIFY_WINDOW_HANDLE)` on the
  agent's message window → `WM_POWERBROADCAST`:
  - `PBT_APMSUSPEND` → if and only if `mode == GamingActive` (service-
    triggered): send TV sleep on the already-open ADB socket. Hard budget
    ~1.5 s; never block suspend beyond that.
  - `PBT_APMRESUMEAUTOMATIC` / `PBT_APMRESUMESUSPEND` → if
    `mode == GamingActive`: wait for network (`NotifyIpInterfaceChange` or
    poll, ≤10 s) → WoL + ADB wake with retries → re-assert the gaming display
    topology (Windows sometimes re-enables monitors on resume) → refocus Big
    Picture.
- **Modern Standby (S0ix) laptops/boards**: additionally register
  `GUID_CONSOLE_DISPLAY_STATE` via `RegisterPowerSettingNotification` and
  treat "display off for N s while on battery-less standby" equivalently.
  WP-05 decides from testing which signal is reliable on Dhia's machine.
- Steam's own power menu "Sleep" just calls system suspend, so it goes through
  the same path. "Restart"/"Shut down" are ignored (R9 out of scope).
- Because the check is on the agent's own state machine, manual Big Picture
  in desktop mode never triggers TV actions (R8).

### 2.8 Detecting "exit Big Picture"

- Big Picture's top-level window: class `SDL_app`, title
  "Steam Big Picture Mode" (verify in WP-03; make the matcher configurable).
- `SetWinEventHook(EVENT_OBJECT_DESTROY / EVENT_OBJECT_HIDE)` filtered to
  steam.exe's PID, plus a cheap 1 s fallback poll. When the BP window is gone
  for > 2 s (debounce for BP restarting itself after updates) **and** gaming
  mode is active → run exit sequence. Covers "Exit Big Picture", "Switch to
  desktop" and Steam crashing/closing.

### 2.9 Overlay (integrated into the agent)

- Same process, separate UI thread. Window created hidden at startup:
  `WS_POPUP`, `WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP`,
  rendered with **Direct2D + DirectWrite on a DirectComposition swapchain**
  (per-pixel alpha, GPU-composited, no frames rendered while hidden →
  zero idle cost).
- Toggle hotkey (default `Ctrl+Alt+O`, configurable). For the controller:
  recommend mapping a Steam Input chord (e.g. Guide+Y) to that keyboard
  shortcut. While visible, the overlay polls **XInput** (and keyboard) for
  navigation; it stops polling when hidden.
- Panels (simple, console-like, centered card):
  1. **Volume**: master slider + mute; per-app volume of selected task.
  2. **Task switcher**: grid of alt-tab-eligible top-level windows
     (`EnumWindows` + the standard alt-tab filter: visible, not tool window,
     no owner, not cloaked) with **live DWM thumbnails**
     (`DwmRegisterThumbnail`, effectively free). Actions: switch to, close
     (WM_CLOSE), kill process (confirm).
  3. **Info for selected task**: CPU % (`GetProcessTimes` deltas), RAM
     (working set / private bytes), GPU % (PDH `\GPU Engine(pid_*)\Utilization
     Percentage`), process uptime. Plus small system line: total CPU/GPU/RAM.
     Sampling only while overlay is open, 1 Hz.
- Focus switching: overlay is foreground when shown, so it's allowed to call
  `SetForegroundWindow` on the chosen target (with `AllowSetForegroundWindow`
  / Alt-key trick as fallback).
- Limitation to document: topmost windows can't draw over **exclusive
  fullscreen** games. Most modern games use borderless/flip-model (and Win11
  "Optimizations for windowed games" upgrades older ones), where it works.
  Opening the overlay may minimise an exclusive-fullscreen game; acceptable.
- Overlay also works in desktop mode if the user enables it (setting), but
  default is gaming-mode only.

### 2.10 Settings app

- Separate exe `uc-settings.exe`, launched from Start menu, only runs when
  open. Rust + **egui/eframe** (single small exe, fast to build, works with
  mouse and keyboard; controller navigation not required for a settings app).
- Talks to the agent over a **named pipe** `\\.\pipe\UltimateConsole-<user SID>`
  (JSON lines, request/response + event stream). If the agent isn't running,
  the app can start it.
- Pages:
  - **General**: start with Windows, gaming-mode hotkey, panic hotkey, overlay
    hotkey, overlay-in-desktop toggle.
  - **Display**: list displays (from agent), "Identify" (agent flashes the
    number on each screen), pick TV, resolution/refresh/HDR, wait timeout.
  - **Audio**: switch default device on/off, pick device.
  - **TV**: IP / discover, Pair, MAC (auto), test Wake / Sleep / State, WoL
    on/off, extra after-wake commands, retry timings.
  - **Steam**: path (auto-detected from registry `HKCU\Software\Valve\Steam\SteamExe`),
    launch args, BP window matcher (advanced).
  - **Sleep/Resume**: enable TV sleep on suspend, wake on resume.
  - **About/Updates**: version, check now, auto-check toggle, open logs, Uninstall.
- Status header: agent running? current mode, TV reachable?

### 2.11 Config, state, logs

| What | Where | Format |
|---|---|---|
| Config | `%APPDATA%\UltimateConsole\config.toml` | TOML, versioned (`schema = 1`), agent hot-reloads on change |
| Runtime state | `%LOCALAPPDATA%\UltimateConsole\state.json` | written atomically (temp + rename) |
| ADB key | `%LOCALAPPDATA%\UltimateConsole\adbkey` | RSA |
| Logs | `%LOCALAPPDATA%\UltimateConsole\logs\` | `tracing` rolling daily, keep 7 |

### 2.12 Install / update / uninstall

- **Velopack** (has a Rust SDK): per-user install into
  `%LOCALAPPDATA%\UltimateConsole`, no admin, Start-menu shortcut, delta
  updates, clean uninstall entry in "Apps & features", and install/uninstall
  hooks the app handles itself.
- Updates come from **GitHub Releases**. ⚠️ The repo is private, and
  unauthenticated clients can't read private releases. Options (decision for
  Dhia, see §7): (a) publish releases to a small **public** "releases-only"
  repo, (b) make this repo public, (c) embed a read-only fine-grained token
  (not recommended).
- Uninstall hook (`--veloapp-uninstall`) must, in order:
  1. If gaming mode is active → run full restore (display, Explorer, audio).
  2. Stop the agent.
  3. Remove the HKCU Run key, any scheduled task (if the AutoRestartShell
     fallback was used: restore the value to 1 and delete the helper task).
  4. Delete `%APPDATA%\UltimateConsole` and `%LOCALAPPDATA%\UltimateConsole`
     (config, state, logs, ADB key).
  5. Velopack removes binaries, shortcuts and the uninstall entry.
  An `uninstall-manifest` test in CI lists every path/key the app can create,
  and WP-09 verifies each is gone after uninstall.

---

## 3. Process & module architecture

```
uc-agent.exe  (one process, non-elevated, autostart)
├── main thread: message-only window
│   ├── RegisterHotKey (gaming toggle, overlay, panic)
│   ├── WM_POWERBROADCAST (suspend/resume) + power setting notifications
│   └── WinEvent hook (Big Picture window lifecycle)
├── orchestrator (state machine, owns state.json)
├── worker thread / small async runtime: TV (ADB/WoL), network waits
├── overlay UI thread (D2D/DComp, idle when hidden)
└── IPC server thread (named pipe)

uc-settings.exe  (runs only when opened) ──named pipe──► uc-agent
```

### 3.1 State machine (orchestrator)

```
Desktop ──hotkey──► Entering ──ok──► Gaming ──BP closed / hotkey──► Exiting ──► Desktop
            ▲           │fail             │  ▲                               │
            └───────────┴──rollback───────┘  │suspend: TV sleep              │
                                             │resume: TV wake, re-assert     │
                                       panic hotkey (any state) ────────────►┘
```

Entering steps (each step has an undo; failure rolls back what was done):
1. Save `state.json` (topology, default audio device).
2. TV: WoL (if enabled) + ADB wake (async, starts first because it's slowest).
3. Exit Explorer.
4. Wait for TV display target to be available → apply gaming topology.
5. Switch audio device (optional).
6. Launch Steam Big Picture, wait for its window (timeout 30 s), focus it.
7. Mode = Gaming; open persistent ADB connection.

Exiting steps:
1. Restore topology → 2. restore audio → 3. start Explorer →
4. TV sleep → 5. clear `state.json`, mode = Desktop.
(Steam stays running in normal desktop mode.)

### 3.2 Cargo workspace layout

```
ultimate-console/
├── Cargo.toml                 (workspace)
├── crates/
│   ├── uc-core/               config schema, state.json, IPC message types, errors, logging setup
│   ├── uc-display/            CCD snapshot/restore/apply, stable display identity
│   ├── uc-tv/                 ADB-over-TCP client, WoL, discovery, pairing
│   ├── uc-shell/              Explorer exit/start, Steam launch & BP window detection
│   ├── uc-power/              suspend/resume + display-state notifications
│   ├── uc-audio/              endpoints, default device switch, volumes
│   ├── uc-sysinfo/            per-process CPU/RAM/GPU sampling
│   └── uc-overlay/            D2D/DComp window, UI panels, XInput nav
├── apps/
│   ├── uc-agent/              orchestrator, hotkeys, IPC server, recovery
│   ├── uc-settings/           egui settings app (IPC client)
│   └── uc-probe/              CLI to exercise each crate on real hardware
├── packaging/                 Velopack config, icons, install/uninstall hooks
├── docs/                      this plan, test checklists
└── .github/workflows/         ci.yml, release.yml
```

Each `uc-*` crate exposes a plain Rust API with no dependency on the agent,
so packages can be built in parallel and tested via `uc-probe`.

---

## 4. Work packages

Every package: own branch + PR, builds on CI, adds its `uc-probe` subcommand,
adds a short manual test checklist under `docs/tests/<wp>.md` for Dhia to run
on the real PC. "Interfaces" lists what other packages may rely on.

### WP-00 · Repo foundation (do first, blocks everything)
- Cargo workspace skeleton with all crates/apps as empty stubs.
- `uc-core`: config schema (serde + TOML, defaults, validation, `schema`
  version), `state.json` read/write (atomic), IPC message enum (JSON), logging
  setup (`tracing` + rolling file).
- CI: `windows-latest` → fmt, clippy (`-D warnings`), test, build release.
- README (what it is, escape hatches), LICENSE, `.gitignore`, `CONTRIBUTING`
  (branch/PR conventions for the threads).
- **Interfaces:** `Config`, `RuntimeState`, `IpcRequest/IpcResponse/IpcEvent`.
- Size: S.

### WP-01 · Display switching (`uc-display`)
- Enumerate displays with stable identity + friendly name + current mode.
- `snapshot() -> Topology`, `restore(&Topology)`, `apply_single(target, mode, hdr)`,
  `wait_available(target, timeout)`, `identify()` (flash numbers).
- Topology serialisable into `state.json`.
- Probe: `uc-probe display list | snapshot | switch <id> | restore <file>`.
- Depends on: WP-00. Size: M.

### WP-02 · Android TV control (`uc-tv`)
- ADB-over-TCP client (connect, RSA auth with persisted key, `shell:` exec,
  keepalive), WoL sender, MAC lookup from ARP, mDNS discovery of `_adb._tcp`.
- `TvController { wake(), sleep(), state(), run(cmd), pair() }`, all with
  timeouts and retry policy from config.
- Probe: `uc-probe tv discover | pair <ip> | wake | sleep | state`.
- Depends on: WP-00. Size: M–L (protocol work). Riskiest piece for hardware variance.

### WP-03 · Shell & Steam control (`uc-shell`)
- **Spike first:** confirm `0x5B4` clean-exit doesn't trigger Explorer
  auto-restart on Win11 24H2/25H2; record result in `docs/spikes/shell.md`.
  If it does, implement the elevated-helper fallback (§2.3).
- `exit_explorer()`, `start_explorer()`, `explorer_running()`.
- Steam: locate exe, `launch_big_picture()`, `BigPictureWatcher` (WinEvent
  hook + poll, debounce) emitting `Appeared/Closed`.
- Probe: `uc-probe shell kill-explorer | start-explorer | bp-launch | bp-watch`.
- Depends on: WP-00. Size: M.

### WP-04 · Audio (`uc-audio`)
- List endpoints, get/set default (IPolicyConfig), master volume/mute,
  per-process session volume.
- Probe: `uc-probe audio list | set-default <id> | volume 40`.
- Depends on: WP-00. Size: S.

### WP-05 · Power hooks (`uc-power`)
- Suspend/resume via `RegisterSuspendResumeNotification`; display-state
  notifications; network-ready waiter.
- Exposes events to a callback/channel; does **no** policy itself (the
  gaming-only rule lives in the orchestrator).
- Probe: `uc-probe power watch` (logs events with timestamps; Dhia sleeps/wakes
  the PC to verify timing budget and which signals fire, S3 vs Modern Standby).
- Depends on: WP-00. Size: S.

### WP-06 · System info (`uc-sysinfo`)
- Per-PID CPU %, working set/private bytes, GPU % (PDH GPU Engine counters),
  system totals; sampler that only runs when asked.
- Probe: `uc-probe sysinfo <pid>`.
- Depends on: WP-00. Size: S.

### WP-07 · Overlay (`uc-overlay`)
- Hidden topmost DComp/D2D window, show/hide with fade, centered card UI,
  keyboard + XInput navigation, the three panels from §2.9.
- Uses `uc-audio` and `uc-sysinfo` through traits so it can be built against
  mocks before WP-04/06 land.
- Probe: `uc-probe overlay` (shows it on the desktop for testing).
- Depends on: WP-00 (traits), integrates WP-04 and WP-06. Size: L.

### WP-08 · Agent / orchestrator (`uc-agent`)
- Message-only window, hotkeys, state machine (§3.1) with per-step rollback,
  crash recovery from `state.json`, panic hotkey, `RegisterApplicationRestart`,
  single-instance mutex, config hot-reload, power policy (gaming-only), IPC
  server, overlay hosting, autostart registration.
- Can start against stub implementations and swap in real crates as WP-01…07 merge.
- Depends on: WP-00; full function needs WP-01…07. Size: L.

### WP-09 · Settings app (`uc-settings`)
- egui app, pages from §2.10, IPC client, "agent not running" handling,
  pairing wizard, display identify, test buttons, open logs, uninstall button.
- Can be built against a mock IPC server early.
- Depends on: WP-00 (IPC types). Size: M.

### WP-10 · Installer, updater, uninstaller, release pipeline
- Velopack packaging of `uc-agent` + `uc-settings`, Start-menu shortcut
  (settings only), install hook (register autostart, start agent), update
  check in agent (daily + "check now" via IPC; apply on next exit from
  gaming mode / at idle, never during gaming mode), uninstall hook (§2.12).
- `release.yml`: on tag `v*` → build, `vpk pack`, upload to the releases
  target chosen in §7.
- Uninstall verification checklist (every file/key from the manifest).
- Optional: code signing (self-signed is fine for personal use; SmartScreen
  will warn on unsigned).
- Depends on: WP-00; final packaging needs WP-08/09. Size: M.

### WP-11 · (Stretch) Hardening & extras
- `uiAccess` signed build so the overlay works over elevated windows.
- Shutdown → TV off (R9).
- FPS counter in overlay via ETW/PresentMon.
- HDMI-CEC via USB-CEC adapter.

### Dependency graph / suggested order

```
WP-00 ──┬── WP-01 ─┐
        ├── WP-02 ─┤
        ├── WP-03 ─┤
        ├── WP-04 ─┼── WP-07 ─┐
        ├── WP-05 ─┤          ├── WP-08 ── WP-10
        ├── WP-06 ─┘          │
        └── WP-09 ────────────┘
```

Wave 1: WP-00. Wave 2 (parallel threads): WP-01, 02, 03, 04, 05, 06, 09.
Wave 3: WP-07, WP-08. Wave 4: WP-10, then real-hardware end-to-end test.

---

## 5. Milestones

- **M1 – "It switches"**: WP-00/01/03/08 minimal → hotkey kills Explorer,
  switches display, launches BP, and restores on exit. (Usable already.)
- **M2 – "TV follows"**: + WP-02/05 → TV wakes/sleeps on enter/exit and on
  suspend/resume in gaming mode only.
- **M3 – "Console polish"**: + WP-04/06/07 → overlay.
- **M4 – "Shippable"**: + WP-09/10 → settings app, installer, updates, clean uninstall.

---

## 6. Testing strategy

- Unit tests for pure logic (config, state machine with fake modules,
  topology serialisation, ADB packet encode/decode, alt-tab filter).
- `uc-probe` subcommands for every hardware-touching feature; each WP adds
  `docs/tests/WP-xx.md` with a 5-minute manual checklist.
- End-to-end checklist (after M2): enter/exit 10× in a row; exit via BP menu,
  via hotkey, via Steam crash (kill steam.exe), via panic hotkey; sleep/resume
  in gaming mode (TV follows) and in desktop mode and manual BP (TV ignored);
  agent killed mid-gaming → restart recovers; uninstall while in gaming mode.

---

## 7. Open decisions for Dhia

1. **Update hosting** for a private repo: public releases-only repo
   (recommended) vs make this repo public vs embedded token.
2. **Default hotkeys**: proposal `Ctrl+Alt+G` (gaming mode toggle),
   `Ctrl+Alt+O` (overlay), `Ctrl+Alt+Shift+R` (panic restore).
3. **TV model** (brand/OS version), so WP-02 can check its deep-sleep / WoL behaviour.
4. **Machine type**: desktop with S3 sleep or Modern Standby? (`powercfg /a`
   output answers it; affects WP-05.)

## 8. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Winlogon restarts Explorer on some builds | Shell swap breaks | WP-03 spike; elevated-helper fallback |
| TV unreachable in deep sleep | No wake | WoL, retries, docs for "network standby" setting |
| Suspend budget too short for ADB sleep | TV stays on | Persistent connection; fire-and-forget command |
| Agent crash in gaming mode | User stuck without Explorer | state.json recovery, restart registration, panic hotkey, Ctrl+Shift+Esc |
| Overlay can't draw over exclusive fullscreen | Overlay invisible in some games | Document; most games are borderless/flip-model |
| Big Picture window identity changes with Steam updates | Exit not detected | Configurable matcher, process-based fallback |
| Display device path changes after GPU driver update | Wrong/no display | Fallback matching on friendly name + connector; settings warns |
