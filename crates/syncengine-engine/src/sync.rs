//! The sync orchestration (LLD `SyncEngine`).
//!
//! [`SyncEngine`] connects the store, staging cache, and the shared-folder
//! roots. It turns local on-disk state into journaled protocol changes and
//! applies remote changes idempotently with version-vector conflict detection
//! (SRD §10, §11).
//!
//! A typical cycle is:
//!
//! 1. `queue_local_changes` — scan a root, stage edits, and journal them.
//! 2. exchange `ChangeRecord`s over a transport (not here).
//! 3. `plan_remote_change` then `commit_remote_file` / `commit_remote_delete`
//!    to apply a received change.
//!
//! `queue_local_changes` must run *before* applying remote changes for a
//! folder so that in-progress local edits are journaled first.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use syncengine_protocol::path::RelativePath;
use syncengine_protocol::{
    ChangeId, ChangeRecord, DeviceId, EntryId, FolderId, Operation, VersionVector, now_rfc3339,
};
use syncengine_storage::{
    Conflict, ConflictStatus, ContentObject, EntryState, FileEntry, Store, Tombstone,
};

use crate::error::EngineError;
use crate::integrity::hash_file;
use crate::scanner::scan_root;
use crate::staging::StagingStore;

/// Summary of one `queue_local_changes` pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct QueueReport {
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    pub unchanged: usize,
    /// `true` when the scan was incomplete and deletions were not processed.
    pub deletions_skipped: bool,
}

/// What to do with a received change, decided without its content bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePlan {
    /// Already applied earlier; nothing to do (idempotency).
    AlreadyApplied,
    /// Remote is causally older than local state; ignore it.
    Stale,
    /// Remote deletes the entry; commit with [`SyncEngine::commit_remote_delete`].
    Delete,
    /// Remote writes file content; stage it then call
    /// [`SyncEngine::commit_remote_file`].
    ApplyFile,
    /// Concurrent edits; local wins but the remote content must be fetched
    /// and preserved with [`SyncEngine::preserve_conflict_copy`], then the
    /// change marked applied with [`SyncEngine::mark_change_applied`].
    Conflict,
    /// A remote delete races a concurrent local edit; the local revision is
    /// preserved as a conflict copy and the delete applied atomically with
    /// [`SyncEngine::commit_conflict_delete`].
    ConflictDelete,
}

/// Result of committing a planned remote change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Stale,
    AlreadyApplied,
    Conflict,
}

/// How to settle an outstanding conflict (SRD §8.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Keep the current shared-path revision; the conflict copy stays on disk.
    KeepLocal,
    /// Overwrite the shared path with the preserved remote revision and
    /// journal the result so it propagates.
    KeepRemote,
    /// Mark the conflict dismissed without touching any files.
    Dismiss,
}

/// Result of resolving a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictOutcome {
    pub conflict_id: String,
    pub status: ConflictStatus,
    /// Shared-folder-relative path involved, when it could be determined.
    pub relative_path: Option<String>,
}

/// Summary of one staging garbage-collection run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GcReport {
    pub removed: usize,
    pub bytes: u64,
}

/// A unique on-disk target for a preserved conflict revision.
fn conflict_copy_target(
    root: &Path,
    relative: &str,
    origin_label: &str,
) -> Result<PathBuf, EngineError> {
    let base = format!("{relative} (conflict from {origin_label})");
    let mut candidate = base.clone();
    let mut counter = 2u32;
    loop {
        let validated = RelativePath::validate(&candidate)
            .map_err(|e| EngineError::Path(format!("conflict name invalid: {e}")))?;
        let target = root.join(validated.as_str());
        if !target.exists() {
            return Ok(target);
        }
        candidate = format!("{base}-{counter}");
        counter += 1;
    }
}

/// The newest on-disk conflict copy preserved next to `relative`, if any.
/// Label-agnostic so resolution does not depend on the origin device.
fn find_conflict_copy(root: &Path, relative: &str) -> Option<PathBuf> {
    let original = Path::new(relative);
    let file_name = original.file_name()?.to_string_lossy().into_owned();
    let marker = format!("{file_name} (conflict from ");
    let dir = root.join(original.parent().unwrap_or_else(|| Path::new("")));
    let mut candidates: Vec<(u32, String, PathBuf)> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rest = name.strip_prefix(&marker)?;
            Some((conflict_copy_rank(rest), name, entry.path()))
        })
        .collect();
    candidates.sort();
    candidates.pop().map(|(_, _, path)| path)
}

