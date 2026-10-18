//! Content-addressed staging of local file revisions (SRD §8.14).
//!
//! A file's bytes are copied into `staging/<blake3-hex>` before they are
//! advertised to peers, so an older revision survives even when the source
//! file changes or disappears while delivery is still pending. Staged objects
//! are immutable and verified by hash.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use syncengine_protocol::ContentHash;

use crate::error::EngineError;
use crate::integrity::{hash_bytes, verify_file};

/// A reference to one staged content object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedObject {
    pub content_hash: ContentHash,
    pub size: u64,
    pub path: PathBuf,
}

/// A content-addressed staging directory.
pub struct StagingStore {
    root: PathBuf,
}

impl StagingStore {
    /// Opens (creating if needed) a staging directory at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, EngineError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The staging directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where an object with `hash` is stored.
    pub fn path_for(&self, hash: &ContentHash) -> PathBuf {
        self.root.join(hash.as_str())
    }

    /// Whether an object is present (without verifying its bytes).
    pub fn contains(&self, hash: &ContentHash) -> bool {
        self.path_for(hash).is_file()
    }

    /// Verifies a staged object against an expected hash and size.
    pub fn verify(&self, hash: &ContentHash, size: u64) -> Result<bool, EngineError> {
        verify_file(&self.path_for(hash), hash, size)
    }

