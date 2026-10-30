//! Finds Android TVs on the local network with one-shot mDNS queries.
//!
//! `adbd` advertises `_adb._tcp` while network debugging is on. Many TVs
//! only advertise their remote/cast services, so those are asked for too:
//! the IP is the same and ADB then lives on the default port 5555.

use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

pub const SERVICE_ADB: &str = "_adb._tcp.local";
/// Services asked for, best first.
pub const SERVICES: [&str; 3] = [
    SERVICE_ADB,
    "_androidtvremote2._tcp.local",
    "_googlecast._tcp.local",
];

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;

/// A device that answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredTv {
    pub ip: IpAddr,
    /// Friendly name if the device gave one, else its mDNS instance name.
    pub name: String,
    /// Set when the TV advertises ADB itself (network debugging is on).
    pub adb_port: Option<u16>,
    /// Which of [`SERVICES`] it answered for.
    pub services: Vec<String>,
}

/// Queries for [`SERVICES`] for `duration` and returns everything that
/// answered, ADB-advertising devices first.
pub fn discover(duration: Duration) -> io::Result<Vec<DiscoveredTv>> {
    let socket = open_socket()?;
    let query = build_query(&SERVICES);
    let target = SocketAddr::from((MDNS_GROUP, MDNS_PORT));

    let start = Instant::now();
    let deadline = start + duration;
    // Ask three times: mDNS is UDP and the first packet is often lost on Wi-Fi.
    let mut sends = (0..3).map(|i| start + duration * i / 3).peekable();
    let mut found = BTreeMap::new();
    let mut buf = [0u8; 9000];

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        while sends.next_if(|t| *t <= now).is_some() {
            if let Err(e) = socket.send_to(&query, target) {
                tracing::debug!(error = %e, "mDNS query send failed");
            }
        }
        let wake = sends.peek().copied().unwrap_or(deadline).min(deadline);
        let wait = wake
            .saturating_duration_since(now)
            .max(Duration::from_millis(10));
        socket.set_read_timeout(Some(wait))?;
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(records) = parse_message(&buf[..n]) {
                    merge(&mut found, &records, from.ip());
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            // Windows reports ICMP "port unreachable" from earlier sends here.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => return Err(e),
        }
    }

    let mut list: Vec<DiscoveredTv> = found.into_values().collect();
    list.sort_by_key(|tv| (tv.adb_port.is_none(), tv.name.to_lowercase()));
    Ok(list)
}

/// Binds the mDNS port (shared with Windows' own responder) and joins the
/// group, so multicast answers reach us. If that is not possible, an
/// ephemeral port still gets unicast answers ("legacy unicast", RFC 6762 §6.7).
fn open_socket() -> io::Result<UdpSocket> {
    let shared = (|| {
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_reuse_address(true)?;
        s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, MDNS_PORT)).into())?;
        s.join_multicast_v4(&MDNS_GROUP, &Ipv4Addr::UNSPECIFIED)?;
        io::Result::Ok(UdpSocket::from(s))
    })();
    match shared {
        Ok(s) => Ok(s),
        Err(e) => {
            tracing::debug!(error = %e, "cannot share port 5353, using legacy unicast queries");
            UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        }
    }
}

pub(crate) fn build_query(services: &[&str]) -> Vec<u8> {
    let mut q = Vec::new();
    // id 0, flags 0 (standard query), qdcount, an/ns/ar = 0
    q.extend_from_slice(&[0, 0, 0, 0]);
    q.extend_from_slice(&(services.len() as u16).to_be_bytes());
    q.extend_from_slice(&[0; 6]);
    for service in services {
        for label in service.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&TYPE_PTR.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes()); // class IN
    }
    q
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RData {
    Ptr(String),
    Srv { port: u16, target: String },
    A(Ipv4Addr),
    Txt(Vec<String>),
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub name: String,
    pub data: RData,
}

