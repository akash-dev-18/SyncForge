//! End-to-end session tests: two real stores, engines, and identities
//! exchanging changes over an in-memory duplex channel with the real
//! handshake and sealing enabled.

mod common;

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use common::{read, side, test_options};
use tokio::io::duplex;

use syncengine_engine::Identity;
use syncengine_protocol::{DeviceId, FolderId};
use syncengine_storage::Store;
use syncengine_transport::channel::{FramedIo, PeerChannel};
use syncengine_transport::handshake::{Role, handshake, verify_peer};
use syncengine_transport::session::{SessionReport, run_session};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A duplex endpoint that starts failing reads after a byte budget, which
/// interrupts a session mid-transfer deterministically.
struct FailAfterReads {
    inner: Pin<Box<tokio::io::DuplexStream>>,
    remaining: usize,
}

impl FailAfterReads {
    fn new(inner: tokio::io::DuplexStream, budget: usize) -> Self {
        Self {
            inner: Box::pin(inner),
            remaining: budget,
        }
    }
}

impl AsyncRead for FailAfterReads {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.remaining == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "test budget exhausted",
            )));
        }
        let before = buf.filled().len();
        let result = self.inner.as_mut().poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            let consumed = buf.filled().len() - before;
            self.remaining = self.remaining.saturating_sub(consumed);
        }
        result
    }
}

impl AsyncWrite for FailAfterReads {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.as_mut().poll_shutdown(cx)
    }
}

#[tokio::test]
async fn interrupted_transfer_resumes_from_the_durable_checkpoint() {
    let folder_id = FolderId::new();
    let alice_id = DeviceId::new();
    let bob_id = DeviceId::new();
    let alice_identity = Identity::generate();
    let bob_identity = Identity::generate();

    let mut payload = Vec::with_capacity(512 * 1024);
    let mut state = 0xfeed_face_cafe_beefu64;
    while payload.len() < 512 * 1024 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        payload.extend_from_slice(&state.to_le_bytes());
    }

    let mut alice = side(
        "alice",
        alice_identity.secret_bytes(),
        alice_id,
        bob_id,
        &bob_identity,
        folder_id,
        &[("resume.bin", payload.as_slice())],
    );
    let mut bob = side(
        "bob",
        bob_identity.secret_bytes(),
        bob_id,
        alice_id,
        &alice_identity,
        folder_id,
        &[],
    );

    // Round 1: bob's read side is cut off after ~96 KiB, mid-transfer.
    let options = test_options(16 * 1024);
    let (alice_stream, bob_stream) = duplex(64 * 1024);
    let (alice_first, bob_first) = tokio::join!(
        async {
            let alice = &mut alice;
            let mut channel =
                PeerChannel::new(Box::new(FramedIo::new(alice_stream)), &alice.identity);
            handshake(
                &mut channel,
                &alice.identity,
                alice.device_id,
                Role::Initiator,
            )
            .await
            .expect("initiator handshake");
            run_session(&mut alice.engine, &mut channel, &alice.peer, &options).await
        },
        async {
            let bob = &mut bob;
            let stream = FailAfterReads::new(bob_stream, 96 * 1024);
            let mut channel = PeerChannel::new(Box::new(FramedIo::new(stream)), &bob.identity);
            handshake(&mut channel, &bob.identity, bob.device_id, Role::Responder)
                .await
                .expect("responder handshake");
            run_session(&mut bob.engine, &mut channel, &bob.peer, &options).await
        }
    );
    assert!(alice_first.is_err(), "alice's session is interrupted");
    assert!(bob_first.is_err(), "bob's session is interrupted");
    assert!(!bob.root.join("resume.bin").exists());

    let staging_dir = bob._dir.path().join("staging");
    let partial: Vec<_> = std::fs::read_dir(&staging_dir)
        .expect("staging readable")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".recv-"))
        .collect();
    assert_eq!(partial.len(), 1, "exactly one durable checkpoint exists");
    let partial_path = partial[0].path();
    let received = partial[0].metadata().expect("partial metadata").len();
    assert!(
        received > 0 && received < payload.len() as u64,
        "the checkpoint holds a mid-transfer prefix ({received} bytes)"
    );

    // Simulate a torn tail from a crash during the interrupted write; a
    // receiver that restarts from zero or resumes at the wrong offset now
    // fails loudly instead of silently succeeding.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&partial_path)
        .expect("partial open")
        .set_len(received / 2)
        .expect("truncate");

    // Round 2: the real session pair must complete the transfer.
    let (alice_report, bob_report) = sync_pair(&mut alice, &mut bob, 16 * 1024).await;
    assert_eq!(bob_report.applied, 1, "bob applies the resumed file");
    assert_eq!(bob_report.errors, 0);
    assert_eq!(alice_report.delivered, 1, "alice's change was acknowledged");
    assert_eq!(read(&bob.root.join("resume.bin")), payload);

    let leftovers: Vec<_> = std::fs::read_dir(&staging_dir)
        .expect("staging readable")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".recv-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the checkpoint is consumed on success"
    );
}

