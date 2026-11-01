//! CCD (`QueryDisplayConfig` / `SetDisplayConfig`) implementation.

use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_2DREGION, DISPLAYCONFIG_ADAPTER_NAME, DISPLAYCONFIG_DESKTOP_IMAGE_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_0,
    DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
    DISPLAYCONFIG_MODE_INFO_TYPE_TARGET, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_PIXELFORMAT,
    DISPLAYCONFIG_RATIONAL, DISPLAYCONFIG_ROTATION, DISPLAYCONFIG_SCALING,
    DISPLAYCONFIG_SCANLINE_ORDERING, DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_SOURCE_MODE, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_MODE, DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY,
    DISPLAYCONFIG_VIDEO_SIGNAL_INFO, DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ALL_PATHS, QDC_ONLY_ACTIVE_PATHS, QUERY_DISPLAY_CONFIG_FLAGS,
    QueryDisplayConfig, SDC_ALLOW_CHANGES, SDC_APPLY, SDC_SAVE_TO_DATABASE, SDC_TOPOLOGY_EXTEND,
    SDC_TOPOLOGY_SUPPLIED, SDC_USE_SUPPLIED_DISPLAY_CONFIG, SET_DISPLAY_CONFIG_FLAGS,
    SetDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LUID, POINTL, RECTL};