/// Parses the answer, authority and additional sections of a response.
/// Returns `None` for queries and malformed packets.
pub(crate) fn parse_message(pkt: &[u8]) -> Option<Vec<Record>> {
    let u16_at = |i: usize| Some(u16::from_be_bytes(pkt.get(i..i + 2)?.try_into().ok()?));
    if u16_at(2)? & 0x8000 == 0 {
        return None; // a query, not a response
    }
    let qd = u16_at(4)?;
    let records = u16_at(6)? as usize + u16_at(8)? as usize + u16_at(10)? as usize;

    let mut pos = 12;
    for _ in 0..qd {
        read_name(pkt, &mut pos)?;
        pos += 4;
    }
    let mut out = Vec::with_capacity(records);
    for _ in 0..records {
        let name = read_name(pkt, &mut pos)?;
        let rtype = u16_at(pos)?;
        let rdlen = u16_at(pos + 8)? as usize;
        let start = pos + 10;
        let end = start.checked_add(rdlen).filter(|e| *e <= pkt.len())?;
        let data = match rtype {
            TYPE_PTR => RData::Ptr(read_name(pkt, &mut start.clone())?),
            TYPE_SRV => RData::Srv {
                port: u16_at(start + 4)?,
                target: read_name(pkt, &mut (start + 6))?,
            },
            TYPE_A if rdlen == 4 => {
                let o: [u8; 4] = pkt[start..end].try_into().ok()?;
                RData::A(Ipv4Addr::from(o))
            }
            TYPE_TXT => {
                let mut strings = Vec::new();
                let mut i = start;
                while i < end {
                    let len = pkt[i] as usize;
                    let s = pkt.get(i + 1..i + 1 + len)?;
                    strings.push(String::from_utf8_lossy(s).into_owned());
                    i += 1 + len;
                }
                RData::Txt(strings)
            }
            _ => RData::Other,
        };
        out.push(Record { name, data });
        pos = end;
    }
    Some(out)
}

/// Reads a (possibly compressed) domain name at `*pos` and advances `*pos`
/// past it.
fn read_name(pkt: &[u8], pos: &mut usize) -> Option<String> {
    let mut labels = Vec::new();
    let mut i = *pos;
    let mut jumped = false;
    for _ in 0..128 {
        let len = *pkt.get(i)? as usize;
        match len {
            0 => {
                if !jumped {
                    *pos = i + 1;
                }
                return Some(labels.join("."));
            }
            l if l & 0xC0 == 0xC0 => {
                let target = (l & 0x3F) << 8 | *pkt.get(i + 1)? as usize;
                if !jumped {
                    *pos = i + 2;
                }
                jumped = true;
                i = target;
            }
            l if l < 64 => {
                let label = pkt.get(i + 1..i + 1 + l)?;
                labels.push(String::from_utf8_lossy(label).into_owned());
                i += 1 + l;
            }
            _ => return None,
        }
    }
    None // pointer loop
}

