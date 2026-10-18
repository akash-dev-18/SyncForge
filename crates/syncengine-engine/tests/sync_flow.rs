use std::path::Path;

use syncengine_engine::{ApplyOutcome, ConflictResolution, RemotePlan, StagingStore, SyncEngine};
use syncengine_protocol::{
    ChangeId, ChangeRecord, ContentHash, DeviceId, EntryId, FolderId, Operation, VersionVector,
    now_rfc3339,
};
use syncengine_storage::{
    ConflictStatus, EntryState, FileEntry, FolderPeer, Peer, Permission, SharedFolder, SqliteStore,
    Store, TrustState,
};

fn peer(id: DeviceId, name: &str) -> Peer {
    Peer {
        device_id: id,
        device_name: name.to_string(),
        public_key: "11".repeat(32),
        encryption_key: "22".repeat(32),
        platform: "linux".to_string(),
        trust: TrustState::Trusted,
        last_seen_at: None,
        created_at: now_rfc3339(),
    }
}

fn setup_engine(
    device: DeviceId,
    name: &str,
    folder_id: FolderId,
    root: &Path,
    staging_root: &Path,
) -> SyncEngine<SqliteStore> {
    let mut store = SqliteStore::open_in_memory().expect("open store");
    store.set_local_device(device, name).expect("set device");
    store
        .upsert_folder(&SharedFolder {
            folder_id,
            name: "shared".to_string(),
            local_root: root.to_path_buf(),
            enabled: true,
            created_at: now_rfc3339(),
        })
        .expect("set folder");
    SyncEngine::new(
        device,
        store,
        StagingStore::new(staging_root).expect("staging"),
    )
}

fn register_peer(engine: &mut SyncEngine<SqliteStore>, folder_id: FolderId, peer: Peer) {
    let peer_id = peer.device_id;
    engine.store_mut().upsert_peer(&peer).expect("peer");
    engine
        .store_mut()
        .set_folder_peer(&FolderPeer {
            folder_id,
            peer_id,
            permission: Permission::ReadWrite,
            joined_at: now_rfc3339(),
        })
        .expect("membership");
}

