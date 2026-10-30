# Spike: does Windows restart Explorer after a clean exit? (WP-03)

**Question (plan §2.3):** when the agent posts `0x5B4` to `Shell_TrayWnd`
(the same as Ctrl+Shift+right-click on the taskbar → "Exit Explorer"), does
Winlogon start Explorer again by itself on Windows 11 24H2/25H2?

- **No** → the primary method stays. No admin, no registry change.
- **Yes** → WP-03 adds the fallback from §2.3: an elevated helper task,
  created once by the installer, that sets
  `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\AutoRestartShell = 0`
  around the exit and back to `1` afterwards.

## Expectation

Winlogon only restarts the shell when it *crashes or is killed*. A clean
exit through `0x5B4` has been the documented way to stop Explorer without a
restart since Windows 7, and it still works on Windows 11 23H2. That is why
`uc-shell` never terminates the shell process itself (only leftover
folder-window `explorer.exe` processes): a kill would trigger exactly the
restart we want to avoid. If the shell doesn't exit within the timeout,
`exit_explorer` returns an error and leaves the desktop alone.

## How to run it

On the real PC, with the `uc-probe.exe` from the PR's CI artifact:

```powershell
winver                                   # note the version, e.g. 24H2 26100.xxxx
.\uc-probe.exe shell kill-explorer --watch-secs 30 --restore
```

The taskbar and desktop disappear, the probe watches 30 s for Explorer coming
back by itself, prints a `RESULT:` line, then starts Explorer again. If
anything goes wrong: Ctrl+Shift+Esc → Run new task → `explorer`.

Run it twice: once with no folder windows open, once with a File Explorer
window open (and, if you use it, "Launch folder windows in a separate
process" ticked).

## Results

| Date | Windows build | Folder window open | `RESULT:` line | Notes |
|------|---------------|--------------------|----------------|-------|
| _pending, Dhia's PC_ | | no | | |
| _pending, Dhia's PC_ | | yes | | |

Decision: _pending._ Until a row says "restarted by itself", the elevated
helper is not built.
