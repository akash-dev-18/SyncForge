//! Peer handshake: prove device identity before any session runs.
//!
//! The initiator sends a plaintext [`PeerMessage::Hello`] carrying its device
//! ID, public keys, and a signature over
//! [`PeerMessage::hello_transcript`]. The responder verifies it and answers
//! with a [`PeerMessage::HelloAccepted`] whose fields are signed the same way.
//! Both sides then switch to sealed frames.

use std::time::Duration;

use syncengine_engine::Identity;
use syncengine_protocol::DeviceId;
use syncengine_protocol::messages::{PROTOCOL_VERSION, PeerMessage};
use syncengine_storage::{Peer, TrustState};

use crate::channel::PeerChannel;
use crate::error::TransportError;

/// How long one side waits for the other's hello frame.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(15);

/// Which side of the handshake this device plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Dials or connects first and sends [`PeerMessage::Hello`].
    Initiator,
    /// Waits for a hello, then answers with [`PeerMessage::HelloAccepted`].
    Responder,
}

/// Verified identity claims from one side of the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloInfo {
    pub device_id: DeviceId,
    pub signing_key: String,
    pub encryption_key: String,
    pub protocol_version: u32,
}

fn signed_fields(identity: &Identity, device_id: DeviceId) -> (String, String, String) {
    let signing_key = identity.ed25519_public_hex();
    let encryption_key = identity.x25519_public_hex();
    let transcript = PeerMessage::hello_transcript(&device_id, &signing_key, &encryption_key);
    let signature = identity.sign_hex(transcript.as_bytes());
    (signing_key, encryption_key, signature)
}

fn build_hello(identity: &Identity, device_id: DeviceId) -> PeerMessage {
    let (signing_key, encryption_key, signature) = signed_fields(identity, device_id);
    PeerMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        device_id,
        signing_key,
        encryption_key,
        signature,
    }
}

fn build_hello_accepted(identity: &Identity, device_id: DeviceId) -> PeerMessage {
    let (signing_key, encryption_key, signature) = signed_fields(identity, device_id);
    PeerMessage::HelloAccepted {
        protocol_version: PROTOCOL_VERSION,
        device_id,
        signing_key,
        encryption_key,
        signature,
    }
}

fn verify_fields(
    protocol_version: u32,
    device_id: DeviceId,
    signing_key: &str,
    encryption_key: &str,
    signature: &str,
) -> Result<HelloInfo, TransportError> {
    if protocol_version != PROTOCOL_VERSION {
        return Err(TransportError::Protocol(format!(
            "peer speaks protocol version {protocol_version}, this build speaks {PROTOCOL_VERSION}"
        )));
    }
    let transcript = PeerMessage::hello_transcript(&device_id, signing_key, encryption_key);
    Identity::verify_hex(signing_key, transcript.as_bytes(), signature)
        .map_err(|_| TransportError::Unauthorized("hello signature is invalid".into()))?;
    Ok(HelloInfo {
        device_id,
        signing_key: signing_key.to_ascii_lowercase(),
        encryption_key: encryption_key.to_ascii_lowercase(),
        protocol_version,
    })
}

/// Runs the handshake for `role`, returning the peer's verified claims and
/// leaving the channel sealed.
pub async fn handshake(
    channel: &mut PeerChannel<'_>,
    identity: &Identity,
    device_id: DeviceId,
    role: Role,
) -> Result<HelloInfo, TransportError> {
    match role {
        Role::Initiator => {
            let hello = build_hello(identity, device_id);
            tokio::time::timeout(HELLO_TIMEOUT, channel.send(&hello))
                .await
                .map_err(|_| TransportError::Timeout(HELLO_TIMEOUT))??;

            let response = tokio::time::timeout(HELLO_TIMEOUT, channel.recv())
                .await
                .map_err(|_| TransportError::Timeout(HELLO_TIMEOUT))??;
            let peer = match response {
                Some(PeerMessage::HelloAccepted {
                    protocol_version,
                    device_id,
                    signing_key,
                    encryption_key,
                    signature,
                }) => verify_fields(
                    protocol_version,
                    device_id,
                    &signing_key,
                    &encryption_key,
                    &signature,
                )?,
                Some(PeerMessage::Error { code, message }) => {
                    return Err(TransportError::Unauthorized(format!("{code}: {message}")));
                }
                Some(other) => {
                    return Err(TransportError::Protocol(format!(
                        "expected hello_accepted, got {other:?}"
                    )));
                }
                None => return Err(TransportError::Closed),
            };
            channel.enable_sealing(&peer.encryption_key);
            Ok(peer)
        }
        Role::Responder => {
            let response = tokio::time::timeout(HELLO_TIMEOUT, channel.recv())
                .await
                .map_err(|_| TransportError::Timeout(HELLO_TIMEOUT))??;
            let peer = match response {
                Some(PeerMessage::Hello {
                    protocol_version,
                    device_id,
                    signing_key,
                    encryption_key,
                    signature,
                }) => verify_fields(
                    protocol_version,
                    device_id,
                    &signing_key,
                    &encryption_key,
                    &signature,
                )?,
                Some(PeerMessage::Error { code, message }) => {
                    return Err(TransportError::Unauthorized(format!("{code}: {message}")));
                }
                Some(other) => {
                    return Err(TransportError::Protocol(format!(
                        "expected hello, got {other:?}"
                    )));
                }
                None => return Err(TransportError::Closed),
            };
            let accepted = build_hello_accepted(identity, device_id);
            tokio::time::timeout(HELLO_TIMEOUT, channel.send(&accepted))
                .await
                .map_err(|_| TransportError::Timeout(HELLO_TIMEOUT))??;
            channel.enable_sealing(&peer.encryption_key);
            Ok(peer)
        }
    }
}

