//! The `/v1` control-plane API: device registration, pairing, folder
//! memberships, relay-session authorization, and the relay WebSocket.

use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use syncengine_protocol::control::{
    HEADER_DEVICE_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, canonical_request, pairing_code_proof,
    sha256_hex,
};
use syncengine_protocol::messages::MAX_FRAME_BYTES;
use time::OffsetDateTime;
use tokio_postgres::error::SqlState;
use uuid::Uuid;

use crate::auth;
use crate::db::{PgPool, to_rfc3339};
use crate::error::ApiError;
use crate::models::{DeviceRow, PairingRequestRow, RelaySessionRow};
use crate::relay;
use crate::state::AppState;

/// Maximum accepted control-plane JSON body.
pub const MAX_CONTROL_BODY_BYTES: usize = 64 * 1024;

/// How long a pairing code stays valid.
const PAIRING_TTL_MINUTES: i32 = 10;

/// How many pending pairing requests one device may have at once.
const MAX_PENDING_PAIRING_REQUESTS: i64 = 5;

/// How long a relay session may be open before it must be re-requested.
const RELAY_SESSION_TTL_MINUTES: i32 = 5;

/// Concurrent open/active relay sessions allowed per sender.
const MAX_OPEN_RELAY_SESSIONS: i64 = 8;

/// Builds the complete `/v1` router.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/devices/register", post(register_device))
        .route("/v1/devices", get(list_devices))
        .route("/v1/devices/{device_id}", delete(revoke_device))
        .route("/v1/devices/{device_id}/keys", get(device_keys))
        .route("/v1/pairing-requests", post(create_pairing_request))
        .route("/v1/pairing-requests/{request_id}", get(pairing_status))
        .route(
            "/v1/pairing-requests/{request_id}/approve",
            post(approve_pairing_request),
        )
        .route(
            "/v1/folders/{folder_id}/memberships",
            put(upsert_membership),
        )
        .route(
            "/v1/relay-sessions",
            get(list_relay_sessions).post(create_relay_session),
        )
        .route("/v1/relay/{session_id}", get(relay_ws))
        .layer(DefaultBodyLimit::max(MAX_CONTROL_BODY_BYTES))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

fn parse_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|_| ApiError::invalid_json())
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Compares two hex digests without early exit.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn canonical_pair(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
    if a < b { (a, b) } else { (b, a) }
}

async fn active_relationship(pool: &PgPool, a: Uuid, b: Uuid) -> Result<bool, ApiError> {
    let (device_a, device_b) = canonical_pair(a, b);
    let client = pool.get().await.map_err(ApiError::database)?;
    let row = client
        .query_opt(
            "SELECT status FROM device_relationships WHERE device_a = $1 AND device_b = $2",
            &[&device_a, &device_b],
        )
        .await
        .map_err(ApiError::database)?;
    Ok(row
        .map(|row| row.get::<_, String>(0) == "active")
        .unwrap_or(false))
}

async fn device_by_id(pool: &PgPool, device_id: Uuid) -> Result<Option<DeviceRow>, ApiError> {
    let client = pool.get().await.map_err(ApiError::database)?;
    client
        .query_opt("SELECT * FROM devices WHERE device_id = $1", &[&device_id])
        .await
        .map_err(ApiError::database)?
        .map(|row| DeviceRow::from_row(&row).map_err(ApiError::database))
        .transpose()
}

/// `POST /v1/devices/register`: binds a device ID to its public keys.
#[derive(Deserialize)]
struct RegisterRequest {
    device_id: Uuid,
    device_name: String,
    platform: String,
    public_key: String,
    encryption_key: String,
}