use windows::Win32::Graphics::Gdi::{
    CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
    DISPLAYCONFIG_PATH_ACTIVE, DISPLAYCONFIG_PATH_MODE_IDX_INVALID, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
    EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

use crate::identity::{self, Found};
use crate::topology::{
    Adapter, HdrState, Luid, ModeEntry, PathEntry, PathSource, PathTarget, Rational, Rect,
    TOPOLOGY_VERSION, VideoSignal,
};
use crate::{
    ApplyOutcome, DisplayId, DisplayInfo, DisplayMode, Error, RestoreOutcome, Result, Topology,
};

/// How often [`wait_available`] re-queries.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// One `QueryDisplayConfig` result.
struct Config {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

impl Config {
    fn query(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<Self> {
        // The layout can change between the size query and the real one;
        // retry a few times on ERROR_INSUFFICIENT_BUFFER.
        for _ in 0..5 {
            let (mut np, mut nm) = (0u32, 0u32);
            // SAFETY: both out-pointers point to live locals.
            let err = unsafe { GetDisplayConfigBufferSizes(flags, &mut np, &mut nm) };
            if err != ERROR_SUCCESS {
                return Err(Error::win32("GetDisplayConfigBufferSizes", err.0));
            }
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
            // SAFETY: the arrays hold `np` / `nm` elements, as the counts say.
            let err = unsafe {
                QueryDisplayConfig(
                    flags,
                    &mut np,
                    paths.as_mut_ptr(),
                    &mut nm,
                    modes.as_mut_ptr(),
                    None,
                )
            };
            if err == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            if err != ERROR_SUCCESS {
                return Err(Error::win32("QueryDisplayConfig", err.0));
            }
            paths.truncate(np as usize);
            modes.truncate(nm as usize);
            return Ok(Self { paths, modes });
        }
        Err(Error::win32(
            "QueryDisplayConfig",
            ERROR_INSUFFICIENT_BUFFER.0,
        ))
    }

    fn source_mode(&self, path: &DISPLAYCONFIG_PATH_INFO) -> Option<DISPLAYCONFIG_SOURCE_MODE> {
        let m = self.modes.get(source_idx(path) as usize)?;
        if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            return None;
        }
        // SAFETY: `infoType` says the source member is the live one.
        Some(unsafe { m.Anonymous.sourceMode })
    }

    /// Distinct adapters referenced by this config.
    fn adapters(&self) -> Vec<Adapter> {
        let mut luids: Vec<Luid> = Vec::new();
        let all = self
            .paths
            .iter()
            .flat_map(|p| [p.sourceInfo.adapterId, p.targetInfo.adapterId])
            .chain(self.modes.iter().map(|m| m.adapterId));
        for l in all.map(luid_from) {
            if !luids.contains(&l) {
                luids.push(l);
            }
        }
        luids
            .into_iter()
            .filter_map(|luid| match adapter_path(luid) {
                Ok(device_path) => Some(Adapter { luid, device_path }),
                Err(e) => {
                    warn!(?luid, "skipping adapter without a name: {e}");
                    None
                }
            })
            .collect()
    }
}

fn source_idx(p: &DISPLAYCONFIG_PATH_INFO) -> u32 {
    // SAFETY: plain u32 view of the union; we never set QDC_VIRTUAL_MODE_AWARE.
    unsafe { p.sourceInfo.Anonymous.modeInfoIdx }
}

fn target_idx(p: &DISPLAYCONFIG_PATH_INFO) -> u32 {
    // SAFETY: as above.
    unsafe { p.targetInfo.Anonymous.modeInfoIdx }
}

fn is_active(p: &DISPLAYCONFIG_PATH_INFO) -> bool {
    p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
}

fn luid_from(l: LUID) -> Luid {
    Luid {
        low: l.LowPart,
        high: l.HighPart,
    }
}

fn luid_to(l: Luid) -> LUID {
    LUID {
        LowPart: l.low,
        HighPart: l.high,
    }
}

fn rational_from(r: DISPLAYCONFIG_RATIONAL) -> Rational {
    Rational {
        num: r.Numerator,
        den: r.Denominator,
    }
}

fn rational_to(r: Rational) -> DISPLAYCONFIG_RATIONAL {
    DISPLAYCONFIG_RATIONAL {
        Numerator: r.num,
        Denominator: r.den,
    }
}

fn rect_from(r: RECTL) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

fn rect_to(r: Rect) -> RECTL {
    RECTL {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

fn header<T>(
    kind: windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_TYPE,
    adapter: Luid,
    id: u32,
) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: size_of::<T>() as u32,
        adapterId: luid_to(adapter),
        id,
    }
}

/// `DisplayConfigGetDeviceInfo` for a packet that starts with its header.
///
/// # Safety
/// `T` must be a `DISPLAYCONFIG_*` packet whose first field is a
/// `DISPLAYCONFIG_DEVICE_INFO_HEADER` describing `T`.
unsafe fn get_info<T>(packet: &mut T, call: &'static str) -> Result<()> {
    // SAFETY: guaranteed by the caller; the header's size matches `T`.
    let err = unsafe { DisplayConfigGetDeviceInfo(packet as *mut T as *mut _) };
    if err == 0 {
        Ok(())
    } else {
        Err(Error::win32(call, err))
    }
}

fn target_name(adapter: Luid, target_id: u32) -> Result<DISPLAYCONFIG_TARGET_DEVICE_NAME> {
    let mut p = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: header::<DISPLAYCONFIG_TARGET_DEVICE_NAME>(
            DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            adapter,
            target_id,
        ),
        ..Default::default()
    };
    // SAFETY: the packet starts with a matching header.
    unsafe { get_info(&mut p, "DisplayConfigGetDeviceInfo(TARGET_NAME)")? };
    Ok(p)
}

fn display_id(p: &DISPLAYCONFIG_PATH_INFO) -> Result<DisplayId> {
    let n = target_name(luid_from(p.targetInfo.adapterId), p.targetInfo.id)?;
    // The name packet's own output technology is the monitor's connector; the
    // path's can say "internal" for some docks, so prefer the packet's.
    Ok(DisplayId {
        device_path: wide_to_string(&n.monitorDevicePath),
        friendly_name: wide_to_string(&n.monitorFriendlyDeviceName),
        connector: identity::connector_name(n.outputTechnology.0).to_owned(),
    })
}

fn adapter_path(adapter: Luid) -> Result<String> {
    let mut p = DISPLAYCONFIG_ADAPTER_NAME {
        header: header::<DISPLAYCONFIG_ADAPTER_NAME>(
            DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
            adapter,
            0,
        ),
        ..Default::default()
    };
    // SAFETY: the packet starts with a matching header.
    unsafe { get_info(&mut p, "DisplayConfigGetDeviceInfo(ADAPTER_NAME)")? };
    Ok(wide_to_string(&p.adapterDevicePath))
}

/// GDI name of a source, e.g. `\\.\DISPLAY1`, NUL-terminated for Win32.
fn source_gdi_name(adapter: Luid, source_id: u32) -> Result<Vec<u16>> {
    let mut p = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: header::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            adapter,
            source_id,
        ),
        ..Default::default()
    };
    // SAFETY: the packet starts with a matching header.
    unsafe { get_info(&mut p, "DisplayConfigGetDeviceInfo(SOURCE_NAME)")? };
    let end = p
        .viewGdiDeviceName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(p.viewGdiDeviceName.len());
    let mut name = p.viewGdiDeviceName[..end].to_vec();
    name.push(0);
    Ok(name)
}

