//! Deletes everything the app itself created (plan §2.12, uninstall step 4).
//!
//! Velopack removes the binaries, its own folders, the shortcut and the
//! "Apps & features" entry afterwards. The install root
//! (`%LOCALAPPDATA%\UltimateConsole`) is also our data folder, so this never
//! deletes that folder itself, only the app's files inside it: Velopack's
//! `Update.exe` is still running from there.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use uc_core::Paths;

/// What happened to one manifest entry.
#[derive(Debug)]
pub struct Removed {
    pub path: PathBuf,
    /// `Ok(true)` deleted, `Ok(false)` wasn't there.
    pub result: std::io::Result<bool>,
}

/// Deletes every path in [`Paths::uninstall_manifest`] (files, the logs
/// folder, the whole roaming config folder) and, if it ends up empty, the data
/// folder. Keeps going after errors (e.g. a log file still open) and reports
/// each entry.
pub fn remove_app_data(paths: &Paths) -> Vec<Removed> {
    let mut report: Vec<Removed> = paths
        .uninstall_manifest()
        .into_iter()
        .map(|path| {
            let result = remove(&path);
            match &result {
                Ok(true) => tracing::info!(path = %path.display(), "removed"),
                Ok(false) => {}
                Err(e) => tracing::warn!(path = %path.display(), "could not remove: {e}"),
            }
            Removed { path, result }
        })
        .collect();
    // Only when empty: in an installed copy Velopack's files live here too.
    let data = paths.data_dir.clone();
    if fs::read_dir(&data).is_ok_and(|mut d| d.next().is_none()) {
        let result = fs::remove_dir(&data).map(|()| true);
        report.push(Removed { path: data, result });
    }
    report
}

fn remove(path: &Path) -> std::io::Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if meta.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_app_files_but_keeps_installer_files() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path().join("roaming"), root.path().join("local"));
        fs::create_dir_all(paths.logs_dir()).unwrap();
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(paths.config_file(), "schema = 1").unwrap();
        fs::write(paths.config_dir.join("config.toml.bak"), "").unwrap();
        fs::write(paths.state_file(), "{}").unwrap();
        fs::write(paths.adb_key_file(), "k").unwrap();
        fs::write(paths.update_token_file(), "t").unwrap();
        fs::write(paths.logs_dir().join("agent.log.2026-10-30"), "x").unwrap();
        // Velopack's own files in the same root.
        fs::write(paths.data_dir.join("Update.exe"), "").unwrap();
        fs::create_dir_all(paths.data_dir.join("current")).unwrap();

        let report = remove_app_data(&paths);
        assert!(report.iter().all(|r| r.result.is_ok()), "{report:?}");
        for p in paths.uninstall_manifest() {
            assert!(!p.exists(), "{} still there", p.display());
        }
        assert!(paths.data_dir.join("Update.exe").exists());
        assert!(paths.data_dir.join("current").exists());

        // Second run: nothing left, nothing fails.
        let again = remove_app_data(&paths);
        assert!(again.iter().all(|r| matches!(r.result, Ok(false))));
    }

    #[test]
    fn removes_empty_data_dir_in_dev_layout() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path().join("config"), root.path().join("data"));
        fs::create_dir_all(&paths.data_dir).unwrap();
        fs::write(paths.state_file(), "{}").unwrap();
        remove_app_data(&paths);
        assert!(!paths.data_dir.exists());
    }
}