fn write(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn read(root: &Path, rel: &str) -> Option<Vec<u8>> {
    std::fs::read(root.join(rel)).ok()
}

fn push_change(
    from: &SyncEngine<SqliteStore>,
    to: &SyncEngine<SqliteStore>,
    change: &ChangeRecord,
) {
    if let Some(hash) = &change.content_hash {
        let bytes = std::fs::read(from.staging().path_for(hash)).expect("source staged");
        to.staging().stage_bytes(&bytes).expect("stage at receiver");
    }
}

fn pending(engine: &SyncEngine<SqliteStore>, peer_id: &DeviceId) -> Vec<ChangeRecord> {
    engine.store().pending_for_peer(peer_id).expect("pending")
}

/// Simulates the receiver's acknowledgement coming back to the sender, which
/// marks the delivery target `delivered` so it no longer counts as pending.
fn ack(sender: &mut SyncEngine<SqliteStore>, peer_id: &DeviceId, change: &ChangeRecord) {
    sender
        .store_mut()
        .acknowledge_change(&change.change_id, peer_id, &now_rfc3339())
        .expect("acknowledge");
}

fn malicious_record(folder: FolderId, origin: DeviceId, path: &str) -> ChangeRecord {
    ChangeRecord {
        change_id: ChangeId::new(),
        folder_id: folder,
        entry_id: EntryId::new(),
        operation: Operation::Create,
        relative_path: path.to_string(),
        old_relative_path: None,
        content_hash: Some(ContentHash("cafebabe".repeat(4))),
        size: 5,
        version: VersionVector::new(),
        base_version: VersionVector::new(),
        origin_device: origin,
        timestamp: now_rfc3339(),
    }
}

#[test]
fn create_then_apply_files_end_to_end() {
    let folder = FolderId::new();
    let a = DeviceId::new();
    let b = DeviceId::new();

    let tmp = tempfile::tempdir().unwrap();
    let a_root = tmp.path().join("a-root");
    let b_root = tmp.path().join("b-root");

    write(&a_root, "hello.txt", b"hello from A");
    write(&a_root, "docs/note.md", b"note contents");

    let mut engine_a = setup_engine(a, "A", folder, &a_root, &tmp.path().join("a-st"));
    let mut engine_b = setup_engine(b, "B", folder, &b_root, &tmp.path().join("b-st"));
    register_peer(&mut engine_a, folder, peer(b, "B"));

    let report = engine_a.queue_local_changes(&folder).expect("queue A");
    assert_eq!(report.created, 2);
    assert_eq!(report.updated, 0);
    assert_eq!(report.deleted, 0);
    assert!(!report.deletions_skipped);

    let changes = pending(&engine_a, &b);
    assert_eq!(changes.len(), 2);

    for change in &changes {
        push_change(&engine_a, &engine_b, change);
        assert_eq!(
            engine_b.plan_remote_change(change).expect("plan"),
            RemotePlan::ApplyFile
        );
        assert_eq!(
            engine_b.commit_remote_file(change).expect("commit"),
            ApplyOutcome::Applied
        );
    }

    assert_eq!(read(&b_root, "hello.txt").unwrap(), b"hello from A");
    assert_eq!(read(&b_root, "docs/note.md").unwrap(), b"note contents");
    assert_eq!(
        read(&a_root, "hello.txt").unwrap(),
        read(&b_root, "hello.txt").unwrap()
    );
}

#[test]
fn remote_apply_is_idempotent() {
    let folder = FolderId::new();
    let a = DeviceId::new();
    let b = DeviceId::new();

    let tmp = tempfile::tempdir().unwrap();
    let a_root = tmp.path().join("a-root");
    let b_root = tmp.path().join("b-root");
    write(&a_root, "file.txt", b"v1");

    let mut engine_a = setup_engine(a, "A", folder, &a_root, &tmp.path().join("a-st"));
    let mut engine_b = setup_engine(b, "B", folder, &b_root, &tmp.path().join("b-st"));
    register_peer(&mut engine_a, folder, peer(b, "B"));

    engine_a.queue_local_changes(&folder).expect("queue");
    let change = pending(&engine_a, &b).remove(0);
    push_change(&engine_a, &engine_b, &change);

    assert_eq!(
        engine_b.plan_remote_change(&change).unwrap(),
        RemotePlan::ApplyFile
    );
    assert_eq!(
        engine_b.commit_remote_file(&change).unwrap(),
        ApplyOutcome::Applied
    );
    assert_eq!(
        engine_b.plan_remote_change(&change).unwrap(),
        RemotePlan::AlreadyApplied
    );
    assert_eq!(
        engine_b.commit_remote_file(&change).unwrap(),
        ApplyOutcome::AlreadyApplied
    );
    assert_eq!(read(&b_root, "file.txt").unwrap(), b"v1");
}

#[test]
fn updates_and_deletes_propagate() {
    let folder = FolderId::new();
    let a = DeviceId::new();
    let b = DeviceId::new();

    let tmp = tempfile::tempdir().unwrap();
    let a_root = tmp.path().join("a-root");
    let b_root = tmp.path().join("b-root");

    write(&a_root, "doc.txt", b"v1");
    let mut engine_a = setup_engine(a, "A", folder, &a_root, &tmp.path().join("a-st"));
    let mut engine_b = setup_engine(b, "B", folder, &b_root, &tmp.path().join("b-st"));
    register_peer(&mut engine_a, folder, peer(b, "B"));

    engine_a.queue_local_changes(&folder).unwrap();
    let create = pending(&engine_a, &b).remove(0);
    push_change(&engine_a, &engine_b, &create);
    engine_b.plan_remote_change(&create).unwrap();
    engine_b.commit_remote_file(&create).unwrap();
    ack(&mut engine_a, &b, &create);
    assert_eq!(read(&b_root, "doc.txt").unwrap(), b"v1");

    write(&a_root, "doc.txt", b"v2");
    let report = engine_a.queue_local_changes(&folder).unwrap();
    assert_eq!(report.updated, 1);
    let update = pending(&engine_a, &b).remove(0);
    assert_eq!(update.operation, Operation::Update);
    push_change(&engine_a, &engine_b, &update);
    assert_eq!(
        engine_b.plan_remote_change(&update).unwrap(),
        RemotePlan::ApplyFile
    );
    engine_b.commit_remote_file(&update).unwrap();
    ack(&mut engine_a, &b, &update);
    assert_eq!(read(&b_root, "doc.txt").unwrap(), b"v2");

    std::fs::remove_file(a_root.join("doc.txt")).unwrap();
    let report = engine_a.queue_local_changes(&folder).unwrap();
    assert_eq!(report.deleted, 1);
    let delete = pending(&engine_a, &b).remove(0);
    assert_eq!(delete.operation, Operation::Delete);
    assert_eq!(
        engine_b.plan_remote_change(&delete).unwrap(),
        RemotePlan::Delete
    );
    engine_b.commit_remote_delete(&delete).unwrap();
    assert!(read(&b_root, "doc.txt").is_none());
}

#[test]
fn failed_scan_does_not_delete_entries() {
    let folder = FolderId::new();
    let a = DeviceId::new();
    let tmp = tempfile::tempdir().unwrap();
    let a_root = tmp.path().join("a-root");
    write(&a_root, "keep.txt", b"data");

    let mut engine_a = setup_engine(a, "A", folder, &a_root, &tmp.path().join("a-st"));
    engine_a.queue_local_changes(&folder).unwrap();
    assert_eq!(
        engine_a
            .store()
            .list_present_entries(&folder)
            .unwrap()
            .len(),
        1
    );

    // Remove the root: the scan fails outright, so nothing may be deleted.
    std::fs::remove_dir_all(&a_root).unwrap();
    assert!(
        engine_a.queue_local_changes(&folder).is_err(),
        "missing root surfaces an error rather than wiping entries"
    );
    assert_eq!(
        engine_a
            .store()
            .list_present_entries(&folder)
            .unwrap()
            .len(),
        1,
        "the indexed entry survives a failed scan"
    );
}

#[test]
fn stale_remote_change_is_ignored() {
    let folder = FolderId::new();
    let a = DeviceId::new();
    let b = DeviceId::new();
    let tmp = tempfile::tempdir().unwrap();
    let a_root = tmp.path().join("a-root");
    let b_root = tmp.path().join("b-root");

    write(&a_root, "f.txt", b"v1");
    let mut engine_a = setup_engine(a, "A", folder, &a_root, &tmp.path().join("a-st"));
    let mut engine_b = setup_engine(b, "B", folder, &b_root, &tmp.path().join("b-st"));
    register_peer(&mut engine_a, folder, peer(b, "B"));

    engine_a.queue_local_changes(&folder).unwrap();
    let v1_change = pending(&engine_a, &b).remove(0);

    write(&a_root, "f.txt", b"v2");
    let report = engine_a.queue_local_changes(&folder).unwrap();
    assert_eq!(report.updated, 1);

    // Apply the NEWER (v2) change to B first, then replay the older v1.
    let all = pending(&engine_a, &b);
    assert_eq!(all.len(), 2, "both v1 and v2 changes are pending");
    let v2 = all
        .iter()
        .find(|c| c.change_id != v1_change.change_id)
        .expect("v2 present")
        .clone();
    push_change(&engine_a, &engine_b, &v2);
    assert_eq!(
        engine_b.plan_remote_change(&v2).unwrap(),
        RemotePlan::ApplyFile
    );
    engine_b.commit_remote_file(&v2).unwrap();
    assert_eq!(read(&b_root, "f.txt").unwrap(), b"v2");

    // Replaying the now-older v1 change must be ignored as stale.
    assert_eq!(
        engine_b.plan_remote_change(&v1_change).unwrap(),
        RemotePlan::Stale
    );
    assert_eq!(
        read(&b_root, "f.txt").unwrap(),
        b"v2",
        "stale change does not clobber newer content"
    );
}

#[test]
fn concurrent_edits_detect_conflict_and_preserve_copy() {
    let folder = FolderId::new();
    let a = DeviceId::new();
    let b = DeviceId::new();
    let tmp = tempfile::tempdir().unwrap();
    let a_root = tmp.path().join("a-root");
    let b_root = tmp.path().join("b-root");

    write(&a_root, "same.txt", b"base");
    let mut engine_a = setup_engine(a, "A", folder, &a_root, &tmp.path().join("a-st"));
    let mut engine_b = setup_engine(b, "B", folder, &b_root, &tmp.path().join("b-st"));
    register_peer(&mut engine_a, folder, peer(b, "B"));
    register_peer(&mut engine_b, folder, peer(a, "A"));

    engine_a.queue_local_changes(&folder).unwrap();
    let create = pending(&engine_a, &b).remove(0);
    push_change(&engine_a, &engine_b, &create);
    engine_b.plan_remote_change(&create).unwrap();
    engine_b.commit_remote_file(&create).unwrap();
    ack(&mut engine_a, &b, &create);
    assert_eq!(read(&b_root, "same.txt").unwrap(), b"base");

    // Concurrent edits on both sides.
    write(&a_root, "same.txt", b"edit from A");
    write(&b_root, "same.txt", b"edit from B");
    assert_eq!(engine_a.queue_local_changes(&folder).unwrap().updated, 1);
    assert_eq!(engine_b.queue_local_changes(&folder).unwrap().updated, 1);

    let change_a = pending(&engine_a, &b).remove(0);
    push_change(&engine_a, &engine_b, &change_a);

    assert_eq!(
        engine_b.plan_remote_change(&change_a).unwrap(),
        RemotePlan::Conflict
    );
    assert_eq!(read(&b_root, "same.txt").unwrap(), b"edit from B");

    let conflicts = engine_b.store().list_conflicts(None).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].remote_change_id, Some(change_a.change_id));

    let copy_path = engine_b.preserve_conflict_copy(&change_a).unwrap();
    assert_eq!(std::fs::read(&copy_path).unwrap(), b"edit from A");
    let name = copy_path.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.contains("conflict"), "conflict copy name: {name}");

    // The change stays retryable until the copy is actually preserved...
    assert_eq!(
        engine_b.plan_remote_change(&change_a).unwrap(),
        RemotePlan::Conflict
    );
    assert_eq!(engine_b.store().list_conflicts(None).unwrap().len(), 1);

    // ...and only then becomes terminal.
    assert!(engine_b.mark_change_applied(&change_a).unwrap());
    assert_eq!(
        engine_b.plan_remote_change(&change_a).unwrap(),
        RemotePlan::AlreadyApplied
    );
    assert_eq!(engine_b.store().list_conflicts(None).unwrap().len(), 1);
}

