//! Crash-recovery state: `%LOCALAPPDATA%\UltimateConsole\state.json` (plan §2.3, §3.1).
//!
//! The orchestrator writes this **before** each destructive step of entering
//! gaming mode (killing Explorer, changing the topology, switching audio), so
//! that after a crash the agent knows exactly what to undo. When the agent
//! starts and finds a state that is not [`Mode::Desktop`], it runs the restore.
//!
//! No file means desktop mode.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, fsutil};

/// Current `state.json` format.
pub const STATE_VERSION: u32 = 1;

/// Orchestrator mode (plan §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Desktop,
    Entering,
    /// Service-triggered gaming mode. The only mode in which sleep/resume
    /// touches the TV (R8).
    Gaming,
    Exiting,
}

/// Steps of entering gaming mode that have an undo. Recorded in the order
/// they were done; restore undoes them in reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterStep {
    TvWoken,
    ExplorerExited,
    TopologyApplied,
    AudioSwitched,
    BigPictureLaunched,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeState {
    pub version: u32,
    pub mode: Mode,
    /// When gaming mode was entered (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    /// Display topology before entering. Opaque here; `uc-display` owns the
    /// format (it serialises its `Topology` into this value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_topology: Option<serde_json::Value>,
    /// Default audio endpoint id before entering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_audio_device: Option<String>,
    #[serde(default)]
    pub steps_done: Vec<EnterStep>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            mode: Mode::Desktop,
            started_at: None,
            saved_topology: None,
            saved_audio_device: None,
            steps_done: Vec::new(),
        }
    }
}

impl RuntimeState {
    /// Fresh state for the start of an enter sequence.
    pub fn entering() -> Self {
        Self {
            mode: Mode::Entering,
            started_at: Some(unix_now()),
            ..Self::default()
        }
    }

    /// True when the last run did not end cleanly in desktop mode.
    pub fn needs_recovery(&self) -> bool {
        self.mode != Mode::Desktop
    }

    /// Records a finished step (idempotent).
    pub fn mark_done(&mut self, step: EnterStep) {
        if !self.steps_done.contains(&step) {
            self.steps_done.push(step);
        }
    }

    pub fn is_done(&self, step: EnterStep) -> bool {
        self.steps_done.contains(&step)
    }

    /// Steps to undo, most recent first.
    pub fn undo_order(&self) -> impl Iterator<Item = EnterStep> + '_ {
        self.steps_done.iter().rev().copied()
    }

    /// Loads the state. A missing file is desktop mode. A corrupt file is an
    /// error: the caller should log it and run a full restore to be safe.
    pub fn load(path: &Path) -> Result<Self> {
        match fsutil::read_optional(path)? {
            None => Ok(Self::default()),
            Some(text) => serde_json::from_str(&text).map_err(|source| Error::StateParse {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Writes atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        fsutil::write_atomic(path, &json)
    }

    /// Back to desktop mode: deletes the file.
    pub fn clear(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io(path, e)),
        }
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_is_desktop() {
        let dir = tempfile::tempdir().unwrap();
        let s = RuntimeState::load(&dir.path().join("state.json")).unwrap();
        assert_eq!(s.mode, Mode::Desktop);
        assert!(!s.needs_recovery());
    }

    #[test]
    fn save_load_clear() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        let mut s = RuntimeState::entering();
        s.saved_topology = Some(serde_json::json!({"paths": [1, 2]}));
        s.saved_audio_device = Some("{0.0.0.00000000}.{abc}".into());
        s.mark_done(EnterStep::TvWoken);
        s.mark_done(EnterStep::ExplorerExited);
        s.mark_done(EnterStep::ExplorerExited);
        s.save(&p).unwrap();

        let back = RuntimeState::load(&p).unwrap();
        assert_eq!(back, s);
        assert!(back.needs_recovery());
        assert_eq!(
            back.undo_order().collect::<Vec<_>>(),
            [EnterStep::ExplorerExited, EnterStep::TvWoken]
        );

        RuntimeState::clear(&p).unwrap();
        RuntimeState::clear(&p).unwrap();
        assert_eq!(RuntimeState::load(&p).unwrap().mode, Mode::Desktop);
    }

    #[test]
    fn json_shape_is_stable() {
        let s = RuntimeState {
            mode: Mode::Gaming,
            steps_done: vec![EnterStep::TopologyApplied],
            ..RuntimeState::default()
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"version": 1, "mode": "gaming", "steps_done": ["topology_applied"]})
        );
    }

    #[test]
    fn corrupt_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        std::fs::write(&p, "{not json").unwrap();
        assert!(matches!(
            RuntimeState::load(&p),
            Err(Error::StateParse { .. })
        ));
    }
}
