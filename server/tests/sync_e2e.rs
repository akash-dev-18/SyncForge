//! End-to-end relay test: two real client installations, a real server
//! (in-process), real pairing through the control plane, and one
//! synchronization pass per device through the relay WebSocket.
//!
//! Skipped (with a clear SKIP line) unless `SYNCENGINE_TEST_DATABASE_URL` is
//! set, exactly like `tests/api.rs`.

use std::path::PathBuf;
use std::time::Duration;

use syncengine_client::delivery::{PassOptions, ensure_folder_peer_links, run_pass};
use syncengine_client::remote::RemoteClient;
use syncengine_client::{commands, config::Config, state};
use syncengine_engine::{Identity, StagingStore, SyncEngine};
use syncengine_protocol::{DeviceId, FolderId, now_rfc3339};
use syncengine_server::db::{PgPool, pool_from_url, run_migrations};
use syncengine_server::{AppState, api};
use syncengine_storage::{Peer, SharedFolder, SqliteStore, Store, TrustState};
use tempfile::TempDir;

struct Fixture {
    root: PathBuf,
    identity: Identity,
    device_id: DeviceId,
    store: SqliteStore,
    staging_root: PathBuf,
}

fn setup(dir: &std::path::Path, name: &str, server_url: &str) -> Fixture {
    let base = dir.join(name);
    std::fs::create_dir_all(&base).expect("mkdir");
    let config = Config {
        device_name: name.to_string(),
        data_dir: base.join("data"),
        server_url: Some(server_url.to_string()),
        ..Config::default()
    };
    syncengine_client::config::save(&base.join("config.toml"), &config).expect("save config");

    let root = base.join("root");
    std::fs::create_dir_all(&root).expect("mkdir root");

    let opened = state::open(&config).expect("state");
    let (identity, device_id, store, data_dir) = opened.into_parts();
    Fixture {
        root,
        identity,
        device_id,
        store,
        staging_root: data_dir.join(state::STAGING_DIR_NAME),
    }
}

async fn spawn_server(url: &str) -> (String, PgPool) {
    let pool = pool_from_url(url).expect("pool");
    run_migrations(&pool).await.expect("migrations");
    let client = pool.get().await.expect("connection");
    client
        .batch_execute(
            "TRUNCATE devices, pairing_requests, device_relationships, \
             folder_memberships, relay_sessions CASCADE",
        )
        .await
        .expect("truncate");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = api::app(AppState::new(pool.clone()));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}"), pool)
}

async fn pair_devices(
    alice: &RemoteClient<'_>,
    bob: &RemoteClient<'_>,
    alice_id: DeviceId,
    bob_id: DeviceId,
    alice_identity: &Identity,
    bob_identity: &Identity,
    stores: (&mut SqliteStore, &mut SqliteStore),
) -> (Peer, Peer) {
    alice
        .register(
            "alice",
            "linux",
            &alice_identity.ed25519_public_hex(),
            &alice_identity.x25519_public_hex(),
        )
        .await
        .expect("alice register");
    bob.register(
        "bob",
        "linux",
        &bob_identity.ed25519_public_hex(),
        &bob_identity.x25519_public_hex(),
    )
    .await
    .expect("bob register");

    let created = alice
        .create_pairing_request(bob_id)
        .await
        .expect("create pairing request");
    bob.approve_pairing_request(&created.request_id, &created.code)
        .await
        .expect("approve pairing request");

    let bob_keys = alice.device_keys(bob_id).await.expect("bob keys");
    let alice_keys = bob.device_keys(alice_id).await.expect("alice keys");

    let bob_record = Peer {
        device_id: bob_id,
        device_name: bob_keys.device_name.clone(),
        public_key: bob_keys.signing_key.clone(),
        encryption_key: bob_keys.encryption_key.clone(),
        platform: bob_keys.platform.clone(),
        trust: TrustState::Trusted,
        last_seen_at: None,
        created_at: now_rfc3339(),
    };
    let alice_record = Peer {
        device_id: alice_id,
        device_name: alice_keys.device_name.clone(),
        public_key: alice_keys.signing_key.clone(),
        encryption_key: alice_keys.encryption_key.clone(),
        platform: alice_keys.platform.clone(),
        trust: TrustState::Trusted,
        last_seen_at: None,
        created_at: now_rfc3339(),
    };
    stores.0.upsert_peer(&bob_record).expect("alice trusts bob");
    stores
        .1
        .upsert_peer(&alice_record)
        .expect("bob trusts alice");
    (alice_record, bob_record)
}