#[test]
fn malicious_remote_paths_are_rejected_before_any_write() {
    let folder = FolderId::new();
    let a = DeviceId::new();
    let tmp = tempfile::tempdir().unwrap();
    let a_root = tmp.path().join("a-root");
    let mut engine = setup_engine(a, "A", folder, &a_root, &tmp.path().join("a-st"));

    for bad_path in ["../../escape.txt", "/absolute.txt", "a/../b.txt"] {
        let record = malicious_record(folder, a, bad_path);
        assert_eq!(
            engine.plan_remote_change(&record).unwrap(),
            RemotePlan::ApplyFile,
            "planning does not touch disk for {bad_path}"
        );
        assert!(
            engine.commit_remote_file(&record).is_err(),
            "path {bad_path} must be rejected before any filesystem write"
        );
        assert!(
            !tmp.path().join("escape.txt").exists()
                && !tmp.path().parent().unwrap().join("escape.txt").exists(),
            "no file escaped the shared root"
        );
    }
}

fn setup_pair(tmp: &Path) -> (FolderId, SyncEngine<SqliteStore>, SyncEngine<SqliteStore>) {
    let folder = FolderId::new();
    let a = DeviceId::new();
    let b = DeviceId::new();
    let a_root = tmp.join("a-root");
    let b_root = tmp.join("b-root");
    let mut engine_a = setup_engine(a, "A", folder, &a_root, &tmp.join("a-st"));
    let mut engine_b = setup_engine(b, "B", folder, &b_root, &tmp.join("b-st"));
    register_peer(&mut engine_a, folder, peer(b, "B"));
    register_peer(&mut engine_b, folder, peer(a, "A"));
    (folder, engine_a, engine_b)
}

