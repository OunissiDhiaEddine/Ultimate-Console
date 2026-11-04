//! Pure display-matching and mode-picking logic (plan §2.4). No Win32 here,
//! so it is unit-tested on every platform.

use uc_core::config::{DisplayId, DisplayMode};

/// How a saved [`DisplayId`] matched a connected display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// Same monitor device path (EDID-based, the normal case).
    DevicePath,
    /// Device path changed (driver update, other port) but friendly name and
    /// connector still match exactly one display.
    NameAndConnector,
}

/// Short human label, e.g. `IRIS TV (HDMI)`.
pub fn describe(id: &DisplayId) -> String {
    let name = if id.friendly_name.is_empty() {
        id.device_path.as_str()
    } else {
        id.friendly_name.as_str()
    };
    if id.connector.is_empty() {
        name.to_owned()
    } else {
        format!("{name} ({})", id.connector)
    }
}

/// Outcome of [`find_display`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    One(usize, MatchKind),
    None,
    /// The fallback matched several displays (e.g. two identical monitors).
    Ambiguous,
}

/// Finds `want` among `candidates` (one entry per connected display).
/// The device path wins; the name + connector fallback is only used when it
/// is unambiguous.
pub fn find_display<'a>(
    want: &DisplayId,
    candidates: impl IntoIterator<Item = &'a DisplayId>,
) -> Found {
    let candidates: Vec<&DisplayId> = candidates.into_iter().collect();
    if !want.device_path.is_empty()
        && let Some(i) = candidates
            .iter()
            .position(|c| c.device_path.eq_ignore_ascii_case(&want.device_path))
    {
        return Found::One(i, MatchKind::DevicePath);
    }
    if want.friendly_name.is_empty() {
        return Found::None;
    }
    let mut hits = candidates.iter().enumerate().filter(|(_, c)| {
        c.friendly_name == want.friendly_name
            && (want.connector.is_empty() || c.connector == want.connector)
    });
    match (hits.next(), hits.next()) {
        (Some((i, _)), None) => Found::One(i, MatchKind::NameAndConnector),
        (Some(_), Some(_)) => Found::Ambiguous,
        _ => Found::None,
    }
}

/// Connector name for a `DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY` value.
pub fn connector_name(output_technology: i32) -> &'static str {
    match output_technology {
        0 => "VGA",
        1 => "S-Video",
        2 => "Composite",
        3 => "Component",
        4 => "DVI",
        5 => "HDMI",
        6 => "LVDS",
        8 => "D-JPN",
        9 => "SDI",
        10 => "DisplayPort",
        11 => "eDP",
        12 => "UDI",
        13 => "UDI (embedded)",
        14 => "SDTV dongle",
        15 => "Miracast",
        16 | 17 => "Indirect",
        18 => "USB-C DisplayPort",
        i32::MIN => "Internal",
        _ => "",
    }
}

/// Refresh rate in Hz from a CCD rational, rounded to two decimals
/// (`60000/1001` → `59.94`). `None` when the rate is unknown.
pub fn refresh_hz(numerator: u32, denominator: u32) -> Option<f64> {
    if numerator == 0 || denominator == 0 {
        return None;
    }
    let hz = f64::from(numerator) / f64::from(denominator);
    Some((hz * 100.0).round() / 100.0)
}

/// Picks the mode to set for `want` from the modes the driver lists
/// (`EnumDisplaySettings`, whole Hz). Width and height must match exactly.
/// With no refresh asked, the highest one wins. A fractional rate like 59.94
/// is listed by Windows as 59, so the rounded-down rate is preferred, then
/// the nearest within 1 Hz.
pub fn pick_mode<'a>(available: &'a [DisplayMode], want: &DisplayMode) -> Option<&'a DisplayMode> {
    let same_size = available
        .iter()
        .filter(|m| m.width == want.width && m.height == want.height);
    let Some(hz) = want.refresh_hz else {
        return same_size.max_by(|a, b| hz_of(a).total_cmp(&hz_of(b)));
    };
    let same_size: Vec<_> = same_size.collect();
    if hz.fract() != 0.0
        && let Some(m) = same_size.iter().find(|m| hz_of(m) == hz.floor())
    {
        return Some(m);
    }
    same_size
        .into_iter()
        .filter(|m| (hz_of(m) - hz).abs() < 1.0)
        .min_by(|a, b| (hz_of(a) - hz).abs().total_cmp(&(hz_of(b) - hz).abs()))
}

fn hz_of(m: &DisplayMode) -> f64 {
    m.refresh_hz.unwrap_or(0.0)
}

