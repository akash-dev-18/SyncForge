//! Errors surfaced by the transport layer.

use syncengine_engine::{CryptoError, EngineError};
use syncengine_protocol::messages::MAX_FRAME_BYTES;
use syncengine_storage::StoreError;

/// Anything that can go wrong while connecting to, or talking with, a peer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection closed before the session finished")]
    Closed,
    #[error("peer rejected the handshake: {0}")]
    Unauthorized(String),
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_BYTES} byte limit")]
    FrameTooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error("websocket error: {0}")]
    WebSocket(String),
    #[error("relay rejected the connection: HTTP {status} ({body})")]
    RelayRejected { status: u16, body: String },
    #[error("discovery error: {0}")]
    Discovery(String),
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
    #[error("engine error: {0}")]
    Engine(#[from] EngineError),
    #[error("storage error: {0}")]
    Store(#[from] StoreError),
    #[error("peer {0} is not a trusted device")]
    UnknownPeer(String),
    #[error("the session timed out after {0:?} of inactivity")]
    Timeout(std::time::Duration),
}