    /// Copies `source` into staging, returning the verified object.
    pub fn stage_file(&self, source: &Path) -> Result<StagedObject, EngineError> {
        let mut src = std::fs::File::open(source)?;
        let tmp = self.temp_path();
        let mut dst = std::fs::File::create(&tmp)?;
        let mut hasher = blake3::Hasher::new();
        let mut size = 0u64;
        let mut buf = [0u8; 64 * 1024];
        let result = loop {
            match src.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    hasher.update(&buf[..n]);
                    if let Err(e) = dst.write_all(&buf[..n]) {
                        break Err(e);
                    }
                    size += n as u64;
                }
                Err(e) => break Err(e),
            }
        };
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        dst.sync_all()?;
        drop(dst);

        let content_hash = ContentHash(hasher.finalize().to_hex().to_string());
        let final_path = self.path_for(&content_hash);
        if final_path.exists() {
            std::fs::remove_file(&tmp)?;
        } else {
            std::fs::rename(&tmp, &final_path)?;
        }
        Ok(StagedObject {
            content_hash,
            size,
            path: final_path,
        })
    }

    /// Stages an in-memory buffer (used by tests and small payloads).
    pub fn stage_bytes(&self, bytes: &[u8]) -> Result<StagedObject, EngineError> {
        let content_hash = hash_bytes(bytes);
        let final_path = self.path_for(&content_hash);
        if !final_path.exists() {
            let tmp = self.temp_path();
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &final_path)?;
        }
        Ok(StagedObject {
            content_hash,
            size: bytes.len() as u64,
            path: final_path,
        })
    }

    /// Ensures a staged object exists and matches `size`; returns whether it
    /// is ready to use.
    pub fn ensure(&self, hash: &ContentHash, size: u64) -> Result<bool, EngineError> {
        if !self.contains(hash) {
            return Ok(false);
        }
        self.verify(hash, size)
    }

    /// Starts receiving a content object of `expected_size` bytes that must
    /// hash to `expected_hash`. Chunks are written sequentially with
    /// [`IncomingFile::write_chunk`]; [`IncomingFile::finish`] verifies and
    /// publishes the object into staging.
    ///
    /// The receive file is content-addressed (`.recv-{hash}`) and kept on an
    /// interrupted session, so [`IncomingFile::written`] after a later
    /// `receive` for the same object is a durable resume offset; callers
    /// request the transfer from that offset. An overlong leftover is
    /// discarded and the object restarts from zero.
    pub fn receive(
        &self,
        expected_hash: ContentHash,
        expected_size: u64,
    ) -> Result<IncomingFile<'_>, EngineError> {
        let path = self.root.join(format!(".recv-{}", expected_hash.as_str()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)?;
        let mut written = file.metadata()?.len();
        if written > expected_size {
            file.set_len(0)?;
            written = 0;
        }

        let mut hasher = blake3::Hasher::new();
        if written > 0 {
            use std::io::Read;
            let mut prefix = std::fs::File::open(&path)?;
            let mut remaining = written;
            let mut buffer = vec![0u8; 64 * 1024];
            while remaining > 0 {
                let take = remaining.min(buffer.len() as u64) as usize;
                prefix.read_exact(&mut buffer[..take])?;
                hasher.update(&buffer[..take]);
                remaining -= take as u64;
            }
        }

        Ok(IncomingFile {
            staging: self,
            expected_hash,
            expected_size,
            path,
            file,
            hasher,
            written,
        })
    }

    fn temp_path(&self) -> PathBuf {
        self.root.join(format!(
            ".tmp-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }
}

/// A content object being received chunk by chunk.
pub struct IncomingFile<'a> {
    staging: &'a StagingStore,
    expected_hash: ContentHash,
    expected_size: u64,
    path: PathBuf,
    file: std::fs::File,
    hasher: blake3::Hasher,
    written: u64,
}

impl IncomingFile<'_> {
    /// Bytes durably written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// The hash this object must produce.
    pub fn expected_hash(&self) -> &ContentHash {
        &self.expected_hash
    }

    /// Appends `data` at `offset`. Only strictly sequential writes (offset ==
    /// bytes written so far) are accepted; anything else is a protocol error.
    pub fn write_chunk(&mut self, offset: u64, data: &[u8]) -> Result<(), EngineError> {
        if offset != self.written {
            return Err(EngineError::Integrity(format!(
                "chunk offset {offset} does not match expected {}",
                self.written
            )));
        }
        if self.written + data.len() as u64 > self.expected_size {
            return Err(EngineError::Integrity(format!(
                "received more than the declared {} bytes",
                self.expected_size
            )));
        }
        use std::io::Write;
        self.file.write_all(data)?;
        self.hasher.update(data);
        self.written += data.len() as u64;
        Ok(())
    }

    /// Flushes, verifies the hash and size, and publishes the object into the
    /// staging directory. On any mismatch the temporary file is removed and an
    /// error is returned.
    pub fn finish(self) -> Result<StagedObject, EngineError> {
        let hash = ContentHash(self.hasher.finalize().to_hex().to_string());
        if hash != self.expected_hash {
            let _ = std::fs::remove_file(&self.path);
            return Err(EngineError::Integrity(format!(
                "received content hashes to {hash}, expected {}",
                self.expected_hash
            )));
        }
        if self.written != self.expected_size {
            let _ = std::fs::remove_file(&self.path);
            return Err(EngineError::Integrity(format!(
                "received {} bytes, expected {}",
                self.written, self.expected_size
            )));
        }
        self.file.sync_all()?;
        let final_path = self.staging.path_for(&self.expected_hash);
        if final_path.exists() {
            std::fs::remove_file(&self.path)?;
        } else {
            std::fs::rename(&self.path, &final_path)?;
        }
        Ok(StagedObject {
            content_hash: self.expected_hash.clone(),
            size: self.expected_size,
            path: final_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_file_is_content_addressed_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingStore::new(dir.path().join("staging")).unwrap();

        let src_a = dir.path().join("a.bin");
        std::fs::write(&src_a, b"same bytes").unwrap();
        let src_b = dir.path().join("b.bin");
        std::fs::write(&src_b, b"same bytes").unwrap();

        let first = staging.stage_file(&src_a).unwrap();
        let second = staging.stage_file(&src_b).unwrap();
        assert_eq!(first.content_hash, second.content_hash);
        assert_eq!(first.path, second.path);
        assert!(staging.verify(&first.content_hash, first.size).unwrap());
    }

    #[test]
    fn staged_copy_survives_source_change() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingStore::new(dir.path().join("staging")).unwrap();
        let src = dir.path().join("file.bin");
        std::fs::write(&src, b"original").unwrap();
        let staged = staging.stage_file(&src).unwrap();
        std::fs::write(&src, b"changed!").unwrap();
        assert!(staging.verify(&staged.content_hash, staged.size).unwrap());
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"original");
    }

    #[test]
    fn verify_rejects_wrong_size_or_missing() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingStore::new(dir.path().join("staging")).unwrap();
        let staged = staging.stage_bytes(b"hello").unwrap();
        assert!(staging.verify(&staged.content_hash, 5).unwrap());
        assert!(!staging.verify(&staged.content_hash, 6).unwrap());
        assert!(!staging.verify(&ContentHash("nope".into()), 5).unwrap());
        assert!(!staging.ensure(&staged.content_hash, 6).unwrap());
    }

    #[test]
    fn incoming_file_assembles_chunks_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingStore::new(dir.path().join("staging")).unwrap();
        let payload = b"streamed content";
        let hash = hash_bytes(payload);

        let mut receiver = staging.receive(hash.clone(), payload.len() as u64).unwrap();
        receiver.write_chunk(0, &payload[..4]).unwrap();
        receiver.write_chunk(4, &payload[4..9]).unwrap();
        assert_eq!(receiver.written(), 9);
        receiver.write_chunk(9, &payload[9..]).unwrap();
        let staged = receiver.finish().unwrap();

        assert_eq!(staged.content_hash, hash);
        assert_eq!(std::fs::read(&staged.path).unwrap(), payload);
    }

    #[test]
    fn incoming_file_rejects_out_of_order_and_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingStore::new(dir.path().join("staging")).unwrap();
        let mut receiver = staging.receive(ContentHash("00".repeat(32)), 10).unwrap();
        assert!(receiver.write_chunk(1, b"x").is_err());
        receiver.write_chunk(0, b"12345").unwrap();
        assert!(receiver.write_chunk(5, b"123456").is_err());
    }

    #[test]
    fn incoming_file_hash_mismatch_is_rejected_and_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingStore::new(dir.path().join("staging")).unwrap();
        let mut receiver = staging.receive(ContentHash("11".repeat(32)), 3).unwrap();
        receiver.write_chunk(0, b"abc").unwrap();
        assert!(receiver.finish().is_err());
        let leftovers = std::fs::read_dir(staging.root())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .count();
        assert_eq!(leftovers, 0, "temporary receive files must be removed");
    }

    #[test]
    fn dropped_incoming_file_keeps_a_durable_resume_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingStore::new(dir.path().join("staging")).unwrap();
        let payload = b"interrupted payload";
        let hash = hash_bytes(payload);
        {
            let mut receiver = staging.receive(hash.clone(), payload.len() as u64).unwrap();
            receiver.write_chunk(0, &payload[..5]).unwrap();
        }
        let leftovers: Vec<_> = std::fs::read_dir(staging.root())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();
        assert_eq!(leftovers.len(), 1, "the partial must be kept for resume");

        let mut resumed = staging.receive(hash.clone(), payload.len() as u64).unwrap();
        assert_eq!(resumed.written(), 5, "resume offset is the partial length");
        resumed.write_chunk(5, &payload[5..]).unwrap();
        let staged = resumed.finish().unwrap();
        assert_eq!(std::fs::read(&staged.path).unwrap(), payload);
        assert_eq!(
            std::fs::read_dir(staging.root())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .count(),
            1,
            "only the published object remains"
        );
    }

    #[test]
    fn overlong_leftover_restarts_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingStore::new(dir.path().join("staging")).unwrap();
        let payload = b"short";
        let hash = hash_bytes(payload);
        std::fs::write(
            staging.path_for(&hash),
            b"this is much longer than the object",
        )
        .unwrap();

        let receiver = staging.receive(hash.clone(), payload.len() as u64).unwrap();
        assert_eq!(receiver.written(), 0);
    }
}