/// Sorts modes largest first and drops duplicates (the driver lists each
/// size/rate once per colour depth).
pub fn normalize_modes(modes: &mut Vec<DisplayMode>) {
    modes.sort_by(|a, b| {
        (b.width * b.height)
            .cmp(&(a.width * a.height))
            .then(b.width.cmp(&a.width))
            .then(hz_of(b).total_cmp(&hz_of(a)))
    });
    modes.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(path: &str, name: &str, conn: &str) -> DisplayId {
        DisplayId {
            device_path: path.into(),
            friendly_name: name.into(),
            connector: conn.into(),
        }
    }

    fn mode(w: u32, h: u32, hz: f64) -> DisplayMode {
        DisplayMode {
            width: w,
            height: h,
            refresh_hz: Some(hz),
        }
    }

    #[test]
    fn device_path_wins_case_insensitive() {
        let c = [
            id(r"\\?\DISPLAY#AAA#1", "IRIS TV", "HDMI"),
            id(r"\\?\DISPLAY#IRS0001#2", "IRIS TV", "HDMI"),
        ];
        let want = id(r"\\?\display#irs0001#2", "whatever", "");
        assert_eq!(
            find_display(&want, &c),
            Found::One(1, MatchKind::DevicePath)
        );
    }

    #[test]
    fn falls_back_to_name_and_connector() {
        let c = [
            id("p1", "DELL U2720Q", "DisplayPort"),
            id("p2-new", "IRIS TV", "HDMI"),
        ];
        let want = id("p2-old", "IRIS TV", "HDMI");
        assert_eq!(
            find_display(&want, &c),
            Found::One(1, MatchKind::NameAndConnector)
        );
        let wrong_port = id("p2-old", "IRIS TV", "DisplayPort");
        assert_eq!(find_display(&wrong_port, &c), Found::None);
    }

    #[test]
    fn identical_monitors_are_ambiguous() {
        let c = [
            id("a", "VG27", "DisplayPort"),
            id("b", "VG27", "DisplayPort"),
        ];
        assert_eq!(
            find_display(&id("gone", "VG27", "DisplayPort"), &c),
            Found::Ambiguous
        );
        assert_eq!(find_display(&DisplayId::default(), &c), Found::None);
    }

    #[test]
    fn describe_labels() {
        assert_eq!(describe(&id("p", "IRIS TV", "HDMI")), "IRIS TV (HDMI)");
        assert_eq!(describe(&id("p", "", "")), "p");
    }

    #[test]
    fn connectors() {
        assert_eq!(connector_name(5), "HDMI");
        assert_eq!(connector_name(10), "DisplayPort");
        assert_eq!(connector_name(i32::MIN), "Internal");
        assert_eq!(connector_name(-1), "");
    }

    #[test]
    fn refresh_rounding() {
        assert_eq!(refresh_hz(60000, 1001), Some(59.94));
        assert_eq!(refresh_hz(144, 1), Some(144.0));
        assert_eq!(refresh_hz(0, 0), None);
    }

    #[test]
    fn mode_picking() {
        let modes = [
            mode(3840, 2160, 60.0),
            mode(3840, 2160, 59.0),
            mode(3840, 2160, 30.0),
            mode(1920, 1080, 120.0),
        ];
        let pick = |w, h, hz: Option<f64>| {
            pick_mode(
                &modes,
                &DisplayMode {
                    width: w,
                    height: h,
                    refresh_hz: hz,
                },
            )
            .and_then(|m| m.refresh_hz)
        };
        assert_eq!(pick(3840, 2160, Some(59.94)), Some(59.0));
        assert_eq!(pick(3840, 2160, Some(60.0)), Some(60.0));
        assert_eq!(pick(3840, 2160, None), Some(60.0));
        assert_eq!(pick(1920, 1080, Some(119.88)), Some(120.0));
        assert_eq!(pick(1920, 1080, Some(60.0)), None);
        assert_eq!(pick(2560, 1440, None), None);
    }

    #[test]
    fn modes_sorted_and_deduped() {
        let mut m = vec![
            mode(1920, 1080, 60.0),
            mode(3840, 2160, 30.0),
            mode(1920, 1080, 60.0),
            mode(3840, 2160, 60.0),
        ];
        normalize_modes(&mut m);
        assert_eq!(
            m,
            [
                mode(3840, 2160, 60.0),
                mode(3840, 2160, 30.0),
                mode(1920, 1080, 60.0)
            ]
        );
    }
}
