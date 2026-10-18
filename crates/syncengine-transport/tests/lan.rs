//! LAN transport tests over real loopback TCP.

mod common;

use common::{read, side};
use syncengine_engine::Identity;
use syncengine_protocol::{DeviceId, FolderId};
use syncengine_transport::handshake::verify_peer;
use syncengine_transport::lan::{LanListener, connect_lan};
use syncengine_transport::session::run_session;

#[tokio::test]
async fn lan_transport_syncs_two_engines_over_tcp() {
    let folder_id = FolderId::new();
    let alice_id = DeviceId::new();
    let bob_id = DeviceId::new();
    let alice_identity = Identity::generate();
    let bob_identity = Identity::generate();

    let options = common::test_options(16 * 1024);

    let mut alice = side(
        "alice",
        alice_identity.secret_bytes(),
        alice_id,
        bob_id,
        &bob_identity,
        folder_id,
        &[("lan/hello.txt", b"over the wire")],
    );
    let mut bob = side(
        "bob",
        bob_identity.secret_bytes(),
        bob_id,
        alice_id,
        &alice_identity,
        folder_id,
        &[("reply.txt", b"and back")],
    );

    let listener = LanListener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");

    let (alice_result, bob_result) = tokio::join!(
        async {
            let alice = &mut alice;
            let (mut channel, info) = connect_lan(addr, &alice.identity, alice.device_id)
                .await
                .expect("dial");
            verify_peer(&info, &alice.peer).expect("verify alice");
            run_session(&mut alice.engine, &mut channel, &alice.peer, &options)
                .await
                .expect("alice session")
        },
        async {
            let bob = &mut bob;
            let (mut channel, info, _addr) = listener
                .accept(&bob.identity, bob.device_id)
                .await
                .expect("accept");
            verify_peer(&info, &bob.peer).expect("verify bob");
            run_session(&mut bob.engine, &mut channel, &bob.peer, &options)
                .await
                .expect("bob session")
        }
    );

    assert_eq!(alice_result.delivered, 1);
    assert_eq!(bob_result.delivered, 1);
    assert_eq!(read(&bob.root.join("lan/hello.txt")), b"over the wire");
    assert_eq!(read(&alice.root.join("reply.txt")), b"and back");
}

#[tokio::test]
async fn unknown_devices_fail_the_trusted_peer_check() {
    let folder_id = FolderId::new();
    let bob_id = DeviceId::new();
    let alice_id = DeviceId::new();
    let mallory_id = DeviceId::new();
    let alice_identity = Identity::generate();
    let bob_identity = Identity::generate();
    let mallory_identity = Identity::generate();

    let bob = side(
        "bob",
        bob_identity.secret_bytes(),
        bob_id,
        alice_id,
        &alice_identity,
        folder_id,
        &[],
    );

    let listener = LanListener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");

    let mallory_secret = mallory_identity.secret_bytes();
    let (accepted, _) = tokio::join!(
        async {
            let (channel, info, _addr) = listener
                .accept(&bob.identity, bob.device_id)
                .await
                .expect("the handshake completes");
            drop(channel);
            verify_peer(&info, &bob.peer)
        },
        async {
            let mallory_identity = Identity::from_secret_bytes(mallory_secret.0, mallory_secret.1);
            let (_channel, info) = connect_lan(addr, &mallory_identity, mallory_id)
                .await
                .expect("handshake");
            info
        }
    );

    assert!(
        accepted.is_err(),
        "an unknown device must fail the trusted-peer check"
    );
}