/// Checks the peer's claims against the locally stored, trusted device record.
///
/// Discovery and handshakes alone never grant access (SRD §4.2): the device
/// must already be paired, its stored keys must match exactly, and it must be
/// trusted.
pub fn verify_peer(hello: &HelloInfo, peer: &Peer) -> Result<(), TransportError> {
    if hello.device_id != peer.device_id {
        return Err(TransportError::Unauthorized(format!(
            "hello claims device {}, expected {}",
            hello.device_id, peer.device_id
        )));
    }
    if !hello.signing_key.eq_ignore_ascii_case(&peer.public_key) {
        return Err(TransportError::Unauthorized(
            "the peer's signing key does not match the paired device".into(),
        ));
    }
    if !hello
        .encryption_key
        .eq_ignore_ascii_case(&peer.encryption_key)
    {
        return Err(TransportError::Unauthorized(
            "the peer's encryption key does not match the paired device".into(),
        ));
    }
    if peer.trust != TrustState::Trusted {
        return Err(TransportError::Unauthorized(format!(
            "device {} is {:?}, not trusted",
            peer.device_id, peer.trust
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::FramedIo;
    use tokio::io::duplex;

    fn test_peer(identity: &Identity, device_id: DeviceId) -> Peer {
        Peer {
            device_id,
            device_name: "peer".into(),
            public_key: identity.ed25519_public_hex(),
            encryption_key: identity.x25519_public_hex(),
            platform: "linux".into(),
            trust: TrustState::Trusted,
            last_seen_at: None,
            created_at: "now".into(),
        }
    }

    #[tokio::test]
    async fn handshake_exchanges_verified_identities() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_id = DeviceId::new();
        let bob_id = DeviceId::new();
        let (alice_stream, bob_stream) = duplex(64 * 1024);

        let alice_identity = &alice;
        let bob_identity = &bob;
        let (alice_result, bob_result) = tokio::join!(
            async {
                let mut channel =
                    PeerChannel::new(Box::new(FramedIo::new(alice_stream)), alice_identity);
                handshake(&mut channel, alice_identity, alice_id, Role::Initiator).await
            },
            async {
                let mut channel =
                    PeerChannel::new(Box::new(FramedIo::new(bob_stream)), bob_identity);
                handshake(&mut channel, bob_identity, bob_id, Role::Responder).await
            }
        );

        let bob_info = alice_result.expect("initiator handshake");
        let alice_info = bob_result.expect("responder handshake");
        assert_eq!(bob_info.device_id, bob_id);
        assert_eq!(alice_info.device_id, alice_id);
        assert!(verify_peer(&bob_info, &test_peer(&bob, bob_id)).is_ok());
        assert!(verify_peer(&alice_info, &test_peer(&alice, alice_id)).is_ok());
    }

    #[test]
    fn verify_peer_rejects_key_mismatch_and_distrust() {
        let alice = Identity::generate();
        let other = Identity::generate();
        let alice_id = DeviceId::new();
        let hello = HelloInfo {
            device_id: alice_id,
            signing_key: alice.ed25519_public_hex(),
            encryption_key: alice.x25519_public_hex(),
            protocol_version: PROTOCOL_VERSION,
        };

        assert!(verify_peer(&hello, &test_peer(&alice, alice_id)).is_ok());

        let mut wrong_key = test_peer(&other, alice_id);
        wrong_key.encryption_key = alice.x25519_public_hex();
        assert!(matches!(
            verify_peer(&hello, &wrong_key),
            Err(TransportError::Unauthorized(_))
        ));

        let mut pending = test_peer(&alice, alice_id);
        pending.trust = TrustState::Pending;
        assert!(matches!(
            verify_peer(&hello, &pending),
            Err(TransportError::Unauthorized(_))
        ));

        let other_id = DeviceId::new();
        assert!(verify_peer(&hello, &test_peer(&alice, other_id)).is_err());
    }
}