/// Creates `rel` at A, syncs it to B, and acknowledges it end to end.
fn seed_from_a(
    engine_a: &mut SyncEngine<SqliteStore>,
    engine_b: &mut SyncEngine<SqliteStore>,
    b: &DeviceId,
    rel: &str,
    contents: &[u8],
) {
    let folder = engine_a
        .store()
        .list_folders()
        .unwrap()
        .first()
        .expect("folder")
        .folder_id;
    let root = engine_a
        .store()
        .get_folder(&folder)
        .unwrap()
        .expect("folder")
        .local_root;
    write(&root, rel, contents);
    engine_a.queue_local_changes(&folder).unwrap();
    let change = engine_a
        .store()
        .pending_for_peer(b)
        .unwrap()
        .into_iter()
        .find(|c| c.relative_path == rel)
        .expect("pending change");
    push_change(engine_a, engine_b, &change);
    engine_b.plan_remote_change(&change).unwrap();
    engine_b.commit_remote_file(&change).unwrap();
    ack(engine_a, b, &change);
}

#[test]
fn delete_vs_concurrent_edit_preserves_local_revision_and_applies_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let (folder, mut engine_a, mut engine_b) = setup_pair(tmp.path());
    let b_id = engine_b.device_id();

    seed_from_a(&mut engine_a, &mut engine_b, &b_id, "doc.txt", b"base");

    // B edits while A deletes: both bump their own counter from the same base.
    let b_root = tmp.path().join("b-root");
    let a_root = tmp.path().join("a-root");
    write(&b_root, "doc.txt", b"edit from B");
    assert_eq!(engine_b.queue_local_changes(&folder).unwrap().updated, 1);
    std::fs::remove_file(a_root.join("doc.txt")).unwrap();
    let report = engine_a.queue_local_changes(&folder).unwrap();
    assert_eq!(report.deleted, 1);

    assert_eq!(
        engine_a
            .store()
            .pending_for_peer(&b_id)
            .unwrap()
            .into_iter()
            .map(|c| c.operation)
            .collect::<Vec<_>>(),
        vec![Operation::Delete]
    );
    let delete = engine_a
        .store()
        .pending_for_peer(&b_id)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(
        engine_b.plan_remote_change(&delete).unwrap(),
        RemotePlan::ConflictDelete
    );
    assert_eq!(
        engine_b.commit_conflict_delete(&delete).unwrap(),
        ApplyOutcome::Applied
    );
    assert!(
        read(&b_root, "doc.txt").is_none(),
        "the delete wins the path"
    );

    let conflicts = engine_b.store().list_conflicts(None).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].remote_change_id, Some(delete.change_id));

    let copies: Vec<_> = std::fs::read_dir(&b_root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains("conflict"))
        .collect();
    assert_eq!(copies.len(), 1, "exactly one preserved copy");
    assert_eq!(std::fs::read(copies[0].path()).unwrap(), b"edit from B");
    let name = copies[0].file_name().to_string_lossy().to_string();
    assert!(
        name.contains(&b_id.to_string()),
        "copy is attributed to the preserving device: {name}"
    );
    assert!(
        name.contains("doc.txt"),
        "copy stays next to the original path: {name}"
    );

    // Retrying the same delete is idempotent.
    assert_eq!(
        engine_b.commit_conflict_delete(&delete).unwrap(),
        ApplyOutcome::AlreadyApplied
    );
    assert_eq!(engine_b.store().list_conflicts(None).unwrap().len(), 1);
}

