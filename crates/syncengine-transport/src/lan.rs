//! LAN transport: direct TCP connections between paired devices.
//!
//! The wire format is the length-prefixed framing from [`crate::codec`]; the
//! handshake identifies both devices and switches the channel to sealed
//! frames, so LAN traffic is authenticated and encrypted as well.

use std::net::SocketAddr;

use tokio::net::{TcpListener, TcpStream};

use syncengine_engine::Identity;
use syncengine_protocol::DeviceId;

use crate::channel::{FramedIo, PeerChannel};
use crate::error::TransportError;
use crate::handshake::{HELLO_TIMEOUT, HelloInfo, Role, handshake};

/// Dials a LAN peer and completes the responder-verified handshake.
pub async fn connect_lan(
    addr: SocketAddr,
    identity: &Identity,
    device_id: DeviceId,
) -> Result<(PeerChannel<'_>, HelloInfo), TransportError> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    let mut channel = PeerChannel::new(Box::new(FramedIo::new(stream)), identity);
    let info = tokio::time::timeout(HELLO_TIMEOUT * 2, async {
        handshake(&mut channel, identity, device_id, Role::Initiator).await
    })
    .await
    .map_err(|_| TransportError::Timeout(HELLO_TIMEOUT * 2))??;
    Ok((channel, info))
}

/// Accepts inbound LAN connections. Bind to port 0 to let the OS choose.
pub struct LanListener {
    listener: TcpListener,
}

impl LanListener {
    /// Binds the listener (typically on `0.0.0.0:<lan_port>`).
    pub async fn bind(addr: SocketAddr) -> Result<Self, TransportError> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self { listener })
    }

    /// The address actually bound (useful with port 0).
    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.listener.local_addr()?)
    }

    /// The port actually bound.
    pub fn port(&self) -> Result<u16, TransportError> {
        Ok(self.local_addr()?.port())
    }

    /// Waits for one inbound connection and verifies the caller's handshake.
    /// The caller must still check the returned [`HelloInfo`] against its
    /// trusted peers (see [`crate::handshake::verify_peer`]) before running a
    /// session.
    pub async fn accept<'a>(
        &'a self,
        identity: &'a Identity,
        device_id: DeviceId,
    ) -> Result<(PeerChannel<'a>, HelloInfo, SocketAddr), TransportError> {
        let (stream, addr) = self.listener.accept().await?;
        stream.set_nodelay(true)?;
        let mut channel = PeerChannel::new(Box::new(FramedIo::new(stream)), identity);
        let info = tokio::time::timeout(HELLO_TIMEOUT * 2, async {
            handshake(&mut channel, identity, device_id, Role::Responder).await
        })
        .await
        .map_err(|_| TransportError::Timeout(HELLO_TIMEOUT * 2))??;
        Ok((channel, info, addr))
    }
}