/// `(supported, enabled)`, or `None` if the driver does not answer.
fn hdr_state(adapter: Luid, target_id: u32) -> Option<(bool, bool)> {
    let mut p = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
        header: header::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>(
            DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
            adapter,
            target_id,
        ),
        ..Default::default()
    };
    // SAFETY: the packet starts with a matching header.
    unsafe { get_info(&mut p, "DisplayConfigGetDeviceInfo(ADVANCED_COLOR_INFO)") }.ok()?;
    // SAFETY: plain u32 view of the bitfield union.
    let bits = unsafe { p.Anonymous.value };
    // Bit 0: advancedColorSupported, bit 1: advancedColorEnabled,
    // bit 3: advancedColorForceDisabled.
    let supported = bits & 0b1 != 0 && bits & 0b1000 == 0;
    Some((supported, bits & 0b10 != 0))
}

fn set_hdr(adapter: Luid, target_id: u32, enabled: bool) -> Result<()> {
    let mut p = DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE {
        header: header::<DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>(
            DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
            adapter,
            target_id,
        ),
        ..Default::default()
    };
    // Bit 0: enableAdvancedColor.
    p.Anonymous.value = u32::from(enabled);
    // SAFETY: the packet starts with a matching header.
    let err = unsafe { DisplayConfigSetDeviceInfo(&p.header) };
    if err == 0 {
        Ok(())
    } else {
        Err(Error::win32(
            "DisplayConfigSetDeviceInfo(ADVANCED_COLOR_STATE)",
            err,
        ))
    }
}

fn set_config(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
    flags: SET_DISPLAY_CONFIG_FLAGS,
) -> Result<()> {
    let paths = (!paths.is_empty()).then_some(paths);
    let modes = (!modes.is_empty()).then_some(modes);
    // SAFETY: the slices are valid for the call; CCD copies them.
    let err = unsafe { SetDisplayConfig(paths, modes, flags) };
    if err == 0 {
        Ok(())
    } else {
        Err(Error::win32("SetDisplayConfig", err))
    }
}

/// Every display mode the driver offers for a source, whole Hz.
fn driver_modes(gdi_name: &[u16]) -> Vec<DisplayMode> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut dm = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: `gdi_name` is NUL-terminated and `dm` is a sized DEVMODEW.
        let ok = unsafe {
            EnumDisplaySettingsW(
                PCWSTR(gdi_name.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &mut dm,
            )
        };
        if !ok.as_bool() {
            break;
        }
        out.push(DisplayMode {
            width: dm.dmPelsWidth,
            height: dm.dmPelsHeight,
            refresh_hz: (dm.dmDisplayFrequency > 1).then_some(f64::from(dm.dmDisplayFrequency)),
        });
    }
    identity::normalize_modes(&mut out);
    out
}

/// Sets resolution/refresh on a source without saving it to the registry.
fn set_mode(gdi_name: &[u16], mode: &DisplayMode) -> Result<()> {
    let mut dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: as in `driver_modes`.
    let ok =
        unsafe { EnumDisplaySettingsW(PCWSTR(gdi_name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) };
    if !ok.as_bool() {
        return Err(Error::win32("EnumDisplaySettingsW", 0u32));
    }
    let same_rate = mode
        .refresh_hz
        .is_none_or(|hz| dm.dmDisplayFrequency == hz as u32);
    if dm.dmPelsWidth == mode.width && dm.dmPelsHeight == mode.height && same_rate {
        // Already there: a mode set would only blank the TV for a moment.
        return Ok(());
    }
    dm.dmPelsWidth = mode.width;
    dm.dmPelsHeight = mode.height;
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
    if let Some(hz) = mode.refresh_hz {
        dm.dmDisplayFrequency = hz as u32;
        dm.dmFields |= DM_DISPLAYFREQUENCY;
    }
    // SAFETY: `gdi_name` is NUL-terminated and `dm` is a filled DEVMODEW.
    // CDS_UPDATEREGISTRY: persisted, so a game that resets the display
    // mode on exit (ChangeDisplaySettings(NULL)) lands on this one.
    let r = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(gdi_name.as_ptr()),
            Some(&dm),
            None,
            CDS_UPDATEREGISTRY,
            None,
        )
    };
    if r == DISP_CHANGE_SUCCESSFUL {
        Ok(())
    } else {
        Err(Error::win32("ChangeDisplaySettingsExW", r.0))
    }
}

