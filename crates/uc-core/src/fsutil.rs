//! Small file helpers.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, Result};

/// Writes `bytes` to `path` atomically: write a temp file in the same
/// directory, flush it to disk, then rename it over the target. A crash at
/// any point leaves either the old file or the new one, never a torn file.
///
/// Creates the parent directory if needed.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    // pid + counter: unique across processes and across threads of this one.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{file_name}.{}.{n}.tmp", std::process::id()));

    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        // On Windows std uses MoveFileExW(MOVEFILE_REPLACE_EXISTING).
        fs::rename(&tmp, path)
    })();

    result.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::io(path, e)
    })
}

/// Reads a file, returning `Ok(None)` if it does not exist.
pub fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_creates_dirs_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b.txt");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        // No temp files left behind.
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn read_optional_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_optional(&dir.path().join("nope")).unwrap().is_none());
    }
}