async fn register_device(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let request: RegisterRequest = parse_body(&body)?;

    let device_name = request.device_name.trim().to_string();
    if device_name.is_empty() || device_name.chars().count() > 128 {
        return Err(ApiError::bad_request(
            "DEVICE_NAME_INVALID",
            "`device_name` must be 1 to 128 characters.",
        ));
    }
    let platform = request.platform.trim().to_string();
    if platform.is_empty() || platform.chars().count() > 64 {
        return Err(ApiError::bad_request(
            "PLATFORM_INVALID",
            "`platform` must be 1 to 64 characters.",
        ));
    }
    let public_key = request.public_key.to_ascii_lowercase();
    if !is_hex64(&public_key) {
        return Err(ApiError::bad_request(
            "PUBLIC_KEY_INVALID",
            "`public_key` must be 32 bytes of lowercase hex.",
        ));
    }
    let encryption_key = request.encryption_key.to_ascii_lowercase();
    if !is_hex64(&encryption_key) {
        return Err(ApiError::bad_request(
            "ENCRYPTION_KEY_INVALID",
            "`encryption_key` must be 32 bytes of lowercase hex.",
        ));
    }

    auth::authenticate_registration(
        "POST",
        uri.path(),
        &headers,
        &body,
        request.device_id,
        &public_key,
    )?;

    if let Some(existing) = device_by_id(&state.pool, request.device_id).await? {
        if existing.status != "active" {
            return Err(ApiError::forbidden(
                "DEVICE_REVOKED",
                "This device has been revoked and cannot register again.",
            ));
        }
        if existing.public_key != public_key {
            return Err(ApiError::conflict(
                "DEVICE_KEY_MISMATCH",
                "This device ID is already bound to a different public key.",
            ));
        }
        let client = state.pool.get().await.map_err(ApiError::database)?;
        let row = client
            .query_one(
                "UPDATE devices SET device_name = $2, platform = $3, encryption_key = $4, \
                 last_seen_at = now() \
                 WHERE device_id = $1 RETURNING *",
                &[&request.device_id, &device_name, &platform, &encryption_key],
            )
            .await
            .map_err(ApiError::database)?;
        let updated = DeviceRow::from_row(&row).map_err(ApiError::database)?;
        return Ok((StatusCode::OK, Json(json!({"device": updated.to_json()}))));
    }

    let client = state.pool.get().await.map_err(ApiError::database)?;
    let row = client
        .query_one(
            "INSERT INTO devices (device_id, device_name, platform, public_key, encryption_key, \
             last_seen_at) \
             VALUES ($1, $2, $3, $4, $5, now()) RETURNING *",
            &[
                &request.device_id,
                &device_name,
                &platform,
                &public_key,
                &encryption_key,
            ],
        )
        .await
        .map_err(|error| {
            if error.code() == Some(&SqlState::UNIQUE_VIOLATION) {
                ApiError::conflict(
                    "DEVICE_ALREADY_REGISTERED",
                    "This device ID was registered concurrently.",
                )
            } else {
                ApiError::database(error)
            }
        })?;
    let inserted = DeviceRow::from_row(&row).map_err(ApiError::database)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"device": inserted.to_json()})),
    ))
}

