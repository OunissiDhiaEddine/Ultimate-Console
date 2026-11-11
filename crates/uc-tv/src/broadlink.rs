//! Broadlink RM4 (RM4 Pro, RM4 mini) IR blaster over its local UDP protocol:
//! hello, auth, send a learned IR packet. No cloud, no vendor app, no Python.
//!
//! Protocol (as in `python-broadlink`): every frame is a 0x38-byte header
//! followed by an AES-128-CBC payload (fixed IV, zero padding). The device
//! starts with a well-known key; `auth` (0x65) returns a session id and key.
//! RM4 commands (0x6A) carry `u16 len, u32 command, data`; command 2 sends
//! IR/RF data in Broadlink's own pulse format (what learning gives you).

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};

use crate::{Result, TvError};

pub const PORT: u16 = 80;
const INITIAL_KEY: [u8; 16] = [
    0x09, 0x76, 0x28, 0x34, 0x3f, 0xe9, 0x9e, 0x23, 0x76, 0x5c, 0x15, 0x13, 0xac, 0xcf, 0x8b, 0x02,
];
const IV: [u8; 16] = [
    0x56, 0x2e, 0x17, 0x99, 0x6d, 0x09, 0x3d, 0x28, 0xdd, 0xb3, 0xba, 0x69, 0x5a, 0x2e, 0x6f, 0x58,
];
const MAGIC: [u8; 8] = [0x5a, 0xa5, 0xaa, 0x55, 0x5a, 0xa5, 0xaa, 0x55];
const HEADER: usize = 0x38;
const TYPE_AUTH: u16 = 0x65;
const TYPE_COMMAND: u16 = 0x6a;
const CMD_SEND_DATA: u32 = 0x02;
/// Tries per request: UDP may drop a frame, and a resend is harmless for
/// hello/auth. For send_data a lost *reply* would mean the IR went out twice,
/// so that one is never retried (a second toggle would undo the first).
const TRIES: u32 = 3;

fn checksum(bytes: &[u8]) -> u16 {
    bytes
        .iter()
        .fold(0xBEAFu32, |sum, &b| sum.wrapping_add(b as u32)) as u16
}

fn cipher(key: &[u8; 16]) -> Aes128 {
    Aes128::new(GenericArray::from_slice(key))
}

/// AES-128-CBC with the fixed IV over `data`, zero-padded to 16 bytes.
fn encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let aes = cipher(key);
    let mut out = data.to_vec();
    out.resize(data.len().div_ceil(16) * 16, 0);
    let mut prev = IV;
    for block in out.as_chunks_mut::<16>().0 {
        for (b, p) in block.iter_mut().zip(prev) {
            *b ^= p;
        }
        aes.encrypt_block(GenericArray::from_mut_slice(block));
        prev = *block;
    }
    out
}

/// Decrypts whole blocks; a trailing partial block is dropped.
fn decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let aes = cipher(key);
    let mut out = data[..data.len() / 16 * 16].to_vec();
    let mut prev = IV;
    for block in out.as_chunks_mut::<16>().0 {
        let cipher_block = *block;
        aes.decrypt_block(GenericArray::from_mut_slice(block));
        for (b, p) in block.iter_mut().zip(prev) {
            *b ^= p;
        }
        prev = cipher_block;
    }
    out
}

/// An authenticated session with one RM4.
pub struct Broadlink {
    addr: SocketAddr,
    timeout: Duration,
    devtype: u16,
    /// As the device sends it (reversed); goes into the header as-is.
    mac: [u8; 6],
    id: u32,
    key: [u8; 16],
    count: u16,
}

impl Broadlink {
    /// Says hello to the device at `addr` and authenticates.
    pub fn connect(addr: SocketAddr, timeout: Duration) -> Result<Self> {
        let mut device = Self {
            addr,
            timeout: timeout.max(Duration::from_millis(50)),
            devtype: 0,
            mac: [0; 6],
            id: 0,
            key: INITIAL_KEY,
            count: 0x8000 | (seed() as u16 & 0x7fff),
        };
        device.hello()?;
        device.auth()?;
        tracing::debug!(%addr, devtype = format_args!("{:#06x}", device.devtype), "Broadlink authenticated");
        Ok(device)
    }

