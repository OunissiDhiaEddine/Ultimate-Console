//! "Is the network usable yet?" Pure logic; the adapter list comes from
//! `network_snapshot` on Windows.

use std::fmt;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// An IPv4 address with its on-link prefix length, e.g. `192.168.1.20/24`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Net {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl Ipv4Net {
    pub fn new(addr: Ipv4Addr, prefix: u8) -> Self {
        Self {
            addr,
            prefix: prefix.min(32),
        }
    }

    /// True if `ip` is on the same subnet.
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        let mask = if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(self.prefix))
        };
        u32::from(self.addr) & mask == u32::from(ip) & mask
    }

    /// Not loopback, link-local (169.254/16, i.e. no DHCP lease yet) or 0.0.0.0.
    pub fn is_usable(&self) -> bool {
        !(self.addr.is_loopback() || self.addr.is_link_local() || self.addr.is_unspecified())
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// One network adapter as far as readiness is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    /// Friendly name, e.g. "Ethernet".
    pub name: String,
    /// Operational status is "up" (link present).
    pub up: bool,
    /// IPv4 addresses that finished duplicate-address detection.
    pub ipv4: Vec<Ipv4Net>,
    /// The adapter has a default gateway.
    pub has_gateway: bool,
}

impl AdapterInfo {
    fn usable_addrs(&self) -> impl Iterator<Item = &Ipv4Net> {
        self.ipv4.iter().filter(|n| self.up && n.is_usable())
    }
}

impl fmt::Display for AdapterInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [{}]", self.name, if self.up { "up" } else { "down" })?;
        for n in &self.ipv4 {
            write!(f, " {n}")?;
        }
        if self.has_gateway {
            f.write_str(" gw")?;
        }
        Ok(())
    }
}

/// Decides whether the network is usable.
///
/// - With a `target` (the TV's IP): some up adapter has a usable address on
///   the target's subnet, so packets to the TV can go out directly.
/// - Without: some up adapter has a usable address and a default gateway.
pub fn network_ready(adapters: &[AdapterInfo], target: Option<Ipv4Addr>) -> bool {
    adapters.iter().any(|a| match target {
        Some(ip) => a.usable_addrs().any(|n| n.contains(ip)),
        None => a.has_gateway && a.usable_addrs().next().is_some(),
    })
}

/// Result of a wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkWait {
    pub ready: bool,
    /// Time until ready, or until giving up.
    pub elapsed: Duration,
}

/// Calls `check` every `poll` until it returns `true` or `timeout` passes.
/// `check` runs at least once, right away.
pub fn wait_until(
    timeout: Duration,
    poll: Duration,
    mut check: impl FnMut() -> bool,
) -> NetworkWait {
    let start = Instant::now();
    loop {
        if check() {
            return NetworkWait {
                ready: true,
                elapsed: start.elapsed(),
            };
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return NetworkWait {
                ready: false,
                elapsed,
            };
        }
        std::thread::sleep(poll.min(timeout - elapsed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str, prefix: u8) -> Ipv4Net {
        Ipv4Net::new(s.parse().unwrap(), prefix)
    }

    fn adapter(up: bool, ipv4: Vec<Ipv4Net>, has_gateway: bool) -> AdapterInfo {
        AdapterInfo {
            name: "Ethernet".into(),
            up,
            ipv4,
            has_gateway,
        }
    }

    #[test]
    fn subnet_contains() {
        let n = net("192.168.1.20", 24);
        assert!(n.contains("192.168.1.50".parse().unwrap()));
        assert!(!n.contains("192.168.2.50".parse().unwrap()));
        assert!(net("10.0.0.1", 0).contains("8.8.8.8".parse().unwrap()));
        assert!(net("10.0.0.1", 32).contains("10.0.0.1".parse().unwrap()));
        assert!(!net("10.0.0.1", 32).contains("10.0.0.2".parse().unwrap()));
    }

    #[test]
    fn usable_addresses() {
        assert!(net("192.168.1.20", 24).is_usable());
        assert!(!net("169.254.3.4", 16).is_usable());
        assert!(!net("127.0.0.1", 8).is_usable());
        assert!(!net("0.0.0.0", 0).is_usable());
    }

    #[test]
    fn ready_without_target_needs_gateway() {
        let tv = None;
        assert!(!network_ready(&[], tv));
        assert!(!network_ready(
            &[adapter(true, vec![net("192.168.1.20", 24)], false)],
            tv
        ));
        assert!(network_ready(
            &[adapter(true, vec![net("192.168.1.20", 24)], true)],
            tv
        ));
        // Link-local only: DHCP hasn't answered yet.
        assert!(!network_ready(
            &[adapter(true, vec![net("169.254.9.9", 16)], true)],
            tv
        ));
        // Down adapter with a stale address.
        assert!(!network_ready(
            &[adapter(false, vec![net("192.168.1.20", 24)], true)],
            tv
        ));
    }

    #[test]
    fn ready_with_target_needs_same_subnet() {
        let tv = Some("192.168.1.50".parse().unwrap());
        let lan = adapter(true, vec![net("192.168.1.20", 24)], false);
        let vpn = adapter(true, vec![net("10.8.0.2", 24)], true);
        assert!(network_ready(std::slice::from_ref(&lan), tv));
        assert!(!network_ready(std::slice::from_ref(&vpn), tv));
        assert!(network_ready(&[vpn, lan], tv));
    }

    #[test]
    fn wait_until_ready_and_timeout() {
        let mut n = 0;
        let w = wait_until(Duration::from_secs(5), Duration::from_millis(1), || {
            n += 1;
            n >= 3
        });
        assert!(w.ready);
        assert_eq!(n, 3);

        let w = wait_until(Duration::from_millis(20), Duration::from_millis(5), || {
            false
        });
        assert!(!w.ready);
        assert!(w.elapsed >= Duration::from_millis(20));

        // Zero timeout still checks once.
        let w = wait_until(Duration::ZERO, Duration::from_millis(5), || true);
        assert!(w.ready);
    }
}
