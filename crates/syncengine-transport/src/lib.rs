//! Peer transports and the sync session driver.
//!
//! Two devices exchange [`syncengine_protocol::messages::PeerMessage`] frames
//! over one of two transports:
//!
//! - **LAN**: a direct TCP connection found through mDNS ([`discovery`],
//!   [`lan`]), or
//! - **relay**: a WebSocket connection through the coordination server
//!   ([`relay`]), which forwards opaque bytes only.
//!
//! On both transports the first frame is a plaintext [`PeerMessage::Hello`]
//! exchange that proves possession of the device's Ed25519 key. Every frame
//! after the handshake is sealed with the two devices' X25519 keys
//! (ChaCha20-Poly1305), so file contents are end-to-end protected even when
//! they travel through the relay.
//!
//! [`session::run_session`] drives one complete synchronization exchange over
//! an established [`channel::PeerChannel`].

pub mod channel;
pub mod codec;
pub mod discovery;
pub mod error;
pub mod handshake;
pub mod lan;
pub mod relay;
pub mod session;

pub use channel::{FrameIo, FramedIo, PeerChannel, WsIo};
pub use discovery::{Advertiser, SERVICE_TYPE, find_peer};
pub use error::TransportError;
pub use handshake::{HelloInfo, Role, verify_peer};
pub use lan::{LanListener, connect_lan};
pub use relay::connect_relay;
pub use session::{SessionOptions, SessionReport, run_session};