#[test]
fn delete_dominated_by_a_local_edit_is_stale_and_never_touches_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (folder, mut engine_a, mut engine_b) = setup_pair(tmp.path());
    let b_id = engine_b.device_id();

    seed_from_a(&mut engine_a, &mut engine_b, &b_id, "doc.txt", b"base");

    let b_root = tmp.path().join("b-root");
    write(&b_root, "doc.txt", b"newer local edit");
    assert_eq!(engine_b.queue_local_changes(&folder).unwrap().updated, 1);

    // A deletes, but craft a delete of the ORIGINAL revision: B's local edit
    // dominates it, so it must be discarded rather than win or conflict.
    std::fs::remove_file(tmp.path().join("a-root/doc.txt")).unwrap();
    assert_eq!(engine_a.queue_local_changes(&folder).unwrap().deleted, 1);
    let mut stale_delete = engine_a
        .store()
        .pending_for_peer(&b_id)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    stale_delete.version = VersionVector::new();

    assert_eq!(
        engine_b.plan_remote_change(&stale_delete).unwrap(),
        RemotePlan::Stale
    );
    assert_eq!(
        engine_b.plan_remote_change(&stale_delete).unwrap(),
        RemotePlan::AlreadyApplied
    );
    assert_eq!(read(&b_root, "doc.txt").unwrap(), b"newer local edit");
    assert!(engine_b.store().list_conflicts(None).unwrap().is_empty());
}