async fn sync_pair(
    alice: &mut common::Side,
    bob: &mut common::Side,
    chunk_size: usize,
) -> (SessionReport, SessionReport) {
    let options = test_options(chunk_size);
    let (alice_stream, bob_stream) = duplex(8 * 1024 * 1024);
    tokio::join!(
        async {
            let alice = &mut *alice;
            let mut channel =
                PeerChannel::new(Box::new(FramedIo::new(alice_stream)), &alice.identity);
            let info = handshake(
                &mut channel,
                &alice.identity,
                alice.device_id,
                Role::Initiator,
            )
            .await
            .expect("initiator handshake");
            verify_peer(&info, &alice.peer).expect("peer verification");
            run_session(&mut alice.engine, &mut channel, &alice.peer, &options)
                .await
                .expect("alice session")
        },
        async {
            let bob = &mut *bob;
            let mut channel = PeerChannel::new(Box::new(FramedIo::new(bob_stream)), &bob.identity);
            let info = handshake(&mut channel, &bob.identity, bob.device_id, Role::Responder)
                .await
                .expect("responder handshake");
            verify_peer(&info, &bob.peer).expect("peer verification");
            run_session(&mut bob.engine, &mut channel, &bob.peer, &options)
                .await
                .expect("bob session")
        }
    )
}

#[tokio::test]
async fn files_sync_both_ways_and_deletions_propagate() {
    let folder_id = FolderId::new();
    let alice_id = DeviceId::new();
    let bob_id = DeviceId::new();
    let alice_identity = Identity::generate();
    let bob_identity = Identity::generate();

    let mut alice = side(
        "alice",
        alice_identity.secret_bytes(),
        alice_id,
        bob_id,
        &bob_identity,
        folder_id,
        &[
            ("docs/hello.txt", b"hello from alice"),
            ("notes/todo.md", b"- sync everything"),
        ],
    );
    let mut bob = side(
        "bob",
        bob_identity.secret_bytes(),
        bob_id,
        alice_id,
        &alice_identity,
        folder_id,
        &[("bob-only.txt", b"bob was here")],
    );

    let (alice_report, bob_report) = sync_pair(&mut alice, &mut bob, 16 * 1024).await;

    assert_eq!(alice_report.delivered, 2, "alice's two files were acked");
    assert_eq!(bob_report.delivered, 1, "bob's file was acked");
    assert_eq!(bob_report.applied, 2, "bob applied two files");
    assert_eq!(alice_report.applied, 1, "alice applied one file");
    assert_eq!(alice_report.errors, 0);
    assert_eq!(bob_report.errors, 0);

    assert_eq!(read(&bob.root.join("docs/hello.txt")), b"hello from alice");
    assert_eq!(read(&bob.root.join("notes/todo.md")), b"- sync everything");
    assert_eq!(read(&alice.root.join("bob-only.txt")), b"bob was here");

    assert!(
        alice
            .engine
            .store()
            .pending_for_peer(&bob_id)
            .unwrap()
            .is_empty(),
        "alice must have nothing pending after acknowledgements"
    );
    assert!(
        bob.engine
            .store()
            .pending_for_peer(&alice_id)
            .unwrap()
            .is_empty()
    );

    // Second round: bob deletes his file and updates one of alice's; alice
    // deletes one of hers; the session must propagate all three.
    std::fs::remove_file(bob.root.join("bob-only.txt")).unwrap();
    std::fs::write(bob.root.join("docs/hello.txt"), b"edited by bob").unwrap();
    std::fs::remove_file(alice.root.join("notes/todo.md")).unwrap();
    bob.engine
        .queue_local_changes(&folder_id)
        .expect("queue bob");
    alice
        .engine
        .queue_local_changes(&folder_id)
        .expect("queue alice");

    let (_, _) = sync_pair(&mut alice, &mut bob, 16 * 1024).await;

    assert!(!alice.root.join("bob-only.txt").exists());
    assert!(!bob.root.join("notes/todo.md").exists());
    assert_eq!(read(&alice.root.join("docs/hello.txt")), b"edited by bob");
}

