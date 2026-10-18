use syncengine_protocol::{
    ChangeId, ChangeRecord, ContentHash, DeviceId, EntryId, FolderId, Operation, VersionVector,
};
use syncengine_storage::{
    Conflict, ConflictStatus, ContentObject, EntryState, FileEntry, FolderPeer, Peer,
    PeerSyncState, Permission, SharedFolder, SqliteStore, Store, TargetState, Tombstone, Transfer,
    TransferDirection, TransferState, TrustState,
};

fn device() -> DeviceId {
    DeviceId::new()
}

fn folder() -> FolderId {
    FolderId::new()
}

fn now() -> String {
    "2026-01-01T00:00:00Z".to_string()
}

fn store() -> SqliteStore {
    SqliteStore::open_in_memory().expect("open in-memory store")
}

fn peer(id: DeviceId) -> Peer {
    Peer {
        device_id: id,
        device_name: "peer".to_string(),
        public_key: "aa".repeat(32),
        encryption_key: "bb".repeat(32),
        platform: "linux".to_string(),
        trust: TrustState::Trusted,
        last_seen_at: None,
        created_at: now(),
    }
}

fn folder_record(id: FolderId, root: &str) -> SharedFolder {
    SharedFolder {
        folder_id: id,
        name: "docs".to_string(),
        local_root: root.into(),
        enabled: true,
        created_at: now(),
    }
}

fn entry(
    folder_id: FolderId,
    path: &str,
    state: EntryState,
    base: &VersionVector,
    origin: DeviceId,
) -> FileEntry {
    FileEntry {
        entry_id: EntryId::new(),
        folder_id,
        relative_path: path.to_string(),
        state,
        content_hash: Some(ContentHash("c0ffee".to_string())),
        size: 42,
        modified_at: None,
        version: base.incremented(origin),
        last_change_id: None,
        updated_at: now(),
    }
}

fn change(
    folder_id: FolderId,
    entry_id: EntryId,
    origin: DeviceId,
    version: VersionVector,
) -> ChangeRecord {
    ChangeRecord {
        change_id: ChangeId::new(),
        folder_id,
        entry_id,
        operation: Operation::Create,
        relative_path: "a.txt".to_string(),
        old_relative_path: None,
        content_hash: Some(ContentHash("c0ffee".to_string())),
        size: 42,
        version,
        base_version: VersionVector::new(),
        origin_device: origin,
        timestamp: now(),
    }
}

#[test]
fn local_device_is_singleton_and_round_trips() {
    let mut s = store();
    assert!(s.local_device().unwrap().is_none());
    let id = device();
    s.set_local_device(id, "my-laptop").unwrap();
    let got = s.local_device().unwrap().unwrap();
    assert_eq!(got.device_id, id);
    assert_eq!(got.device_name, "my-laptop");

    s.set_local_device(id, "renamed").unwrap();
    assert_eq!(s.local_device().unwrap().unwrap().device_name, "renamed");
}

#[test]
fn peers_upsert_and_trust_transitions() {
    let mut s = store();
    let id = device();
    let mut p = peer(id);
    p.trust = TrustState::Pending;
    s.upsert_peer(&p).unwrap();

    let mut p2 = p.clone();
    p2.device_name = "renamed".to_string();
    s.upsert_peer(&p2).unwrap();
    let got = s.get_peer(&id).unwrap().unwrap();
    assert_eq!(got.device_name, "renamed");

    s.set_peer_trust(&id, TrustState::Trusted).unwrap();
    assert_eq!(s.get_peer(&id).unwrap().unwrap().trust, TrustState::Trusted);
    assert_eq!(s.list_peers().unwrap().len(), 1);

    s.touch_peer_seen(&id, "2026-02-02T00:00:00Z").unwrap();
    assert_eq!(
        s.get_peer(&id).unwrap().unwrap().last_seen_at.as_deref(),
        Some("2026-02-02T00:00:00Z")
    );

    assert!(matches!(
        s.set_peer_trust(&device(), TrustState::Trusted),
        Err(syncengine_storage::StoreError::NotFound(_))
    ));
}

