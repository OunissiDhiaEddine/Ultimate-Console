//! Non-Windows stand-ins so the workspace type-checks anywhere.

use std::io;
use std::net::Ipv4Addr;
use std::time::Duration;

use crate::{AdapterInfo, NetworkWait, PowerEvent};

fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "uc-power needs Windows")
}

/// See the Windows version.
#[derive(Debug)]
pub struct PowerWatcher(());

impl PowerWatcher {
    pub fn start<F>(_callback: F) -> io::Result<Self>
    where
        F: FnMut(PowerEvent) + Send + 'static,
    {
        Err(unsupported())
    }
}

pub fn network_snapshot() -> io::Result<Vec<AdapterInfo>> {
    Err(unsupported())
}

pub fn wait_for_network(_target: Option<Ipv4Addr>, _timeout: Duration) -> io::Result<NetworkWait> {
    Err(unsupported())
}