#[test]
fn concurrent_recreate_after_delete_keeps_the_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let (folder, mut engine_a, mut engine_b) = setup_pair(tmp.path());
    let b_id = engine_b.device_id();

    seed_from_a(&mut engine_a, &mut engine_b, &b_id, "gone.txt", b"base");

    // B deletes the file locally while A edits it.
    let b_root = tmp.path().join("b-root");
    let a_root = tmp.path().join("a-root");
    std::fs::remove_file(b_root.join("gone.txt")).unwrap();
    assert_eq!(engine_b.queue_local_changes(&folder).unwrap().deleted, 1);
    write(&a_root, "gone.txt", b"edit from A");
    assert_eq!(engine_a.queue_local_changes(&folder).unwrap().updated, 1);

    let change = engine_a
        .store()
        .pending_for_peer(&b_id)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(change.operation, Operation::Update);
    push_change(&engine_a, &engine_b, &change);

    assert_eq!(
        engine_b.plan_remote_change(&change).unwrap(),
        RemotePlan::Conflict
    );
    assert!(
        read(&b_root, "gone.txt").is_none(),
        "the local deletion is respected"
    );

    let copy_path = engine_b.preserve_conflict_copy(&change).unwrap();
    assert_eq!(std::fs::read(&copy_path).unwrap(), b"edit from A");
    assert!(engine_b.mark_change_applied(&change).unwrap());
    assert_eq!(
        engine_b.plan_remote_change(&change).unwrap(),
        RemotePlan::AlreadyApplied
    );
}

#[test]
fn crash_after_publication_before_applied_record_replans_as_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let (folder, mut engine_a, mut engine_b) = setup_pair(tmp.path());
    let b_id = engine_b.device_id();

    let a_root = tmp.path().join("a-root");
    write(&a_root, "payload.bin", b"payload bytes");
    engine_a.queue_local_changes(&folder).unwrap();
    let change = engine_a
        .store()
        .pending_for_peer(&b_id)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    // Simulate a crash after the file was published to disk and the entry
    // recorded, but before the `applied_remote_changes` row was committed:
    // only the durable file + entry exist at B.
    let b_root = tmp.path().join("b-root");
    push_change(&engine_a, &engine_b, &change);
    write(&b_root, "payload.bin", b"payload bytes");
    engine_b
        .store_mut()
        .upsert_entry(&FileEntry {
            entry_id: change.entry_id,
            folder_id: folder,
            relative_path: "payload.bin".to_string(),
            state: EntryState::Present,
            content_hash: change.content_hash.clone(),
            size: change.size,
            modified_at: None,
            version: change.version.clone(),
            last_change_id: Some(change.change_id),
            updated_at: now_rfc3339(),
        })
        .unwrap();

    // The redelivered change is recognized as already published.
    assert_eq!(
        engine_b.plan_remote_change(&change).unwrap(),
        RemotePlan::Stale
    );
    assert_eq!(
        engine_b.plan_remote_change(&change).unwrap(),
        RemotePlan::AlreadyApplied
    );
    assert_eq!(read(&b_root, "payload.bin").unwrap(), b"payload bytes");
    assert!(engine_b.store().list_conflicts(None).unwrap().is_empty());
}

