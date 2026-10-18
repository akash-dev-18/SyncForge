//! End-to-end LAN test: two independent client installations, one paired via
//! a shared folder id, synchronized by the real `sync` command over mDNS +
//! TCP. The Alice side is driven by [`commands::sync`]; the Bob side is a
//! minimal fixture that advertises and serves one inbound session, standing
//! in for Bob's own `sync`/daemon.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use syncengine_client::{commands, config::Config, state};
use syncengine_engine::{Identity, StagingStore, SyncEngine};
use syncengine_protocol::{DeviceId, FolderId, now_rfc3339};
use syncengine_storage::{FolderPeer, Peer, Permission, SqliteStore, Store, TrustState};
use syncengine_transport::discovery::Advertiser;
use syncengine_transport::handshake::verify_peer;
use syncengine_transport::lan::LanListener;
use syncengine_transport::session::{SessionOptions, run_session};

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    listener.local_addr().expect("addr").port()
}

struct Fixture {
    config_path: PathBuf,
    root: PathBuf,
    identity: Identity,
    device_id: DeviceId,
    store: SqliteStore,
    staging_root: PathBuf,
}

fn setup(dir: &std::path::Path, name: &str) -> Fixture {
    let base = dir.join(name);
    std::fs::create_dir_all(&base).expect("mkdir");
    let config = Config {
        device_name: name.to_string(),
        data_dir: base.join("data"),
        lan_port: free_port(),
        ..Config::default()
    };
    let config_path = base.join("config.toml");
    syncengine_client::config::save(&config_path, &config).expect("save config");

    let root = base.join("root");
    std::fs::create_dir_all(&root).expect("mkdir root");

    let opened = state::open(&config).expect("state");
    let (identity, device_id, store, data_dir) = opened.into_parts();
    let staging_root = data_dir.join(state::STAGING_DIR_NAME);
    Fixture {
        config_path,
        root,
        identity,
        device_id,
        store,
        staging_root,
    }
}

fn peer_record(id: DeviceId, name: &str, identity: &Identity) -> Peer {
    Peer {
        device_id: id,
        device_name: name.to_string(),
        public_key: identity.ed25519_public_hex(),
        encryption_key: identity.x25519_public_hex(),
        platform: "linux".to_string(),
        trust: TrustState::Trusted,
        last_seen_at: None,
        created_at: now_rfc3339(),
    }
}

/// Bob advertises and serves exactly one inbound session from Alice.
fn start_bob_fixture(
    bob: Fixture,
    alice_id: DeviceId,
    alice_record: Peer,
    folder_id: FolderId,
) -> std::thread::JoinHandle<syncengine_transport::session::SessionReport> {
    let bob_identity = bob.identity;
    let bob_device_id = bob.device_id;
    let bob_store = bob.store;
    let bob_staging = bob.staging_root;
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut engine = SyncEngine::new(
            bob_device_id,
            bob_store,
            StagingStore::new(bob_staging).expect("staging"),
        );
        engine
            .store_mut()
            .set_folder_peer(&FolderPeer {
                folder_id,
                peer_id: alice_id,
                permission: Permission::ReadWrite,
                joined_at: now_rfc3339(),
            })
            .expect("link");
        engine.queue_local_changes(&folder_id).expect("scan");

        runtime.block_on(async move {
            let listener = LanListener::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap())
                .await
                .expect("bind");
            let port = listener.port().expect("port");
            let _advertiser = Advertiser::start(&bob_device_id, "bob-e2e", port).expect("mdns");
            let (mut channel, info, _addr) = tokio::time::timeout(
                Duration::from_secs(60),
                listener.accept(&bob_identity, bob_device_id),
            )
            .await
            .expect("alice must connect within 60s")
            .expect("accept");
            verify_peer(&info, &alice_record).expect("alice must be trusted");
            run_session(
                &mut engine,
                &mut channel,
                &alice_record,
                &SessionOptions::default(),
            )
            .await
            .expect("bob session")
        })
    })
}

