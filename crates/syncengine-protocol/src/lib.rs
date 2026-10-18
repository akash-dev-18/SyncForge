//! Shared protocol foundation for SyncEngine.
//!
//! This crate contains the identifiers, causal version vectors, change
//! records, wire messages, and relative-path validation shared by the client
//! and the server. It has no filesystem, database, or network dependencies
//! so it stays trivially testable.

pub mod control;
pub mod ids;
pub mod messages;
pub mod path;
pub mod version;

pub use ids::{ChangeId, DeviceId, EntryId, FolderId};
pub use messages::{PROTOCOL_VERSION, PeerMessage};
pub use version::{CausalOrder, VersionVector};

use serde::{Deserialize, Serialize};

/// Logical file operation recorded in the change journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Create,
    Update,
    Delete,
    Rename,
}

/// Content identity: BLAKE3 hash, lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash(pub String);

impl ContentHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One journal entry describing a local or remote change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub change_id: ChangeId,
    pub folder_id: FolderId,
    pub entry_id: EntryId,
    pub operation: Operation,
    pub relative_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_relative_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<ContentHash>,
    #[serde(default)]
    pub size: u64,
    /// Version vector of this revision (after the change).
    pub version: VersionVector,
    /// Version vector this revision was based on (for conflict detection).
    #[serde(default)]
    pub base_version: VersionVector,
    pub origin_device: DeviceId,
    /// RFC 3339 UTC timestamp.
    pub timestamp: String,
}

/// Errors returned when peers cannot agree on protocol metadata.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("protocol version mismatch: peer speaks v{peer}, this build speaks v{local}")]
    VersionMismatch { local: u32, peer: u32 },
}

/// Verifies that a peer speaking protocol version `peer` is compatible with
/// `local`.
pub fn check_protocol_version(local: u32, peer: u32) -> Result<(), ProtocolError> {
    if local == peer {
        Ok(())
    } else {
        Err(ProtocolError::VersionMismatch { local, peer })
    }
}

/// Current UTC timestamp in the RFC 3339 format used across the protocol.
pub fn now_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc();
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unix-ms:0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_versions_are_accepted() {
        assert_eq!(
            check_protocol_version(PROTOCOL_VERSION, PROTOCOL_VERSION),
            Ok(())
        );
    }

    #[test]
    fn mismatched_versions_report_both_numbers() {
        let err = check_protocol_version(PROTOCOL_VERSION, 99).unwrap_err();
        assert_eq!(
            err,
            ProtocolError::VersionMismatch {
                local: PROTOCOL_VERSION,
                peer: 99,
            }
        );
        assert!(err.to_string().contains("v99"));
        assert!(err.to_string().contains('v') && err.to_string().contains('1'));
    }
}