#[tokio::test]
async fn two_clients_sync_through_a_relay_session() {
    let Some(url) = std::env::var("SYNCENGINE_TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        eprintln!("SKIP: set SYNCENGINE_TEST_DATABASE_URL to run the relay end-to-end test");
        return;
    };

    let temp = TempDir::new().expect("tempdir");
    let (base, _pool) = spawn_server(&url).await;

    let mut alice = setup(temp.path(), "alice", &base);
    let mut bob = setup(temp.path(), "bob", &base);
    let folder_id = FolderId::new();

    std::fs::write(alice.root.join("from-alice.txt"), b"relayed from alice").expect("write");
    std::fs::write(bob.root.join("from-bob.txt"), b"relayed from bob").expect("write");
    for fixture in [&mut alice, &mut bob] {
        fixture
            .store
            .upsert_folder(&SharedFolder {
                folder_id,
                name: "root".to_string(),
                local_root: fixture.root.clone(),
                enabled: true,
                created_at: now_rfc3339(),
            })
            .expect("share folder");
        commands::announce_folder(
            &base,
            &fixture.identity,
            fixture.device_id,
            "fixture",
            folder_id,
        )
        .await
        .expect("announce folder");
    }

    let alice_remote = RemoteClient::new(&base, &alice.identity, alice.device_id).expect("client");
    let bob_remote = RemoteClient::new(&base, &bob.identity, bob.device_id).expect("client");
    let (alice_record, _bob_record) = pair_devices(
        &alice_remote,
        &bob_remote,
        alice.device_id,
        bob.device_id,
        &alice.identity,
        &bob.identity,
        (&mut alice.store, &mut bob.store),
    )
    .await;

    let alice_staging = StagingStore::new(alice.staging_root.clone()).expect("staging");
    let mut alice_engine = SyncEngine::new(alice.device_id, alice.store, alice_staging);
    let bob_staging = StagingStore::new(bob.staging_root.clone()).expect("staging");
    let mut bob_engine = SyncEngine::new(bob.device_id, bob.store, bob_staging);

    let alice_peers = alice_engine.store().list_peers().expect("peers");
    ensure_folder_peer_links(&mut alice_engine, &alice_peers).expect("links");
    let bob_peers = bob_engine.store().list_peers().expect("peers");
    ensure_folder_peer_links(&mut bob_engine, &bob_peers).expect("links");
    alice_engine
        .queue_local_changes(&folder_id)
        .expect("alice scan");
    bob_engine
        .queue_local_changes(&folder_id)
        .expect("bob scan");

    let options = PassOptions {
        listen_port: 0,
        lan_dial_timeout: Duration::from_millis(200),
        inbound_window: Duration::ZERO,
        relay_wait: Duration::from_secs(10),
        ..PassOptions::default()
    };

    let alice_pass = run_pass(
        &mut alice_engine,
        &alice.identity,
        "alice",
        Some(&alice_remote),
        &options,
    );
    let bob_pass = run_pass(
        &mut bob_engine,
        &bob.identity,
        "bob",
        Some(&bob_remote),
        &options,
    );
    let (alice_report, bob_report) = tokio::join!(alice_pass, bob_pass);
    let alice_report = alice_report.expect("alice pass");
    let bob_report = bob_report.expect("bob pass");

    for report in [&alice_report, &bob_report] {
        assert_eq!(report.outcomes.len(), 1, "one peer each");
        assert_eq!(
            report.outcomes[0].transport, "relay",
            "the pass must go through the relay: {:?}",
            report.outcomes[0].error
        );
        assert!(report.connected() >= 1);
    }

    assert_eq!(
        std::fs::read(temp.path().join("bob/root/from-alice.txt")).expect("bob file"),
        b"relayed from alice"
    );
    assert_eq!(
        std::fs::read(temp.path().join("alice/root/from-bob.txt")).expect("alice file"),
        b"relayed from bob"
    );
    let _ = alice_record;
}