#[tokio::test]
async fn large_files_transfer_in_multiple_chunks() {
    let folder_id = FolderId::new();
    let alice_id = DeviceId::new();
    let bob_id = DeviceId::new();
    let alice_identity = Identity::generate();
    let bob_identity = Identity::generate();

    let mut payload = Vec::with_capacity(700 * 1024);
    let mut state = 0x1234_5678_9abc_def0u64;
    while payload.len() < 700 * 1024 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        payload.extend_from_slice(&state.to_le_bytes());
    }

    let mut alice = side(
        "alice",
        alice_identity.secret_bytes(),
        alice_id,
        bob_id,
        &bob_identity,
        folder_id,
        &[("big.bin", payload.as_slice())],
    );
    let mut bob = side(
        "bob",
        bob_identity.secret_bytes(),
        bob_id,
        alice_id,
        &alice_identity,
        folder_id,
        &[],
    );

    let (alice_report, bob_report) = sync_pair(&mut alice, &mut bob, 64 * 1024).await;
    assert_eq!(alice_report.delivered, 1);
    assert_eq!(bob_report.applied, 1);
    assert_eq!(read(&bob.root.join("big.bin")), payload);
}

#[tokio::test]
async fn concurrent_edits_are_preserved_as_conflict_copies() {
    let folder_id = FolderId::new();
    let alice_id = DeviceId::new();
    let bob_id = DeviceId::new();
    let alice_identity = Identity::generate();
    let bob_identity = Identity::generate();

    let mut alice = side(
        "alice",
        alice_identity.secret_bytes(),
        alice_id,
        bob_id,
        &bob_identity,
        folder_id,
        &[("shared.txt", b"alice version")],
    );
    let mut bob = side(
        "bob",
        bob_identity.secret_bytes(),
        bob_id,
        alice_id,
        &alice_identity,
        folder_id,
        &[("shared.txt", b"bob version")],
    );

    let (alice_report, bob_report) = sync_pair(&mut alice, &mut bob, 16 * 1024).await;

    assert_eq!(bob_report.conflicts, 1, "bob preserves alice's revision");
    assert_eq!(alice_report.conflicts, 1, "alice preserves bob's revision");
    assert_eq!(read(&alice.root.join("shared.txt")), b"alice version");
    assert_eq!(read(&bob.root.join("shared.txt")), b"bob version");

    let alice_conflicts: Vec<_> = std::fs::read_dir(&alice.root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains("conflict"))
        .collect();
    assert_eq!(alice_conflicts.len(), 1);
    assert_eq!(
        read(&alice_conflicts[0].path()),
        b"bob version",
        "the conflict copy holds the remote revision"
    );

    let conflicts = alice
        .engine
        .store()
        .list_conflicts(None)
        .expect("conflicts");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].folder_id, folder_id);
}

#[tokio::test]
async fn sessions_are_idempotent_when_nothing_changed() {
    let folder_id = FolderId::new();
    let alice_id = DeviceId::new();
    let bob_id = DeviceId::new();
    let alice_identity = Identity::generate();
    let bob_identity = Identity::generate();

    let mut alice = side(
        "alice",
        alice_identity.secret_bytes(),
        alice_id,
        bob_id,
        &bob_identity,
        folder_id,
        &[("a.txt", b"payload")],
    );
    let mut bob = side(
        "bob",
        bob_identity.secret_bytes(),
        bob_id,
        alice_id,
        &alice_identity,
        folder_id,
        &[],
    );

    let (_, _) = sync_pair(&mut alice, &mut bob, 16 * 1024).await;
    let (alice_report, bob_report) = sync_pair(&mut alice, &mut bob, 16 * 1024).await;
    assert_eq!(alice_report.delivered, 0);
    assert_eq!(bob_report.applied, 0);
    assert_eq!(alice_report.conflicts, 0);
    assert_eq!(read(&bob.root.join("a.txt")), b"payload");
}