    /// Sends a learned IR/RF packet once.
    pub fn send_data(&mut self, data: &[u8]) -> Result<()> {
        let mut payload = Vec::with_capacity(6 + data.len());
        payload.extend_from_slice(&((data.len() + 4) as u16).to_le_bytes());
        payload.extend_from_slice(&CMD_SEND_DATA.to_le_bytes());
        payload.extend_from_slice(data);
        self.request("sending the IR packet", TYPE_COMMAND, &payload, 1)?;
        Ok(())
    }

    fn hello(&mut self) -> Result<()> {
        // Local IP and port left at zero, like python-broadlink's `hello`:
        // the device answers the sender's address.
        let mut packet = [0u8; 0x30];
        packet[0x26] = 6;
        let sum = checksum(&packet);
        packet[0x20..0x22].copy_from_slice(&sum.to_le_bytes());
        // python-broadlink doesn't check the hello reply's checksum either.
        let resp = self.exchange(&packet, TRIES, false, |r| r.len() >= 0x40)?;
        self.devtype = u16::from_le_bytes([resp[0x34], resp[0x35]]);
        self.mac.copy_from_slice(&resp[0x3A..0x40]);
        Ok(())
    }

    fn auth(&mut self) -> Result<()> {
        let mut payload = [0u8; 0x50];
        payload[0x04..0x14].fill(0x31);
        payload[0x1E] = 0x01;
        payload[0x2D] = 0x01;
        payload[0x30..0x36].copy_from_slice(b"Test 1");
        let reply = self.request("login", TYPE_AUTH, &payload, TRIES)?;
        if reply.len() < 0x14 {
            return Err(TvError::Broadlink("short auth reply".into()));
        }
        self.id = u32::from_le_bytes(reply[..4].try_into().expect("4 bytes"));
        self.key.copy_from_slice(&reply[0x04..0x14]);
        Ok(())
    }

    /// Sends one encrypted frame and returns the decrypted reply payload.
    fn request(&mut self, what: &str, kind: u16, payload: &[u8], tries: u32) -> Result<Vec<u8>> {
        self.count = (self.count.wrapping_add(1)) | 0x8000;
        let frame = self.frame(kind, payload);
        let resp = self.exchange(&frame, tries, true, |r| r.len() >= HEADER)?;
        let code = i16::from_le_bytes([resp[0x22], resp[0x23]]);
        if code != 0 {
            let reason: String = match code {
                -1 => "the device refused the session (error -1, authentication failed)".into(),
                -7 => "the device's session key expired (error -7)".into(),
                -4 => "the device does not support this command (error -4)".into(),
                _ => format!("the device answered with error {code}"),
            };
            return Err(TvError::Broadlink(format!("{what}: {reason}")));
        }
        Ok(decrypt(&self.key, &resp[HEADER..]))
    }