#[test]
fn delete_peer_removes_the_peer_and_its_folder_memberships() {
    let mut s = store();
    let f = folder();
    let id = device();
    s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();
    s.upsert_peer(&peer(id)).unwrap();
    s.set_folder_peer(&FolderPeer {
        folder_id: f,
        peer_id: id,
        permission: Permission::ReadWrite,
        joined_at: now(),
    })
    .unwrap();

    s.delete_peer(&id).unwrap();
    assert!(s.get_peer(&id).unwrap().is_none());
    assert!(s.list_peer_folders(&id).unwrap().is_empty());
    assert!(s.list_folder_peers(&f).unwrap().is_empty());
    s.delete_peer(&id)
        .expect("deleting an absent peer is a no-op");
}

#[test]
fn folder_membership_lists_both_directions() {
    let mut s = store();
    let f = folder();
    s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();
    let peer_a = device();
    let peer_b = device();
    s.upsert_peer(&peer(peer_a)).unwrap();
    s.upsert_peer(&peer(peer_b)).unwrap();

    s.set_folder_peer(&FolderPeer {
        folder_id: f,
        peer_id: peer_a,
        permission: Permission::ReadWrite,
        joined_at: now(),
    })
    .unwrap();
    s.set_folder_peer(&FolderPeer {
        folder_id: f,
        peer_id: peer_b,
        permission: Permission::ReadOnly,
        joined_at: now(),
    })
    .unwrap();

    assert_eq!(s.list_folder_peers(&f).unwrap().len(), 2);
    assert_eq!(s.list_peer_folders(&peer_a).unwrap().len(), 1);

    s.remove_folder_peer(&f, &peer_a).unwrap();
    assert_eq!(s.list_folder_peers(&f).unwrap().len(), 1);

    s.delete_folder(&f).unwrap();
    assert!(s.get_folder(&f).unwrap().is_none());
    assert!(s.list_folder_peers(&f).unwrap().is_empty());
}

#[test]
fn folder_peer_with_unknown_folder_violates_foreign_key() {
    let mut s = store();
    let err = s.set_folder_peer(&FolderPeer {
        folder_id: folder(),
        peer_id: device(),
        permission: Permission::ReadWrite,
        joined_at: now(),
    });
    assert!(err.is_err(), "expected foreign-key violation");
}

#[test]
fn only_one_present_entry_per_path_but_deleted_is_allowed() {
    let mut s = store();
    let f = folder();
    let origin = device();
    s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();
    let base = VersionVector::new();

    let h1 = entry(f, "same.txt", EntryState::Present, &base, origin);
    s.upsert_entry(&h1).unwrap();

    let h2 = entry(f, "same.txt", EntryState::Present, &base, origin);
    let err = s.upsert_entry(&h2);
    assert!(err.is_err(), "second present entry at same path must fail");

    let deleted = entry(f, "same.txt", EntryState::Deleted, &base, origin);
    s.upsert_entry(&deleted).unwrap();
    assert_eq!(s.list_entries(&f).unwrap().len(), 2);
    assert_eq!(s.list_present_entries(&f).unwrap().len(), 1);

    let got = s.get_entry(&f, "same.txt").unwrap().unwrap();
    assert_eq!(got.entry_id, h1.entry_id);
    assert!(s.get_entry_by_id(&h1.entry_id).unwrap().is_some());
}

#[test]
fn append_change_creates_pending_targets_atomically() {
    let mut s = store();
    let f = folder();
    let origin = device();
    let peer_a = device();
    let peer_b = device();
    s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();
    s.upsert_peer(&peer(peer_a)).unwrap();
    s.upsert_peer(&peer(peer_b)).unwrap();

    let id = EntryId::new();
    let rec = change(f, id, origin, VersionVector::new().incremented(origin));
    s.append_change(&rec, &[peer_a, peer_b]).unwrap();

    assert_eq!(s.max_change_seq().unwrap(), 1);
    assert_eq!(s.pending_for_peer(&peer_a).unwrap().len(), 1);
    assert_eq!(s.pending_for_peer(&peer_b).unwrap().len(), 1);
    let fetched = s.get_change(&rec.change_id).unwrap().unwrap();
    assert_eq!(fetched, rec);

    let after = s.changes_after(0).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].0, 1);
    assert_eq!(after[0].1, rec);
    assert!(s.changes_after(1).unwrap().is_empty());
}