#[test]
fn changes_stay_journaled_while_the_peer_is_offline_and_deliver_on_reconnect() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut alice = setup(dir.path(), "alice");
    let mut bob = setup(dir.path(), "bob");

    let folder_id = FolderId::new();
    std::fs::write(alice.root.join("queued.txt"), b"written while bob was away").expect("write");
    commands::share(
        &alice.root,
        Some(&folder_id.to_string()),
        Some(&alice.config_path),
    )
    .expect("alice share");
    commands::share(
        &bob.root,
        Some(&folder_id.to_string()),
        Some(&bob.config_path),
    )
    .expect("bob share");

    let bob_record = peer_record(bob.device_id, "bob", &bob.identity);
    let alice_record = peer_record(alice.device_id, "alice", &alice.identity);
    alice
        .store
        .upsert_peer(&bob_record)
        .expect("alice trusts bob");
    bob.store
        .upsert_peer(&alice_record)
        .expect("bob trusts alice");

    // Bob is not running: the pass must not lose the change.
    commands::sync(Some(&alice.config_path)).expect("first alice sync");
    assert!(
        !bob.root.join("queued.txt").exists(),
        "nothing can arrive while bob is offline"
    );
    let queued = alice
        .store
        .pending_for_peer(&bob.device_id)
        .expect("pending query");
    assert_eq!(queued.len(), 1, "the change must stay journaled for bob");

    // Bob comes online; the next pass delivers the journaled change.
    let bob_id = bob.device_id;
    let handle = start_bob_fixture(bob, alice.device_id, alice_record, folder_id);
    std::thread::sleep(Duration::from_millis(700));
    commands::sync(Some(&alice.config_path)).expect("second alice sync");
    let bob_report = handle.join().expect("bob fixture must not panic");

    assert!(bob_report.applied >= 1, "bob must have applied the change");
    assert_eq!(
        std::fs::read(dir.path().join("bob/root/queued.txt")).expect("bob file"),
        b"written while bob was away"
    );
    let queued = alice
        .store
        .pending_for_peer(&bob_id)
        .expect("pending query");
    assert!(
        queued.is_empty(),
        "the journal must be drained after the peer acknowledged"
    );
}

#[test]
fn sync_command_transfers_files_over_lan_between_two_devices() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut alice = setup(dir.path(), "alice");
    let mut bob = setup(dir.path(), "bob");

    let folder_id = FolderId::new();
    std::fs::write(alice.root.join("from-alice.txt"), b"alice was here").expect("write");
    std::fs::write(bob.root.join("from-bob.txt"), b"bob was here").expect("write");
    commands::share(
        &alice.root,
        Some(&folder_id.to_string()),
        Some(&alice.config_path),
    )
    .expect("alice share");
    commands::share(
        &bob.root,
        Some(&folder_id.to_string()),
        Some(&bob.config_path),
    )
    .expect("bob share");

    // Both sides trust each other exactly as pairing would have recorded it.
    let bob_record = peer_record(bob.device_id, "bob", &bob.identity);
    let alice_record = peer_record(alice.device_id, "alice", &alice.identity);
    alice
        .store
        .upsert_peer(&bob_record)
        .expect("alice trusts bob");
    bob.store
        .upsert_peer(&alice_record)
        .expect("bob trusts alice");

    // Bob's fixture: scan, advertise, serve exactly one inbound session.
    let bob_handle = start_bob_fixture(bob, alice.device_id, alice_record, folder_id);

    // Give the advertisement a moment to land, then run the real command.
    std::thread::sleep(Duration::from_millis(700));
    commands::sync(Some(&alice.config_path)).expect("alice sync");

    let bob_report = bob_handle.join().expect("bob fixture must not panic");
    assert!(bob_report.applied >= 1, "bob must have applied a change");

    assert_eq!(
        std::fs::read(dir.path().join("bob/root/from-alice.txt")).expect("bob file"),
        b"alice was here"
    );
    assert_eq!(
        std::fs::read(dir.path().join("alice/root/from-bob.txt")).expect("alice file"),
        b"bob was here"
    );
}
