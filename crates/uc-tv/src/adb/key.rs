//! The PC's ADB identity: a 2048-bit RSA key, stored as PKCS#8 PEM (the same
//! format `adb` itself writes to `~/.android/adbkey`, so an existing key can
//! be copied over if wanted).

use std::path::Path;

use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Pkcs1v15Sign, RsaPrivateKey};
use sha1::Sha1;

use crate::{Result, TvError};

/// ADB only accepts 2048-bit keys (`ANDROID_PUBKEY_MODULUS_SIZE`).
const KEY_BITS: usize = 2048;
const MODULUS_BYTES: usize = KEY_BITS / 8;
/// Length of the token in an `AUTH TOKEN` message (`TOKEN_SIZE`).
pub const TOKEN_LEN: usize = 20;

pub struct AdbKey {
    key: RsaPrivateKey,
    /// `base64(android_pubkey) + " name"`, as sent in `AUTH RSAPUBLICKEY`.
    public: String,
}

impl std::fmt::Debug for AdbKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdbKey").finish_non_exhaustive()
    }
}

impl AdbKey {
    /// Generates a fresh key. Takes up to a few seconds.
    pub fn generate() -> Result<Self, String> {
        let mut rng = rsa::rand_core::OsRng;
        let key = RsaPrivateKey::new(&mut rng, KEY_BITS).map_err(|e| e.to_string())?;
        Self::from_key(key)
    }

    /// Parses a PKCS#8 (`BEGIN PRIVATE KEY`) or PKCS#1 (`BEGIN RSA PRIVATE KEY`) PEM.
    pub fn from_pem(pem: &str) -> Result<Self, String> {
        let key = RsaPrivateKey::from_pkcs8_pem(pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
            .map_err(|e| format!("not an RSA private key: {e}"))?;
        Self::from_key(key)
    }

    fn from_key(key: RsaPrivateKey) -> Result<Self, String> {
        if key.size() != MODULUS_BYTES {
            return Err(format!(
                "ADB needs a {KEY_BITS}-bit RSA key, this one is {} bits",
                key.size() * 8
            ));
        }
        let public = format!(
            "{} {}",
            base64(&android_pubkey(key.n(), key.e())),
            key_name()
        );
        Ok(Self { key, public })
    }

    pub fn to_pem(&self) -> Result<String, String> {
        self.key
            .to_pkcs8_pem(LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|e| e.to_string())
    }

    /// Loads the key at `path`, or generates and saves one if the file does
    /// not exist. A corrupt file is an error rather than silently replaced,
    /// because replacing it would un-pair the TV.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        let err = |reason: String| TvError::Key {
            path: path.to_path_buf(),
            reason,
        };
        if let Some(pem) = uc_core::fsutil::read_optional(path)? {
            return Self::from_pem(&pem).map_err(err);
        }
        tracing::info!(path = %path.display(), "generating a new ADB key");
        let key = Self::generate().map_err(err)?;
        uc_core::fsutil::write_atomic(path, key.to_pem().map_err(err)?.as_bytes())?;
        Ok(key)
    }

    /// Signs an `AUTH TOKEN` challenge. adbd treats the token as a SHA-1
    /// digest and checks a PKCS#1 v1.5 signature over it (`RSA_sign(NID_sha1)`).
    pub fn sign_token(&self, token: &[u8]) -> Result<Vec<u8>> {
        if token.len() != TOKEN_LEN {
            return Err(TvError::Protocol(format!(
                "AUTH token is {} bytes, expected {TOKEN_LEN}",
                token.len()
            )));
        }
        self.key
            .sign(Pkcs1v15Sign::new::<Sha1>(), token)
            .map_err(|e| TvError::Protocol(format!("signing failed: {e}")))
    }

    /// The public key line sent to the TV when pairing.
    pub fn public_key(&self) -> &str {
        &self.public
    }

    #[cfg(test)]
    pub(crate) fn rsa_public(&self) -> rsa::RsaPublicKey {
        self.key.to_public_key()
    }
}