#[test]
fn renames_propagate_as_a_conservative_delete_plus_create() {
    let tmp = tempfile::tempdir().unwrap();
    let (folder, mut engine_a, mut engine_b) = setup_pair(tmp.path());
    let b_id = engine_b.device_id();

    seed_from_a(
        &mut engine_a,
        &mut engine_b,
        &b_id,
        "old.txt",
        b"stable content",
    );

    // Rename on A: the scanner cannot correlate reliably across platforms, so
    // the change is journaled conservatively as delete + create (SRD §10).
    let a_root = tmp.path().join("a-root");
    let b_root = tmp.path().join("b-root");
    std::fs::rename(a_root.join("old.txt"), a_root.join("new.txt")).unwrap();
    let report = engine_a.queue_local_changes(&folder).unwrap();
    assert_eq!(report.created, 1);
    assert_eq!(report.deleted, 1);

    let changes = pending(&engine_a, &b_id);
    assert_eq!(changes.len(), 2);
    let create = changes
        .iter()
        .find(|c| c.operation == Operation::Create)
        .expect("create change")
        .clone();
    let delete = changes
        .iter()
        .find(|c| c.operation == Operation::Delete)
        .expect("delete change")
        .clone();
    assert_eq!(create.relative_path, "new.txt");
    assert_eq!(delete.relative_path, "old.txt");
    assert!(
        delete.content_hash.is_none(),
        "deletes carry no payload, so the pair stays minimal"
    );
    assert!(create.content_hash.is_some());

    for change in [&create, &delete] {
        push_change(&engine_a, &engine_b, change);
        match change.operation {
            Operation::Delete => {
                assert_eq!(
                    engine_b.plan_remote_change(change).unwrap(),
                    RemotePlan::Delete
                );
                engine_b.commit_remote_delete(change).unwrap();
            }
            _ => {
                assert_eq!(
                    engine_b.plan_remote_change(change).unwrap(),
                    RemotePlan::ApplyFile
                );
                engine_b.commit_remote_file(change).unwrap();
            }
        }
        ack(&mut engine_a, &b_id, change);
    }

    assert!(
        read(&b_root, "old.txt").is_none(),
        "the old path disappears"
    );
    assert_eq!(read(&b_root, "new.txt").unwrap(), b"stable content");
    assert!(engine_b.store().list_conflicts(None).unwrap().is_empty());
}

#[test]
fn gc_staging_keeps_content_referenced_or_awaiting_delivery() {
    let tmp = tempfile::tempdir().unwrap();
    let (folder, mut engine_a, engine_b) = setup_pair(tmp.path());
    let b_id = engine_b.device_id();
    let a_root = tmp.path().join("a-root");

    write(&a_root, "file.txt", b"v1");
    engine_a.queue_local_changes(&folder).unwrap();
    let first = engine_a
        .store()
        .pending_for_peer(&b_id)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let first_hash = first.content_hash.clone().unwrap();

    write(&a_root, "file.txt", b"v2");
    assert_eq!(engine_a.queue_local_changes(&folder).unwrap().updated, 1);
    let second = engine_a
        .store()
        .pending_for_peer(&b_id)
        .unwrap()
        .into_iter()
        .find(|c| c.change_id != first.change_id)
        .expect("v2 change");
    let second_hash = second.content_hash.clone().unwrap();
    assert_ne!(first_hash, second_hash);

    // v1 is superseded but still pending for B: it must survive.
    let gc = engine_a.gc_staging().unwrap();
    assert_eq!(gc.removed, 0);
    assert!(engine_a.staging().contains(&first_hash));
    assert!(engine_a.staging().contains(&second_hash));

    // Once B has acknowledged v1, only the current revision remains.
    ack(&mut engine_a, &b_id, &first);
    let gc = engine_a.gc_staging().unwrap();
    assert_eq!(gc.removed, 1);
    assert!(gc.bytes > 0);
    assert!(!engine_a.staging().contains(&first_hash));
    assert!(engine_a.staging().contains(&second_hash));
}

#[test]
fn resolve_conflict_keep_remote_restores_the_remote_revision_and_rejournals() {
    let tmp = tempfile::tempdir().unwrap();
    let (folder, mut engine_a, mut engine_b) = setup_pair(tmp.path());
    let a_id = engine_a.device_id();
    let b_id = engine_b.device_id();

    seed_from_a(&mut engine_a, &mut engine_b, &b_id, "same.txt", b"base");

    let a_root = tmp.path().join("a-root");
    let b_root = tmp.path().join("b-root");
    write(&a_root, "same.txt", b"edit from A");
    write(&b_root, "same.txt", b"edit from B");
    engine_a.queue_local_changes(&folder).unwrap();
    engine_b.queue_local_changes(&folder).unwrap();

    let change = engine_a
        .store()
        .pending_for_peer(&b_id)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    push_change(&engine_a, &engine_b, &change);
    assert_eq!(
        engine_b.plan_remote_change(&change).unwrap(),
        RemotePlan::Conflict
    );
    engine_b.preserve_conflict_copy(&change).unwrap();
    assert!(engine_b.mark_change_applied(&change).unwrap());

    let conflict = engine_b.store().list_conflicts(None).unwrap().remove(0);
    let pending_before = engine_b.store().pending_for_peer(&a_id).unwrap().len();
    let outcome = engine_b
        .resolve_conflict(&conflict.conflict_id, ConflictResolution::KeepRemote)
        .unwrap();
    assert_eq!(outcome.status, ConflictStatus::Resolved);
    assert_eq!(outcome.relative_path.as_deref(), Some("same.txt"));
    assert_eq!(read(&b_root, "same.txt").unwrap(), b"edit from A");

    // The resolution is journaled so it propagates back to A.
    let pending_for_a = engine_b.store().pending_for_peer(&a_id).unwrap();
    assert!(
        pending_for_a.len() > pending_before,
        "resolution is queued for peers"
    );
    assert!(
        pending_for_a
            .iter()
            .any(|c| c.relative_path == "same.txt" && c.operation == Operation::Update),
        "the journaled resolution updates the shared path"
    );

    let after = engine_b
        .store()
        .list_conflicts(Some(ConflictStatus::Resolved))
        .unwrap();
    assert_eq!(after.len(), 1);
}

