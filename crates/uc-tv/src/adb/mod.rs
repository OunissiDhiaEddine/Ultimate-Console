//! Native ADB-over-TCP: wire format, RSA identity, client.

pub mod client;
pub mod key;
pub mod message;

pub use client::{AdbConnection, ConnectOptions};
pub use key::AdbKey;