/// Orders conflict-copy variants: the unsuffixed name ranks first, then
/// `-2`, `-3`, … so the newest copy always wins.
fn conflict_copy_rank(rest: &str) -> u32 {
    match rest.rsplit_once(")-") {
        Some((_, suffix)) => suffix.parse().unwrap_or(1),
        None => 1,
    }
}

/// Sync orchestration bound to one device, one store, and a staging cache.
pub struct SyncEngine<S: Store> {
    device_id: DeviceId,
    store: S,
    staging: StagingStore,
}

impl<S: Store> SyncEngine<S> {
    pub fn new(device_id: DeviceId, store: S, staging: StagingStore) -> Self {
        Self {
            device_id,
            store,
            staging,
        }
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    pub fn staging(&self) -> &StagingStore {
        &self.staging
    }

    pub fn into_store(self) -> S {
        self.store
    }

    // ---------------------------------------------------------------- local

    /// Scans a folder's root and journals local creates, updates, and
    /// (when the scan is complete) deletes.
    pub fn queue_local_changes(
        &mut self,
        folder_id: &FolderId,
    ) -> Result<QueueReport, EngineError> {
        let now = now_rfc3339();
        let folder = self
            .store
            .get_folder(folder_id)?
            .ok_or_else(|| EngineError::State(format!("folder {folder_id} not found")))?;

        let scan = scan_root(&folder.local_root)?;
        let present = self.store.list_present_entries(folder_id)?;
        let existing: HashMap<String, FileEntry> = present
            .into_iter()
            .map(|e| (e.relative_path.clone(), e))
            .collect();
        let peers: Vec<DeviceId> = self
            .store
            .list_folder_peers(folder_id)?
            .into_iter()
            .map(|m| m.peer_id)
            .collect();

        let mut report = QueueReport::default();

        for file in scan.files.iter() {
            let rel = file.relative_path.as_str();
            match existing.get(rel) {
                None => {
                    let base = VersionVector::new();
                    let version = base.incremented(self.device_id);
                    let record = ChangeRecord {
                        change_id: ChangeId::new(),
                        folder_id: *folder_id,
                        entry_id: EntryId::new(),
                        operation: Operation::Create,
                        relative_path: rel.to_string(),
                        old_relative_path: None,
                        content_hash: Some(file.content_hash.clone()),
                        size: file.size,
                        version,
                        base_version: base,
                        origin_device: self.device_id,
                        timestamp: now_rfc3339(),
                    };
                    self.commit_local_change(
                        &record,
                        &file.absolute_path,
                        file.size,
                        None,
                        &peers,
                    )?;
                    report.created += 1;
                }
                Some(entry) => {
                    if entry.content_hash.as_ref() == Some(&file.content_hash) {
                        report.unchanged += 1;
                        continue;
                    }
                    let base = entry.version.clone();
                    let version = base.incremented(self.device_id);
                    let record = ChangeRecord {
                        change_id: ChangeId::new(),
                        folder_id: *folder_id,
                        entry_id: entry.entry_id,
                        operation: Operation::Update,
                        relative_path: rel.to_string(),
                        old_relative_path: None,
                        content_hash: Some(file.content_hash.clone()),
                        size: file.size,
                        version,
                        base_version: base,
                        origin_device: self.device_id,
                        timestamp: now_rfc3339(),
                    };
                    let old_hash = entry.content_hash.clone();
                    self.commit_local_change(
                        &record,
                        &file.absolute_path,
                        file.size,
                        old_hash.as_ref(),
                        &peers,
                    )?;
                    report.updated += 1;
                }
            }
        }

        if !scan.complete {
            report.deletions_skipped = true;
            return Ok(report);
        }

        let scanned: HashSet<&str> = scan
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        for entry in existing.values() {
            if scanned.contains(entry.relative_path.as_str()) {
                continue;
            }
            let base = entry.version.clone();
            let version = base.incremented(self.device_id);
            let record = ChangeRecord {
                change_id: ChangeId::new(),
                folder_id: *folder_id,
                entry_id: entry.entry_id,
                operation: Operation::Delete,
                relative_path: entry.relative_path.clone(),
                old_relative_path: None,
                content_hash: None,
                size: 0,
                version,
                base_version: base,
                origin_device: self.device_id,
                timestamp: now_rfc3339(),
            };
            self.store.append_change(&record, &peers)?;

            if let Some(hash) = &entry.content_hash {
                self.bump_refcount(hash, -1)?;
            }
            let mut tomb = entry.clone();
            tomb.state = EntryState::Deleted;
            tomb.version = record.version.clone();
            tomb.last_change_id = Some(record.change_id);
            tomb.updated_at = now.clone();
            self.store.upsert_entry(&tomb)?;

            self.store.insert_tombstone(&Tombstone {
                change_id: record.change_id,
                entry_id: entry.entry_id,
                folder_id: *folder_id,
                last_path: entry.relative_path.clone(),
                version: record.version.clone(),
                deleted_at: now.clone(),
            })?;
            report.deleted += 1;
        }

        Ok(report)
    }

    // --------------------------------------------------------------- remote

    /// Decides how a received change applies. Terminal outcomes
    /// (`AlreadyApplied`, `Stale`) are sticky; a `Conflict` stays a
    /// `Conflict` until the content is actually preserved, so an interrupted
    /// session can retry instead of losing the remote revision.
    pub fn plan_remote_change(&mut self, change: &ChangeRecord) -> Result<RemotePlan, EngineError> {
        if self.store.is_applied(&change.change_id)? {
            return Ok(RemotePlan::AlreadyApplied);
        }

        let local_present = self
            .store
            .get_entry(&change.folder_id, &change.relative_path)?;

        match change.operation {
            Operation::Delete => {
                let Some(local) = local_present else {
                    self.mark_change_applied_now(change)?;
                    return Ok(RemotePlan::Stale);
                };
                match change.version.compare(&local.version) {
                    syncengine_protocol::CausalOrder::DominatedBy => {
                        self.mark_change_applied_now(change)?;
                        Ok(RemotePlan::Stale)
                    }
                    syncengine_protocol::CausalOrder::Concurrent => Ok(RemotePlan::ConflictDelete),
                    syncengine_protocol::CausalOrder::Dominates
                    | syncengine_protocol::CausalOrder::Equal => Ok(RemotePlan::Delete),
                }
            }
            Operation::Create | Operation::Update | Operation::Rename => {
                let Some(local) = local_present else {
                    // No present entry: an update racing a local deletion is
                    // decided against the tombstone's version (SRD §10).
                    let Some(tombstone) = self
                        .store
                        .get_tombstone(&change.folder_id, &change.relative_path)?
                    else {
                        return Ok(RemotePlan::ApplyFile);
                    };
                    return match change.version.compare(&tombstone.version) {
                        syncengine_protocol::CausalOrder::Dominates => Ok(RemotePlan::ApplyFile),
                        syncengine_protocol::CausalOrder::Equal
                        | syncengine_protocol::CausalOrder::DominatedBy => {
                            self.mark_change_applied_now(change)?;
                            Ok(RemotePlan::Stale)
                        }
                        syncengine_protocol::CausalOrder::Concurrent => {
                            self.record_conflict(
                                change.folder_id,
                                tombstone.entry_id,
                                Some(tombstone.change_id),
                                change,
                            )?;
                            Ok(RemotePlan::Conflict)
                        }
                    };
                };
                match change.version.compare(&local.version) {
                    syncengine_protocol::CausalOrder::Dominates => Ok(RemotePlan::ApplyFile),
                    syncengine_protocol::CausalOrder::Equal
                    | syncengine_protocol::CausalOrder::DominatedBy => {
                        self.mark_change_applied_now(change)?;
                        Ok(RemotePlan::Stale)
                    }
                    syncengine_protocol::CausalOrder::Concurrent => {
                        self.record_conflict(
                            local.folder_id,
                            local.entry_id,
                            local.last_change_id,
                            change,
                        )?;
                        Ok(RemotePlan::Conflict)
                    }
                }
            }
        }
    }

    /// Marks a received change as durably applied on this device.
    pub fn mark_change_applied(&mut self, change: &ChangeRecord) -> Result<bool, EngineError> {
        Ok(self
            .store
            .try_mark_applied(&change.change_id, &change.origin_device, &now_rfc3339())?)
    }

    fn mark_change_applied_now(&mut self, change: &ChangeRecord) -> Result<(), EngineError> {
        self.mark_change_applied(change)?;
        Ok(())
    }

    /// Applies a remote `create`/`update`/`rename`. Requires the change's
    /// content to already be staged under its hash.
    pub fn commit_remote_file(
        &mut self,
        change: &ChangeRecord,
    ) -> Result<ApplyOutcome, EngineError> {
        let now = now_rfc3339();
        if self.store.is_applied(&change.change_id)? {
            return Ok(ApplyOutcome::AlreadyApplied);
        }
        let folder = self
            .store
            .get_folder(&change.folder_id)?
            .ok_or_else(|| EngineError::State(format!("folder {} not found", change.folder_id)))?;
        let root = folder.local_root.clone();

        let relative = RelativePath::validate(&change.relative_path)
            .map_err(|e| EngineError::Path(e.to_string()))?;
        let path = relative.as_str().to_string();

        if change.operation == Operation::Rename {
            if let Some(old) = change.old_relative_path.as_ref() {
                self.remove_path(&root, old)?;
            }
        }

        let hash = change
            .content_hash
            .as_ref()
            .ok_or_else(|| EngineError::State("change has no content hash".into()))?;
        let staged_path = self.ensure_staged(hash, change.size)?;

        if let Some(local) = self.store.get_entry(&change.folder_id, &path)? {
            if local.entry_id != change.entry_id {
                if let Some(old_hash) = &local.content_hash {
                    self.bump_refcount(old_hash, -1)?;
                }
                let mut superseded = local.clone();
                superseded.state = EntryState::Deleted;
                superseded.updated_at = now.clone();
                self.store.upsert_entry(&superseded)?;
            }
        }

        self.write_atomically(&root, &path, &staged_path)?;

        let entry = FileEntry {
            entry_id: change.entry_id,
            folder_id: change.folder_id,
            relative_path: path.clone(),
            state: EntryState::Present,
            content_hash: Some(hash.clone()),
            size: change.size,
            modified_at: None,
            version: change.version.clone(),
            last_change_id: Some(change.change_id),
            updated_at: now.clone(),
        };
        self.store.upsert_entry(&entry)?;
        self.note_content(hash, change.size, &staged_path.to_string_lossy())?;
        self.store
            .try_mark_applied(&change.change_id, &change.origin_device, &now)?;
        Ok(ApplyOutcome::Applied)
    }

    /// Applies a remote delete: removes the file, tombstones it, and marks the
    /// change applied.
    pub fn commit_remote_delete(
        &mut self,
        change: &ChangeRecord,
    ) -> Result<ApplyOutcome, EngineError> {
        let now = now_rfc3339();
        if self.store.is_applied(&change.change_id)? {
            return Ok(ApplyOutcome::AlreadyApplied);
        }
        let folder = self
            .store
            .get_folder(&change.folder_id)?
            .ok_or_else(|| EngineError::State(format!("folder {} not found", change.folder_id)))?;
        let root = folder.local_root.clone();

        let relative = RelativePath::validate(&change.relative_path)
            .map_err(|e| EngineError::Path(e.to_string()))?;

        if let Some(local) = self.store.get_entry(&change.folder_id, relative.as_str())? {
            if let Some(hash) = &local.content_hash {
                self.bump_refcount(hash, -1)?;
            }
            let mut deleted = local.clone();
            deleted.state = EntryState::Deleted;
            deleted.version = change.version.clone();
            deleted.last_change_id = Some(change.change_id);
            deleted.updated_at = now.clone();
            self.store.upsert_entry(&deleted)?;
        }

        let target = root.join(relative.as_str());
        if target.is_file() {
            std::fs::remove_file(&target)?;
        }

        self.store.insert_tombstone(&Tombstone {
            change_id: change.change_id,
            entry_id: change.entry_id,
            folder_id: change.folder_id,
            last_path: relative.as_str().to_string(),
            version: change.version.clone(),
            deleted_at: now.clone(),
        })?;
        self.store
            .try_mark_applied(&change.change_id, &change.origin_device, &now)?;
        Ok(ApplyOutcome::Applied)
    }

    /// Writes the remote content of a conflicting change to a sibling conflict
    /// file so no data is lost.
    pub fn preserve_conflict_copy(&self, change: &ChangeRecord) -> Result<PathBuf, EngineError> {
        let folder = self
            .store
            .get_folder(&change.folder_id)?
            .ok_or_else(|| EngineError::State(format!("folder {} not found", change.folder_id)))?;
        let relative = RelativePath::validate(&change.relative_path)
            .map_err(|e| EngineError::Path(e.to_string()))?;
        let hash = change
            .content_hash
            .as_ref()
            .ok_or_else(|| EngineError::State("conflict change has no content".into()))?;
        let staged_path = self.ensure_staged(hash, change.size)?;

        let target = conflict_copy_target(
            &folder.local_root,
            relative.as_str(),
            &change.origin_device.to_string(),
        )?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&staged_path, &target)?;
        Ok(target)
    }

