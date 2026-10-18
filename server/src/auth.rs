//! Device authentication for control-plane requests.
//!
//! A request is authenticated when it carries `x-device-id`, `x-timestamp`,
//! and `x-signature` headers and the signature verifies over the canonical
//! request string (see [`syncengine_protocol::control`]). The signing device
//! must be registered and active; every authenticated request refreshes its
//! `last_seen_at`.

use axum::http::HeaderMap;
use syncengine_protocol::control::{
    HEADER_DEVICE_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, canonical_request_for_body,
    timestamp_is_fresh,
};
use uuid::Uuid;

use crate::db::PgPool;
use crate::error::ApiError;
use crate::models::DeviceRow;

/// The three authentication values extracted from request headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthHeaders {
    pub device_id: Uuid,
    pub timestamp: i64,
    pub signature_hex: String,
}

/// Parses and validates the authentication headers.
pub fn parse_auth_headers(headers: &HeaderMap) -> Result<AuthHeaders, ApiError> {
    let missing = |name: &str| {
        ApiError::unauthorized(
            "MISSING_AUTH",
            format!("Missing authentication header `{name}`."),
        )
    };
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| missing(name))
    };

    let device_id = header(HEADER_DEVICE_ID)?
        .parse()
        .map_err(|_| ApiError::unauthorized("MISSING_AUTH", "`x-device-id` is not a UUID."))?;
    let timestamp = header(HEADER_TIMESTAMP)?.parse::<i64>().map_err(|_| {
        ApiError::unauthorized("MISSING_AUTH", "`x-timestamp` is not unix seconds.")
    })?;
    let signature_hex = header(HEADER_SIGNATURE)?.to_string();

    Ok(AuthHeaders {
        device_id,
        timestamp,
        signature_hex,
    })
}

/// Loads a registered, active device and verifies `signature_hex` over
/// `canonical`, refreshing `last_seen_at` on success.
pub async fn verify_device_signature(
    pool: &PgPool,
    device_id: Uuid,
    timestamp: i64,
    signature_hex: &str,
    canonical: &str,
) -> Result<DeviceRow, ApiError> {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    if !timestamp_is_fresh(timestamp, now) {
        return Err(ApiError::unauthorized(
            "INVALID_TIMESTAMP",
            "The request timestamp is missing, stale, or in the future.",
        ));
    }

    let client = pool.get().await.map_err(ApiError::database)?;
    let row = client
        .query_opt("SELECT * FROM devices WHERE device_id = $1", &[&device_id])
        .await
        .map_err(ApiError::database)?
        .ok_or_else(|| {
            ApiError::unauthorized(
                "DEVICE_NOT_REGISTERED",
                "This device is not registered. Register it before signing requests.",
            )
        })?;
    let device = DeviceRow::from_row(&row).map_err(ApiError::database)?;
    if device.status != "active" {
        return Err(ApiError::forbidden(
            "DEVICE_REVOKED",
            "This device has been revoked and cannot authenticate.",
        ));
    }

    verify_signature(&device.public_key, signature_hex, canonical)?;

    client
        .execute(
            "UPDATE devices SET last_seen_at = now() WHERE device_id = $1",
            &[&device_id],
        )
        .await
        .map_err(ApiError::database)?;

    Ok(device)
}

fn verify_signature(
    public_key_hex: &str,
    signature_hex: &str,
    canonical: &str,
) -> Result<(), ApiError> {
    let key_bytes: [u8; 32] = hex::decode(public_key_hex)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| ApiError::internal("stored device public key is not 32-byte hex"))?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&key_bytes)
        .map_err(|error| ApiError::internal(format!("stored public key is invalid: {error}")))?;

    let signature_bytes: [u8; 64] = hex::decode(signature_hex)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| {
            ApiError::unauthorized("INVALID_SIGNATURE", "`x-signature` is not 64-byte hex.")
        })?;
    let signature = ed25519_dalek::Signature::from_bytes(&signature_bytes);

    key.verify_strict(canonical.as_bytes(), &signature)
        .map_err(|_| {
            ApiError::unauthorized("INVALID_SIGNATURE", "The request signature is invalid.")
        })
}

