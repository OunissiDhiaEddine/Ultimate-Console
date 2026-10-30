use std::time::Duration;

use crate::{
    ApplyOutcome, DisplayId, DisplayInfo, DisplayMode, Error, RestoreOutcome, Result, Topology,
};

pub fn list() -> Result<Vec<DisplayInfo>> {
    Err(Error::Unsupported)
}

pub fn snapshot() -> Result<Topology> {
    Err(Error::Unsupported)
}

pub fn restore(_: &Topology) -> Result<RestoreOutcome> {
    Err(Error::Unsupported)
}

pub fn apply_single(_: &DisplayId, _: Option<&DisplayMode>, _: bool) -> Result<ApplyOutcome> {
    Err(Error::Unsupported)
}

pub fn wait_available(_: &DisplayId, _: Duration) -> Result<DisplayId> {
    Err(Error::Unsupported)
}

pub fn identify(_: Duration) -> Result<()> {
    Err(Error::Unsupported)
}
