<p align="center"><img src="packaging/brand/logo.svg" alt="Ultimate Console logo" width="96" height="96"></p>

# UC-SteamUI

**Ultimate Console with its menu inside Steam.** A fork of
[Ultimate-Console](https://github.com/OunissiDhiaEddine/Ultimate-Console)
that moves the menu (volume, sound output, windows, performance, batteries,
leaving gaming mode) into an **Ultimate Console tab in Big Picture's Quick
Access Menu** (Steam + A), instead of a separate full-screen overlay.

Everything else is Ultimate Console as before: a lightweight Windows
background agent that turns the PC into a couch console, GameScope-style. One
hotkey hides the Explorer shell (no sign-out), wakes the Android TV, switches
output to it and launches Steam Big Picture as the shell. Leaving Big Picture
(or the same hotkey) puts the desktop, the monitors and the TV back the way
they were.

Status: **early.** The console mode is Ultimate Console's, used daily. The
Steam tab is new and still being tried on the real PC
([checklist](docs/tests/WP-12.md)); a Steam update can break it, in which
case the hotkey falls back to the full-screen overlay.

## Install

1. Download **UltimateConsole-win-Setup.exe** from the
   [latest release](https://github.com/OunissiDhiaEddine/UC-SteamUI/releases/latest)
   and run it. No admin needed. SmartScreen may warn because it isn't
   code-signed: *More info → Run anyway*.
   - Already have Ultimate Console? This installs over it as an update and
     keeps your settings, TV pairing and hotkeys.
2. **Restart Steam once** (*Steam → Exit*, then start it), so it opens the
   port the tab needs (see [below](#the-menu-in-steam-steam--a)).
3. Open Big Picture, press **Steam + A** and pick the controller icon. The
   settings app's *Steam* page shows whether the tab is ready.

Updates come from this repo's releases, checked daily and installed in
desktop mode. To go back to plain Ultimate Console, install its Setup.exe and
set `updates.repo` back to `OunissiDhiaEddine/Ultimate-Console` in
`config.toml` (otherwise it updates from here again).

## What's in the box

| Binary | What it does |
|---|---|
| `uc-agent.exe` | The one background process. Starts at logon, non-elevated. Owns the hotkeys, the gaming-mode state machine, TV control, sleep/resume hooks, the Ultimate Console tab in Steam's Quick Access Menu and the fallback overlay. |
| `uc-settings.exe` | Settings app. Only runs while open; talks to the agent over a named pipe. |
| `uc-probe.exe` | Developer CLI to test each piece on its own on the real PC. |

## Default hotkeys

All of them can be changed in the settings app.

| Action | Default |
|---|---|
| Enter / leave gaming mode | `Ctrl+Alt+G` |
| Open the menu (Steam's Quick Access Menu on the Ultimate Console tab; the full-screen overlay over other apps) | `Ctrl+Alt+O` |
| **Panic restore** (always brings the desktop back) | `Ctrl+Alt+Shift+R` |

## The menu in Steam (Steam + A)

In Big Picture, and in Steam's in-game overlay, press **Steam + A** (or the
**…** button) and pick the **Ultimate Console** tab (the controller icon):

- **Console**: mode, TV state, *Leave gaming mode* (press twice).
- **Volume**: master volume and mute, and a slider per app.
- **Sound output**: make another device the default.
- **Windows**: every open window, with *Switch to*, *Close* and *End*
  (press twice). Handy for apps that weren't started from Steam.
- **Performance**: CPU, GPU, memory and the display mode.
- **Batteries**: controllers, headsets and mice.

It replaces the full-screen overlay as the main menu, so everything works with
the controller like the rest of Big Picture. The agent's own idle cost is a
localhost connection attempt every few seconds while Steam isn't up.

How it gets in: Steam's interface is Chromium. With an empty
`.cef-enable-remote-debugging` file in Steam's folder, Steam serves the Chrome
DevTools protocol on `127.0.0.1:8080` when it starts; the agent connects,
adds the tab to the Quick Access Menu's React component and answers it.
Decky Loader and Millennium use the same switch.

- **Restart Steam once** after installing or updating, so it sees the file.
  The settings app's Steam page shows the tab's status.
- The agent creates the file and removes it again when you turn the tab off or
  uninstall (only if it made it). Turn *Turn on Steam's DevTools port* off if
  Decky, Millennium or similar manage it.
- While the port is open, any program on this PC can control Steam's
  interface. That's the trade-off every Steam UI mod makes; turn the tab off
  if you don't want it.
- Steam's interface isn't a public API and changes with Steam updates. The
  script looks everything up defensively and falls back to simpler controls;
  if the tab stops showing, the hotkey still opens the full-screen overlay.
  `uc-probe steam-menu run` shows what it found.

Settings → General → *Overlay hotkey opens the full-screen overlay* goes back
to the old behaviour. The volume popup for keyboard and headset volume keys in
gaming mode is unchanged.

## Escape hatches: getting your desktop back

Gaming mode closes Explorer, so if something goes wrong you might be left
with no taskbar or a black screen. In order of preference:

1. **Panic hotkey** `Ctrl+Alt+Shift+R`. Restores the display layout, audio
   device and Explorer unconditionally, from any state.
2. **Exit Big Picture** from Steam's menu. The agent notices the window is
   gone and restores everything.
3. **Restart the agent.** If the agent crashed mid-game, Windows restarts it
   and it finds `state.json` saying "gaming" and restores. Signing out and
   back in does the same.
4. **Start Explorer by hand.** Press `Ctrl+Shift+Esc` to open Task Manager →
   *Run new task* → type `explorer` → OK. That brings the taskbar back.
5. **Displays still wrong?** Press `Win+P` and pick *Extend* or *PC screen
   only*, or in Task Manager run `displayswitch /extend`.
6. **Last resort.** `Ctrl+Alt+Del` → sign out, or restart the PC. Explorer
   always comes back on the next sign-in. Windows may remember the TV-only
   display layout, so use step 5 if the monitors stay off.

If the agent keeps putting you back into a bad state, delete
`%LOCALAPPDATA%\UltimateConsole\state.json` (Task Manager → Run →
`cmd`, then `del "%LOCALAPPDATA%\UltimateConsole\state.json"`).

## Files

| What | Where |
|---|---|
| Config | `%APPDATA%\UltimateConsole\config.toml` |
| Crash-recovery state | `%LOCALAPPDATA%\UltimateConsole\state.json` |
| Logs (7 days) | `%LOCALAPPDATA%\UltimateConsole\logs\` |
| TV pairing key | `%LOCALAPPDATA%\UltimateConsole\adbkey` |
| Steam's DevTools switch (if the agent made it) | `<Steam folder>\.cef-enable-remote-debugging`, noted in `%LOCALAPPDATA%\UltimateConsole\steam-debug-file.txt` |

Uninstalling removes all of them.

## Building

Needs Rust stable (pinned in `rust-toolchain.toml`) and the MSVC build tools
on Windows.

```powershell
cargo build --workspace --release
cargo test --workspace
.\target\release\uc-probe.exe --help
```

From Linux or macOS you can type-check with
`cargo check --workspace --target x86_64-pc-windows-msvc`; anything that
touches hardware has to be tried on the real PC with `uc-probe`. The Steam
tab's script has its own test against a fake Steam:
`node crates/uc-steamui/js/menu.test.js`.

Set `UC_HOME=C:\some\folder` to make every binary use that folder instead of
`%APPDATA%`/`%LOCALAPPDATA%`, which is handy for trying things safely.

## Docs

- Setup guide (TV, display, hotkeys) on the original project's website:
  https://ounissidhiaeddine.github.io/Ultimate-Console/ (source in `site/`).
  Its download buttons point at the original project; for this fork use the
  [releases here](https://github.com/OunissiDhiaEddine/UC-SteamUI/releases).
- `docs/PLAN.md`: architecture, decisions and work packages; §2.14 is the
  Steam tab.
- `docs/tests/`: short manual test checklists, one per work package.
- `CONTRIBUTING.md`: branch, PR and code conventions.