/// A connected display: one entry per (adapter, target id).
struct Target {
    adapter: Luid,
    id: u32,
    display: DisplayId,
    /// Index into `Config::paths` of the path to use for it: the active one
    /// if any, otherwise the first available one.
    path: usize,
    active: bool,
}

fn targets(cfg: &Config) -> Vec<Target> {
    let mut out: Vec<Target> = Vec::new();
    for (i, p) in cfg.paths.iter().enumerate() {
        if !p.targetInfo.targetAvailable.as_bool() {
            continue;
        }
        let adapter = luid_from(p.targetInfo.adapterId);
        let id = p.targetInfo.id;
        if let Some(t) = out.iter_mut().find(|t| t.adapter == adapter && t.id == id) {
            if is_active(p) && !t.active {
                t.path = i;
                t.active = true;
            }
            continue;
        }
        match display_id(p) {
            Ok(display) => out.push(Target {
                adapter,
                id,
                display,
                path: i,
                active: is_active(p),
            }),
            Err(e) => debug!(target_id = id, "skipping target without a name: {e}"),
        }
    }
    out
}

fn find_target<'a>(targets: &'a [Target], want: &DisplayId) -> Result<&'a Target> {
    match identity::find_display(want, targets.iter().map(|t| &t.display)) {
        Found::One(i, kind) => {
            debug!(?kind, display = %identity::describe(&targets[i].display), "matched display");
            Ok(&targets[i])
        }
        Found::Ambiguous => Err(Error::Ambiguous(identity::describe(want))),
        Found::None => Err(Error::NotFound(identity::describe(want))),
    }
}

pub fn list() -> Result<Vec<DisplayInfo>> {
    let cfg = Config::query(QDC_ALL_PATHS)?;
    let mut rows: Vec<(Option<(i32, i32)>, DisplayInfo)> = targets(&cfg)
        .into_iter()
        .map(|t| {
            let path = &cfg.paths[t.path];
            let source = t.active.then(|| cfg.source_mode(path)).flatten();
            let refresh = identity::refresh_hz(
                path.targetInfo.refreshRate.Numerator,
                path.targetInfo.refreshRate.Denominator,
            );
            let modes = if t.active {
                source_gdi_name(luid_from(path.sourceInfo.adapterId), path.sourceInfo.id)
                    .map(|n| driver_modes(&n))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let pos = source.map(|s| (s.position.x, s.position.y));
            let info = DisplayInfo {
                id: t.display,
                index: 0,
                active: t.active,
                primary: pos == Some((0, 0)),
                current_mode: source.map(|s| DisplayMode {
                    width: s.width,
                    height: s.height,
                    refresh_hz: refresh,
                }),
                modes,
                hdr_supported: hdr_state(t.adapter, t.id).is_some_and(|(s, _)| s),
            };
            (pos, info)
        })
        .collect();
    // Active displays left to right (then top to bottom), inactive ones after.
    rows.sort_by(|(a, ai), (b, bi)| match (a, b) {
        (Some((ax, ay)), Some((bx, by))) => (ax, ay).cmp(&(bx, by)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => ai.id.friendly_name.cmp(&bi.id.friendly_name),
    });
    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(i, (_, mut info))| {
            info.index = i as u32 + 1;
            info
        })
        .collect())
}