#[test]
fn resolve_conflict_keep_local_and_dismiss_only_update_state() {
    let tmp = tempfile::tempdir().unwrap();
    let (folder, mut engine_a, mut engine_b) = setup_pair(tmp.path());
    let b_id = engine_b.device_id();

    seed_from_a(&mut engine_a, &mut engine_b, &b_id, "same.txt", b"base");

    let a_root = tmp.path().join("a-root");
    let b_root = tmp.path().join("b-root");

    let conflict_id = |engine_b: &mut SyncEngine<SqliteStore>| -> String {
        engine_b
            .store()
            .list_conflicts(Some(ConflictStatus::Unresolved))
            .unwrap()
            .first()
            .expect("unresolved conflict")
            .conflict_id
            .clone()
    };

    // First conflict: keep the local revision in place.
    write(&a_root, "same.txt", b"edit 1 from A");
    write(&b_root, "same.txt", b"edit 1 from B");
    engine_a.queue_local_changes(&folder).unwrap();
    engine_b.queue_local_changes(&folder).unwrap();
    let change = engine_a
        .store()
        .pending_for_peer(&b_id)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    push_change(&engine_a, &engine_b, &change);
    assert_eq!(
        engine_b.plan_remote_change(&change).unwrap(),
        RemotePlan::Conflict
    );
    engine_b.preserve_conflict_copy(&change).unwrap();
    engine_b.mark_change_applied(&change).unwrap();
    ack(&mut engine_a, &b_id, &change);
    let first_id = conflict_id(&mut engine_b);
    let pending_before = engine_b
        .store()
        .pending_for_peer(&engine_a.device_id())
        .unwrap()
        .len();
    let outcome = engine_b
        .resolve_conflict(&first_id, ConflictResolution::KeepLocal)
        .unwrap();
    assert_eq!(outcome.status, ConflictStatus::Resolved);
    assert_eq!(read(&b_root, "same.txt").unwrap(), b"edit 1 from B");
    let pending_for_a = engine_b
        .store()
        .pending_for_peer(&engine_a.device_id())
        .unwrap();
    assert_eq!(
        pending_for_a.len(),
        pending_before,
        "dismissal-style resolutions do not journal new changes"
    );

    // Second conflict: dismiss it.
    write(&a_root, "same.txt", b"edit 2 from A");
    write(&b_root, "same.txt", b"edit 2 from B");
    engine_a.queue_local_changes(&folder).unwrap();
    engine_b.queue_local_changes(&folder).unwrap();
    let change = engine_a
        .store()
        .pending_for_peer(&b_id)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    push_change(&engine_a, &engine_b, &change);
    assert_eq!(
        engine_b.plan_remote_change(&change).unwrap(),
        RemotePlan::Conflict
    );
    engine_b.preserve_conflict_copy(&change).unwrap();
    engine_b.mark_change_applied(&change).unwrap();
    ack(&mut engine_a, &b_id, &change);
    let second_id = conflict_id(&mut engine_b);
    let outcome = engine_b
        .resolve_conflict(&second_id, ConflictResolution::Dismiss)
        .unwrap();
    assert_eq!(outcome.status, ConflictStatus::Dismissed);
    assert_eq!(read(&b_root, "same.txt").unwrap(), b"edit 2 from B");
}
