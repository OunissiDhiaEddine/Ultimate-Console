//! Steam's DevTools switch: an empty `.cef-enable-remote-debugging` file in
//! Steam's folder. Steam reads it when it starts and then serves the Chrome
//! DevTools Protocol on `127.0.0.1:8080`, which is how the Quick Access Menu
//! tab gets in (Decky Loader and Millennium use the same switch).
//!
//! The agent creates the file when the Steam menu is on and writes a marker
//! (`Paths::steam_debug_marker_file`) so that it only ever removes a file it
//! made itself: when the menu is turned off, and on uninstall.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Name of the switch file.
pub const FILE_NAME: &str = ".cef-enable-remote-debugging";

/// The switch file for a Steam folder (the folder `steam.exe` is in).
pub fn path_in(steam_dir: &Path) -> PathBuf {
    steam_dir.join(FILE_NAME)
}

/// What [`ensure`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ensured {
    /// It was already there (made by us earlier, or by another tool).
    AlreadyThere,
    /// Created now. A Steam that is already running needs a restart before
    /// the port opens.
    Created,
}

/// Creates the switch in `steam_dir` if missing, and records it in `marker`
/// when we made it.
pub fn ensure(steam_dir: &Path, marker: &Path) -> std::io::Result<Ensured> {
    let file = path_in(steam_dir);
    if file.exists() {
        return Ok(Ensured::AlreadyThere);
    }
    fs::write(&file, b"")?;
    if let Some(dir) = marker.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(marker, file.to_string_lossy().as_bytes())?;
    tracing::info!(path = %file.display(), "created Steam's DevTools switch");
    Ok(Ensured::Created)
}

/// Removes the switch if `marker` says we created it, then the marker.
/// Returns whether a switch file was removed. A switch someone else made is
/// left alone.
pub fn remove_ours(marker: &Path) -> std::io::Result<bool> {
    let text = match fs::read_to_string(marker) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let file = PathBuf::from(text.trim());
    let mut removed = false;
    if file.file_name().is_some_and(|n| n == FILE_NAME) {
        match fs::remove_file(&file) {
            Ok(()) => {
                tracing::info!(path = %file.display(), "removed Steam's DevTools switch");
                removed = true;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    fs::remove_file(marker)?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_once_and_removes_only_its_own() {
        let root = tempfile::tempdir().unwrap();
        let steam = root.path().join("Steam");
        fs::create_dir_all(&steam).unwrap();
        let marker = root.path().join("data").join("marker.txt");

        assert_eq!(ensure(&steam, &marker).unwrap(), Ensured::Created);
        assert!(path_in(&steam).exists());
        assert_eq!(ensure(&steam, &marker).unwrap(), Ensured::AlreadyThere);

        assert!(remove_ours(&marker).unwrap());
        assert!(!path_in(&steam).exists());
        assert!(!marker.exists());
        assert!(!remove_ours(&marker).unwrap());
    }

    #[test]
    fn leaves_a_switch_it_did_not_make() {
        let root = tempfile::tempdir().unwrap();
        let steam = root.path().join("Steam");
        fs::create_dir_all(&steam).unwrap();
        fs::write(path_in(&steam), b"").unwrap();
        let marker = root.path().join("marker.txt");

        assert_eq!(ensure(&steam, &marker).unwrap(), Ensured::AlreadyThere);
        assert!(!marker.exists());
        assert!(!remove_ours(&marker).unwrap());
        assert!(path_in(&steam).exists());
    }

    #[test]
    fn a_damaged_marker_deletes_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let precious = root.path().join("config.toml");
        fs::write(&precious, b"x").unwrap();
        let marker = root.path().join("marker.txt");
        fs::write(&marker, precious.to_string_lossy().as_bytes()).unwrap();
        assert!(!remove_ours(&marker).unwrap());
        assert!(precious.exists());
        assert!(!marker.exists());
    }
}