    fn frame(&self, kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0u8; HEADER];
        frame[..8].copy_from_slice(&MAGIC);
        frame[0x24..0x26].copy_from_slice(&self.devtype.to_le_bytes());
        frame[0x26..0x28].copy_from_slice(&kind.to_le_bytes());
        frame[0x28..0x2A].copy_from_slice(&self.count.to_le_bytes());
        frame[0x2A..0x30].copy_from_slice(&self.mac);
        frame[0x30..0x34].copy_from_slice(&self.id.to_le_bytes());
        frame[0x34..0x36].copy_from_slice(&checksum(payload).to_le_bytes());
        frame.extend(encrypt(&self.key, payload));
        let sum = checksum(&frame);
        frame[0x20..0x22].copy_from_slice(&sum.to_le_bytes());
        frame
    }

    /// Sends `frame` and waits for a reply from the device that passes `ok`
    /// (and its checksum, if `check`), resending up to `tries` times on timeout.
    ///
    /// Each exchange uses its own socket, like python-broadlink: an RM4 can
    /// answer a hello twice, and on a shared socket the late duplicate was
    /// taken for the auth reply, so the session id and key were garbage and
    /// the device refused the IR packet with -1 ("authentication failed").
    fn exchange(
        &self,
        frame: &[u8],
        tries: u32,
        check: bool,
        ok: impl Fn(&[u8]) -> bool,
    ) -> Result<Vec<u8>> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_read_timeout(Some(self.timeout))?;
        let mut buf = [0u8; 2048];
        for attempt in 1..=tries.max(1) {
            socket.send_to(frame, self.addr)?;
            loop {
                match socket.recv_from(&mut buf) {
                    Ok((n, from)) if from.ip() == self.addr.ip() => {
                        let resp = &buf[..n];
                        if ok(resp) && (!check || valid_checksum(resp)) {
                            return Ok(resp.to_vec());
                        }
                        tracing::debug!(n, "ignoring malformed Broadlink reply");
                    }
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                    {
                        tracing::debug!(attempt, addr = %self.addr, "no Broadlink reply");
                        break;
                    }
                    // Windows reports an ICMP "port unreachable" from an
                    // earlier send as a reset on the next receive.
                    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Err(TvError::Broadlink(format!(
            "no answer from the Broadlink at {} (check its IP address)",
            self.addr.ip()
        )))
    }
}

fn valid_checksum(resp: &[u8]) -> bool {
    if resp.len() < 0x22 {
        return false;
    }
    let mut copy = resp.to_vec();
    let want = u16::from_le_bytes([copy[0x20], copy[0x21]]);
    copy[0x20] = 0;
    copy[0x21] = 0;
    checksum(&copy) == want
}

/// A cheap varying seed for the frame counter (no `rand` dependency).
fn seed() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
        .unwrap_or(0)
}

/// Connects to the RM4 at `addr` (port [`PORT`]) and sends `packet` once.
pub fn send(addr: SocketAddr, packet: &[u8], timeout: Duration) -> Result<()> {
    let mut device = Broadlink::connect(addr, timeout)?;
    device.send_data(packet)?;
    tracing::info!(%addr, bytes = packet.len(), "IR packet sent through Broadlink");
    Ok(())
}

