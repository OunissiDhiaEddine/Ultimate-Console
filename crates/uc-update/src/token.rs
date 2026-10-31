//! The GitHub token used to read this repo's private Releases (plan §2.12).
//!
//! Dhia pastes a fine-grained token (this repo only, *Contents: read-only*)
//! into the settings app once. The agent stores it in
//! `%LOCALAPPDATA%\UltimateConsole\update-token.bin`, encrypted with DPAPI for
//! the current Windows user, so another account (or a copied file) can't read
//! it. It is never logged, compiled in or committed.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Longest token accepted. GitHub tokens are well under this.
const MAX_TOKEN_LEN: usize = 512;

/// Reads and writes the encrypted token file.
#[derive(Debug, Clone)]
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Store for [`uc_core::Paths::update_token_file`].
    pub fn for_paths(paths: &uc_core::Paths) -> Self {
        Self::new(paths.update_token_file())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether a token file exists. Cheap; does not decrypt.
    pub fn is_set(&self) -> bool {
        self.path.is_file()
    }

    /// Validates, encrypts and saves `token` (surrounding whitespace is trimmed).
    pub fn save(&self, token: &str) -> Result<()> {
        let token = validate(token)?;
        let blob = sys::protect(token.as_bytes())?;
        uc_core::fsutil::write_atomic(&self.path, &blob).map_err(|e| match e {
            uc_core::Error::Io { path, source } => Error::io(path, source),
            other => Error::io(&self.path, std::io::Error::other(other.to_string())),
        })?;
        tracing::info!(path = %self.path.display(), "update token saved");
        Ok(())
    }

    /// Decrypts the token. `Ok(None)` when none is saved.
    pub fn load(&self) -> Result<Option<String>> {
        let blob = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io(&self.path, e)),
        };
        let bytes = sys::unprotect(&blob)?;
        let token = String::from_utf8(bytes).map_err(|_| Error::InvalidToken("not UTF-8"))?;
        Ok(Some(token))
    }

    /// Deletes the token file. Not an error if there is none.
    pub fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => {
                tracing::info!("update token removed");
                Ok(())
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io(&self.path, e)),
        }
    }
}

/// Trims and sanity-checks a pasted token. Returns the trimmed token.
pub fn validate(token: &str) -> Result<&str> {
    let token = token.trim();
    if token.is_empty() {
        return Err(Error::InvalidToken("empty"));
    }
    if token.len() > MAX_TOKEN_LEN {
        return Err(Error::InvalidToken("too long"));
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::InvalidToken(
            "only letters, digits, '_' and '-' are allowed",
        ));
    }
    Ok(token)
}

#[cfg(windows)]
mod sys {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::w;

    use crate::{Error, Result};

    /// Extra entropy so other apps using DPAPI for this user can't simply
    /// call `CryptUnprotectData` on the file and get the token back.
    const ENTROPY: &[u8] = b"UltimateConsole update token v1";

    fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        }
    }

    /// Copies a DPAPI output blob into a Vec and frees it.
    ///
    /// # Safety
    /// `out` must have been filled by CryptProtectData/CryptUnprotectData.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: DPAPI returned `cbData` valid bytes at `pbData`, allocated
        // with LocalAlloc, which we free exactly once after copying.
        unsafe {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
            v
        }
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>> {
        let (input, entropy) = (blob(plain), blob(ENTROPY));
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input and entropy point at live slices for the whole call
        // (DPAPI only reads them); `out` is written by the call and freed in `take`.
        unsafe {
            CryptProtectData(
                &input,
                w!("Ultimate Console update token"),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| Error::win32("CryptProtectData", e))?;
            Ok(take(out))
        }
    }

    pub fn unprotect(encrypted: &[u8]) -> Result<Vec<u8>> {
        let (input, entropy) = (blob(encrypted), blob(ENTROPY));
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as in `protect`.
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| Error::win32("CryptUnprotectData", e))?;
            Ok(take(out))
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use crate::{Error, Result};

    pub fn protect(_plain: &[u8]) -> Result<Vec<u8>> {
        Err(Error::Unsupported)
    }

    pub fn unprotect(_encrypted: &[u8]) -> Result<Vec<u8>> {
        Err(Error::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_tokens() {
        assert_eq!(
            validate("  github_pat_11ABC-def_9\n").unwrap(),
            "github_pat_11ABC-def_9"
        );
        assert!(validate("").is_err());
        assert!(validate("   ").is_err());
        assert!(validate("has space").is_err());
        assert!(validate("Bearer\tx").is_err());
        assert!(validate(&"a".repeat(MAX_TOKEN_LEN + 1)).is_err());
    }

    #[test]
    fn missing_file_is_none_and_clear_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("update-token.bin"));
        assert!(!store.is_set());
        assert!(store.load().unwrap().is_none());
        store.clear().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("update-token.bin"));
        store.save(" github_pat_test_123 ").unwrap();
        assert!(store.is_set());
        let raw = fs::read(store.path()).unwrap();
        assert!(!raw.windows(8).any(|w| w == b"github_p"), "stored in clear");
        assert_eq!(
            store.load().unwrap().as_deref(),
            Some("github_pat_test_123")
        );
        store.clear().unwrap();
        assert!(!store.is_set());
    }
}
