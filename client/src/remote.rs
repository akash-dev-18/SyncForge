//! Signed client for the coordination server's `/v1` control plane.
//!
//! Every request is authenticated with the device's Ed25519 key using the
//! canonical request scheme shared with the server
//! ([`syncengine_protocol::control`]). The client never sends raw key material;
//! the server already knows the device's public keys from registration, and
//! returns paired devices' keys through the dedicated keys endpoint.

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde_json::{Value, json};
use syncengine_engine::Identity;
use syncengine_protocol::DeviceId;
use syncengine_protocol::control::{
    HEADER_DEVICE_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, canonical_request,
    canonical_request_for_body, pairing_code_proof, sha256_hex,
};

/// Identity keys of a paired device, as served by `GET /v1/devices/{id}/keys`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceKeys {
    pub device_id: DeviceId,
    pub device_name: String,
    pub platform: String,
    pub signing_key: String,
    pub encryption_key: String,
}

/// A pairing request the caller created.
#[derive(Debug, Clone)]
pub struct PairingCreated {
    pub request_id: String,
    pub code: String,
    pub expires_at: String,
}

/// One open relay session involving this device.
#[derive(Debug, Clone)]
pub struct RelaySessionInfo {
    pub session_id: String,
    pub sender_device_id: DeviceId,
    pub receiver_device_id: DeviceId,
    pub folder_id: syncengine_protocol::FolderId,
}

/// Blocking-friendly HTTP client bound to one device identity.
pub struct RemoteClient<'a> {
    base: String,
    identity: &'a Identity,
    device_id: DeviceId,
    client: reqwest::Client,
}

impl<'a> RemoteClient<'a> {
    /// Builds a client for `base` (like `http://127.0.0.1:8443`).
    pub fn new(base: &str, identity: &'a Identity, device_id: DeviceId) -> Result<Self> {
        let base = base.trim().trim_end_matches('/').to_string();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            bail!("server URL {base:?} must start with http:// or https://");
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to build the HTTP client")?;
        Ok(Self {
            base,
            identity,
            device_id,
            client,
        })
    }

    /// The server base URL without a trailing slash.
    pub fn base(&self) -> &str {
        &self.base
    }

