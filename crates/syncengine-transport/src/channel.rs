//! Frame channels and the sealed peer channel.
//!
//! A [`PeerChannel`] wraps any [`FrameIo`] implementation and, once sealing is
//! enabled after the handshake, encrypts every outgoing frame and decrypts
//! every incoming one with the peer's static X25519 key.

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use syncengine_engine::Identity;
use syncengine_protocol::messages::PeerMessage;

use crate::codec::{read_frame, write_frame};
use crate::error::TransportError;

/// A duplex stream of raw payload frames.
#[async_trait]
pub trait FrameIo: Send {
    /// Sends one payload.
    async fn send(&mut self, payload: &[u8]) -> Result<(), TransportError>;
    /// Receives one payload, or `None` when the peer closed cleanly.
    async fn recv(&mut self) -> Result<Option<Vec<u8>>, TransportError>;
}

/// Length-prefixed framing over any byte stream (used for LAN TCP).
pub struct FramedIo<S> {
    stream: S,
}

impl<S> FramedIo<S> {
    pub fn new(stream: S) -> Self {
        Self { stream }
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send> FrameIo for FramedIo<S> {
    async fn send(&mut self, payload: &[u8]) -> Result<(), TransportError> {
        write_frame(&mut self.stream, payload).await
    }

    async fn recv(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        read_frame(&mut self.stream).await
    }
}

/// One WebSocket connection (used for the relay).
pub struct WsIo {
    socket: WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
}

impl WsIo {
    pub fn new(socket: WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>) -> Self {
        Self { socket }
    }

    pub fn into_inner(self) -> WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
        self.socket
    }
}

#[async_trait]
impl FrameIo for WsIo {
    async fn send(&mut self, payload: &[u8]) -> Result<(), TransportError> {
        self.socket
            .send(Message::Binary(payload.to_vec().into()))
            .await
            .map_err(|error| TransportError::WebSocket(error.to_string()))
    }

    async fn recv(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        loop {
            let message = match self.socket.next().await {
                None => return Ok(None),
                Some(Ok(message)) => message,
                Some(Err(error)) => return Err(TransportError::WebSocket(error.to_string())),
            };
            match message {
                Message::Binary(bytes) => return Ok(Some(bytes.to_vec())),
                Message::Text(text) => return Ok(Some(text.as_bytes().to_vec())),
                Message::Ping(payload) => {
                    let _ = self.socket.send(Message::Pong(payload)).await;
                }
                Message::Pong(_) => continue,
                Message::Close(frame) => {
                    let _ = self
                        .socket
                        .send(Message::Close(Some(CloseFrame {
                            code: CloseCode::Normal,
                            reason: "closed".into(),
                        })))
                        .await;
                    tracing::debug!(?frame, "relay websocket closed");
                    return Ok(None);
                }
                Message::Frame(_) => continue,
            }
        }
    }
}

/// An authenticated, optionally sealed message channel to one peer.
///
/// Before sealing is enabled (during the handshake) frames travel as plain
/// JSON; afterwards every frame is `nonce || ciphertext`.
pub struct PeerChannel<'a> {
    io: Box<dyn FrameIo + 'a>,
    identity: &'a Identity,
    peer_encryption_key: Option<String>,
}

impl<'a> PeerChannel<'a> {
    /// Builds a channel over `io`. Sealing stays off until
    /// [`PeerChannel::enable_sealing`] is called with the peer's X25519 key.
    pub fn new(io: Box<dyn FrameIo + 'a>, identity: &'a Identity) -> Self {
        Self {
            io,
            identity,
            peer_encryption_key: None,
        }
    }

    /// Whether outgoing frames are currently sealed.
    pub fn is_sealed(&self) -> bool {
        self.peer_encryption_key.is_some()
    }

    /// Starts sealing all frames with `peer_x25519_hex`.
    pub fn enable_sealing(&mut self, peer_x25519_hex: &str) {
        self.peer_encryption_key = Some(peer_x25519_hex.to_ascii_lowercase());
    }