/// `GET /v1/devices`: the caller plus its actively related devices.
async fn list_devices(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let device = auth::authenticate(&state.pool, "GET", uri.path(), &headers, b"").await?;

    let client = state.pool.get().await.map_err(ApiError::database)?;
    let rows = client
        .query(
            "SELECT d.* FROM devices d \
             JOIN device_relationships r \
               ON (r.device_a = d.device_id AND r.device_b = $1) \
               OR (r.device_b = d.device_id AND r.device_a = $1) \
             WHERE r.status = 'active' AND d.device_id <> $1 \
             ORDER BY d.device_name",
            &[&device.device_id],
        )
        .await
        .map_err(ApiError::database)?;

    let related = rows
        .iter()
        .map(|row| DeviceRow::from_row(row).map(|d| d.to_json()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ApiError::database)?;

    Ok(Json(json!({
        "this_device": device.to_json(),
        "devices": related,
    })))
}

/// `DELETE /v1/devices/{device_id}`: revokes a relationship, or the caller
/// itself when the ID matches.
/// `GET /v1/devices/{device_id}/keys`: the identity keys of a device the
/// caller is paired with (or the caller itself). These keys are not part of
/// the regular device listing.
async fn device_keys(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(target_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let device = auth::authenticate(&state.pool, "GET", uri.path(), &headers, b"").await?;

    let target = device_by_id(&state.pool, target_id).await?.ok_or_else(|| {
        ApiError::not_found("DEVICE_NOT_FOUND", "No device with that ID is registered.")
    })?;
    if target.status != "active" {
        return Err(ApiError::forbidden(
            "DEVICE_REVOKED",
            "That device has been revoked.",
        ));
    }
    if target.device_id != device.device_id
        && !active_relationship(&state.pool, device.device_id, target.device_id).await?
    {
        return Err(ApiError::forbidden(
            "NOT_PAIRED",
            "The devices must be paired to read each other's keys.",
        ));
    }
    let Some(encryption_key) = &target.encryption_key else {
        return Err(ApiError::conflict(
            "ENCRYPTION_KEY_MISSING",
            "That device has not re-registered since encryption keys were introduced.",
        ));
    };

    Ok(Json(json!({
        "device_id": target.device_id,
        "device_name": target.device_name,
        "platform": target.platform,
        "signing_key": target.public_key,
        "encryption_key": encryption_key,
    })))
}

async fn revoke_device(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(target_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let device = auth::authenticate(&state.pool, "DELETE", uri.path(), &headers, b"").await?;

    let mut client = state.pool.get().await.map_err(ApiError::database)?;
    let tx = client.transaction().await.map_err(ApiError::database)?;

    if target_id == device.device_id {
        tx.execute(
            "UPDATE devices SET status = 'revoked' WHERE device_id = $1",
            &[&target_id],
        )
        .await
        .map_err(ApiError::database)?;
        tx.execute(
            "UPDATE device_relationships SET status = 'revoked' \
             WHERE device_a = $1 OR device_b = $1",
            &[&target_id],
        )
        .await
        .map_err(ApiError::database)?;
    } else {
        let (device_a, device_b) = canonical_pair(device.device_id, target_id);
        let rows = tx
            .execute(
                "UPDATE device_relationships SET status = 'revoked' \
                 WHERE device_a = $1 AND device_b = $2 AND status = 'active'",
                &[&device_a, &device_b],
            )
            .await
            .map_err(ApiError::database)?;
        if rows == 0 {
            return Err(ApiError::not_found(
                "RELATIONSHIP_NOT_FOUND",
                "No active relationship exists with that device.",
            ));
        }
    }

    tx.execute(
        "UPDATE relay_sessions SET status = 'revoked' \
         WHERE status IN ('open', 'active') \
           AND ((sender_device_id = $1 AND receiver_device_id = $2) \
             OR (sender_device_id = $2 AND receiver_device_id = $1))",
        &[&device.device_id, &target_id],
    )
    .await
    .map_err(ApiError::database)?;

    tx.commit().await.map_err(ApiError::database)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/pairing-requests`: starts a pairing request toward a device.
#[derive(Deserialize)]
struct CreatePairingRequestBody {
    target_device_id: Uuid,
}

async fn create_pairing_request(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let request: CreatePairingRequestBody = parse_body(&body)?;
    let device = auth::authenticate(&state.pool, "POST", uri.path(), &headers, &body).await?;

    if request.target_device_id == device.device_id {
        return Err(ApiError::bad_request(
            "SELF_PAIRING",
            "A device cannot pair with itself.",
        ));
    }
    let target = device_by_id(&state.pool, request.target_device_id)
        .await?
        .ok_or_else(|| {
            ApiError::not_found("DEVICE_NOT_FOUND", "No device with that ID is registered.")
        })?;
    if target.status != "active" {
        return Err(ApiError::forbidden(
            "DEVICE_REVOKED",
            "The target device has been revoked.",
        ));
    }
    if active_relationship(&state.pool, device.device_id, target.device_id).await? {
        return Err(ApiError::conflict(
            "ALREADY_PAIRED",
            "These devices are already paired.",
        ));
    }

    let client = state.pool.get().await.map_err(ApiError::database)?;
    let pending: i64 = client
        .query_one(
            "SELECT count(*) FROM pairing_requests \
             WHERE initiator_device_id = $1 AND status = 'pending' \
               AND created_at > now() - interval '10 minutes'",
            &[&device.device_id],
        )
        .await
        .map_err(ApiError::database)?
        .get(0);
    if pending >= MAX_PENDING_PAIRING_REQUESTS {
        return Err(ApiError::too_many_requests(
            "TOO_MANY_PAIRING_REQUESTS",
            "Too many pending pairing requests. Wait for the earlier ones to expire.",
        ));
    }

    let request_id = Uuid::new_v4();
    let code = generate_pairing_code();
    let code_hash = pairing_code_proof(&request_id.to_string(), &code);
    let expires_at: OffsetDateTime = client
        .query_one(
            "INSERT INTO pairing_requests \
               (request_id, initiator_device_id, target_device_id, code_hash, expires_at) \
             VALUES ($1, $2, $3, $4, now() + make_interval(mins => $5)) \
             RETURNING expires_at",
            &[
                &request_id,
                &device.device_id,
                &target.device_id,
                &code_hash,
                &PAIRING_TTL_MINUTES,
            ],
        )
        .await
        .map_err(ApiError::database)?
        .get(0);

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "request_id": request_id,
            "code": code,
            "target_device_id": target.device_id,
            "expires_at": to_rfc3339(expires_at),
        })),
    ))
}

/// `POST /v1/pairing-requests/{request_id}/approve`: the target device
/// presents the pairing-code proof.
#[derive(Deserialize)]
struct ApprovePairingBody {
    proof: String,
}

async fn approve_pairing_request(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(request_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let request: ApprovePairingBody = parse_body(&body)?;
    let device = auth::authenticate(&state.pool, "POST", uri.path(), &headers, &body).await?;

    let pairing = load_pairing_request(&state.pool, request_id)
        .await?
        .ok_or_else(|| {
            ApiError::not_found(
                "PAIRING_REQUEST_NOT_FOUND",
                "No pairing request with that ID.",
            )
        })?;
    if pairing.target_device_id != device.device_id {
        return Err(ApiError::forbidden(
            "NOT_PAIRING_TARGET",
            "Only the target device can approve this pairing request.",
        ));
    }
    if pairing.status != "pending" {
        return Err(ApiError::conflict(
            "PAIRING_NOT_PENDING",
            "This pairing request is no longer pending.",
        ));
    }
    if pairing.expires_at < OffsetDateTime::now_utc() {
        mark_pairing_expired(&state.pool, request_id).await?;
        return Err(ApiError::gone(
            "PAIRING_EXPIRED",
            "This pairing request has expired.",
        ));
    }

    let mut client = state.pool.get().await.map_err(ApiError::database)?;
    let attempts: i32 = client
        .query_one(
            "UPDATE pairing_requests SET attempts = attempts + 1 \
             WHERE request_id = $1 RETURNING attempts",
            &[&request_id],
        )
        .await
        .map_err(ApiError::database)?
        .get(0);
    if attempts > pairing.max_attempts {
        return Err(ApiError::too_many_requests(
            "TOO_MANY_ATTEMPTS",
            "Too many code attempts. Start a new pairing request.",
        ));
    }
    if !constant_time_eq(&request.proof, &pairing.code_hash) {
        return Err(ApiError::unauthorized(
            "INVALID_PAIRING_CODE",
            "The pairing code is incorrect.",
        ));
    }

    let (device_a, device_b) =
        canonical_pair(pairing.initiator_device_id, pairing.target_device_id);
    let tx = client.transaction().await.map_err(ApiError::database)?;
    tx.execute(
        "UPDATE pairing_requests SET status = 'approved', approved_at = now() \
         WHERE request_id = $1",
        &[&request_id],
    )
    .await
    .map_err(ApiError::database)?;
    tx.execute(
        "INSERT INTO device_relationships (device_a, device_b, status) \
         VALUES ($1, $2, 'active') \
         ON CONFLICT (device_a, device_b) DO UPDATE SET status = 'active'",
        &[&device_a, &device_b],
    )
    .await
    .map_err(ApiError::database)?;
    tx.commit().await.map_err(ApiError::database)?;

    let initiator = device_by_id(&state.pool, pairing.initiator_device_id).await?;

    Ok(Json(json!({
        "request_id": request_id,
        "status": "approved",
        "initiator": initiator.map(|d| d.to_json()),
    })))
}

/// `GET /v1/pairing-requests/{request_id}`: status for either participant.
async fn pairing_status(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(request_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let device = auth::authenticate(&state.pool, "GET", uri.path(), &headers, b"").await?;

    let mut pairing = load_pairing_request(&state.pool, request_id)
        .await?
        .ok_or_else(|| {
            ApiError::not_found(
                "PAIRING_REQUEST_NOT_FOUND",
                "No pairing request with that ID.",
            )
        })?;
    if pairing.initiator_device_id != device.device_id
        && pairing.target_device_id != device.device_id
    {
        return Err(ApiError::forbidden(
            "NOT_PAIRING_PARTICIPANT",
            "Only the two devices involved can view this pairing request.",
        ));
    }
    if pairing.status == "pending" && pairing.expires_at < OffsetDateTime::now_utc() {
        mark_pairing_expired(&state.pool, request_id).await?;
        pairing.status = "expired".to_string();
    }

    Ok(Json(json!({
        "request_id": pairing.request_id,
        "status": pairing.status,
        "initiator_device_id": pairing.initiator_device_id,
        "target_device_id": pairing.target_device_id,
        "created_at": to_rfc3339(pairing.created_at),
        "expires_at": to_rfc3339(pairing.expires_at),
        "approved_at": pairing.approved_at.map(to_rfc3339),
    })))
}

async fn load_pairing_request(
    pool: &PgPool,
    request_id: Uuid,
) -> Result<Option<PairingRequestRow>, ApiError> {
    let client = pool.get().await.map_err(ApiError::database)?;
    client
        .query_opt(
            "SELECT * FROM pairing_requests WHERE request_id = $1",
            &[&request_id],
        )
        .await
        .map_err(ApiError::database)?
        .map(|row| PairingRequestRow::from_row(&row).map_err(ApiError::database))
        .transpose()
}

async fn mark_pairing_expired(pool: &PgPool, request_id: Uuid) -> Result<(), ApiError> {
    let client = pool.get().await.map_err(ApiError::database)?;
    client
        .execute(
            "UPDATE pairing_requests SET status = 'expired' \
             WHERE request_id = $1 AND status = 'pending'",
            &[&request_id],
        )
        .await
        .map_err(ApiError::database)?;
    Ok(())
}

/// `PUT /v1/folders/{folder_id}/memberships`: records the caller's own
/// membership in a synced folder. Devices can only change their own row.
#[derive(Deserialize)]
struct MembershipBody {
    permission: String,
}

async fn upsert_membership(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(folder_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let request: MembershipBody = parse_body(&body)?;
    let device = auth::authenticate(&state.pool, "PUT", uri.path(), &headers, &body).await?;

    if request.permission != "read-only" && request.permission != "read-write" {
        return Err(ApiError::bad_request(
            "PERMISSION_INVALID",
            "`permission` must be `read-only` or `read-write`.",
        ));
    }

    let client = state.pool.get().await.map_err(ApiError::database)?;
    let created_at: OffsetDateTime = client
        .query_one(
            "INSERT INTO folder_memberships (folder_id, device_id, permission) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (folder_id, device_id) DO UPDATE SET permission = EXCLUDED.permission \
             RETURNING created_at",
            &[&folder_id, &device.device_id, &request.permission],
        )
        .await
        .map_err(ApiError::database)?
        .get(0);

    Ok(Json(json!({
        "folder_id": folder_id,
        "device_id": device.device_id,
        "permission": request.permission,
        "created_at": to_rfc3339(created_at),
    })))
}

/// `GET /v1/relay-sessions`: open relay sessions involving the caller, so a
/// receiver can discover sessions a sender created for it.
async fn list_relay_sessions(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let device = auth::authenticate(&state.pool, "GET", uri.path(), &headers, b"").await?;

    let client = state.pool.get().await.map_err(ApiError::database)?;
    let rows = client
        .query(
            "SELECT * FROM relay_sessions \
             WHERE status IN ('open', 'active') AND expires_at > now() \
               AND (sender_device_id = $1 OR receiver_device_id = $1) \
             ORDER BY created_at DESC LIMIT 32",
            &[&device.device_id],
        )
        .await
        .map_err(ApiError::database)?;

    let sessions = rows
        .iter()
        .map(|row| {
            let session = RelaySessionRow::from_row(row).map_err(ApiError::database)?;
            Ok(json!({
                "session_id": session.session_id,
                "sender_device_id": session.sender_device_id,
                "receiver_device_id": session.receiver_device_id,
                "folder_id": session.folder_id,
                "status": session.status,
                "relay_path": format!("/v1/relay/{}", session.session_id),
                "expires_at": to_rfc3339(session.expires_at),
                "created_at": to_rfc3339(session.created_at),
            }))
        })
        .collect::<Result<Vec<Value>, ApiError>>()?;

    Ok(Json(json!({ "sessions": sessions })))
}

/// `POST /v1/relay-sessions`: authorizes a relay path between two devices.
#[derive(Deserialize)]
struct CreateRelaySessionBody {
    receiver_device_id: Uuid,
    folder_id: Uuid,
}

async fn create_relay_session(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let request: CreateRelaySessionBody = parse_body(&body)?;
    let device = auth::authenticate(&state.pool, "POST", uri.path(), &headers, &body).await?;

    if request.receiver_device_id == device.device_id {
        return Err(ApiError::bad_request(
            "SELF_RELAY",
            "A relay session needs a different receiver.",
        ));
    }
    let receiver = device_by_id(&state.pool, request.receiver_device_id)
        .await?
        .ok_or_else(|| {
            ApiError::not_found("DEVICE_NOT_FOUND", "No device with that ID is registered.")
        })?;
    if receiver.status != "active" {
        return Err(ApiError::forbidden(
            "DEVICE_REVOKED",
            "The receiver device has been revoked.",
        ));
    }
    if !active_relationship(&state.pool, device.device_id, receiver.device_id).await? {
        return Err(ApiError::forbidden(
            "NOT_PAIRED",
            "The devices must be paired before a relay session can be created.",
        ));
    }

    let client = state.pool.get().await.map_err(ApiError::database)?;
    let members: i64 = client
        .query_one(
            "SELECT count(*) FROM folder_memberships \
             WHERE folder_id = $1 AND device_id IN ($2, $3)",
            &[&request.folder_id, &device.device_id, &receiver.device_id],
        )
        .await
        .map_err(ApiError::database)?
        .get(0);
    if members != 2 {
        return Err(ApiError::forbidden(
            "FOLDER_NOT_AUTHORIZED",
            "Both devices must be members of the folder.",
        ));
    }

    let open_sessions: i64 = client
        .query_one(
            "SELECT count(*) FROM relay_sessions \
             WHERE sender_device_id = $1 AND status IN ('open', 'active') \
               AND expires_at > now()",
            &[&device.device_id],
        )
        .await
        .map_err(ApiError::database)?
        .get(0);
    if open_sessions >= MAX_OPEN_RELAY_SESSIONS {
        return Err(ApiError::too_many_requests(
            "TOO_MANY_SESSIONS",
            "Too many open relay sessions.",
        ));
    }

    let session_id = Uuid::new_v4();
    let expires_at: OffsetDateTime = client
        .query_one(
            "INSERT INTO relay_sessions \
               (session_id, sender_device_id, receiver_device_id, folder_id, expires_at) \
             VALUES ($1, $2, $3, $4, now() + make_interval(mins => $5)) \
             RETURNING expires_at",
            &[
                &session_id,
                &device.device_id,
                &receiver.device_id,
                &request.folder_id,
                &RELAY_SESSION_TTL_MINUTES,
            ],
        )
        .await
        .map_err(ApiError::database)?
        .get(0);

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "session_id": session_id,
            "folder_id": request.folder_id,
            "expires_at": to_rfc3339(expires_at),
            "relay_path": format!("/v1/relay/{session_id}"),
        })),
    ))
}

/// `GET /v1/relay/{session_id}`: WebSocket upgrade for one of the two
/// authorized peers. Authentication travels in query parameters because
/// WebSocket clients cannot always set custom headers.
async fn relay_ws(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(session_id): Path<Uuid>,
    Query(params): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let missing = || {
        ApiError::unauthorized(
            "MISSING_AUTH",
            "The relay WebSocket requires device_id, timestamp, and signature parameters.",
        )
    };
    let device_id: Uuid = params
        .get(HEADER_DEVICE_ID)
        .and_then(|value| value.parse().ok())
        .ok_or_else(missing)?;
    let timestamp: i64 = params
        .get(HEADER_TIMESTAMP)
        .and_then(|value| value.parse().ok())
        .ok_or_else(missing)?;
    let signature = params.get(HEADER_SIGNATURE).ok_or_else(missing)?.clone();

    let canonical = canonical_request("GET", uri.path(), timestamp, &sha256_hex(b""));
    let device =
        auth::verify_device_signature(&state.pool, device_id, timestamp, &signature, &canonical)
            .await?;

    let client = state.pool.get().await.map_err(ApiError::database)?;
    let session = client
        .query_opt(
            "SELECT * FROM relay_sessions WHERE session_id = $1",
            &[&session_id],
        )
        .await
        .map_err(ApiError::database)?
        .map(|row| RelaySessionRow::from_row(&row).map_err(ApiError::database))
        .transpose()?
        .ok_or_else(|| {
            ApiError::not_found("SESSION_NOT_FOUND", "No relay session with that ID.")
        })?;

    if device.device_id != session.sender_device_id
        && device.device_id != session.receiver_device_id
    {
        return Err(ApiError::forbidden(
            "SESSION_NOT_PARTICIPANT",
            "This device is not part of the relay session.",
        ));
    }
    if session.status == "closed" || session.status == "revoked" {
        return Err(ApiError::conflict(
            "SESSION_NOT_OPEN",
            "This relay session is no longer open.",
        ));
    }
    if session.expires_at < OffsetDateTime::now_utc() {
        relay::cleanup_session(&state.pool, session_id).await;
        return Err(ApiError::gone(
            "SESSION_EXPIRED",
            "This relay session has expired.",
        ));
    }

    let pool = state.pool.clone();
    let registry = state.relay.clone();
    let device_id = device.device_id;
    let response = ws
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| relay::run(pool, registry, session_id, device_id, socket))
        .into_response();
    Ok(response)
}

const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

fn generate_pairing_code() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let mut code = String::with_capacity(9);
    for position in 0..8 {
        if position == 4 {
            code.push('-');
        }
        let index = rng.random_range(0..CODE_ALPHABET.len());
        code.push(CODE_ALPHABET[index] as char);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_codes_have_expected_shape_and_alphabet() {
        for _ in 0..50 {
            let code = generate_pairing_code();
            assert_eq!(code.len(), 9);
            assert_eq!(code.as_bytes()[4], b'-');
            for byte in code.bytes().filter(|b| *b != b'-') {
                assert!(
                    CODE_ALPHABET.contains(&byte),
                    "unexpected byte {byte} in {code}"
                );
            }
        }
    }

    #[test]
    fn constant_time_eq_matches_equality() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn canonical_pair_is_order_independent() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        assert_eq!(canonical_pair(a, b), (a, b));
        assert_eq!(canonical_pair(b, a), (a, b));
    }

    #[test]
    fn is_hex64_rejects_wrong_shapes() {
        assert!(is_hex64(&"ab".repeat(32)));
        assert!(!is_hex64(&"ab".repeat(31)));
        assert!(!is_hex64(&format!("{}g", "ab".repeat(31))));
    }
}
