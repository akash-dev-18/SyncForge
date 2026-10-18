//! Content hashing and verification (LLD `IntegrityVerifier`).
//!
//! Content identity is BLAKE3 over the file bytes, rendered as lowercase hex.
//! The same hash is used for the staging cache, transfer verification, and the
//! journal's `content_hash`.

use std::io::Read;
use std::path::Path;

use syncengine_protocol::ContentHash;

use crate::error::EngineError;

/// Hashes an in-memory buffer.
pub fn hash_bytes(bytes: &[u8]) -> ContentHash {
    ContentHash(blake3::hash(bytes).to_hex().to_string())
}

/// Streams a file and returns its BLAKE3 hash.
pub fn hash_file(path: &Path) -> Result<ContentHash, EngineError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(ContentHash(hasher.finalize().to_hex().to_string()))
}

/// Returns `true` when `path` exists, has `expected_size` bytes, and hashes to
/// `expected`.
pub fn verify_file(
    path: &Path,
    expected: &ContentHash,
    expected_size: u64,
) -> Result<bool, EngineError> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if metadata.len() != expected_size {
        return Ok(false);
    }
    Ok(&hash_file(path)? == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn hash_bytes_matches_hash_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"hello syncengine").unwrap();
        drop(f);
        assert_eq!(hash_bytes(b"hello syncengine"), hash_file(&path).unwrap());
    }

    #[test]
    fn verify_detects_size_and_content_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, b"abc").unwrap();
        let good = hash_bytes(b"abc");
        assert!(verify_file(&path, &good, 3).unwrap());
        assert!(!verify_file(&path, &good, 4).unwrap());
        assert!(!verify_file(&path, &hash_bytes(b"xyz"), 3).unwrap());
        assert!(!verify_file(&dir.path().join("missing"), &good, 3).unwrap());
    }
}