/// Sends `tv.broadlink_packet` to the RM4 at `tv.broadlink_ip:port` once,
/// with `tv.connect_timeout_ms` per try.
pub fn send_configured(tv: &uc_core::config::Tv, port: u16) -> Result<()> {
    let packet = uc_core::config::parse_hex(&tv.broadlink_packet).ok_or(TvError::BadIrPacket)?;
    let timeout = Duration::from_millis(tv.connect_timeout_ms.max(500) as u64);
    send(SocketAddr::new(tv.broadlink_ip, port), &packet, timeout)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use uc_core::config::{DEFAULT_IR_POWER_PACKET, parse_hex};

    #[test]
    fn aes_cbc_round_trip_and_known_vector() {
        // NIST SP 800-38A F.2.1 (CBC-AES128), first block, with our IV swapped
        // in by pre-xoring: check the block cipher itself.
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let mut block = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        cipher(&key).encrypt_block(GenericArray::from_mut_slice(&mut block));
        assert_eq!(
            block,
            [
                0x3a, 0xd7, 0x7b, 0xb4, 0x0d, 0x7a, 0x36, 0x60, 0xa8, 0x9e, 0xca, 0xf3, 0x24, 0x66,
                0xef, 0x97
            ]
        );

        let data: Vec<u8> = (0..37).collect();
        let enc = encrypt(&INITIAL_KEY, &data);
        assert_eq!(enc.len(), 48);
        let dec = decrypt(&INITIAL_KEY, &enc);
        assert_eq!(&dec[..37], &data[..]);
        assert!(dec[37..].iter().all(|&b| b == 0));
    }

    /// A fake RM4 on localhost speaking the device side of the protocol.
    pub struct FakeRm4 {
        pub addr: SocketAddr,
        pub sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl FakeRm4 {
        pub fn start() -> Self {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr = socket.local_addr().unwrap();
            let sent = Arc::new(Mutex::new(Vec::new()));
            let log = sent.clone();
            std::thread::spawn(move || {
                let session_key = [0x42u8; 16];
                let mac = [1, 2, 3, 4, 5, 6];
                let mut key = INITIAL_KEY;
                let mut buf = [0u8; 2048];
                while let Ok((n, from)) = socket.recv_from(&mut buf) {
                    let req = &buf[..n];
                    let mut resp;
                    if n == 0x30 {
                        assert!(valid_checksum(req), "hello checksum");
                        resp = vec![0u8; 0x80];
                        resp[0x34..0x36].copy_from_slice(&0x6026u16.to_le_bytes());
                        resp[0x3A..0x40].copy_from_slice(&mac);
                    } else {
                        assert!(valid_checksum(req), "frame checksum");
                        assert_eq!(&req[..8], &MAGIC);
                        assert_eq!(&req[0x2A..0x30], &mac);
                        assert_eq!(u16::from_le_bytes([req[0x24], req[0x25]]), 0x6026);
                        let kind = u16::from_le_bytes([req[0x26], req[0x27]]);
                        let payload = decrypt(&key, &req[HEADER..]);
                        let reply = match kind {
                            TYPE_AUTH => {
                                assert_eq!(&payload[0x30..0x36], b"Test 1");
                                let mut r = vec![0u8; 0x20];
                                r[..4].copy_from_slice(&7u32.to_le_bytes());
                                r[4..20].copy_from_slice(&session_key);
                                let r = encrypt(&key, &r);
                                key = session_key;
                                r
                            }
                            TYPE_COMMAND => {
                                assert_eq!(
                                    u32::from_le_bytes([
                                        req[0x30], req[0x31], req[0x32], req[0x33]
                                    ]),
                                    7
                                );
                                let len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
                                let cmd = u32::from_le_bytes(payload[2..6].try_into().unwrap());
                                assert_eq!(cmd, CMD_SEND_DATA);
                                log.lock().unwrap().push(payload[6..len + 2].to_vec());
                                encrypt(&key, &[4, 0, 2, 0, 0, 0])
                            }
                            other => panic!("unexpected frame type {other:#x}"),
                        };
                        resp = req[..HEADER].to_vec();
                        resp[0x22] = 0;
                        resp[0x23] = 0;
                        resp.extend(reply);
                    }
                    resp[0x20] = 0;
                    resp[0x21] = 0;
                    let sum = checksum(&resp);
                    resp[0x20..0x22].copy_from_slice(&sum.to_le_bytes());
                    socket.send_to(&resp, from).unwrap();
                    // Real RM4s can answer one hello more than once (that
                    // is why python-broadlink's scan drops duplicates).
                    if n == 0x30 {
                        std::thread::sleep(Duration::from_millis(20));
                        socket.send_to(&resp, from).unwrap();
                    }
                }
            });
            Self { addr, sent }
        }
    }

    #[test]
    fn sends_packet_to_fake_rm4() {
        let rm = FakeRm4::start();
        let packet = parse_hex(DEFAULT_IR_POWER_PACKET).unwrap();
        let mut device = Broadlink::connect(rm.addr, Duration::from_secs(2)).unwrap();
        device.send_data(&packet).unwrap();
        assert_eq!(*rm.sent.lock().unwrap(), vec![packet]);
    }

    #[test]
    fn silent_device_times_out() {
        let quiet = UdpSocket::bind("127.0.0.1:0").unwrap();
        let start = std::time::Instant::now();
        let err = Broadlink::connect(quiet.local_addr().unwrap(), Duration::from_millis(100))
            .err()
            .unwrap();
        assert!(matches!(err, TvError::Broadlink(_)), "{err}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
