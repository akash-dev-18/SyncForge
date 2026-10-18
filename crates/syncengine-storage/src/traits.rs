//! The `Store` trait and its error type.

use syncengine_protocol::{ChangeId, ChangeRecord, DeviceId, EntryId, FolderId, VersionVector};

use crate::{
    Conflict, ContentObject, FileEntry, FolderPeer, LocalDevice, Peer, PeerSyncState, SharedFolder,
    Tombstone, Transfer,
};

/// Errors surfaced by any [`Store`] implementation.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid data: {0}")]
    Invalid(String),
}

impl StoreError {
    pub fn serialization<E: std::fmt::Display>(err: E) -> Self {
        StoreError::Serialization(err.to_string())
    }
}

/// Local persistence interface used by the sync engine (LLD `Store`).
///
/// All methods are synchronous; the client runs blocking database work on a
/// dedicated thread where needed.
pub trait Store {
    /// --- local device ---
    fn local_device(&self) -> Result<Option<LocalDevice>, StoreError>;
    fn set_local_device(&mut self, device_id: DeviceId, name: &str) -> Result<(), StoreError>;

    /// --- peers ---
    fn upsert_peer(&mut self, peer: &Peer) -> Result<(), StoreError>;
    fn get_peer(&self, device_id: &DeviceId) -> Result<Option<Peer>, StoreError>;
    fn list_peers(&self) -> Result<Vec<Peer>, StoreError>;
    fn delete_peer(&mut self, device_id: &DeviceId) -> Result<(), StoreError>;
    fn set_peer_trust(
        &mut self,
        device_id: &DeviceId,
        trust: crate::TrustState,
    ) -> Result<(), StoreError>;
    fn touch_peer_seen(&mut self, device_id: &DeviceId, seen_at: &str) -> Result<(), StoreError>;

    /// --- shared folders ---
    fn upsert_folder(&mut self, folder: &SharedFolder) -> Result<(), StoreError>;
    fn get_folder(&self, folder_id: &FolderId) -> Result<Option<SharedFolder>, StoreError>;
    fn list_folders(&self) -> Result<Vec<SharedFolder>, StoreError>;
    fn delete_folder(&mut self, folder_id: &FolderId) -> Result<(), StoreError>;
    fn set_folder_peer(&mut self, membership: &FolderPeer) -> Result<(), StoreError>;
    fn list_folder_peers(&self, folder_id: &FolderId) -> Result<Vec<FolderPeer>, StoreError>;
    fn list_peer_folders(&self, peer_id: &DeviceId) -> Result<Vec<FolderPeer>, StoreError>;
    fn remove_folder_peer(
        &mut self,
        folder_id: &FolderId,
        peer_id: &DeviceId,
    ) -> Result<(), StoreError>;

    /// --- file entries ---
    fn upsert_entry(&mut self, entry: &FileEntry) -> Result<(), StoreError>;
    fn get_entry(
        &self,
        folder_id: &FolderId,
        relative_path: &str,
    ) -> Result<Option<FileEntry>, StoreError>;
    fn get_entry_by_id(&self, entry_id: &EntryId) -> Result<Option<FileEntry>, StoreError>;
    fn list_entries(&self, folder_id: &FolderId) -> Result<Vec<FileEntry>, StoreError>;
    fn list_present_entries(&self, folder_id: &FolderId) -> Result<Vec<FileEntry>, StoreError>;

    /// --- change journal ---
    /// Appends `record` and creates one `pending` target row per peer, in a
    /// single transaction (SRD §8.15).
    fn append_change(
        &mut self,
        record: &ChangeRecord,
        peer_ids: &[DeviceId],
    ) -> Result<(), StoreError>;
    fn get_change(&self, change_id: &ChangeId) -> Result<Option<ChangeRecord>, StoreError>;
    /// All journal changes after `after_seq` (ordered by seq).
    fn changes_after(&self, after_seq: i64) -> Result<Vec<(i64, ChangeRecord)>, StoreError>;
    /// Changes whose delivery to `peer_id` is still outstanding (target state
    /// is anything other than `delivered`), ordered by journal sequence.
    fn pending_for_peer(&self, peer_id: &DeviceId) -> Result<Vec<ChangeRecord>, StoreError>;