/// Encodes a public key the way `android_pubkey_encode` does:
///
/// ```text
/// u32 modulus_size_words (64) | u32 n0inv | u8 n[256] (LE) | u8 rr[256] (LE) | u32 e
/// ```
///
/// where `n0inv = -1 / n mod 2^32` and `rr = 2^4096 mod n` (Montgomery helpers
/// adbd uses to verify signatures).
pub fn android_pubkey(n: &BigUint, e: &BigUint) -> Vec<u8> {
    let n_le = le_padded(n);
    let n0 = u32::from_le_bytes(n_le[0..4].try_into().unwrap());
    let rr = (BigUint::from(1u8) << (2 * KEY_BITS)) % n;
    let mut e_le = e.to_bytes_le();
    e_le.resize(4, 0);
    let e = u32::from_le_bytes(e_le[..4].try_into().unwrap());

    let mut out = Vec::with_capacity(4 + 4 + 2 * MODULUS_BYTES + 4);
    out.extend_from_slice(&((MODULUS_BYTES / 4) as u32).to_le_bytes());
    out.extend_from_slice(&inverse_mod_2_32(n0).wrapping_neg().to_le_bytes());
    out.extend_from_slice(&n_le);
    out.extend_from_slice(&le_padded(&rr));
    out.extend_from_slice(&e.to_le_bytes());
    out
}

fn le_padded(v: &BigUint) -> Vec<u8> {
    let mut bytes = v.to_bytes_le();
    bytes.resize(MODULUS_BYTES, 0);
    bytes
}

/// Inverse of an odd number modulo 2^32 (Newton's iteration; each step
/// doubles the number of correct low bits).
fn inverse_mod_2_32(a: u32) -> u32 {
    debug_assert!(a & 1 == 1, "RSA modulus is odd");
    let mut x = a; // correct to 3 bits for odd a
    for _ in 0..5 {
        x = x.wrapping_mul(2u32.wrapping_sub(a.wrapping_mul(x)));
    }
    x
}

/// Shown next to the key on the TV ("user@host").
fn key_name() -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "pc".into());
    let host: String = host
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    format!(
        "{}@{}",
        uc_core::APP_NAME,
        if host.is_empty() { "pc" } else { &host }
    )
}

/// Standard base64 with padding.
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// One generated key shared by every test in the crate.
    pub(crate) fn test_key() -> &'static AdbKey {
        static KEY: OnceLock<AdbKey> = OnceLock::new();
        KEY.get_or_init(|| AdbKey::generate().unwrap())
    }

    #[test]
    fn base64_vectors() {
        // RFC 4648 §10.
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input:?}");
        }
    }

    #[test]
    fn inverse() {
        for a in [1u32, 3, 0xFFFF_FFFF, 0x1234_5679, 0x8000_0001] {
            assert_eq!(a.wrapping_mul(inverse_mod_2_32(a)), 1, "{a:#x}");
        }
    }

    #[test]
    fn android_pubkey_layout() {
        let key = test_key();
        let pk = key.rsa_public();
        let enc = android_pubkey(pk.n(), pk.e());
        assert_eq!(enc.len(), 524);
        let word = |i: usize| u32::from_le_bytes(enc[i..i + 4].try_into().unwrap());
        assert_eq!(word(0), 64);
        // n0inv * n0 == -1 (mod 2^32)
        assert_eq!(word(4).wrapping_mul(word(8)), u32::MAX);
        assert_eq!(BigUint::from_bytes_le(&enc[8..264]), *pk.n());
        let rr = BigUint::from_bytes_le(&enc[264..520]);
        assert_eq!(rr, (BigUint::from(1u8) << 4096) % pk.n());
        assert_eq!(word(520), 65537);

        let line = key.public_key();
        let (b64, name) = line.split_once(' ').unwrap();
        assert_eq!(b64, base64(&enc));
        assert!(name.starts_with("UltimateConsole@"), "{name}");
    }

    #[test]
    fn signature_verifies() {
        let key = test_key();
        let token = [7u8; TOKEN_LEN];
        let sig = key.sign_token(&token).unwrap();
        assert_eq!(sig.len(), 256);
        key.rsa_public()
            .verify(Pkcs1v15Sign::new::<Sha1>(), &token, &sig)
            .unwrap();
        assert!(key.sign_token(&[0u8; 16]).is_err());
    }

    #[test]
    fn pem_roundtrip_and_persistence() {
        let key = test_key();
        let pem = key.to_pem().unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert_eq!(
            AdbKey::from_pem(&pem).unwrap().public_key(),
            key.public_key()
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("adbkey");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &pem).unwrap();
        let loaded = AdbKey::load_or_create(&path).unwrap();
        assert_eq!(loaded.public_key(), key.public_key());

        std::fs::write(&path, "garbage").unwrap();
        assert!(matches!(
            AdbKey::load_or_create(&path),
            Err(TvError::Key { .. })
        ));
        assert!(AdbKey::from_pem("").is_err());
    }

    #[test]
    fn creates_missing_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("adbkey");
        let created = AdbKey::load_or_create(&path).unwrap();
        assert!(path.exists());
        let again = AdbKey::load_or_create(&path).unwrap();
        assert_eq!(created.public_key(), again.public_key());
    }
}
