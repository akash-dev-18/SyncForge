//! Peer-to-peer wire messages.
//!
//! Peers exchange length-prefixed JSON frames whose payload is this enum.
//! For content transfers the frames are additionally encrypted end-to-end
//! (see the `syncengine-engine` crypto module); the relay never sees these
//! fields in the clear.

use serde::{Deserialize, Serialize};

use crate::{ChangeRecord, ContentHash, DeviceId, EntryId, FolderId, VersionVector};

/// Wire-protocol version spoken by this build.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum accepted frame size (16 MiB) to bound memory and reject abuse.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Messages exchanged between peers once a connection is established.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerMessage {
    /// First frame: protocol version and sender identity.
    Hello {
        protocol_version: u32,
        device_id: DeviceId,
        /// Ed25519 public key, hex.
        signing_key: String,
        /// X25519 public key, hex.
        encryption_key: String,
        /// Signature over the handshake transcript by `signing_key`.
        /// Proves possession of the signing private key.
        signature: String,
    },
    /// Acceptance of a hello. Carries the responder's identity fields and a
    /// self-signature over [`hello_transcript`], mirroring [`PeerMessage::Hello`].
    HelloAccepted {
        protocol_version: u32,
        device_id: DeviceId,
        /// Ed25519 public key, hex.
        signing_key: String,
        /// X25519 public key, hex.
        encryption_key: String,
        /// Signature over the handshake transcript by `signing_key`.
        signature: String,
    },

    /// Request the pending changes a peer has for us.
    ChangesRequest { folder_id: FolderId },
    /// A batch of change metadata (no file content).
    Changes {
        folder_id: FolderId,
        changes: Vec<ChangeRecord>,
    },
    /// Request file content for a change.
    ContentRequest {
        change_id: crate::ChangeId,
        folder_id: FolderId,
        entry_id: EntryId,
        expected_hash: ContentHash,
        expected_size: u64,
        /// Byte offset to resume from.
        offset: u64,
    },
    /// One chunk of file content.
    Chunk {
        change_id: crate::ChangeId,
        offset: u64,
        /// Raw bytes (base64 in JSON).
        data: String,
        /// True when this is the last chunk.
        last: bool,
    },
    /// Receiver confirms durable, verified acceptance of a change.
    Ack {
        change_id: crate::ChangeId,
        folder_id: FolderId,
        /// Version vector after applying (for the sender's bookkeeping).
        version: VersionVector,
    },
    /// Receiver detected a concurrent edit and preserved both versions.
    Conflict {
        change_id: crate::ChangeId,
        folder_id: FolderId,
        entry_id: EntryId,
        conflict_path: String,
    },
    /// Application-level error; the connection stays usable.
    Error { code: String, message: String },
    /// This side has sent everything it has for the peer and received all
    /// acknowledgements; the session ends when both sides have sent `Done`.
    Done,
    /// Keepalive.
    Ping,
    /// Keepalive response.
    Pong,
}

impl PeerMessage {
    /// Encodes a message to a JSON byte vector.
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Decodes a message from JSON bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// The signed bytes proving possession of `signing_key` for a hello-like
    /// message. Both [`PeerMessage::Hello`] and [`PeerMessage::HelloAccepted`]
    /// sign this transcript with their Ed25519 key.
    pub fn hello_transcript(
        device_id: &DeviceId,
        signing_key: &str,
        encryption_key: &str,
    ) -> String {
        format!("syncengine-hello-v1\n{device_id}\n{signing_key}\n{encryption_key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Operation;

    #[test]
    fn messages_round_trip_through_json() {
        let msg = PeerMessage::Changes {
            folder_id: FolderId::new(),
            changes: vec![ChangeRecord {
                change_id: crate::ChangeId::new(),
                folder_id: FolderId::new(),
                entry_id: EntryId::new(),
                operation: Operation::Create,
                relative_path: "a/b.txt".into(),
                old_relative_path: None,
                content_hash: Some(ContentHash("00".repeat(32))),
                size: 42,
                version: VersionVector::new(),
                base_version: VersionVector::new(),
                origin_device: DeviceId::new(),
                timestamp: "unix-ms:0".into(),
            }],
        };
        let bytes = msg.to_bytes().expect("encode");
        let back = PeerMessage::from_bytes(&bytes).expect("decode");
        assert_eq!(back, msg);
    }

    #[test]
    fn message_tag_is_snake_case() {
        let msg = PeerMessage::Ping;
        let json = msg.to_bytes().unwrap();
        assert_eq!(json, br#"{"type":"ping"}"#);
    }

    #[test]
    fn hello_carries_identity_fields() {
        let device_id = DeviceId::new();
        let msg = PeerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_id,
            signing_key: "aa".into(),
            encryption_key: "bb".into(),
            signature: "cc".into(),
        };
        let json = String::from_utf8(msg.to_bytes().unwrap()).unwrap();
        assert!(json.contains("protocol_version"));
        assert!(json.contains("signing_key"));

        let transcript = PeerMessage::hello_transcript(&device_id, "aa", "bb");
        assert!(transcript.starts_with("syncengine-hello-v1\n"));
        assert!(transcript.contains(&device_id.to_string()));
        assert_ne!(
            transcript,
            PeerMessage::hello_transcript(&device_id, "aa", "cc"),
            "the encryption key must be part of the transcript"
        );
    }

    #[test]
    fn chunk_encodes_base64_data() {
        let msg = PeerMessage::Chunk {
            change_id: crate::ChangeId::new(),
            offset: 0,
            data: base64_data(),
            last: true,
        };
        let json = String::from_utf8(msg.to_bytes().unwrap()).unwrap();
        assert!(json.contains("\"last\":true"));
    }

    fn base64_data() -> String {
        "AAAA".to_string()
    }
}