#[test]
fn append_change_rolls_back_when_duplicate_change_id() {
    let mut s = store();
    let f = folder();
    let origin = device();
    let peer_a = device();
    s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();
    s.upsert_peer(&peer(peer_a)).unwrap();

    let rec = change(
        f,
        EntryId::new(),
        origin,
        VersionVector::new().incremented(origin),
    );
    s.append_change(&rec, &[peer_a]).unwrap();
    let err = s.append_change(&rec, &[peer_a]);
    assert!(err.is_err(), "duplicate change id must be rejected");
    assert_eq!(s.max_change_seq().unwrap(), 1);
    assert_eq!(s.pending_for_peer(&peer_a).unwrap().len(), 1);
}

#[test]
fn target_lifecycle_through_acknowledge() {
    let mut s = store();
    let f = folder();
    let origin = device();
    let p = device();
    s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();
    s.upsert_peer(&peer(p)).unwrap();

    let rec = change(
        f,
        EntryId::new(),
        origin,
        VersionVector::new().incremented(origin),
    );
    s.append_change(&rec, &[p]).unwrap();

    s.mark_target(&rec.change_id, &p, TargetState::InProgress, None)
        .unwrap();
    assert_eq!(s.pending_for_peer(&p).unwrap().len(), 1);

    s.mark_target(&rec.change_id, &p, TargetState::Failed, Some("timeout"))
        .unwrap();
    assert_eq!(s.pending_for_peer(&p).unwrap().len(), 1);

    s.mark_target(&rec.change_id, &p, TargetState::Pending, None)
        .unwrap();
    s.acknowledge_change(&rec.change_id, &p, "2026-03-03T00:00:00Z")
        .unwrap();
    assert!(s.pending_for_peer(&p).unwrap().is_empty());
}

#[test]
fn applied_remote_changes_are_idempotent() {
    let mut s = store();
    let id = ChangeId::new();
    let origin = device();
    assert!(!s.is_applied(&id).unwrap());
    assert!(s.try_mark_applied(&id, &origin, &now()).unwrap());
    assert!(!s.try_mark_applied(&id, &origin, &now()).unwrap());
    assert!(s.is_applied(&id).unwrap());
}

#[test]
fn tombstones_retire_only_after_every_trusted_folder_peer_acknowledged() {
    let mut s = store();
    let f = folder();
    let p1 = device();
    let p2 = device();
    s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();
    for p in [p1, p2] {
        s.upsert_peer(&peer(p)).unwrap();
        s.set_folder_peer(&FolderPeer {
            folder_id: f,
            peer_id: p,
            permission: Permission::ReadWrite,
            joined_at: now(),
        })
        .unwrap();
    }

    let origin = device();
    let rec = change(
        f,
        EntryId::new(),
        origin,
        VersionVector::new().incremented(origin),
    );
    s.append_change(&rec, &[p1, p2]).unwrap();
    s.insert_tombstone(&Tombstone {
        change_id: rec.change_id,
        entry_id: rec.entry_id,
        folder_id: f,
        last_path: "a.txt".to_string(),
        version: rec.version.clone(),
        deleted_at: now(),
    })
    .unwrap();

    assert_eq!(
        s.retire_tombstones().unwrap(),
        0,
        "no peer acknowledged yet"
    );
    s.acknowledge_change(&rec.change_id, &p1, &now()).unwrap();
    assert_eq!(s.retire_tombstones().unwrap(), 0, "p2 is still outstanding");
    s.acknowledge_change(&rec.change_id, &p2, &now()).unwrap();
    assert_eq!(s.retire_tombstones().unwrap(), 1);
    assert!(s.list_tombstones(&f).unwrap().is_empty());

    // After unpairing one peer, the remaining peer's acknowledgement is
    // enough (explicit device-removal policy, SRD §14).
    let p3 = device();
    s.upsert_peer(&peer(p3)).unwrap();
    s.set_folder_peer(&FolderPeer {
        folder_id: f,
        peer_id: p3,
        permission: Permission::ReadWrite,
        joined_at: now(),
    })
    .unwrap();
    let rec2 = change(
        f,
        EntryId::new(),
        origin,
        VersionVector::new().incremented(origin).incremented(origin),
    );
    s.append_change(&rec2, &[p1, p2, p3]).unwrap();
    s.insert_tombstone(&Tombstone {
        change_id: rec2.change_id,
        entry_id: rec2.entry_id,
        folder_id: f,
        last_path: "b.txt".to_string(),
        version: rec2.version.clone(),
        deleted_at: now(),
    })
    .unwrap();
    s.acknowledge_change(&rec2.change_id, &p1, &now()).unwrap();
    s.delete_peer(&p2).unwrap();
    s.acknowledge_change(&rec2.change_id, &p3, &now()).unwrap();
    assert_eq!(s.retire_tombstones().unwrap(), 1);
    assert!(s.list_tombstones(&f).unwrap().is_empty());
}

