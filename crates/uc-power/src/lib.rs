//! Suspend/resume and display-state notifications, network-ready waiter (WP-05).
//!
//! This crate only reports what happens. It applies no policy: the rule that
//! sleep/resume touches the TV only in service-triggered gaming mode (R8) lives
//! in the agent (plan §2.7).
//!
//! Two ways to receive [`PowerEvent`]s on Windows:
//!
//! - [`PowerWatcher::start`] creates its own hidden window on its own thread
//!   and calls a callback. Used by `uc-probe power watch`, and fine for the
//!   agent too.
//! - [`Registration::register`] + [`decode_message`] for a window the caller
//!   already owns (the agent's message window, plan §3).
//!
//! **Suspend budget.** Windows delivers [`PowerEvent::Suspend`] before it
//! suspends and waits for the handler to return, but only for about two
//! seconds. The handler must stay inside the plan's ~1.5 s budget: send the TV
//! sleep on an already-open socket and return. On S3 machines the NIC is gone
//! right after that, and on resume it needs a few seconds before the network is
//! usable, hence [`wait_for_network`].
//!
//! The pure logic (decoding, duplicate filtering, "is the network ready")
//! builds and is unit-tested on every platform. The Win32 parts are only
//! available on Windows; elsewhere they return [`std::io::ErrorKind::Unsupported`].

mod event;
mod network;
#[cfg(windows)]
mod win;

pub use event::{
    DisplayState, EventFilter, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
    PBT_POWERSETTINGCHANGE, PowerEvent, WM_POWERBROADCAST, decode_pbt,
};
pub use network::{AdapterInfo, Ipv4Net, NetworkWait, network_ready, wait_until};

#[cfg(windows)]
pub use win::{PowerWatcher, Registration, decode_message, network_snapshot, wait_for_network};

#[cfg(not(windows))]
mod unsupported;
#[cfg(not(windows))]
pub use unsupported::{PowerWatcher, network_snapshot, wait_for_network};