pub fn snapshot() -> Result<Topology> {
    let cfg = Config::query(QDC_ONLY_ACTIVE_PATHS)?;
    let all = Config::query(QDC_ALL_PATHS)?;

    let paths = cfg
        .paths
        .iter()
        .map(|p| PathEntry {
            source: PathSource {
                adapter: luid_from(p.sourceInfo.adapterId),
                id: p.sourceInfo.id,
                mode_idx: source_idx(p),
                status: p.sourceInfo.statusFlags,
            },
            target: PathTarget {
                adapter: luid_from(p.targetInfo.adapterId),
                id: p.targetInfo.id,
                mode_idx: target_idx(p),
                output_technology: p.targetInfo.outputTechnology.0,
                rotation: p.targetInfo.rotation.0,
                scaling: p.targetInfo.scaling.0,
                refresh: rational_from(p.targetInfo.refreshRate),
                scanline_ordering: p.targetInfo.scanLineOrdering.0,
                available: p.targetInfo.targetAvailable.as_bool(),
                status: p.targetInfo.statusFlags,
            },
            flags: p.flags,
            display: display_id(p).unwrap_or_default(),
        })
        .collect();

    let modes = cfg.modes.iter().filter_map(mode_from).collect::<Vec<_>>();
    if modes.len() != cfg.modes.len() {
        // Indices into the array would shift; refuse rather than save garbage.
        return Err(Error::BadTopology("unknown CCD mode type".into()));
    }

    let hdr = targets(&all)
        .into_iter()
        .filter_map(|t| match hdr_state(t.adapter, t.id) {
            Some((true, enabled)) => Some(HdrState {
                display: t.display,
                adapter: t.adapter,
                target_id: t.id,
                enabled,
            }),
            _ => None,
        })
        .collect();

    let t = Topology {
        version: TOPOLOGY_VERSION,
        // Only adapters the active layout uses, so an unrelated adapter going
        // away (e.g. iGPU disabled) does not break the exact restore.
        adapters: cfg.adapters(),
        paths,
        modes,
        hdr,
    };
    t.check()?;
    info!(displays = ?t.summary(), "display topology saved");
    Ok(t)
}

fn mode_from(m: &DISPLAYCONFIG_MODE_INFO) -> Option<ModeEntry> {
    let adapter = luid_from(m.adapterId);
    let id = m.id;
    // SAFETY (all three arms): the union member is chosen by `infoType`.
    Some(match m.infoType {
        DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE => {
            let s = unsafe { m.Anonymous.sourceMode };
            ModeEntry::Source {
                adapter,
                id,
                width: s.width,
                height: s.height,
                pixel_format: s.pixelFormat.0,
                x: s.position.x,
                y: s.position.y,
            }
        }
        DISPLAYCONFIG_MODE_INFO_TYPE_TARGET => {
            let v = unsafe { m.Anonymous.targetMode.targetVideoSignalInfo };
            ModeEntry::Target {
                adapter,
                id,
                signal: VideoSignal {
                    pixel_rate: v.pixelRate,
                    h_sync: rational_from(v.hSyncFreq),
                    v_sync: rational_from(v.vSyncFreq),
                    active_width: v.activeSize.cx,
                    active_height: v.activeSize.cy,
                    total_width: v.totalSize.cx,
                    total_height: v.totalSize.cy,
                    video_standard: unsafe { v.Anonymous.videoStandard },
                    scanline_ordering: v.scanLineOrdering.0,
                },
            }
        }
        DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE => {
            let d = unsafe { m.Anonymous.desktopImageInfo };
            ModeEntry::DesktopImage {
                adapter,
                id,
                source_width: d.PathSourceSize.x,
                source_height: d.PathSourceSize.y,
                region: rect_from(d.DesktopImageRegion),
                clip: rect_from(d.DesktopImageClip),
            }
        }
        _ => return None,
    })
}

