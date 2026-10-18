//! Relay transport: a WebSocket through the coordination server.
//!
//! The server only forwards opaque binary frames (SRD §4.1); authentication
//! uses the same control-plane signature scheme as the HTTP API, carried in
//! query parameters because WebSocket clients cannot always set headers.

use std::time::{SystemTime, UNIX_EPOCH};

use syncengine_engine::Identity;
use syncengine_protocol::DeviceId;
use syncengine_protocol::control::{
    HEADER_DEVICE_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, canonical_request, sha256_hex,
};

use crate::channel::{PeerChannel, WsIo};
use crate::error::TransportError;
use crate::handshake::{HELLO_TIMEOUT, HelloInfo, Role, handshake};

/// Connects to a relay session as `device_id` and completes the handshake.
///
/// The sender of a session acts as [`Role::Initiator`] (sends the hello
/// first); the receiver waits as [`Role::Responder`].
pub async fn connect_relay<'a>(
    server_base: &str,
    session_id: &str,
    identity: &'a Identity,
    device_id: DeviceId,
    role: Role,
) -> Result<(PeerChannel<'a>, HelloInfo), TransportError> {
    let url = relay_url(server_base, session_id, identity, device_id)?;
    let (socket, _response) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(map_connect_error)?;
    let mut channel = PeerChannel::new(Box::new(WsIo::new(socket)), identity);
    let info = tokio::time::timeout(HELLO_TIMEOUT * 2, async {
        handshake(&mut channel, identity, device_id, role).await
    })
    .await
    .map_err(|_| TransportError::Timeout(HELLO_TIMEOUT * 2))??;
    Ok((channel, info))
}

/// Builds the authenticated WebSocket URL for a relay session.
pub fn relay_url(
    server_base: &str,
    session_id: &str,
    identity: &Identity,
    device_id: DeviceId,
) -> Result<String, TransportError> {
    let base = server_base.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if base.starts_with("ws://") || base.starts_with("wss://") {
        base.to_string()
    } else {
        return Err(TransportError::Protocol(format!(
            "server url {server_base:?} must start with http://, https://, ws://, or wss://"
        )));
    };
    let path = format!("/v1/relay/{session_id}");
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TransportError::Protocol("system clock is before the unix epoch".into()))?
        .as_secs() as i64;
    let canonical = canonical_request("GET", &path, timestamp, &sha256_hex(b""));
    let signature = identity.sign_hex(canonical.as_bytes());
    Ok(format!(
        "{base}{path}?{HEADER_DEVICE_ID}={device_id}&{HEADER_TIMESTAMP}={timestamp}&{HEADER_SIGNATURE}={signature}"
    ))
}

fn map_connect_error(error: tokio_tungstenite::tungstenite::Error) -> TransportError {
    use tokio_tungstenite::tungstenite::Error as WsError;
    match error {
        WsError::Http(response) => {
            let status = response.status().as_u16();
            let body = String::from_utf8_lossy(response.body().as_deref().unwrap_or_default())
                .into_owned();
            TransportError::RelayRejected { status, body }
        }
        other => TransportError::WebSocket(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syncengine_protocol::control::{CONTROL_AUTH_CONTEXT, timestamp_is_fresh};

    #[test]
    fn relay_url_carries_a_valid_signature() {
        let identity = Identity::generate();
        let device_id = DeviceId::new();
        let url = relay_url(
            "http://127.0.0.1:8443/",
            "11111111-1111-1111-1111-111111111111",
            &identity,
            device_id,
        )
        .unwrap();

        assert!(url.starts_with("ws://127.0.0.1:8443/v1/relay/11111111-"));
        let parameters: std::collections::HashMap<_, _> = url
            .split_once('?')
            .expect("query parameters")
            .1
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .collect();
        assert_eq!(parameters[HEADER_DEVICE_ID], device_id.to_string());
        let timestamp: i64 = parameters[HEADER_TIMESTAMP].parse().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(timestamp_is_fresh(timestamp, now));
        let expected = canonical_request(
            "GET",
            "/v1/relay/11111111-1111-1111-1111-111111111111",
            timestamp,
            &sha256_hex(b""),
        );
        assert!(expected.starts_with(CONTROL_AUTH_CONTEXT));
        Identity::verify_hex(
            &identity.ed25519_public_hex(),
            expected.as_bytes(),
            parameters[HEADER_SIGNATURE],
        )
        .expect("the URL must carry a valid control-plane signature");
    }

    #[test]
    fn relay_url_rejects_unknown_schemes() {
        let identity = Identity::generate();
        let device_id = DeviceId::new();
        assert!(relay_url("ftp://example.com", "id", &identity, device_id).is_err());
        assert!(relay_url("wss://example.com", "id", &identity, device_id).is_ok());
        assert!(relay_url("https://example.com", "id", &identity, device_id).is_ok());
    }
}