/// Folds one response into the results, keyed by IP.
fn merge(found: &mut BTreeMap<IpAddr, DiscoveredTv>, records: &[Record], from: IpAddr) {
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    for rec in records {
        let RData::Ptr(instance) = &rec.data else {
            continue;
        };
        let Some(service) = SERVICES.iter().find(|s| eq(&rec.name, s)) else {
            continue;
        };
        let srv = records.iter().find_map(|r| match &r.data {
            RData::Srv { port, target } if eq(&r.name, instance) => Some((*port, target)),
            _ => None,
        });
        let ip = srv
            .and_then(|(_, host)| {
                records.iter().find_map(|r| match r.data {
                    RData::A(a) if eq(&r.name, host) => Some(IpAddr::V4(a)),
                    _ => None,
                })
            })
            .unwrap_or(from);
        let friendly = records.iter().find_map(|r| match &r.data {
            RData::Txt(t) if eq(&r.name, instance) => t
                .iter()
                .find_map(|kv| kv.strip_prefix("fn="))
                .map(str::to_string),
            _ => None,
        });
        let instance_name = instance
            .strip_suffix(&format!(".{service}"))
            .unwrap_or(instance)
            .to_string();

        let tv = found.entry(ip).or_insert_with(|| DiscoveredTv {
            ip,
            name: instance_name.clone(),
            adb_port: None,
            services: Vec::new(),
        });
        if *service == SERVICE_ADB {
            tv.adb_port = Some(srv.map_or(5555, |(port, _)| port));
        } else if tv.name.starts_with("adb-") {
            // "adb-<serial>" is the least helpful name; prefer any other.
            tv.name = instance_name;
        }
        if let Some(f) = friendly {
            tv.name = f;
        }
        if !tv.services.iter().any(|s| s == service) {
            tv.services.push(service.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(out: &mut Vec<u8>, n: &str) {
        for label in n.split('.') {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
    }

    fn record(out: &mut Vec<u8>, owner: &[u8], rtype: u16, rdata: &[u8]) {
        out.extend_from_slice(owner);
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&0x8001u16.to_be_bytes()); // cache-flush + IN
        out.extend_from_slice(&120u32.to_be_bytes());
        out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        out.extend_from_slice(rdata);
    }

    /// A response like adbd's: PTR in answers, SRV/TXT/A in additionals,
    /// using a compression pointer for the service name.
    fn adb_response() -> Vec<u8> {
        let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 3];
        let service_at = p.len() as u16;
        let mut owner = Vec::new();
        name(&mut owner, SERVICE_ADB);
        // PTR _adb._tcp.local -> adb-ABC123._adb._tcp.local (compressed)
        let mut rdata = vec![10];
        rdata.extend_from_slice(b"adb-ABC123");
        rdata.extend_from_slice(&(0xC000 | service_at).to_be_bytes());
        record(&mut p, &owner, TYPE_PTR, &rdata);

        let mut inst = Vec::new();
        name(&mut inst, "adb-ABC123._adb._tcp.local");
        let mut srv = vec![0, 0, 0, 0, 0x15, 0xB3]; // prio, weight, port 5555
        name(&mut srv, "Android.local");
        record(&mut p, &inst, TYPE_SRV, &srv);
        record(&mut p, &inst, TYPE_TXT, b"\x0afn=IRIS TV");
        let mut host = Vec::new();
        name(&mut host, "Android.local");
        record(&mut p, &host, TYPE_A, &[192, 168, 1, 50]);
        p
    }

    #[test]
    fn query_layout() {
        let q = build_query(&["_adb._tcp.local"]);
        assert_eq!(&q[..12], &[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&q[12..17], b"\x04_adb");
        assert_eq!(&q[q.len() - 4..], &[0, 12, 0, 1]);
        // Our own query is not mistaken for an answer.
        assert_eq!(parse_message(&q), None);
    }

    #[test]
    fn parses_compressed_response() {
        let recs = parse_message(&adb_response()).unwrap();
        assert_eq!(recs.len(), 4);
        assert_eq!(
            recs[0].data,
            RData::Ptr("adb-ABC123._adb._tcp.local".into())
        );
        assert_eq!(
            recs[1].data,
            RData::Srv {
                port: 5555,
                target: "Android.local".into()
            }
        );
        assert_eq!(recs[3].data, RData::A(Ipv4Addr::new(192, 168, 1, 50)));
    }

    #[test]
    fn merges_into_devices() {
        let mut found = BTreeMap::new();
        let recs = parse_message(&adb_response()).unwrap();
        // Source IP differs from the A record: the A record wins.
        merge(&mut found, &recs, IpAddr::from([10, 0, 0, 9]));
        let tv = &found[&IpAddr::from([192, 168, 1, 50])];
        assert_eq!(tv.name, "IRIS TV");
        assert_eq!(tv.adb_port, Some(5555));
        assert_eq!(tv.services, [SERVICE_ADB]);
    }

    #[test]
    fn ptr_only_uses_source_ip() {
        let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        let mut owner = Vec::new();
        name(&mut owner, "_androidtvremote2._tcp.local");
        let mut rdata = Vec::new();
        name(&mut rdata, "Living Room._androidtvremote2._tcp.local");
        record(&mut p, &owner, TYPE_PTR, &rdata);

        let mut found = BTreeMap::new();
        let from = IpAddr::from([192, 168, 1, 77]);
        merge(&mut found, &parse_message(&p).unwrap(), from);
        let tv = &found[&from];
        assert_eq!(tv.name, "Living Room");
        assert_eq!(tv.adb_port, None);
    }

    #[test]
    fn malformed_packets_do_not_panic() {
        let good = adb_response();
        for cut in 0..good.len() {
            let _ = parse_message(&good[..cut]);
        }
        // Pointer loop.
        let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        p.extend_from_slice(&[0xC0, 12]);
        assert_eq!(parse_message(&p), None);
    }
}