    /// Sends one protocol message.
    pub async fn send(&mut self, message: &PeerMessage) -> Result<(), TransportError> {
        let plaintext = message
            .to_bytes()
            .map_err(|error| TransportError::Malformed(error.to_string()))?;
        match &self.peer_encryption_key {
            Some(key) => {
                let sealed = self.identity.seal(key, &plaintext)?;
                self.io.send(&sealed).await
            }
            None => self.io.send(&plaintext).await,
        }
    }

    /// Receives one protocol message. `None` means a clean close.
    pub async fn recv(&mut self) -> Result<Option<PeerMessage>, TransportError> {
        let raw = match self.io.recv().await? {
            Some(raw) => raw,
            None => return Ok(None),
        };
        let plaintext = match &self.peer_encryption_key {
            Some(key) => self.identity.open(key, &raw)?,
            None => raw,
        };
        let message = PeerMessage::from_bytes(&plaintext)
            .map_err(|error| TransportError::Malformed(error.to_string()))?;
        Ok(Some(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syncengine_protocol::DeviceId;
    use tokio::io::duplex;

    #[tokio::test]
    async fn sealed_channels_round_trip_messages() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let (alice_stream, bob_stream) = duplex(64 * 1024);

        let mut alice_channel = PeerChannel::new(Box::new(FramedIo::new(alice_stream)), &alice);
        let mut bob_channel = PeerChannel::new(Box::new(FramedIo::new(bob_stream)), &bob);

        alice_channel.enable_sealing(&bob.x25519_public_hex());
        bob_channel.enable_sealing(&alice.x25519_public_hex());
        assert!(alice_channel.is_sealed());

        alice_channel.send(&PeerMessage::Ping).await.unwrap();
        assert_eq!(bob_channel.recv().await.unwrap(), Some(PeerMessage::Ping));

        bob_channel
            .send(&PeerMessage::Error {
                code: "X".into(),
                message: "y".into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            alice_channel.recv().await.unwrap(),
            Some(PeerMessage::Error { .. })
        ));
    }

    #[tokio::test]
    async fn unsealed_channels_send_plain_json() {
        let alice = Identity::generate();
        let (alice_stream, mut raw_observer) = duplex(64 * 1024);
        let mut channel = PeerChannel::new(Box::new(FramedIo::new(alice_stream)), &alice);
        channel.send(&PeerMessage::Ping).await.unwrap();
        let frame = read_frame(&mut raw_observer).await.unwrap().unwrap();
        assert_eq!(frame, br#"{"type":"ping"}"#);
    }

    #[tokio::test]
    async fn sealed_frames_do_not_leak_plaintext() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let (alice_stream, mut raw_observer) = duplex(64 * 1024);
        let mut channel = PeerChannel::new(Box::new(FramedIo::new(alice_stream)), &alice);
        channel.enable_sealing(&bob.x25519_public_hex());
        channel.send(&PeerMessage::Ping).await.unwrap();
        let frame = read_frame(&mut raw_observer).await.unwrap().unwrap();
        assert!(!frame.windows(4).any(|window| window == b"ping"));
    }

    #[tokio::test]
    async fn a_stranger_cannot_decrypt_sealed_frames() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let eve = Identity::generate();
        let (alice_stream, bob_stream) = duplex(64 * 1024);
        let mut alice_channel = PeerChannel::new(Box::new(FramedIo::new(alice_stream)), &alice);
        alice_channel.enable_sealing(&bob.x25519_public_hex());
        let mut eve_channel = PeerChannel::new(Box::new(FramedIo::new(bob_stream)), &eve);
        eve_channel.enable_sealing(&alice.x25519_public_hex());
        alice_channel.send(&PeerMessage::Ping).await.unwrap();
        assert!(matches!(
            eve_channel.recv().await,
            Err(TransportError::Crypto(_))
        ));
    }

    #[test]
    fn hello_transcript_helper_is_stable() {
        let device = DeviceId::new();
        let transcript = PeerMessage::hello_transcript(&device, "aa", "bb");
        assert!(transcript.starts_with("syncengine-hello-v1\n"));
    }
}
