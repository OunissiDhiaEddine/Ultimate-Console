//! Android TV control: ADB-over-TCP client, Wake-on-LAN, discovery, pairing (WP-02).
//!
//! - [`TvController`]: `wake()`, `sleep()`, `sleep_within()`, `state()`,
//!   `run()`, `pair()`, `keepalive()`. Timeouts and retries come from
//!   `config.tv` (plan §2.6).
//! - [`adb`]: the native ADB client (no `adb.exe`, no ADB server process).
//! - [`wol`]: Wake-on-LAN magic packets.
//! - [`arp`]: the TV's MAC from the neighbour table, captured at pairing.
//! - [`mdns`]: finding TVs on the network.
//!
//! Load the PC's key once with [`AdbKey::load_or_create`] on
//! `Paths::adb_key_file()` and share it.

pub mod adb;
pub mod arp;
pub mod controller;
pub mod error;
pub mod mdns;
pub mod wol;

pub use adb::AdbKey;
pub use controller::{PairInfo, TvController, TvState, WakeReport, pick_ip};
pub use error::{Result, TvError};
pub use mdns::DiscoveredTv;