#[tokio::test]
async fn idle_sessions_end_cleanly() {
    let folder_id = FolderId::new();
    let alice_id = DeviceId::new();
    let bob_id = DeviceId::new();
    let alice_identity = Identity::generate();
    let bob_identity = Identity::generate();

    let mut alice = side(
        "alice",
        alice_identity.secret_bytes(),
        alice_id,
        bob_id,
        &bob_identity,
        folder_id,
        &[],
    );
    let mut bob = side(
        "bob",
        bob_identity.secret_bytes(),
        bob_id,
        alice_id,
        &alice_identity,
        folder_id,
        &[],
    );

    let options = test_options(16 * 1024);
    let (alice_stream, bob_stream) = duplex(64 * 1024);
    let (a, b) = tokio::join!(
        async {
            let alice = &mut alice;
            let mut channel =
                PeerChannel::new(Box::new(FramedIo::new(alice_stream)), &alice.identity);
            handshake(
                &mut channel,
                &alice.identity,
                alice.device_id,
                Role::Initiator,
            )
            .await
            .expect("handshake");
            tokio::time::timeout(
                Duration::from_secs(5),
                run_session(&mut alice.engine, &mut channel, &alice.peer, &options),
            )
            .await
            .expect("an idle session must end by itself")
        },
        async {
            let bob = &mut bob;
            let mut channel = PeerChannel::new(Box::new(FramedIo::new(bob_stream)), &bob.identity);
            handshake(&mut channel, &bob.identity, bob.device_id, Role::Responder)
                .await
                .expect("handshake");
            tokio::time::timeout(
                Duration::from_secs(5),
                run_session(&mut bob.engine, &mut channel, &bob.peer, &options),
            )
            .await
            .expect("an idle session must end by itself")
        }
    );
    assert_eq!(a.expect("alice report").delivered, 0);
    assert_eq!(b.expect("bob report").delivered, 0);
}

#[tokio::test]
async fn concurrent_edit_versus_delete_preserves_both_revisions_once() {
    let folder_id = FolderId::new();
    let alice_id = DeviceId::new();
    let bob_id = DeviceId::new();
    let alice_identity = Identity::generate();
    let bob_identity = Identity::generate();

    let mut alice = side(
        "alice",
        alice_identity.secret_bytes(),
        alice_id,
        bob_id,
        &bob_identity,
        folder_id,
        &[("doc.txt", b"v1")],
    );
    let mut bob = side(
        "bob",
        bob_identity.secret_bytes(),
        bob_id,
        alice_id,
        &alice_identity,
        folder_id,
        &[],
    );

    let (_, bob_first) = sync_pair(&mut alice, &mut bob, 16 * 1024).await;
    assert_eq!(bob_first.applied, 1);
    assert_eq!(read(&bob.root.join("doc.txt")), b"v1");

    // Bob edits while alice deletes the same path.
    std::fs::write(bob.root.join("doc.txt"), b"edit from bob").unwrap();
    bob.engine
        .queue_local_changes(&folder_id)
        .expect("queue bob");
    std::fs::remove_file(alice.root.join("doc.txt")).unwrap();
    alice
        .engine
        .queue_local_changes(&folder_id)
        .expect("queue alice");

    let (alice_report, bob_report) = sync_pair(&mut alice, &mut bob, 16 * 1024).await;

    assert_eq!(bob_report.conflicts, 1, "bob preserves his edit");
    assert_eq!(
        bob_report.applied, 0,
        "the delete is a conflict, not an apply"
    );
    assert_eq!(bob_report.delivered, 1, "bob's edit is acknowledged");
    assert_eq!(alice_report.conflicts, 1, "alice preserves bob's edit");
    assert_eq!(alice_report.applied, 0);
    assert_eq!(alice_report.delivered, 1, "the delete is acknowledged");
    assert_eq!(alice_report.errors, 0);
    assert_eq!(bob_report.errors, 0);

    assert!(
        !bob.root.join("doc.txt").exists(),
        "the delete wins the path"
    );
    assert!(!alice.root.join("doc.txt").exists());
    assert!(
        alice
            .engine
            .store()
            .pending_for_peer(&bob_id)
            .unwrap()
            .is_empty()
    );
    assert!(
        bob.engine
            .store()
            .pending_for_peer(&alice_id)
            .unwrap()
            .is_empty()
    );

    for (label, side) in [("bob", &bob), ("alice", &alice)] {
        let copies: Vec<_> = std::fs::read_dir(&side.root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains("conflict"))
            .collect();
        assert_eq!(copies.len(), 1, "{label} keeps exactly one copy");
        assert_eq!(
            read(&copies[0].path()),
            b"edit from bob",
            "{label}'s copy holds the surviving revision"
        );
        assert_eq!(
            side.engine.store().list_conflicts(None).unwrap().len(),
            1,
            "{label} records exactly one conflict"
        );
    }
}