fn mode_to(m: &ModeEntry) -> DISPLAYCONFIG_MODE_INFO {
    let (info_type, adapter, id, anon) = match *m {
        ModeEntry::Source {
            adapter,
            id,
            width,
            height,
            pixel_format,
            x,
            y,
        } => (
            DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
            adapter,
            id,
            DISPLAYCONFIG_MODE_INFO_0 {
                sourceMode: DISPLAYCONFIG_SOURCE_MODE {
                    width,
                    height,
                    pixelFormat: DISPLAYCONFIG_PIXELFORMAT(pixel_format),
                    position: POINTL { x, y },
                },
            },
        ),
        ModeEntry::Target {
            adapter,
            id,
            signal: s,
        } => {
            let mut v = DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
                pixelRate: s.pixel_rate,
                hSyncFreq: rational_to(s.h_sync),
                vSyncFreq: rational_to(s.v_sync),
                activeSize: DISPLAYCONFIG_2DREGION {
                    cx: s.active_width,
                    cy: s.active_height,
                },
                totalSize: DISPLAYCONFIG_2DREGION {
                    cx: s.total_width,
                    cy: s.total_height,
                },
                scanLineOrdering: DISPLAYCONFIG_SCANLINE_ORDERING(s.scanline_ordering),
                ..Default::default()
            };
            v.Anonymous.videoStandard = s.video_standard;
            (
                DISPLAYCONFIG_MODE_INFO_TYPE_TARGET,
                adapter,
                id,
                DISPLAYCONFIG_MODE_INFO_0 {
                    targetMode: DISPLAYCONFIG_TARGET_MODE {
                        targetVideoSignalInfo: v,
                    },
                },
            )
        }
        ModeEntry::DesktopImage {
            adapter,
            id,
            source_width,
            source_height,
            region,
            clip,
        } => (
            DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE,
            adapter,
            id,
            DISPLAYCONFIG_MODE_INFO_0 {
                desktopImageInfo: DISPLAYCONFIG_DESKTOP_IMAGE_INFO {
                    PathSourceSize: POINTL {
                        x: source_width,
                        y: source_height,
                    },
                    DesktopImageRegion: rect_to(region),
                    DesktopImageClip: rect_to(clip),
                },
            },
        ),
    };
    DISPLAYCONFIG_MODE_INFO {
        infoType: info_type,
        id,
        adapterId: luid_to(adapter),
        Anonymous: anon,
    }
}

fn path_to(p: &PathEntry) -> DISPLAYCONFIG_PATH_INFO {
    let mut out = DISPLAYCONFIG_PATH_INFO {
        flags: p.flags,
        ..Default::default()
    };
    let s = &mut out.sourceInfo;
    s.adapterId = luid_to(p.source.adapter);
    s.id = p.source.id;
    s.Anonymous.modeInfoIdx = p.source.mode_idx;
    s.statusFlags = p.source.status;
    let t = &mut out.targetInfo;
    t.adapterId = luid_to(p.target.adapter);
    t.id = p.target.id;
    t.Anonymous.modeInfoIdx = p.target.mode_idx;
    t.outputTechnology = DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY(p.target.output_technology);
    t.rotation = DISPLAYCONFIG_ROTATION(p.target.rotation);
    t.scaling = DISPLAYCONFIG_SCALING(p.target.scaling);
    t.refreshRate = rational_to(p.target.refresh);
    t.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING(p.target.scanline_ordering);
    t.targetAvailable = p.target.available.into();
    t.statusFlags = p.target.status;
    out
}

