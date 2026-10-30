//! Where every file lives (plan §2.11).
//!
//! | What        | Where                                           |
//! |-------------|-------------------------------------------------|
//! | Config      | `%APPDATA%\UltimateConsole\config.toml`         |
//! | State       | `%LOCALAPPDATA%\UltimateConsole\state.json`     |
//! | ADB key     | `%LOCALAPPDATA%\UltimateConsole\adbkey`         |
//! | Update token| `%LOCALAPPDATA%\UltimateConsole\update-token.bin` |
//! | Logs        | `%LOCALAPPDATA%\UltimateConsole\logs\`          |
//!
//! Velopack installs binaries under `%LOCALAPPDATA%\UltimateConsole` too, in
//! its own `current\` and `packages\` subfolders, which do not clash with these.
//!
//! Set `UC_HOME` to put everything under one folder instead (handy for
//! `uc-probe` and tests): config in `$UC_HOME\config`, data in `$UC_HOME\data`.

use std::path::PathBuf;

use crate::{APP_NAME, Error, Result};

/// Resolved locations of every file the app reads or writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Roaming folder, holds the config.
    pub config_dir: PathBuf,
    /// Local folder, holds state, keys and logs.
    pub data_dir: PathBuf,
}

impl Paths {
    /// Resolves paths from `UC_HOME`, or else `%APPDATA%` and `%LOCALAPPDATA%`.
    pub fn from_env() -> Result<Self> {
        if let Some(home) = std::env::var_os("UC_HOME").filter(|v| !v.is_empty()) {
            let home = PathBuf::from(home);
            return Ok(Self::new(home.join("config"), home.join("data")));
        }
        let appdata = std::env::var_os("APPDATA").ok_or(Error::MissingEnv("APPDATA"))?;
        let local = std::env::var_os("LOCALAPPDATA").ok_or(Error::MissingEnv("LOCALAPPDATA"))?;
        Ok(Self::new(
            PathBuf::from(appdata).join(APP_NAME),
            PathBuf::from(local).join(APP_NAME),
        ))
    }

    pub fn new(config_dir: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
            data_dir: data_dir.into(),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    pub fn adb_key_file(&self) -> PathBuf {
        self.data_dir.join("adbkey")
    }

    pub fn update_token_file(&self) -> PathBuf {
        self.data_dir.join("update-token.bin")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    /// Every app-owned location, for the uninstaller and its CI check
    /// (plan §2.12). Velopack's own folders are not listed: Velopack removes them.
    pub fn uninstall_manifest(&self) -> Vec<PathBuf> {
        vec![
            self.config_file(),
            self.state_file(),
            self.adb_key_file(),
            self.update_token_file(),
            self.logs_dir(),
            self.config_dir.clone(),
        ]
    }
}

/// Name of the agent's named pipe for a given user SID (plan §2.10).
pub fn pipe_name(user_sid: &str) -> String {
    format!(r"\\.\pipe\{APP_NAME}-{user_sid}")
}

/// Registry value name used under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
pub const AUTOSTART_VALUE_NAME: &str = APP_NAME;

/// Name of the single-instance mutex the agent holds.
pub fn agent_mutex_name() -> String {
    format!(r"Local\{APP_NAME}-agent")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn file_layout() {
        let p = Paths::new("C:/r/UltimateConsole", "C:/l/UltimateConsole");
        assert_eq!(
            p.config_file(),
            Path::new("C:/r/UltimateConsole/config.toml")
        );
        assert_eq!(p.state_file(), Path::new("C:/l/UltimateConsole/state.json"));
        assert_eq!(p.logs_dir(), Path::new("C:/l/UltimateConsole/logs"));
        assert!(p.uninstall_manifest().contains(&p.update_token_file()));
    }

    #[test]
    fn pipe_name_has_sid() {
        assert_eq!(
            pipe_name("S-1-5-21-1"),
            r"\\.\pipe\UltimateConsole-S-1-5-21-1"
        );
    }
}
