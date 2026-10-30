//! Wake-on-LAN magic packets.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

/// 6 × `FF` followed by the MAC 16 times.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xFF; 102];
    for chunk in packet[6..].as_chunks_mut::<6>().0 {
        *chunk = mac;
    }
    packet
}

/// Sends the magic packet to `broadcast:port`, and also straight to the TV's
/// own IP if given. The unicast copy still gets through when the broadcast
/// goes out the wrong network adapter, as long as Windows remembers the
/// TV's MAC (its ARP cache keeps it for a while after the TV goes quiet).
pub fn send(mac: [u8; 6], broadcast: IpAddr, port: u16, tv_ip: Option<IpAddr>) -> io::Result<()> {
    let packet = magic_packet(mac);
    let mut targets = vec![SocketAddr::new(broadcast, port)];
    if let Some(ip) = tv_ip.filter(|ip| *ip != broadcast) {
        targets.push(SocketAddr::new(ip, port));
    }

    let mut first_err = None;
    let mut sent = 0;
    for target in targets {
        let result = socket_for(target).and_then(|s| s.send_to(&packet, target));
        match result {
            Ok(_) => sent += 1,
            Err(e) => {
                tracing::debug!(%target, error = %e, "WoL send failed");
                first_err.get_or_insert(e);
            }
        }
    }
    tracing::debug!(sent, "WoL magic packet sent");
    match (sent, first_err) {
        (0, Some(e)) => Err(e),
        _ => Ok(()),
    }
}

fn socket_for(target: SocketAddr) -> io::Result<UdpSocket> {
    let local: SocketAddr = match target {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(local)?;
    if target.is_ipv4() {
        socket.set_broadcast(true)?;
    }
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn packet_layout() {
        let mac = [0xAA, 0xBB, 0xCC, 0x01, 0x02, 0x03];
        let p = magic_packet(mac);
        assert_eq!(&p[..6], &[0xFF; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..12 + i * 6], &mac);
        }
    }

    #[test]
    fn sends_to_a_listener() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let port = rx.local_addr().unwrap().port();
        let mac = [1, 2, 3, 4, 5, 6];
        send(mac, IpAddr::from([127, 0, 0, 1]), port, None).unwrap();
        let mut buf = [0u8; 200];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], &magic_packet(mac));
    }
}
