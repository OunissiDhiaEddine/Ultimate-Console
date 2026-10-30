//! Platform layer. Only Windows has a real implementation; elsewhere every
//! call returns [`crate::Error::Unsupported`] so the workspace still builds
//! and the pure logic can be tested.

#[cfg(windows)]
mod ccd;
#[cfg(windows)]
mod identify;

#[cfg(windows)]
pub use ccd::{apply_single, list, restore, snapshot, wait_available};
#[cfg(windows)]
pub use identify::identify;

#[cfg(not(windows))]
mod unsupported;
#[cfg(not(windows))]
pub use unsupported::*;
