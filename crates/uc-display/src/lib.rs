//! CCD display snapshot/restore/apply and stable display identity (WP-01).
//!
//! Plan §2.4. The orchestrator uses it like this:
//!
//! 1. [`snapshot`] the current layout and store [`Topology::to_value`] in
//!    `state.json`.
//! 2. Wake the TV, then [`wait_available`] until Windows sees it.
//! 3. [`apply_single`]: only the TV active, primary, optional mode and HDR.
//!    Call it again after resume to re-assert the layout.
//! 4. On exit (or crash recovery) [`restore`] the saved [`Topology`].
//!
//! Displays are identified by [`DisplayId`] (monitor device path, then
//! friendly name + connector as a fallback), never by GPU target ids, which
//! change across driver updates.
//!
//! Every call blocks and must not run on a thread that pumps the agent's
//! hotkey or overlay messages. All of it works without admin rights.

mod error;
pub mod identity;
mod sys;
pub mod topology;

use std::time::Duration;

pub use error::{Error, Result};
pub use topology::Topology;
pub use uc_core::config::{DisplayId, DisplayMode};
pub use uc_core::ipc::DisplayInfo;

/// What [`apply_single`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplyOutcome {
    /// The display that was matched (its device path may differ from the
    /// configured one when the fallback match was used).
    pub display: DisplayId,
    /// Non-fatal problems, e.g. the asked mode is not offered or HDR is not
    /// supported. The TV is active and primary regardless.
    pub warnings: Vec<String>,
}

/// How [`restore`] got the layout back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The saved topology was applied as is.
    Exact,
    /// The exact restore failed; the saved displays were turned back on
    /// with modes picked by Windows, then given their saved resolution.
    Database,
    /// Both failed; every connected display was extended.
    Extend,
}

/// Every connected display, active ones first (left to right), then the
/// inactive ones. [`DisplayInfo::index`] is the number [`identify`] shows.
pub fn list() -> Result<Vec<DisplayInfo>> {
    sys::list()
}

/// The current layout (active paths and modes, plus HDR states).
pub fn snapshot() -> Result<Topology> {
    sys::snapshot()
}

/// Puts a saved layout back, falling back to turning the saved displays on
/// with Windows' modes and then to "extend" if the exact restore is refused
/// (plan §2.4). Each pass is verified and repeated if Windows switches the
/// layout back, and the result is saved to Windows' display database.
pub fn restore(topology: &Topology) -> Result<RestoreOutcome> {
    sys::restore(topology)
}

/// Makes `target` the only active display (and so the primary one), then
/// applies `mode` and turns HDR on if `hdr` is set (`false` leaves HDR as it
/// is). The layout is not saved to Windows' display database, so a reboot
/// without a restore still comes back to the desktop layout.
pub fn apply_single(
    target: &DisplayId,
    mode: Option<&DisplayMode>,
    hdr: bool,
) -> Result<ApplyOutcome> {
    sys::apply_single(target, mode, hdr)
}

/// Polls until `target` is connected and available (an HDMI TV that is
/// still off often is not). Returns the matched display.
pub fn wait_available(target: &DisplayId, timeout: Duration) -> Result<DisplayId> {
    sys::wait_available(target, timeout)
}

/// Shows each active display's [`DisplayInfo::index`] on that display for
/// `duration`. Blocks for that long and runs its own message loop.
pub fn identify(duration: Duration) -> Result<()> {
    sys::identify(duration)
}