    /// Creates `pending` targets for every journaled change in `folder_id`
    /// that has no target row for `peer_id` yet (used when a peer starts
    /// sharing a folder it was not paired with when the changes were
    /// journaled). Returns the number of targets created.
    fn ensure_change_targets(
        &mut self,
        peer_id: &DeviceId,
        folder_id: &FolderId,
    ) -> Result<usize, StoreError>;
    fn mark_target(
        &mut self,
        change_id: &ChangeId,
        peer_id: &DeviceId,
        state: crate::TargetState,
        error: Option<&str>,
    ) -> Result<(), StoreError>;
    /// Records that `peer_id` durably applied the change (acknowledgement).
    fn acknowledge_change(
        &mut self,
        change_id: &ChangeId,
        peer_id: &DeviceId,
        at: &str,
    ) -> Result<(), StoreError>;

    /// --- tombstones ---
    fn insert_tombstone(&mut self, tombstone: &Tombstone) -> Result<(), StoreError>;
    fn list_tombstones(&self, folder_id: &FolderId) -> Result<Vec<Tombstone>, StoreError>;

    /// The most recent tombstone for `last_path`, used to detect remote
    /// updates that race a local deletion.
    fn get_tombstone(
        &self,
        folder_id: &FolderId,
        last_path: &str,
    ) -> Result<Option<Tombstone>, StoreError>;

    /// Removes tombstones whose deletion has been acknowledged by every
    /// trusted peer currently sharing the folder (SRD §14 tombstone
    /// retirement). Returns how many were retired.
    fn retire_tombstones(&mut self) -> Result<usize, StoreError>;

    /// --- peer sync state ---
    fn peer_sync_state(
        &self,
        folder_id: &FolderId,
        peer_id: &DeviceId,
    ) -> Result<Option<PeerSyncState>, StoreError>;
    fn set_peer_sync_state(&mut self, state: &PeerSyncState) -> Result<(), StoreError>;

    /// --- transfers / checkpoints ---
    fn upsert_transfer(&mut self, transfer: &Transfer) -> Result<(), StoreError>;
    fn get_transfer(&self, transfer_id: &str) -> Result<Option<Transfer>, StoreError>;
    fn get_transfer_for_change(
        &self,
        change_id: &ChangeId,
        direction: crate::TransferDirection,
    ) -> Result<Option<Transfer>, StoreError>;
    /// Saves a durable checkpoint (contiguous acknowledged bytes).
    fn save_checkpoint(
        &mut self,
        transfer_id: &str,
        acknowledged_bytes: u64,
        state: crate::TransferState,
    ) -> Result<(), StoreError>;

    /// --- idempotent remote application ---
    /// Returns `true` the first time a change id is recorded, `false` if it
    /// was already applied (duplicate/replay).
    fn try_mark_applied(
        &mut self,
        change_id: &ChangeId,
        origin_device: &DeviceId,
        at: &str,
    ) -> Result<bool, StoreError>;
    fn is_applied(&self, change_id: &ChangeId) -> Result<bool, StoreError>;

    /// --- conflicts ---
    fn insert_conflict(&mut self, conflict: &Conflict) -> Result<(), StoreError>;
    fn list_conflicts(
        &self,
        status: Option<crate::ConflictStatus>,
    ) -> Result<Vec<Conflict>, StoreError>;
    fn resolve_conflict(
        &mut self,
        conflict_id: &str,
        status: crate::ConflictStatus,
        resolution: Option<&str>,
        at: &str,
    ) -> Result<(), StoreError>;

    /// --- content staging metadata ---
    fn put_content_object(&mut self, object: &ContentObject) -> Result<(), StoreError>;
    fn get_content_object(
        &self,
        hash: &syncengine_protocol::ContentHash,
    ) -> Result<Option<ContentObject>, StoreError>;
    fn bump_content_refcount(
        &mut self,
        hash: &syncengine_protocol::ContentHash,
        delta: i64,
    ) -> Result<(), StoreError>;
    fn list_content_objects(&self) -> Result<Vec<ContentObject>, StoreError>;

    /// Content hashes that are still queued for at least one peer and must
    /// therefore not be garbage-collected (SRD §14).
    fn pending_content_hashes(&self) -> Result<Vec<syncengine_protocol::ContentHash>, StoreError>;

    /// Removes one content-object record (the caller owns the staged file).
    fn delete_content_object(
        &mut self,
        hash: &syncengine_protocol::ContentHash,
    ) -> Result<(), StoreError>;

    /// Current journal sequence (0 when empty).
    fn max_change_seq(&self) -> Result<i64, StoreError>;
}

/// Convenience: parse a JSON version vector from a database column.
pub(crate) fn decode_version(json: &str) -> Result<VersionVector, StoreError> {
    serde_json::from_str(json).map_err(StoreError::serialization)
}

/// Convenience: encode a version vector for a database column.
pub(crate) fn encode_version(version: &VersionVector) -> Result<String, StoreError> {
    serde_json::to_string(version).map_err(StoreError::serialization)
}
