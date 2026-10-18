//! Database row types shared by handlers.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::db::to_rfc3339;

/// A row of `devices`.
#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub device_id: Uuid,
    pub device_name: String,
    pub platform: String,
    pub public_key: String,
    pub encryption_key: Option<String>,
    pub status: String,
    pub created_at: OffsetDateTime,
    pub last_seen_at: Option<OffsetDateTime>,
}

/// A row of `pairing_requests`.
#[derive(Debug, Clone)]
pub struct PairingRequestRow {
    pub request_id: Uuid,
    pub initiator_device_id: Uuid,
    pub target_device_id: Uuid,
    pub code_hash: String,
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub expires_at: OffsetDateTime,
    pub created_at: OffsetDateTime,
    pub approved_at: Option<OffsetDateTime>,
}

/// A row of `relay_sessions`.
#[derive(Debug, Clone)]
pub struct RelaySessionRow {
    pub session_id: Uuid,
    pub sender_device_id: Uuid,
    pub receiver_device_id: Uuid,
    pub folder_id: Uuid,
    pub status: String,
    pub expires_at: OffsetDateTime,
    pub created_at: OffsetDateTime,
}

impl DeviceRow {
    pub fn from_row(row: &tokio_postgres::Row) -> Result<Self, tokio_postgres::Error> {
        Ok(Self {
            device_id: row.try_get("device_id")?,
            device_name: row.try_get("device_name")?,
            platform: row.try_get("platform")?,
            public_key: row.try_get("public_key")?,
            encryption_key: row.try_get("encryption_key")?,
            status: row.try_get("status")?,
            created_at: row.try_get("created_at")?,
            last_seen_at: row.try_get("last_seen_at")?,
        })
    }

    /// The public JSON view of a device (never includes the public key).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "device_id": self.device_id,
            "device_name": self.device_name,
            "platform": self.platform,
            "status": self.status,
            "created_at": to_rfc3339(self.created_at),
            "last_seen_at": self.last_seen_at.map(to_rfc3339),
        })
    }
}

impl PairingRequestRow {
    pub fn from_row(row: &tokio_postgres::Row) -> Result<Self, tokio_postgres::Error> {
        Ok(Self {
            request_id: row.try_get("request_id")?,
            initiator_device_id: row.try_get("initiator_device_id")?,
            target_device_id: row.try_get("target_device_id")?,
            code_hash: row.try_get("code_hash")?,
            status: row.try_get("status")?,
            attempts: row.try_get("attempts")?,
            max_attempts: row.try_get("max_attempts")?,
            expires_at: row.try_get("expires_at")?,
            created_at: row.try_get("created_at")?,
            approved_at: row.try_get("approved_at")?,
        })
    }
}

impl RelaySessionRow {
    pub fn from_row(row: &tokio_postgres::Row) -> Result<Self, tokio_postgres::Error> {
        Ok(Self {
            session_id: row.try_get("session_id")?,
            sender_device_id: row.try_get("sender_device_id")?,
            receiver_device_id: row.try_get("receiver_device_id")?,
            folder_id: row.try_get("folder_id")?,
            status: row.try_get("status")?,
            expires_at: row.try_get("expires_at")?,
            created_at: row.try_get("created_at")?,
        })
    }
}
