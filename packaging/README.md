# Packaging

The app ships as a [Velopack](https://velopack.io) package (plan §2.12, WP-10).

| What | Value |
|---|---|
| Package id | `UltimateConsole` (= `uc_core::APP_NAME`; install folder and "Apps & features" key) |
| Install folder | `%LOCALAPPDATA%\UltimateConsole` (per user, no admin) |
| Main exe | `uc-settings.exe` (Start-menu shortcut, runs the install/uninstall hooks) |
| Also shipped | `uc-agent.exe`, `uc-probe.exe`, `README.md`, `LICENSE` in `current\` |
| Updates | GitHub Releases of this repo (public, no token) |
| Files here | `icon.ico` (installer, shortcut and .exe icon, generated from `brand/`), `brand/` (logo SVG sources, `make-icons.py` that regenerates every raster, `icon_res.rs` used by the apps' `build.rs`), `release-notes.md` (shown on the release and in the package) |

## Making a release

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`) and run
   `cargo check` so `Cargo.lock` follows. Merge that to `main`.
2. Either tag the merge commit and push the tag
   (`git tag v0.2.0 && git push origin v0.2.0`), or open *Actions → Release
   → Run workflow* on `main` and tick **publish** (this creates the tag).
3. `.github/workflows/release.yml` tests, builds, runs `vpk pack` (with a
   delta from the previous release) and `vpk upload github --publish`.

"Run workflow" on the Release workflow does a dry run: same build and pack,
the output is attached to the run as an artifact and nothing is published.

The `vpk` version in `release.yml` must match the `velopack` crate version in
`Cargo.toml`; bump both together.

## Install, update and uninstall flow

Code: `crates/uc-update` (`lifecycle`, `updater`, `autostart`,
`uninstall`).

- **Install:** Velopack runs `uc-settings.exe --veloapp-install`, which writes
  the HKCU Run value for `current\uc-agent.exe`. Then settings opens (first
  run) and starts the agent.
- **Update:** the agent checks GitHub daily and on "Check now", downloads in
  the background, and applies only when in desktop mode. `Update.exe` waits
  for the agent to exit, swaps `current\`, and restarts `uc-settings.exe
  --start-agent`, which starts the new agent without opening a window.
- **Uninstall:** Velopack runs `uc-settings.exe --veloapp-uninstall`, which
  asks the agent to shut down (it restores desktop first), removes the Run
  value and deletes config, state, ADB key, update token and logs. Velopack
  then removes the binaries, the shortcut and the "Apps & features" entry.
  `uc-settings --check-uninstall` lists anything left behind.

## Code signing

Not signed for now (single user). SmartScreen shows "Windows protected your
PC" on the first run of Setup.exe: *More info → Run anyway*. To sign later,
add `--signParams` to `vpk pack` with a certificate from a repo secret.
