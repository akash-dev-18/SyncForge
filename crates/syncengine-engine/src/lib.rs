//! SyncEngine engine: identity, scanning, staging, transfer, and the sync
//! orchestration that turns on-disk state into protocol changes and back.

pub mod chunker;
pub mod crypto;
pub mod error;
pub mod integrity;
pub mod scanner;
pub mod staging;
pub mod sync;

pub use chunker::{Chunk, Chunker, DEFAULT_CHUNK_SIZE};
pub use crypto::{CryptoError, Identity};
pub use error::EngineError;
pub use integrity::{hash_bytes, hash_file, verify_file};
pub use scanner::{ScanResult, ScannedFile, SkippedEntry, scan_root};
pub use staging::{IncomingFile, StagedObject, StagingStore};
pub use sync::{
    ApplyOutcome, ConflictOutcome, ConflictResolution, GcReport, QueueReport, RemotePlan,
    SyncEngine,
};
