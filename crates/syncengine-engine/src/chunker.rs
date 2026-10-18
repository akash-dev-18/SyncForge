//! Fixed-size chunking for resumable transfers (LLD `Chunker`).
//!
//! Files are transferred as ordered fixed-size chunks. Because chunks are
//! addressed by byte offset, a receiver can persist a durable contiguous
//! offset and resume after a reconnect (SRD §8.11, FR-09).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::EngineError;

/// Default chunk size: 256 KiB.
pub const DEFAULT_CHUNK_SIZE: usize = 256 * 1024;

/// One chunk read from a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub offset: u64,
    pub data: Vec<u8>,
    /// Total size of the whole file these chunks belong to.
    pub total_size: u64,
    /// True when this is the final chunk.
    pub eof: bool,
}

/// Splits and reads files into fixed-size chunks.
pub struct Chunker {
    chunk_size: usize,
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new(DEFAULT_CHUNK_SIZE)
    }
}

impl Chunker {
    /// Creates a chunker with `chunk_size` (clamped to at least 1 byte).
    pub fn new(chunk_size: usize) -> Self {
        Self {
            chunk_size: chunk_size.max(1),
        }
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Number of chunks a file of `total` bytes will produce.
    pub fn chunk_count(&self, total: u64) -> u64 {
        if total == 0 {
            0
        } else {
            total.div_ceil(self.chunk_size as u64)
        }
    }

    /// Reads the chunk starting at `offset`.
    pub fn read_chunk(&self, path: &Path, offset: u64) -> Result<Chunk, EngineError> {
        let mut file = std::fs::File::open(path)?;
        let total_size = file.metadata()?.len();
        if offset > total_size {
            return Err(EngineError::State(format!(
                "chunk offset {offset} beyond file size {total_size}"
            )));
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut data = vec![0u8; self.chunk_size];
        let mut filled = 0usize;
        while filled < self.chunk_size {
            let n = file.read(&mut data[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        data.truncate(filled);
        Ok(Chunk {
            offset,
            data,
            total_size,
            eof: offset + filled as u64 >= total_size,
        })
    }

    /// Iterates every chunk of `path` in order.
    pub fn read_all(&self, path: &Path) -> Result<Vec<Chunk>, EngineError> {
        let total_size = std::fs::metadata(path)?.len();
        let mut chunks = Vec::new();
        let mut offset = 0u64;
        if total_size == 0 {
            return Ok(chunks);
        }
        loop {
            let chunk = self.read_chunk(path, offset)?;
            let len = chunk.data.len() as u64;
            let eof = chunk.eof;
            chunks.push(chunk);
            offset += len;
            if eof {
                break;
            }
            if len == 0 {
                return Err(EngineError::State("chunker made no progress".into()));
            }
        }
        Ok(chunks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_count_rounds_up() {
        let c = Chunker::new(10);
        assert_eq!(c.chunk_count(0), 0);
        assert_eq!(c.chunk_count(1), 1);
        assert_eq!(c.chunk_count(10), 1);
        assert_eq!(c.chunk_count(11), 2);
        assert_eq!(c.chunk_count(25), 3);
    }

    #[test]
    fn split_and_reassemble_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let bytes: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();

        let chunker = Chunker::new(256);
        let chunks = chunker.read_all(&path).unwrap();
        assert_eq!(chunks.len(), 4);
        assert!(chunks.last().unwrap().eof);
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.offset, (i * 256) as u64);
            assert_eq!(chunk.total_size, 1000);
        }

        let mut assembled = Vec::new();
        for chunk in &chunks {
            assembled.extend_from_slice(&chunk.data);
        }
        assert_eq!(assembled, bytes);
    }

    #[test]
    fn empty_file_has_no_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        std::fs::write(&path, b"").unwrap();
        let chunks = Chunker::default().read_all(&path).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn offset_beyond_end_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"abc").unwrap();
        assert!(Chunker::new(2).read_chunk(&path, 99).is_err());
    }
}
