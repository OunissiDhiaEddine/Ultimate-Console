//! ADB wire format: a 24-byte little-endian header followed by the payload.
//!
//! ```text
//! command | arg0 | arg1 | data_length | data_crc32 | magic (= command ^ 0xFFFFFFFF)
//! ```
//!
//! "crc32" is really the byte sum of the payload. Devices on protocol
//! version 0x01000001 ignore it, older ones check it, so we always fill it in.

use std::io::{self, Read, Write};

pub const A_SYNC: u32 = u32::from_le_bytes(*b"SYNC");
pub const A_CNXN: u32 = u32::from_le_bytes(*b"CNXN");
pub const A_AUTH: u32 = u32::from_le_bytes(*b"AUTH");
pub const A_OPEN: u32 = u32::from_le_bytes(*b"OPEN");
pub const A_OKAY: u32 = u32::from_le_bytes(*b"OKAY");
pub const A_CLSE: u32 = u32::from_le_bytes(*b"CLSE");
pub const A_WRTE: u32 = u32::from_le_bytes(*b"WRTE");
pub const A_STLS: u32 = u32::from_le_bytes(*b"STLS");

/// Protocol version we speak (the one that allows skipping the checksum).
pub const A_VERSION: u32 = 0x0100_0001;
/// Largest payload we accept or send.
pub const MAX_PAYLOAD: u32 = 256 * 1024;

/// `AUTH` arg0 values.
pub const AUTH_TOKEN: u32 = 1;
pub const AUTH_SIGNATURE: u32 = 2;
pub const AUTH_RSAPUBLICKEY: u32 = 3;

pub const HEADER_LEN: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub command: u32,
    pub arg0: u32,
    pub arg1: u32,
    pub data: Vec<u8>,
}

impl Message {
    pub fn new(command: u32, arg0: u32, arg1: u32, data: impl Into<Vec<u8>>) -> Self {
        Self {
            command,
            arg0,
            arg1,
            data: data.into(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.data.len());
        for word in [
            self.command,
            self.arg0,
            self.arg1,
            self.data.len() as u32,
            checksum(&self.data),
            self.command ^ 0xFFFF_FFFF,
        ] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&self.data);
        out
    }

    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        // One write call, so the header and payload leave in one segment.
        w.write_all(&self.encode())?;
        w.flush()
    }

    pub fn read_from(r: &mut impl Read) -> io::Result<Self> {
        let mut header = [0u8; HEADER_LEN];
        r.read_exact(&mut header)?;
        let word = |i: usize| u32::from_le_bytes(header[i * 4..i * 4 + 4].try_into().unwrap());
        let (command, arg0, arg1, len, sum, magic) =
            (word(0), word(1), word(2), word(3), word(4), word(5));

        let bad = |msg: String| io::Error::new(io::ErrorKind::InvalidData, msg);
        if magic != command ^ 0xFFFF_FFFF {
            return Err(bad(format!("bad magic in ADB header {header:02x?}")));
        }
        if len > MAX_PAYLOAD {
            return Err(bad(format!("ADB payload too large ({len} bytes)")));
        }
        let mut data = vec![0u8; len as usize];
        r.read_exact(&mut data)?;
        // A zero sum is what newer peers send when they skip the checksum.
        if sum != 0 && sum != checksum(&data) {
            return Err(bad("ADB payload checksum mismatch".into()));
        }
        Ok(Self {
            command,
            arg0,
            arg1,
            data,
        })
    }
}

/// The ADB "crc32": sum of all payload bytes.
pub fn checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32))
}

/// Printable command name, for logs and errors.
pub fn command_name(command: u32) -> String {
    let bytes = command.to_le_bytes();
    if bytes.iter().all(u8::is_ascii_uppercase) {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("{command:#010x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_constants_match_adb() {
        // Values from system/core/adb/adb.h.
        assert_eq!(A_CNXN, 0x4e58_4e43);
        assert_eq!(A_AUTH, 0x4854_5541);
        assert_eq!(A_OPEN, 0x4e45_504f);
        assert_eq!(A_OKAY, 0x5941_4b4f);
        assert_eq!(A_CLSE, 0x4553_4c43);
        assert_eq!(A_WRTE, 0x4554_5257);
        assert_eq!(A_STLS, 0x534c_5453);
        assert_eq!(A_SYNC, 0x434e_5953);
    }

    #[test]
    fn encode_layout() {
        let m = Message::new(A_CNXN, A_VERSION, MAX_PAYLOAD, b"host::\0".to_vec());
        let bytes = m.encode();
        assert_eq!(bytes.len(), HEADER_LEN + 7);
        assert_eq!(&bytes[0..4], b"CNXN");
        assert_eq!(&bytes[4..8], &A_VERSION.to_le_bytes());
        assert_eq!(&bytes[8..12], &MAX_PAYLOAD.to_le_bytes());
        assert_eq!(&bytes[12..16], &7u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &checksum(b"host::\0").to_le_bytes());
        assert_eq!(&bytes[20..24], &(A_CNXN ^ 0xFFFF_FFFF).to_le_bytes());
        assert_eq!(&bytes[24..], b"host::\0");
    }

    #[test]
    fn roundtrip() {
        let m = Message::new(A_WRTE, 7, 1, b"Awake\n".to_vec());
        let back = Message::read_from(&mut m.encode().as_slice()).unwrap();
        assert_eq!(back, m);
        let empty = Message::new(A_OKAY, 1, 2, Vec::new());
        assert_eq!(
            Message::read_from(&mut empty.encode().as_slice()).unwrap(),
            empty
        );
    }

    #[test]
    fn zero_checksum_is_accepted() {
        let mut bytes = Message::new(A_WRTE, 1, 2, b"abc".to_vec()).encode();
        bytes[16..20].copy_from_slice(&[0; 4]);
        assert_eq!(
            Message::read_from(&mut bytes.as_slice()).unwrap().data,
            b"abc"
        );
    }

    #[test]
    fn rejects_corruption() {
        let good = Message::new(A_WRTE, 1, 2, b"abc".to_vec()).encode();

        let mut bad_magic = good.clone();
        bad_magic[20] ^= 1;
        assert!(Message::read_from(&mut bad_magic.as_slice()).is_err());

        let mut bad_sum = good.clone();
        bad_sum[24] ^= 1;
        assert!(Message::read_from(&mut bad_sum.as_slice()).is_err());

        let mut too_big = good.clone();
        too_big[12..16].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        assert!(Message::read_from(&mut too_big.as_slice()).is_err());

        assert!(Message::read_from(&mut &good[..26]).is_err());
    }

    #[test]
    fn names() {
        assert_eq!(command_name(A_CNXN), "CNXN");
        assert_eq!(command_name(1), "0x00000001");
    }
}