    /// Preserves the local revision of an entry as a conflict copy before a
    /// concurrent remote delete removes it. Prefers the staged snapshot (the
    /// revision as journaled) and falls back to the live file.
    pub fn preserve_local_conflict_copy(
        &self,
        change: &ChangeRecord,
    ) -> Result<PathBuf, EngineError> {
        let folder = self
            .store
            .get_folder(&change.folder_id)?
            .ok_or_else(|| EngineError::State(format!("folder {} not found", change.folder_id)))?;
        let relative = RelativePath::validate(&change.relative_path)
            .map_err(|e| EngineError::Path(e.to_string()))?;
        let local = self
            .store
            .get_entry(&change.folder_id, relative.as_str())?
            .ok_or_else(|| EngineError::State("no local entry to preserve".into()))?;

        let live = folder.local_root.join(relative.as_str());
        let staged_path = match &local.content_hash {
            Some(hash) if self.staging.contains(hash) => self.staging.path_for(hash),
            _ if live.is_file() => self.staging.stage_file(&live)?.path,
            _ => return Err(EngineError::State("local revision is gone".into())),
        };

        let target = conflict_copy_target(
            &folder.local_root,
            relative.as_str(),
            &self.device_id.to_string(),
        )?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&staged_path, &target)?;
        Ok(target)
    }

    /// Atomically resolves a remote delete that races a concurrent local edit:
    /// the local revision is preserved as a conflict copy, the conflict is
    /// recorded, and the delete is applied. Safe to retry.
    pub fn commit_conflict_delete(
        &mut self,
        change: &ChangeRecord,
    ) -> Result<ApplyOutcome, EngineError> {
        if self.store.is_applied(&change.change_id)? {
            return Ok(ApplyOutcome::AlreadyApplied);
        }
        self.preserve_local_conflict_copy(change)?;
        if let Some(local) = self
            .store
            .get_entry(&change.folder_id, &change.relative_path)?
        {
            self.record_conflict(
                local.folder_id,
                local.entry_id,
                local.last_change_id,
                change,
            )?;
        }
        self.commit_remote_delete(change)
    }

    /// Resolves an outstanding conflict from the CLI.
    ///
    /// `KeepRemote` copies the preserved remote revision back over the shared
    /// path and journals the result, so the resolution also propagates to
    /// peers on the next pass.
    pub fn resolve_conflict(
        &mut self,
        conflict_id: &str,
        resolution: ConflictResolution,
    ) -> Result<ConflictOutcome, EngineError> {
        let conflict = self
            .store
            .list_conflicts(None)?
            .into_iter()
            .find(|c| c.conflict_id == conflict_id)
            .ok_or_else(|| EngineError::State(format!("conflict {conflict_id} not found")))?;

        let target_path = match resolution {
            ConflictResolution::KeepRemote => Some(self.keep_remote(&conflict)?),
            ConflictResolution::KeepLocal => Some(self.conflict_relative_path(&conflict)?),
            ConflictResolution::Dismiss => None,
        };

        let (status, note) = match resolution {
            ConflictResolution::KeepRemote => (
                ConflictStatus::Resolved,
                "kept the remote revision (since overwritten locally)",
            ),
            ConflictResolution::KeepLocal => (
                ConflictStatus::Resolved,
                "kept the local revision (conflict copy left on disk)",
            ),
            ConflictResolution::Dismiss => (ConflictStatus::Dismissed, "dismissed"),
        };
        self.store
            .resolve_conflict(conflict_id, status, Some(note), &now_rfc3339())?;

        if resolution == ConflictResolution::KeepRemote {
            let folder_id = conflict.folder_id;
            self.queue_local_changes(&folder_id)?;
        }

        Ok(ConflictOutcome {
            conflict_id: conflict_id.to_string(),
            status,
            relative_path: target_path,
        })
    }

    fn conflict_relative_path(&self, conflict: &Conflict) -> Result<String, EngineError> {
        Ok(self
            .store
            .get_entry_by_id(&conflict.entry_id)?
            .map(|entry| entry.relative_path)
            .unwrap_or_else(|| "<unknown>".to_string()))
    }

    fn keep_remote(&mut self, conflict: &Conflict) -> Result<String, EngineError> {
        let entry = self
            .store
            .get_entry_by_id(&conflict.entry_id)?
            .ok_or_else(|| EngineError::State("the conflicted entry no longer exists".into()))?;
        let folder = self
            .store
            .get_folder(&conflict.folder_id)?
            .ok_or_else(|| EngineError::State("folder not found".into()))?;
        let relative = RelativePath::validate(&entry.relative_path)
            .map_err(|e| EngineError::Path(e.to_string()))?;

        let target =
            find_conflict_copy(&folder.local_root, relative.as_str()).ok_or_else(|| {
                EngineError::State(format!("no conflict copy of {} exists", relative.as_str()))
            })?;
        let main = folder.local_root.join(relative.as_str());
        if let Some(parent) = main.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&target, &main)?;
        Ok(relative.as_str().to_string())
    }

    /// Retires tombstones whose deletion every trusted folder peer has
    /// acknowledged (SRD §14).
    pub fn retire_acknowledged_tombstones(&mut self) -> Result<usize, EngineError> {
        Ok(self.store.retire_tombstones()?)
    }

    /// Removes staged content objects that are no longer referenced by any
    /// present entry or pending transfer (SRD §14).
    pub fn gc_staging(&mut self) -> Result<GcReport, EngineError> {
        let pending: HashSet<String> = self
            .store
            .pending_content_hashes()?
            .into_iter()
            .map(|hash| hash.as_str().to_string())
            .collect();
        let mut report = GcReport::default();
        for object in self.store.list_content_objects()? {
            if object.refcount > 0 || pending.contains(object.content_hash.as_str()) {
                continue;
            }
            let path = PathBuf::from(&object.staged_path);
            if let Ok(metadata) = std::fs::metadata(&path) {
                report.bytes += metadata.len();
            }
            let _ = std::fs::remove_file(&path);
            self.store.delete_content_object(&object.content_hash)?;
            report.removed += 1;
        }
        Ok(report)
    }

    // -------------------------------------------------------------- helpers

    fn commit_local_change(
        &mut self,
        record: &ChangeRecord,
        source: &Path,
        size: u64,
        old_hash: Option<&syncengine_protocol::ContentHash>,
        peers: &[DeviceId],
    ) -> Result<(), EngineError> {
        let staged = self.staging.stage_file(source)?;
        self.store.append_change(record, peers)?;

        let hash = record
            .content_hash
            .as_ref()
            .expect("create/update records carry a content hash");
        if let Some(old) = old_hash {
            if old != hash {
                self.bump_refcount(old, -1)?;
            }
        }
        self.note_content(hash, size, &staged.path.to_string_lossy())?;

        let entry = FileEntry {
            entry_id: record.entry_id,
            folder_id: record.folder_id,
            relative_path: record.relative_path.clone(),
            state: EntryState::Present,
            content_hash: Some(hash.clone()),
            size,
            modified_at: None,
            version: record.version.clone(),
            last_change_id: Some(record.change_id),
            updated_at: now_rfc3339(),
        };
        self.store.upsert_entry(&entry)?;
        Ok(())
    }

    fn record_conflict(
        &mut self,
        folder_id: FolderId,
        entry_id: EntryId,
        local_change_id: Option<ChangeId>,
        change: &ChangeRecord,
    ) -> Result<(), EngineError> {
        let already_recorded = self
            .store
            .list_conflicts(Some(ConflictStatus::Unresolved))?
            .iter()
            .any(|existing| existing.remote_change_id == Some(change.change_id));
        if already_recorded {
            return Ok(());
        }
        Ok(self.store.insert_conflict(&Conflict {
            conflict_id: format!("cf-{}", ChangeId::new()),
            folder_id,
            entry_id,
            local_change_id,
            remote_change_id: Some(change.change_id),
            status: ConflictStatus::Unresolved,
            resolution: None,
            created_at: now_rfc3339(),
            resolved_at: None,
        })?)
    }

    fn ensure_staged(
        &self,
        hash: &syncengine_protocol::ContentHash,
        size: u64,
    ) -> Result<PathBuf, EngineError> {
        if self.staging.ensure(hash, size)? {
            Ok(self.staging.path_for(hash))
        } else {
            Err(EngineError::State(format!(
                "content {} is not staged",
                hash.as_str()
            )))
        }
    }

    fn note_content(
        &mut self,
        hash: &syncengine_protocol::ContentHash,
        size: u64,
        staged_path: &str,
    ) -> Result<(), EngineError> {
        if self.store.get_content_object(hash)?.is_some() {
            self.store.bump_content_refcount(hash, 1)?;
        } else {
            self.store.put_content_object(&ContentObject {
                content_hash: hash.clone(),
                size,
                staged_path: staged_path.to_string(),
                refcount: 1,
                created_at: now_rfc3339(),
                verified_at: Some(now_rfc3339()),
            })?;
        }
        Ok(())
    }

    fn bump_refcount(
        &mut self,
        hash: &syncengine_protocol::ContentHash,
        delta: i64,
    ) -> Result<(), EngineError> {
        self.store.bump_content_refcount(hash, delta)?;
        Ok(())
    }

    fn remove_path(&self, root: &Path, relative: &str) -> Result<(), EngineError> {
        let relative = RelativePath::validate(relative)
            .map_err(|e| EngineError::Path(format!("old path invalid: {e}")))?;
        let target = root.join(relative.as_str());
        if target.is_file() {
            std::fs::remove_file(&target)?;
        }
        Ok(())
    }

    /// Copies staged bytes into place via a same-directory temp file, `fsync`,
    /// and `rename` so a crash never leaves a torn file (SRD FR-14).
    fn write_atomically(
        &self,
        root: &Path,
        relative: &str,
        staged_path: &Path,
    ) -> Result<(), EngineError> {
        let relative =
            RelativePath::validate(relative).map_err(|e| EngineError::Path(e.to_string()))?;
        let target = root.join(relative.as_str());
        let parent = target
            .parent()
            .ok_or_else(|| EngineError::Path(format!("{} has no parent", relative.as_str())))?;
        std::fs::create_dir_all(parent)?;

        let tmp = parent.join(format!(
            ".{}.tmp-{:016x}",
            relative.as_str().replace('/', "_"),
            rand::random::<u64>()
        ));
        std::fs::copy(staged_path, &tmp)?;
        let handle = std::fs::File::open(&tmp)?;
        handle.sync_all()?;
        drop(handle);
        std::fs::rename(&tmp, &target)?;
        Ok(())
    }

    /// Re-verifies the staged bytes on disk for a hash against `expected_size`
    /// using the file's BLAKE3, without touching the store.
    pub fn verify_staged(
        &self,
        hash: &syncengine_protocol::ContentHash,
        expected_size: u64,
    ) -> Result<bool, EngineError> {
        let path = self.staging.path_for(hash);
        if !path.is_file() {
            return Ok(false);
        }
        if std::fs::metadata(&path)?.len() != expected_size {
            return Ok(false);
        }
        Ok(&hash_file(&path)? == hash)
    }
}