/// Authenticates an HTTP request: canonical string over method, path,
/// timestamp, and the SHA-256 of the real body bytes.
pub async fn authenticate(
    pool: &PgPool,
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<DeviceRow, ApiError> {
    let auth = parse_auth_headers(headers)?;
    let canonical = canonical_request_for_body(method, path, auth.timestamp, body);
    verify_device_signature(
        pool,
        auth.device_id,
        auth.timestamp,
        &auth.signature_hex,
        &canonical,
    )
    .await
}

/// Authenticates a *registration* request by proving possession of the
/// private key for `public_key_hex` (the device row does not exist yet).
pub fn authenticate_registration(
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
    expected_device_id: Uuid,
    public_key_hex: &str,
) -> Result<Uuid, ApiError> {
    let auth = parse_auth_headers(headers)?;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    if !timestamp_is_fresh(auth.timestamp, now) {
        return Err(ApiError::unauthorized(
            "INVALID_TIMESTAMP",
            "The request timestamp is missing, stale, or in the future.",
        ));
    }
    if auth.device_id != expected_device_id {
        return Err(ApiError::unauthorized(
            "DEVICE_ID_MISMATCH",
            "The signature was made by a different device ID than the body claims.",
        ));
    }
    let canonical = canonical_request_for_body(method, path, auth.timestamp, body);
    verify_signature(public_key_hex, &auth.signature_hex, &canonical)?;
    Ok(auth.device_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("name"),
                value.parse().expect("value"),
            );
        }
        headers
    }

    #[test]
    fn parses_complete_headers() {
        let id = Uuid::new_v4();
        let headers = headers_with(&[
            (HEADER_DEVICE_ID, &id.to_string()),
            (HEADER_TIMESTAMP, "1700000000"),
            (HEADER_SIGNATURE, &"ab".repeat(64)),
        ]);
        let parsed = parse_auth_headers(&headers).expect("parse");
        assert_eq!(parsed.device_id, id);
        assert_eq!(parsed.timestamp, 1_700_000_000);
        assert_eq!(parsed.signature_hex, "ab".repeat(64));
    }

    #[test]
    fn rejects_missing_headers_with_missing_auth() {
        for missing in [HEADER_DEVICE_ID, HEADER_TIMESTAMP, HEADER_SIGNATURE] {
            let id = Uuid::new_v4().to_string();
            let mut pairs = vec![
                (HEADER_DEVICE_ID, id.as_str()),
                (HEADER_TIMESTAMP, "1700000000"),
                (HEADER_SIGNATURE, "ab"),
            ];
            pairs.retain(|(name, _)| *name != missing);
            let headers = headers_with(&pairs);
            let err = parse_auth_headers(&headers).unwrap_err();
            assert_eq!(err.code, "MISSING_AUTH", "missing {missing}");
            assert_eq!(err.status, axum::http::StatusCode::UNAUTHORIZED);
        }
    }

    #[test]
    fn rejects_malformed_device_id_and_timestamp() {
        let bad_device = headers_with(&[
            (HEADER_DEVICE_ID, "not-a-uuid"),
            (HEADER_TIMESTAMP, "1"),
            (HEADER_SIGNATURE, "ab"),
        ]);
        assert_eq!(
            parse_auth_headers(&bad_device).unwrap_err().code,
            "MISSING_AUTH"
        );

        let bad_timestamp = headers_with(&[
            (HEADER_DEVICE_ID, &Uuid::new_v4().to_string()),
            (HEADER_TIMESTAMP, "yesterday"),
            (HEADER_SIGNATURE, "ab"),
        ]);
        assert_eq!(
            parse_auth_headers(&bad_timestamp).unwrap_err().code,
            "MISSING_AUTH"
        );
    }

    #[test]
    fn verify_signature_accepts_valid_and_rejects_tampered() {
        use ed25519_dalek::Signer;
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rng());
        let public_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let canonical = "syncengine-control-v1\nGET\n/v1/devices\n1\ndeadbeef\n";
        let signature = signing_key.sign(canonical.as_bytes());
        let signature_hex = hex::encode(signature.to_bytes());

        assert!(verify_signature(&public_hex, &signature_hex, canonical).is_ok());
        assert!(verify_signature(&public_hex, &signature_hex, "other").is_err());

        let mut tampered = signature.to_bytes();
        tampered[63] ^= 0x01;
        let bad_hex = hex::encode(tampered);
        assert_ne!(bad_hex, signature_hex);
        let err = verify_signature(&public_hex, &bad_hex, canonical).unwrap_err();
        assert_eq!(err.code, "INVALID_SIGNATURE");
    }

    #[test]
    fn verify_signature_rejects_non_hex_signature() {
        use ed25519_dalek::Signer;
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rng());
        let public_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let _ = signing_key.sign(b"x");
        let err = verify_signature(&public_hex, "zz", "x").unwrap_err();
        assert_eq!(err.code, "INVALID_SIGNATURE");
    }
}