#[tokio::test]
async fn renames_propagate_without_conflicts() {
    let folder_id = FolderId::new();
    let alice_id = DeviceId::new();
    let bob_id = DeviceId::new();
    let alice_identity = Identity::generate();
    let bob_identity = Identity::generate();

    let mut alice = side(
        "alice",
        alice_identity.secret_bytes(),
        alice_id,
        bob_id,
        &bob_identity,
        folder_id,
        &[("old.txt", b"renamed content")],
    );
    let mut bob = side(
        "bob",
        bob_identity.secret_bytes(),
        bob_id,
        alice_id,
        &alice_identity,
        folder_id,
        &[],
    );

    let (_, bob_first) = sync_pair(&mut alice, &mut bob, 16 * 1024).await;
    assert_eq!(bob_first.applied, 1);
    assert_eq!(read(&bob.root.join("old.txt")), b"renamed content");

    std::fs::rename(alice.root.join("old.txt"), alice.root.join("new.txt")).unwrap();
    alice
        .engine
        .queue_local_changes(&folder_id)
        .expect("queue alice");

    let (alice_report, bob_report) = sync_pair(&mut alice, &mut bob, 16 * 1024).await;

    assert_eq!(alice_report.delivered, 2, "the rename pair is acknowledged");
    assert_eq!(bob_report.applied, 2);
    assert_eq!(bob_report.conflicts, 0, "renames are not conflicts");
    assert_eq!(bob_report.errors, 0);
    assert!(!bob.root.join("old.txt").exists());
    assert_eq!(read(&bob.root.join("new.txt")), b"renamed content");
    assert!(
        alice
            .engine
            .store()
            .pending_for_peer(&bob_id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn apply_then_crash_before_ack_is_recovered_by_the_next_session() {
    let folder_id = FolderId::new();
    let alice_id = DeviceId::new();
    let bob_id = DeviceId::new();
    let alice_identity = Identity::generate();
    let bob_identity = Identity::generate();

    let payload = b"crash-recovery payload";

    let mut alice = side(
        "alice",
        alice_identity.secret_bytes(),
        alice_id,
        bob_id,
        &bob_identity,
        folder_id,
        &[("crash.txt", payload.as_slice())],
    );
    let mut bob = side(
        "bob",
        bob_identity.secret_bytes(),
        bob_id,
        alice_id,
        &alice_identity,
        folder_id,
        &[],
    );

    // Bob already applied the change in an earlier attempt, but the ack never
    // reached alice (crash between apply and acknowledgement).
    let change = alice
        .engine
        .store()
        .pending_for_peer(&bob_id)
        .unwrap()
        .into_iter()
        .next()
        .expect("alice's queued change");
    let hash = change.content_hash.clone().unwrap();
    let bytes = std::fs::read(alice.engine.staging().path_for(&hash)).unwrap();
    bob.engine.staging().stage_bytes(&bytes).unwrap();
    assert_eq!(
        bob.engine.commit_remote_file(&change).unwrap(),
        syncengine_engine::ApplyOutcome::Applied
    );
    assert!(bob.engine.store().is_applied(&change.change_id).unwrap());

    let (alice_report, bob_report) = sync_pair(&mut alice, &mut bob, 16 * 1024).await;

    assert_eq!(alice_report.delivered, 1, "the redelivery is acknowledged");
    assert_eq!(bob_report.applied, 0, "nothing is applied twice");
    assert_eq!(bob_report.conflicts, 0);
    assert_eq!(alice_report.errors, 0);
    assert_eq!(bob_report.errors, 0);
    assert_eq!(read(&bob.root.join("crash.txt")), payload);
    assert!(
        alice
            .engine
            .store()
            .pending_for_peer(&bob_id)
            .unwrap()
            .is_empty()
    );
}