#[test]
fn pending_content_hashes_cover_only_undelivered_changes() {
    let mut s = store();
    let f = folder();
    let p = device();
    s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();
    s.upsert_peer(&peer(p)).unwrap();
    s.set_folder_peer(&FolderPeer {
        folder_id: f,
        peer_id: p,
        permission: Permission::ReadWrite,
        joined_at: now(),
    })
    .unwrap();

    let origin = device();
    let rec = change(
        f,
        EntryId::new(),
        origin,
        VersionVector::new().incremented(origin),
    );
    s.append_change(&rec, &[p]).unwrap();
    assert_eq!(
        s.pending_content_hashes().unwrap(),
        vec![ContentHash("c0ffee".to_string())]
    );

    s.acknowledge_change(&rec.change_id, &p, &now()).unwrap();
    assert!(s.pending_content_hashes().unwrap().is_empty());
}

#[test]
fn tombstones_and_peer_sync_state_round_trip() {
    let mut s = store();
    let f = folder();
    let p = device();
    s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();
    s.upsert_peer(&peer(p)).unwrap();

    let origin = device();
    s.insert_tombstone(&Tombstone {
        change_id: ChangeId::new(),
        entry_id: EntryId::new(),
        folder_id: f,
        last_path: "gone.txt".to_string(),
        version: VersionVector::new().incremented(origin),
        deleted_at: now(),
    })
    .unwrap();
    assert_eq!(s.list_tombstones(&f).unwrap().len(), 1);

    assert!(s.peer_sync_state(&f, &p).unwrap().is_none());
    s.set_peer_sync_state(&PeerSyncState {
        folder_id: f,
        peer_id: p,
        last_acknowledged_seq: 7,
        last_synced_at: Some(now()),
    })
    .unwrap();
    let st = s.peer_sync_state(&f, &p).unwrap().unwrap();
    assert_eq!(st.last_acknowledged_seq, 7);
}

#[test]
fn get_tombstone_returns_the_newest_deletion_for_a_path() {
    let mut s = store();
    let f = folder();
    s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();

    let entry = EntryId::new();
    let older = ChangeId::new();
    let newer = ChangeId::new();
    s.insert_tombstone(&Tombstone {
        change_id: older,
        entry_id: entry,
        folder_id: f,
        last_path: "gone.txt".to_string(),
        version: VersionVector::new().incremented(device()),
        deleted_at: "2026-01-01T00:00:00Z".to_string(),
    })
    .unwrap();
    s.insert_tombstone(&Tombstone {
        change_id: newer,
        entry_id: entry,
        folder_id: f,
        last_path: "gone.txt".to_string(),
        version: VersionVector::new().incremented(device()),
        deleted_at: "2026-01-02T00:00:00Z".to_string(),
    })
    .unwrap();

    let found = s.get_tombstone(&f, "gone.txt").unwrap().expect("tombstone");
    assert_eq!(found.change_id, newer, "the newest deletion wins");
    assert_eq!(found.entry_id, entry);
    assert!(s.get_tombstone(&f, "other.txt").unwrap().is_none());
}