    fn signed_headers(&self, method: &str, path: &str, body: &[u8]) -> [(String, String); 3] {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs() as i64;
        let canonical = canonical_request_for_body(method, path, timestamp, body);
        [
            (HEADER_DEVICE_ID.to_string(), self.device_id.to_string()),
            (HEADER_TIMESTAMP.to_string(), timestamp.to_string()),
            (
                HEADER_SIGNATURE.to_string(),
                self.identity.sign_hex(canonical.as_bytes()),
            ),
        ]
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> reqwest::RequestBuilder {
        let bytes = body
            .as_ref()
            .map(|value| serde_json::to_vec(value).expect("json body"))
            .unwrap_or_default();
        let headers = self.signed_headers(method.as_str(), path, &bytes);
        let request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header(headers[0].0.clone(), headers[0].1.clone())
            .header(headers[1].0.clone(), headers[1].1.clone())
            .header(headers[2].0.clone(), headers[2].1.clone());
        match body {
            Some(_) => request
                .header("content-type", "application/json")
                .body(bytes),
            None => request,
        }
    }

    async fn send(&self, response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read the response")?;
        let value: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text)
                .with_context(|| format!("server returned invalid JSON (HTTP {status}): {text}"))?
        };
        if !status.is_success() {
            let code = value["error"]["code"].as_str().unwrap_or("UNKNOWN");
            let message = value["error"]["message"].as_str().unwrap_or("no message");
            bail!("server error {status} ({code}): {message}");
        }
        Ok(value)
    }

    /// `POST /v1/devices/register`; succeeds when the device is already
    /// registered with the same signing key (idempotent re-registration).
    pub async fn register(
        &self,
        device_name: &str,
        platform: &str,
        signing_key_hex: &str,
        encryption_key_hex: &str,
    ) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::POST,
                "/v1/devices/register",
                Some(json!({
                    "device_id": self.device_id,
                    "device_name": device_name,
                    "platform": platform,
                    "public_key": signing_key_hex,
                    "encryption_key": encryption_key_hex,
                })),
            )
            .send()
            .await
            .context("failed to reach the server for registration")?;
        self.send(response).await?;
        Ok(())
    }

    /// `POST /v1/pairing-requests`: asks the target device to pair.
    pub async fn create_pairing_request(&self, target: DeviceId) -> Result<PairingCreated> {
        let response = self
            .request(
                reqwest::Method::POST,
                "/v1/pairing-requests",
                Some(json!({"target_device_id": target})),
            )
            .send()
            .await
            .context("failed to reach the server to create a pairing request")?;
        let value = self.send(response).await?;
        Ok(PairingCreated {
            request_id: string_field(&value, "request_id")?,
            code: string_field(&value, "code")?,
            expires_at: value["expires_at"].as_str().unwrap_or("").to_string(),
        })
    }

    /// `GET /v1/pairing-requests/{id}`: `pending`, `approved`, or `expired`.
    pub async fn pairing_status(&self, request_id: &str) -> Result<PairingStatus> {
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("/v1/pairing-requests/{request_id}"),
                None,
            )
            .send()
            .await
            .context("failed to reach the server to check the pairing request")?;
        let value = self.send(response).await?;
        Ok(PairingStatus {
            status: string_field(&value, "status")?,
            initiator_device_id: parse_device_id(&value, "initiator_device_id")?,
            target_device_id: parse_device_id(&value, "target_device_id")?,
        })
    }

    /// `POST /v1/pairing-requests/{id}/approve` with the code proof.
    pub async fn approve_pairing_request(&self, request_id: &str, code: &str) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v1/pairing-requests/{request_id}/approve"),
                Some(json!({"proof": pairing_code_proof(request_id, code)})),
            )
            .send()
            .await
            .context("failed to reach the server to approve the pairing request")?;
        self.send(response).await?;
        Ok(())
    }

    /// `GET /v1/devices/{id}/keys`: identity keys of a paired device.
    pub async fn device_keys(&self, device_id: DeviceId) -> Result<DeviceKeys> {
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("/v1/devices/{device_id}/keys"),
                None,
            )
            .send()
            .await
            .context("failed to reach the server to fetch device keys")?;
        let value = self.send(response).await?;
        Ok(DeviceKeys {
            device_id: parse_device_id(&value, "device_id")?,
            device_name: string_field(&value, "device_name")?,
            platform: value["platform"].as_str().unwrap_or("unknown").to_string(),
            signing_key: string_field(&value, "signing_key")?,
            encryption_key: string_field(&value, "encryption_key")?,
        })
    }

    /// `PUT /v1/folders/{folder_id}/memberships`: announces that this device
    /// participates in the folder (needed for relay authorization).
    pub async fn upsert_membership(
        &self,
        folder_id: syncengine_protocol::FolderId,
        permission: &str,
    ) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::PUT,
                &format!("/v1/folders/{folder_id}/memberships"),
                Some(json!({"permission": permission})),
            )
            .send()
            .await
            .context("failed to reach the server to register the folder membership")?;
        self.send(response).await?;
        Ok(())
    }

    /// `POST /v1/relay-sessions`: opens an authorized relay path to a receiver.
    pub async fn create_relay_session(
        &self,
        receiver: DeviceId,
        folder_id: syncengine_protocol::FolderId,
    ) -> Result<String> {
        let response = self
            .request(
                reqwest::Method::POST,
                "/v1/relay-sessions",
                Some(json!({"receiver_device_id": receiver, "folder_id": folder_id})),
            )
            .send()
            .await
            .context("failed to reach the server to create a relay session")?;
        let value = self.send(response).await?;
        string_field(&value, "session_id")
    }

    /// `GET /v1/relay-sessions`: open relay sessions involving this device.
    pub async fn list_relay_sessions(&self) -> Result<Vec<RelaySessionInfo>> {
        let response = self
            .request(reqwest::Method::GET, "/v1/relay-sessions", None)
            .send()
            .await
            .context("failed to reach the server to list relay sessions")?;
        let value = self.send(response).await?;
        let sessions = value["sessions"]
            .as_array()
            .context("server response has no `sessions` array")?;
        sessions
            .iter()
            .map(|session| {
                Ok(RelaySessionInfo {
                    session_id: string_field(session, "session_id")?,
                    sender_device_id: parse_device_id(session, "sender_device_id")?,
                    receiver_device_id: parse_device_id(session, "receiver_device_id")?,
                    folder_id: parse_folder_id(session, "folder_id")?,
                })
            })
            .collect()
    }

    /// `DELETE /v1/devices/{id}`: revokes the active relationship with a peer.
    pub async fn revoke_relationship(&self, peer: DeviceId) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::DELETE,
                &format!("/v1/devices/{peer}"),
                None,
            )
            .send()
            .await
            .context("failed to reach the server to revoke the relationship")?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        self.send(response).await?;
        Ok(())
    }

    /// Builds the signed WebSocket URL for a relay session (forwarded to the
    /// transport layer; kept here so the signing logic lives in one place).
    pub fn relay_ws_url(&self, session_id: &str) -> Result<String> {
        let path = format!("/v1/relay/{session_id}");
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs() as i64;
        let canonical = canonical_request("GET", &path, timestamp, &sha256_hex(b""));
        let signature = self.identity.sign_hex(canonical.as_bytes());
        let ws_base = if let Some(rest) = self.base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else if let Some(rest) = self.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else {
            bail!("server URL {:?} cannot be used for WebSockets", self.base);
        };
        Ok(format!(
            "{ws_base}{path}?{HEADER_DEVICE_ID}={}&{HEADER_TIMESTAMP}={timestamp}&{HEADER_SIGNATURE}={signature}",
            self.device_id
        ))
    }
}

/// Status of a pairing request as seen by either participant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingStatus {
    pub status: String,
    pub initiator_device_id: DeviceId,
    pub target_device_id: DeviceId,
}

fn string_field(value: &Value, field: &str) -> Result<String> {
    value[field]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("server response is missing `{field}`"))
}

fn parse_device_id(value: &Value, field: &str) -> Result<DeviceId> {
    string_field(value, field)?
        .parse()
        .with_context(|| format!("`{field}` is not a valid device id"))
}

fn parse_folder_id(value: &Value, field: &str) -> Result<syncengine_protocol::FolderId> {
    string_field(value, field)?
        .parse()
        .with_context(|| format!("`{field}` is not a valid folder id"))
}
