//! Serialisable copy of a CCD display configuration (plan §2.4).
//!
//! The orchestrator stores a [`Topology`] in `state.json` before switching to
//! the TV and hands it back to [`crate::restore`] on exit or after a crash.
//! It mirrors `DISPLAYCONFIG_PATH_INFO` / `DISPLAYCONFIG_MODE_INFO` field by
//! field so the restore is exact.
//!
//! Adapter LUIDs are not stable across reboots, so each one is saved with its
//! adapter device path and [`Topology::remap_adapters`] patches them before a
//! restore.

use serde::{Deserialize, Serialize};
use uc_core::config::DisplayId;

use crate::{Error, Result};

/// Current [`Topology`] format.
pub const TOPOLOGY_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Topology {
    pub version: u32,
    pub adapters: Vec<Adapter>,
    /// Active paths only, in CCD order.
    pub paths: Vec<PathEntry>,
    pub modes: Vec<ModeEntry>,
    /// HDR state of every connected HDR-capable display, active or not.
    #[serde(default)]
    pub hdr: Vec<HdrState>,
}

/// A graphics adapter `LUID`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Luid {
    pub low: u32,
    pub high: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adapter {
    pub luid: Luid,
    /// `DISPLAYCONFIG_ADAPTER_NAME.adapterDevicePath`, stable across reboots.
    pub device_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rational {
    pub num: u32,
    pub den: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathEntry {
    pub source: PathSource,
    pub target: PathTarget,
    pub flags: u32,
    /// The display at the end of this path. Informational: used for logs and
    /// [`Topology::summary`].
    pub display: DisplayId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathSource {
    pub adapter: Luid,
    pub id: u32,
    /// Raw `modeInfoIdx` union value.
    pub mode_idx: u32,
    pub status: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathTarget {
    pub adapter: Luid,
    pub id: u32,
    /// Raw `modeInfoIdx` union value.
    pub mode_idx: u32,
    pub output_technology: i32,
    pub rotation: i32,
    pub scaling: i32,
    pub refresh: Rational,
    pub scanline_ordering: i32,
    pub available: bool,
    pub status: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoSignal {
    pub pixel_rate: u64,
    pub h_sync: Rational,
    pub v_sync: Rational,
    pub active_width: u32,
    pub active_height: u32,
    pub total_width: u32,
    pub total_height: u32,
    /// Raw `videoStandard` union value.
    pub video_standard: u32,
    pub scanline_ordering: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModeEntry {
    Source {
        adapter: Luid,
        id: u32,
        width: u32,
        height: u32,
        pixel_format: i32,
        x: i32,
        y: i32,
    },
    Target {
        adapter: Luid,
        id: u32,
        signal: VideoSignal,
    },
    DesktopImage {
        adapter: Luid,
        id: u32,
        source_width: i32,
        source_height: i32,
        region: Rect,
        clip: Rect,
    },
}

impl ModeEntry {
    pub fn adapter(&self) -> Luid {
        match *self {
            Self::Source { adapter, .. }
            | Self::Target { adapter, .. }
            | Self::DesktopImage { adapter, .. } => adapter,
        }
    }

    fn adapter_mut(&mut self) -> &mut Luid {
        match self {
            Self::Source { adapter, .. }
            | Self::Target { adapter, .. }
            | Self::DesktopImage { adapter, .. } => adapter,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HdrState {
    pub display: DisplayId,
    pub adapter: Luid,
    pub target_id: u32,
    pub enabled: bool,
}

impl Topology {
    /// For `RuntimeState::saved_topology`.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("Topology always serialises")
    }

    /// Reads back what [`Topology::to_value`] wrote.
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        let t: Self =
            serde_json::from_value(value.clone()).map_err(|e| Error::BadTopology(e.to_string()))?;
        t.check()?;
        Ok(t)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("Topology always serialises")
    }

    pub fn from_json(text: &str) -> Result<Self> {
        let t: Self = serde_json::from_str(text).map_err(|e| Error::BadTopology(e.to_string()))?;
        t.check()?;
        Ok(t)
    }

    /// Structural checks so a hand-edited or truncated file fails here and
    /// not inside `SetDisplayConfig`.
    pub fn check(&self) -> Result<()> {
        if self.version > TOPOLOGY_VERSION {
            return Err(Error::BadTopology(format!(
                "version {} is newer than this build ({TOPOLOGY_VERSION})",
                self.version
            )));
        }
        if self.paths.is_empty() {
            return Err(Error::BadTopology("no active display paths".into()));
        }
        let n = self.modes.len();
        // CCD uses 0xFFFF_FFFF for "no mode". Any other index must be in range.
        let ok = |idx: u32| idx == u32::MAX || (idx as usize) < n;
        for p in &self.paths {
            if !ok(p.source.mode_idx) || !ok(p.target.mode_idx) {
                return Err(Error::BadTopology("mode index out of range".into()));
            }
        }
        Ok(())
    }

    /// Replaces saved adapter LUIDs with the current ones, matched by adapter
    /// device path. Fails if a saved adapter is gone.
    pub fn remap_adapters(&mut self, current: &[Adapter]) -> Result<()> {
        let mut map = Vec::new();
        for saved in &self.adapters {
            let now = current
                .iter()
                .find(|c| c.device_path.eq_ignore_ascii_case(&saved.device_path))
                .ok_or_else(|| {
                    Error::BadTopology(format!("adapter {} is gone", saved.device_path))
                })?;
            if now.luid != saved.luid {
                map.push((saved.luid, now.luid));
            }
        }
        if map.is_empty() {
            return Ok(());
        }
        let fix = |l: &mut Luid| {
            if let Some((_, new)) = map.iter().find(|(old, _)| old == l) {
                *l = *new;
            }
        };
        for a in &mut self.adapters {
            fix(&mut a.luid);
        }
        for p in &mut self.paths {
            fix(&mut p.source.adapter);
            fix(&mut p.target.adapter);
        }
        for m in &mut self.modes {
            fix(m.adapter_mut());
        }
        for h in &mut self.hdr {
            fix(&mut h.adapter);
        }
        Ok(())
    }

    /// One line per active display, e.g. `IRIS TV (HDMI)  3840x2160 @ 60 Hz at (0, 0)`.
    pub fn summary(&self) -> Vec<String> {
        self.paths
            .iter()
            .map(|p| {
                let mut line = crate::identity::describe(&p.display);
                if let Some(ModeEntry::Source {
                    width,
                    height,
                    x,
                    y,
                    ..
                }) = self.modes.get(p.source.mode_idx as usize)
                {
                    line.push_str(&format!("  {width}x{height}"));
                    if let Some(hz) =
                        crate::identity::refresh_hz(p.target.refresh.num, p.target.refresh.den)
                    {
                        line.push_str(&format!(" @ {hz} Hz"));
                    }
                    line.push_str(&format!(" at ({x}, {y})"));
                }
                line
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GPU: Luid = Luid {
        low: 0x1234,
        high: 0,
    };

    fn sample() -> Topology {
        Topology {
            version: TOPOLOGY_VERSION,
            adapters: vec![Adapter {
                luid: GPU,
                device_path: r"\\?\PCI#VEN_10DE&DEV_2684#4&1".into(),
            }],
            paths: vec![PathEntry {
                source: PathSource {
                    adapter: GPU,
                    id: 0,
                    mode_idx: 1,
                    status: 1,
                },
                target: PathTarget {
                    adapter: GPU,
                    id: 4352,
                    mode_idx: 0,
                    output_technology: 5,
                    rotation: 1,
                    scaling: 1,
                    refresh: Rational {
                        num: 60000,
                        den: 1001,
                    },
                    scanline_ordering: 1,
                    available: true,
                    status: 1,
                },
                flags: 1,
                display: DisplayId {
                    device_path: r"\\?\DISPLAY#IRS0001#5&1&UID4352".into(),
                    friendly_name: "IRIS TV".into(),
                    connector: "HDMI".into(),
                },
            }],
            modes: vec![
                ModeEntry::Target {
                    adapter: GPU,
                    id: 4352,
                    signal: VideoSignal {
                        pixel_rate: 594_000_000,
                        h_sync: Rational {
                            num: 135_000,
                            den: 1,
                        },
                        v_sync: Rational {
                            num: 60000,
                            den: 1001,
                        },
                        active_width: 3840,
                        active_height: 2160,
                        total_width: 4400,
                        total_height: 2250,
                        video_standard: 255,
                        scanline_ordering: 1,
                    },
                },
                ModeEntry::Source {
                    adapter: GPU,
                    id: 0,
                    width: 3840,
                    height: 2160,
                    pixel_format: 4,
                    x: 0,
                    y: 0,
                },
            ],
            hdr: vec![HdrState {
                display: DisplayId::default(),
                adapter: GPU,
                target_id: 4352,
                enabled: false,
            }],
        }
    }

    #[test]
    fn roundtrips_through_state_value() {
        let t = sample();
        let back = Topology::from_value(&t.to_value()).unwrap();
        assert_eq!(back, t);
        assert_eq!(Topology::from_json(&t.to_json()).unwrap(), t);
    }

    #[test]
    fn roundtrips_through_runtime_state() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        let mut s = uc_core::RuntimeState::entering();
        s.saved_topology = Some(sample().to_value());
        s.save(&file).unwrap();
        let back = uc_core::RuntimeState::load(&file).unwrap();
        let t = Topology::from_value(back.saved_topology.as_ref().unwrap()).unwrap();
        assert_eq!(t, sample());
    }

    #[test]
    fn mode_entries_are_tagged() {
        let v = sample().to_value();
        assert_eq!(v["modes"][0]["kind"], "target");
        assert_eq!(v["modes"][1]["kind"], "source");
    }

    #[test]
    fn remaps_changed_luid() {
        let mut t = sample();
        let new = Luid {
            low: 0x9999,
            high: 0,
        };
        t.remap_adapters(&[Adapter {
            luid: new,
            device_path: t.adapters[0].device_path.to_lowercase(),
        }])
        .unwrap();
        assert_eq!(t.adapters[0].luid, new);
        assert_eq!(t.paths[0].source.adapter, new);
        assert_eq!(t.paths[0].target.adapter, new);
        assert!(t.modes.iter().all(|m| m.adapter() == new));
        assert_eq!(t.hdr[0].adapter, new);
    }

    #[test]
    fn missing_adapter_fails() {
        let mut t = sample();
        assert!(matches!(t.remap_adapters(&[]), Err(Error::BadTopology(_))));
    }

    #[test]
    fn rejects_bad_files() {
        let mut t = sample();
        t.paths[0].source.mode_idx = 7;
        assert!(Topology::from_json(&t.to_json()).is_err());

        let mut t = sample();
        t.paths[0].source.mode_idx = u32::MAX;
        assert!(Topology::from_json(&t.to_json()).is_ok());

        let mut t = sample();
        t.paths.clear();
        assert!(Topology::from_json(&t.to_json()).is_err());

        let mut t = sample();
        t.version = TOPOLOGY_VERSION + 1;
        assert!(Topology::from_json(&t.to_json()).is_err());

        assert!(Topology::from_json("{").is_err());
    }

    #[test]
    fn summary_line() {
        assert_eq!(
            sample().summary(),
            ["IRIS TV (HDMI)  3840x2160 @ 59.94 Hz at (0, 0)"]
        );
    }
}