#[test]
fn transfer_checkpoints_are_durable() {
    let mut s = store();
    let f = folder();
    let p = device();
    let origin = device();
    s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();
    s.upsert_peer(&peer(p)).unwrap();
    let rec = change(
        f,
        EntryId::new(),
        origin,
        VersionVector::new().incremented(origin),
    );
    s.append_change(&rec, &[p]).unwrap();

    let transfer = Transfer {
        transfer_id: "t1".to_string(),
        change_id: rec.change_id,
        peer_id: p,
        direction: TransferDirection::Send,
        state: TransferState::Queued,
        expected_hash: Some(ContentHash("c0ffee".to_string())),
        expected_size: 100,
        acknowledged_bytes: 0,
        temporary_path: Some("/tmp/part".to_string()),
        updated_at: now(),
    };
    s.upsert_transfer(&transfer).unwrap();
    assert_eq!(s.get_transfer("t1").unwrap().unwrap(), transfer);
    assert_eq!(
        s.get_transfer_for_change(&rec.change_id, TransferDirection::Send)
            .unwrap()
            .unwrap()
            .transfer_id,
        "t1"
    );

    s.save_checkpoint("t1", 64, TransferState::Paused).unwrap();
    let got = s.get_transfer("t1").unwrap().unwrap();
    assert_eq!(got.acknowledged_bytes, 64);
    assert_eq!(got.state, TransferState::Paused);

    assert!(matches!(
        s.save_checkpoint("missing", 1, TransferState::Active),
        Err(syncengine_storage::StoreError::NotFound(_))
    ));
}

#[test]
fn conflicts_insert_and_resolve() {
    let mut s = store();
    let f = folder();
    let e = EntryId::new();
    let c = Conflict {
        conflict_id: "c1".to_string(),
        folder_id: f,
        entry_id: e,
        local_change_id: Some(ChangeId::new()),
        remote_change_id: Some(ChangeId::new()),
        status: ConflictStatus::Unresolved,
        resolution: None,
        created_at: now(),
        resolved_at: None,
    };
    s.insert_conflict(&c).unwrap();
    assert_eq!(s.list_conflicts(None).unwrap().len(), 1);
    assert_eq!(
        s.list_conflicts(Some(ConflictStatus::Unresolved))
            .unwrap()
            .len(),
        1
    );

    s.resolve_conflict("c1", ConflictStatus::Resolved, Some("kept-local"), &now())
        .unwrap();
    assert!(
        s.list_conflicts(Some(ConflictStatus::Unresolved))
            .unwrap()
            .is_empty()
    );
    let got = &s.list_conflicts(Some(ConflictStatus::Resolved)).unwrap()[0];
    assert_eq!(got.resolution.as_deref(), Some("kept-local"));
}

#[test]
fn content_objects_track_refcounts() {
    let mut s = store();
    let hash = ContentHash("deadbeef".to_string());
    s.put_content_object(&ContentObject {
        content_hash: hash.clone(),
        size: 10,
        staged_path: "/tmp/staged".to_string(),
        refcount: 1,
        created_at: now(),
        verified_at: None,
    })
    .unwrap();
    assert_eq!(s.get_content_object(&hash).unwrap().unwrap().refcount, 1);

    s.bump_content_refcount(&hash, 2).unwrap();
    assert_eq!(s.get_content_object(&hash).unwrap().unwrap().refcount, 3);
    s.bump_content_refcount(&hash, -5).unwrap();
    assert_eq!(s.get_content_object(&hash).unwrap().unwrap().refcount, 0);
    assert_eq!(s.list_content_objects().unwrap().len(), 1);
}

#[test]
fn state_survives_close_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("syncengine.db");
    let id = device();
    let f = folder();
    let p = device();

    {
        let mut s = SqliteStore::open(&db_path).unwrap();
        s.set_local_device(id, "persisted").unwrap();
        s.upsert_folder(&folder_record(f, "/tmp/docs")).unwrap();
        s.upsert_peer(&peer(p)).unwrap();
        let rec = change(f, EntryId::new(), id, VersionVector::new().incremented(id));
        s.append_change(&rec, &[p]).unwrap();
    }

    let s = SqliteStore::open(&db_path).unwrap();
    assert_eq!(s.local_device().unwrap().unwrap().device_id, id);
    assert_eq!(s.list_folders().unwrap().len(), 1);
    assert_eq!(s.list_peers().unwrap().len(), 1);
    assert_eq!(s.max_change_seq().unwrap(), 1);
}
