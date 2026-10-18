//! Local persistence for the SyncEngine client.
//!
//! One SQLite database per device holds identity, peers, shared folders, the
//! change journal, delivery targets, tombstones, transfer checkpoints, applied
//! remote changes (for idempotency), conflicts, and staged-content metadata
//! (SRD §8). Foreign keys are enforced on every connection.

mod sqlite;
mod traits;

pub use sqlite::SqliteStore;
pub use traits::{Store, StoreError};

use serde::{Deserialize, Serialize};
use syncengine_protocol::{ChangeId, ContentHash, DeviceId, EntryId, FolderId, VersionVector};

/// Trust state of a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    Pending,
    Trusted,
    Revoked,
}

impl TrustState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrustState::Pending => "pending",
            TrustState::Trusted => "trusted",
            TrustState::Revoked => "revoked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(TrustState::Pending),
            "trusted" => Some(TrustState::Trusted),
            "revoked" => Some(TrustState::Revoked),
            _ => None,
        }
    }
}

/// Permission a peer has on a shared folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    ReadOnly,
    ReadWrite,
}

impl Permission {
    pub fn as_str(&self) -> &'static str {
        match self {
            Permission::ReadOnly => "read_only",
            Permission::ReadWrite => "read_write",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read_only" => Some(Permission::ReadOnly),
            "read_write" => Some(Permission::ReadWrite),
            _ => None,
        }
    }
}

/// Whether a file entry is currently present or tombstoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    Present,
    Deleted,
}

impl EntryState {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryState::Present => "present",
            EntryState::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "present" => Some(EntryState::Present),
            "deleted" => Some(EntryState::Deleted),
            _ => None,
        }
    }
}

/// Delivery state of one change for one peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetState {
    Pending,
    InProgress,
    Delivered,
    Failed,
}

impl TargetState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TargetState::Pending => "pending",
            TargetState::InProgress => "in_progress",
            TargetState::Delivered => "delivered",
            TargetState::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(TargetState::Pending),
            "in_progress" => Some(TargetState::InProgress),
            "delivered" => Some(TargetState::Delivered),
            "failed" => Some(TargetState::Failed),
            _ => None,
        }
    }
}

/// Direction of a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Send,
    Receive,
}

impl TransferDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransferDirection::Send => "send",
            TransferDirection::Receive => "receive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "send" => Some(TransferDirection::Send),
            "receive" => Some(TransferDirection::Receive),
            _ => None,
        }
    }
}

/// Lifecycle state of a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Active,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl TransferState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransferState::Queued => "queued",
            TransferState::Active => "active",
            TransferState::Paused => "paused",
            TransferState::Completed => "completed",
            TransferState::Failed => "failed",
            TransferState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(TransferState::Queued),
            "active" => Some(TransferState::Active),
            "paused" => Some(TransferState::Paused),
            "completed" => Some(TransferState::Completed),
            "failed" => Some(TransferState::Failed),
            "cancelled" => Some(TransferState::Cancelled),
            _ => None,
        }
    }
}

/// Lifecycle of a conflict record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictStatus {
    Unresolved,
    Resolved,
    Dismissed,
}

impl ConflictStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictStatus::Unresolved => "unresolved",
            ConflictStatus::Resolved => "resolved",
            ConflictStatus::Dismissed => "dismissed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "unresolved" => Some(ConflictStatus::Unresolved),
            "resolved" => Some(ConflictStatus::Resolved),
            "dismissed" => Some(ConflictStatus::Dismissed),
            _ => None,
        }
    }
}

/// The singleton local-device record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDevice {
    pub device_id: DeviceId,
    pub device_name: String,
    pub created_at: String,
}

/// A known peer device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub device_id: DeviceId,
    pub device_name: String,
    /// Ed25519 public key, hex-encoded.
    pub public_key: String,
    /// X25519 public key, hex-encoded.
    pub encryption_key: String,
    pub platform: String,
    pub trust: TrustState,
    pub last_seen_at: Option<String>,
    pub created_at: String,
}

/// A registered shared folder (logical ID + device-local root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFolder {
    pub folder_id: FolderId,
    pub name: String,
    pub local_root: std::path::PathBuf,
    pub enabled: bool,
    pub created_at: String,
}

/// Membership of a peer in a shared folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderPeer {
    pub folder_id: FolderId,
    pub peer_id: DeviceId,
    pub permission: Permission,
    pub joined_at: String,
}

/// One indexed file or directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub entry_id: EntryId,
    pub folder_id: FolderId,
    pub relative_path: String,
    pub state: EntryState,
    pub content_hash: Option<ContentHash>,
    pub size: u64,
    pub modified_at: Option<String>,
    pub version: VersionVector,
    pub last_change_id: Option<ChangeId>,
    pub updated_at: String,
}

/// Delivery bookkeeping for one change against one peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeTarget {
    pub change_id: ChangeId,
    pub peer_id: DeviceId,
    pub state: TargetState,
    pub attempts: u32,
    pub next_retry_at: Option<String>,
    pub acknowledged_at: Option<String>,
    pub last_error: Option<String>,
}

/// A deletion record; primary key is the delete change id (SRD §8.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    pub change_id: ChangeId,
    pub entry_id: EntryId,
    pub folder_id: FolderId,
    pub last_path: String,
    pub version: VersionVector,
    pub deleted_at: String,
}

/// Per-peer, per-folder delivery watermark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSyncState {
    pub folder_id: FolderId,
    pub peer_id: DeviceId,
    pub last_acknowledged_seq: i64,
    pub last_synced_at: Option<String>,
}

/// Durable transfer checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub transfer_id: String,
    pub change_id: ChangeId,
    pub peer_id: DeviceId,
    pub direction: TransferDirection,
    pub state: TransferState,
    pub expected_hash: Option<ContentHash>,
    pub expected_size: u64,
    /// Contiguous durable byte offset (SRD §8.11).
    pub acknowledged_bytes: u64,
    pub temporary_path: Option<String>,
    pub updated_at: String,
}

/// A conflict surfaced by concurrent edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub conflict_id: String,
    pub folder_id: FolderId,
    pub entry_id: EntryId,
    pub local_change_id: Option<ChangeId>,
    pub remote_change_id: Option<ChangeId>,
    pub status: ConflictStatus,
    pub resolution: Option<String>,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

/// Immutable staged content tracked by hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentObject {
    pub content_hash: ContentHash,
    pub size: u64,
    pub staged_path: String,
    pub refcount: i64,
    pub created_at: String,
    pub verified_at: Option<String>,
}
