//! Unified engine error type.

use crate::crypto::CryptoError;
use syncengine_storage::StoreError;

/// Errors produced by the scanning, staging, transfer, and sync engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
    #[error("invalid path: {0}")]
    Path(String),
    #[error("integrity check failed for {0}")]
    Integrity(String),
    #[error("protocol error: {0}")]
    Protocol(#[from] syncengine_protocol::ProtocolError),
    #[error("invalid state: {0}")]
    State(String),
}