/// Puts saved HDR states back. Best effort: a display that is inactive or
/// gone just logs.
fn restore_hdr(t: &Topology) {
    for h in &t.hdr {
        match hdr_state(h.adapter, h.target_id) {
            Some((true, now)) if now != h.enabled => {
                match set_hdr(h.adapter, h.target_id, h.enabled) {
                    Ok(()) => {
                        info!(display = %identity::describe(&h.display), enabled = h.enabled, "HDR restored")
                    }
                    Err(e) => {
                        debug!(display = %identity::describe(&h.display), "HDR not restored yet: {e}")
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn restore(saved: &Topology) -> Result<RestoreOutcome> {
    saved.check()?;
    let mut t = saved.clone();
    // HDR first, while displays turned on for gaming are still active, and
    // again after the layout change for the ones that just came back.
    restore_hdr(&t);

    let mut try_exact = || {
        Config::query(QDC_ALL_PATHS).and_then(|now| {
            t.remap_adapters(&now.adapters())?;
            let paths: Vec<_> = t.paths.iter().map(path_to).collect();
            let modes: Vec<_> = t.modes.iter().map(mode_to).collect();
            set_config(
                &paths,
                &modes,
                SDC_APPLY
                    | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                    | SDC_SAVE_TO_DATABASE
                    | SDC_ALLOW_CHANGES,
            )
        })
    };
    // ERROR_GEN_FAILURE (31) shows up now and then right after the TV mode
    // was in use (the driver is still settling); a second try usually works.
    let exact = try_exact().or_else(|e| {
        debug!("exact topology restore failed ({e}); retrying once");
        std::thread::sleep(Duration::from_millis(750));
        try_exact()
    });
    let outcome = match exact {
        Ok(()) => RestoreOutcome::Exact,
        Err(e) => {
            warn!("exact topology restore failed ({e}); asking for the saved topology");
            // Windows' database holds the TV-only layout while gaming (it is
            // saved so that a game resetting the display mode can't bring
            // the monitor back), so first supply the saved topology and let
            // Windows pick the modes.
            let paths: Vec<_> = t
                .paths
                .iter()
                .map(|p| {
                    let mut p = path_to(p);
                    p.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
                    p.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
                    p
                })
                .collect();
            match set_config(&paths, &[], SDC_APPLY | SDC_TOPOLOGY_SUPPLIED) {
                Ok(()) => RestoreOutcome::Database,
                Err(e) => {
                    warn!("saved topology refused ({e}); extending all displays");
                    set_config(&[], &[], SDC_APPLY | SDC_TOPOLOGY_EXTEND)?;
                    RestoreOutcome::Extend
                }
            }
        }
    };
    restore_hdr(&t);
    info!(?outcome, "display topology restored");
    Ok(outcome)
}

pub fn apply_single(
    want: &DisplayId,
    mode: Option<&DisplayMode>,
    hdr: bool,
) -> Result<ApplyOutcome> {
    let all = Config::query(QDC_ALL_PATHS)?;
    let targets = targets(&all);
    let target = find_target(&targets, want)?;
    let matched = target.display.clone();
    let (adapter, target_id) = (target.adapter, target.id);
    if is_single_active(adapter, target_id)? {
        debug!(display = %identity::describe(&matched), "already the only active display");
    } else {
        info!(display = %identity::describe(&matched), "switching to single display");
        activate_only(&all, target)?;
    }

    let mut warnings = Vec::new();
    let mut warn_user = |msg: String| {
        warn!("{msg}");
        warnings.push(msg);
    };

    if mode.is_none() {
        match active_source_name(adapter, target_id) {
            Ok(name) => {
                let modes = driver_modes(&name);
                match identity::auto_mode(&modes) {
                    Some(m) => {
                        info!(width = m.width, height = m.height, hz = ?m.refresh_hz, "automatic TV mode");
                        if let Err(e) = set_mode(&name, m) {
                            warn_user(format!("could not set {}x{}: {e}", m.width, m.height));
                        }
                    }
                    None => debug!("no 16:9 mode listed; keeping the TV's own mode"),
                }
            }
            Err(e) => warn_user(format!("could not read the display's modes: {e}")),
        }
    }

    if let Some(want_mode) = mode {
        match active_source_name(adapter, target_id) {
            Ok(name) => {
                let modes = driver_modes(&name);
                match identity::pick_mode(&modes, want_mode) {
                    Some(m) => {
                        if let Err(e) = set_mode(&name, m) {
                            warn_user(format!("could not set {}x{}: {e}", m.width, m.height));
                        }
                    }
                    None => warn_user(format!(
                        "{}x{}{} is not offered by {}; keeping its current mode",
                        want_mode.width,
                        want_mode.height,
                        want_mode
                            .refresh_hz
                            .map(|hz| format!(" @ {hz} Hz"))
                            .unwrap_or_default(),
                        identity::describe(&matched)
                    )),
                }
            }
            Err(e) => warn_user(format!("could not read the display's modes: {e}")),
        }
    }

    if hdr {
        match hdr_state(adapter, target_id) {
            Some((true, true)) => {}
            Some((true, false)) => {
                if let Err(e) = set_hdr(adapter, target_id, true) {
                    warn_user(format!("could not turn HDR on: {e}"));
                }
            }
            _ => warn_user(format!(
                "{} does not support HDR",
                identity::describe(&matched)
            )),
        }
    }

    Ok(ApplyOutcome {
        display: matched,
        warnings,
    })
}

/// True when exactly one path is active and it ends at this target.
fn is_single_active(adapter: Luid, target_id: u32) -> Result<bool> {
    let cfg = Config::query(QDC_ONLY_ACTIVE_PATHS)?;
    Ok(cfg.paths.len() == 1
        && luid_from(cfg.paths[0].targetInfo.adapterId) == adapter
        && cfg.paths[0].targetInfo.id == target_id)
}

fn active_source_name(adapter: Luid, target_id: u32) -> Result<Vec<u16>> {
    let cfg = Config::query(QDC_ONLY_ACTIVE_PATHS)?;
    let p = cfg
        .paths
        .iter()
        .find(|p| luid_from(p.targetInfo.adapterId) == adapter && p.targetInfo.id == target_id)
        .ok_or_else(|| Error::NotFound("the target is not active".into()))?;
    source_gdi_name(luid_from(p.sourceInfo.adapterId), p.sourceInfo.id)
}

/// Makes `target` the only active path. The single source lands at (0, 0),
/// which makes it the primary display.
fn activate_only(all: &Config, target: &Target) -> Result<()> {
    // 1. Already active (e.g. extended desktop): keep its current modes and
    //    drop the other paths. This is the most predictable switch.
    if target.active {
        let active = Config::query(QDC_ONLY_ACTIVE_PATHS)?;
        if let Some(p) = active.paths.iter().find(|p| {
            luid_from(p.targetInfo.adapterId) == target.adapter && p.targetInfo.id == target.id
        }) {
            let mut path = *p;
            let mut modes = Vec::new();
            let mut take = |idx: u32| -> u32 {
                match active.modes.get(idx as usize) {
                    Some(m) => {
                        modes.push(*m);
                        modes.len() as u32 - 1
                    }
                    None => DISPLAYCONFIG_PATH_MODE_IDX_INVALID,
                }
            };
            path.sourceInfo.Anonymous.modeInfoIdx = take(source_idx(p));
            path.targetInfo.Anonymous.modeInfoIdx = take(target_idx(p));
            for m in &mut modes {
                if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                    // Writing a Copy union field is safe; `infoType` says it is the live one.
                    m.Anonymous.sourceMode.position = POINTL { x: 0, y: 0 };
                }
            }
            match set_config(
                &[path],
                &modes,
                SDC_APPLY
                    | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                    | SDC_ALLOW_CHANGES
                    | SDC_SAVE_TO_DATABASE,
            ) {
                Ok(()) => return Ok(()),
                Err(e) => warn!("keeping current modes failed ({e}); letting Windows pick"),
            }
        }
    }

    // 2. Inactive (or 1. failed): supply only the path and let Windows pick
    //    the modes.
    let mut path = all.paths[target.path];
    path.flags = DISPLAYCONFIG_PATH_ACTIVE;
    path.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    match set_config(
        &[path],
        &[],
        SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE,
    ) {
        Ok(()) => Ok(()),
        // 3. Some drivers want the database to fill in the modes.
        Err(e) => {
            warn!("supplied single-path config refused ({e}); trying TOPOLOGY_SUPPLIED");
            set_config(
                &[path],
                &[],
                SDC_APPLY | SDC_TOPOLOGY_SUPPLIED | SDC_ALLOW_CHANGES,
            )
        }
    }
}

pub fn wait_available(want: &DisplayId, timeout: Duration) -> Result<DisplayId> {
    let start = Instant::now();
    loop {
        let cfg = Config::query(QDC_ALL_PATHS)?;
        match find_target(&targets(&cfg), want) {
            Ok(t) => {
                info!(display = %identity::describe(&t.display), waited = ?start.elapsed(), "display available");
                return Ok(t.display.clone());
            }
            Err(e @ Error::Ambiguous(_)) => return Err(e),
            Err(_) => {}
        }
        if start.elapsed() >= timeout {
            return Err(Error::Timeout {
                display: identity::describe(want),
                timeout,
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Desktop rectangle and index of each active display, for `identify`.
pub(super) fn active_rects() -> Result<Vec<(u32, RECTL)>> {
    let cfg = Config::query(QDC_ALL_PATHS)?;
    let infos = list()?;
    let mut out = Vec::new();
    for t in targets(&cfg).iter().filter(|t| t.active) {
        let Some(s) = cfg.source_mode(&cfg.paths[t.path]) else {
            continue;
        };
        let Some(info) = infos.iter().find(|i| i.id == t.display) else {
            continue;
        };
        out.push((
            info.index,
            RECTL {
                left: s.position.x,
                top: s.position.y,
                right: s.position.x + s.width as i32,
                bottom: s.position.y + s.height as i32,
            },
        ));
    }
    Ok(out)
}
