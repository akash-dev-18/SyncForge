//! Shared test fixtures: real stores, engines, and identities.

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use tempfile::TempDir;

use syncengine_engine::{Identity, StagingStore, SyncEngine};
use syncengine_protocol::{DeviceId, FolderId, now_rfc3339};
use syncengine_storage::{
    FolderPeer, Peer, Permission, SharedFolder, SqliteStore, Store, TrustState,
};
use syncengine_transport::session::SessionOptions;

pub struct Side {
    pub _dir: TempDir,
    pub root: PathBuf,
    pub engine: SyncEngine<SqliteStore>,
    pub identity: Identity,
    pub device_id: DeviceId,
    pub peer: Peer,
}

pub fn peer_record(identity: &Identity, device_id: DeviceId, name: &str) -> Peer {
    Peer {
        device_id,
        device_name: name.to_string(),
        public_key: identity.ed25519_public_hex(),
        encryption_key: identity.x25519_public_hex(),
        platform: "test".into(),
        trust: TrustState::Trusted,
        last_seen_at: None,
        created_at: now_rfc3339(),
    }
}

pub fn side(
    name: &str,
    self_secret: ([u8; 32], [u8; 32]),
    self_id: DeviceId,
    peer_id: DeviceId,
    peer_identity: &Identity,
    folder_id: FolderId,
    files: &[(&str, &[u8])],
) -> Side {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).expect("root");

    for (relative, contents) in files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(&path, contents).expect("write");
    }

    let mut store = SqliteStore::open(&dir.path().join("db.sqlite")).expect("store");
    store.set_local_device(self_id, name).expect("device");
    store
        .upsert_folder(&SharedFolder {
            folder_id,
            name: "shared".into(),
            local_root: root.clone(),
            enabled: true,
            created_at: now_rfc3339(),
        })
        .expect("folder");
    store
        .upsert_peer(&peer_record(peer_identity, peer_id, "peer"))
        .expect("peer");
    store
        .set_folder_peer(&FolderPeer {
            folder_id,
            peer_id,
            permission: Permission::ReadWrite,
            joined_at: now_rfc3339(),
        })
        .expect("membership");

    let staging = StagingStore::new(dir.path().join("staging")).expect("staging");
    let mut engine = SyncEngine::new(self_id, store, staging);
    engine.queue_local_changes(&folder_id).expect("queue");

    Side {
        _dir: dir,
        root,
        engine,
        identity: Identity::from_secret_bytes(self_secret.0, self_secret.1),
        device_id: self_id,
        peer: peer_record(peer_identity, peer_id, "peer"),
    }
}

pub fn test_options(chunk_size: usize) -> SessionOptions {
    SessionOptions {
        chunk_size,
        max_idle: Duration::from_secs(15),
    }
}

pub fn read(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}
