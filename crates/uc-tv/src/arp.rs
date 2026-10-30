//! MAC address of a host on the local network, from the OS neighbour (ARP)
//! table. Used once at pairing time so Wake-on-LAN works later.

use std::net::IpAddr;

use crate::{Result, TvError};

/// Formats a MAC as `AA:BB:CC:DD:EE:FF` (the form `config.tv.mac` uses).
pub fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Looks up the MAC of `ip`. On Windows this asks the neighbour table and,
/// if the entry is missing, sends an ARP request (`ResolveIpNetEntry2`).
/// Works best right after talking to the TV, which fills the table.
pub fn mac_for_ip(ip: IpAddr) -> Result<[u8; 6]> {
    let fail = |reason: String| TvError::MacLookup { ip, reason };
    let IpAddr::V4(v4) = ip else {
        return Err(fail("only IPv4 addresses are supported".into()));
    };
    platform::lookup(v4).map_err(fail)
}

#[cfg(windows)]
mod platform {
    use std::net::Ipv4Addr;

    use windows::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIpNetTable2, MIB_IPNET_ROW2, MIB_IPNET_TABLE2, ResolveIpNetEntry2,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_INET};

    pub fn lookup(ip: Ipv4Addr) -> Result<[u8; 6], String> {
        if let Some(mac) = from_table(ip)? {
            return Ok(mac);
        }
        resolve(ip)
    }

    fn row_mac(row: &MIB_IPNET_ROW2) -> Option<[u8; 6]> {
        let mac: [u8; 6] = row.PhysicalAddress[..6].try_into().ok()?;
        (row.PhysicalAddressLength == 6 && mac != [0; 6]).then_some(mac)
    }

    fn from_table(ip: Ipv4Addr) -> Result<Option<[u8; 6]>, String> {
        let want = u32::from_ne_bytes(ip.octets());
        let mut table: *mut MIB_IPNET_TABLE2 = std::ptr::null_mut();
        // SAFETY: GetIpNetTable2 allocates the table and stores its pointer
        // in `table`; we free it with FreeMibTable below.
        let status = unsafe { GetIpNetTable2(AF_INET, &mut table) };
        if status.is_err() {
            return Err(format!("GetIpNetTable2 failed: {status:?}"));
        }
        // SAFETY: on success `table` points to a MIB_IPNET_TABLE2 whose
        // `Table` array holds `NumEntries` rows; we only read it before freeing.
        let found = unsafe {
            let t = &*table;
            let rows = std::slice::from_raw_parts(t.Table.as_ptr(), t.NumEntries as usize);
            let found = rows
                .iter()
                .filter(|r| {
                    r.Address.si_family == AF_INET && r.Address.Ipv4.sin_addr.S_un.S_addr == want
                })
                .find_map(row_mac);
            FreeMibTable(table as *const _);
            found
        };
        Ok(found)
    }

    fn resolve(ip: Ipv4Addr) -> Result<[u8; 6], String> {
        let mut row = MIB_IPNET_ROW2::default();
        let mut addr = SOCKADDR_INET::default();
        addr.Ipv4.sin_family = AF_INET;
        addr.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes(ip.octets());
        row.Address = addr;
        // SAFETY: `row` is a valid, initialised MIB_IPNET_ROW2 with the
        // address set; the call fills in the rest.
        let status = unsafe { ResolveIpNetEntry2(&mut row, None) };
        if status.is_err() {
            return Err(format!(
                "no ARP reply ({status:?}); is the TV on and on the same network?"
            ));
        }
        row_mac(&row).ok_or_else(|| "the TV did not answer ARP".to_string())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::net::Ipv4Addr;

    pub fn lookup(ip: Ipv4Addr) -> Result<[u8; 6], String> {
        let table = std::fs::read_to_string("/proc/net/arp").map_err(|e| e.to_string())?;
        super::parse_proc_net_arp(&table, ip).ok_or_else(|| "not in the ARP table".into())
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use std::net::Ipv4Addr;

    pub fn lookup(_ip: Ipv4Addr) -> Result<[u8; 6], String> {
        Err("not supported on this platform".into())
    }
}

/// Parses Linux `/proc/net/arp` (handy for development on Linux).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_net_arp(table: &str, ip: std::net::Ipv4Addr) -> Option<[u8; 6]> {
    table.lines().skip(1).find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let (addr, flags, mac) = (cols.first()?, cols.get(2)?, cols.get(3)?);
        // Flags 0x0 = incomplete entry.
        if addr.parse().ok() != Some(ip) || *flags == "0x0" {
            return None;
        }
        uc_core::config::parse_mac(mac).filter(|m| *m != [0; 6])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        let mac = [0xAA, 0x0B, 0xCC, 0x01, 0x02, 0xFF];
        let s = format_mac(mac);
        assert_eq!(s, "AA:0B:CC:01:02:FF");
        assert_eq!(uc_core::config::parse_mac(&s), Some(mac));
    }

    #[test]
    fn proc_net_arp() {
        let table = "\
IP address       HW type     Flags       HW address            Mask     Device
192.168.1.1      0x1         0x2         11:22:33:44:55:66     *        eth0
192.168.1.50     0x1         0x2         aa:bb:cc:dd:ee:ff     *        eth0
192.168.1.60     0x1         0x0         00:00:00:00:00:00     *        eth0
";
        let ip = |s: &str| s.parse().unwrap();
        assert_eq!(
            parse_proc_net_arp(table, ip("192.168.1.50")),
            Some([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])
        );
        assert_eq!(parse_proc_net_arp(table, ip("192.168.1.60")), None);
        assert_eq!(parse_proc_net_arp(table, ip("192.168.1.99")), None);
    }

    #[test]
    fn ipv6_is_refused() {
        let err = mac_for_ip("::1".parse().unwrap()).unwrap_err();
        assert!(matches!(err, TvError::MacLookup { .. }));
    }
}
