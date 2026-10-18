//! Control-plane authentication helpers shared by the client and the server.
//!
//! Devices authenticate to the coordination API with Ed25519 signatures over a
//! canonical request string. Both sides must construct that string identically,
//! which is why it lives in the shared protocol crate. Signatures cover the
//! method, path, a unix-seconds timestamp, and the SHA-256 of the request body
//! so a captured signature cannot be replayed against a different request.

use sha2::{Digest, Sha256};

/// Domain-separation context for control-plane signatures.
pub const CONTROL_AUTH_CONTEXT: &str = "syncengine-control-v1";

/// Allowed clock skew between a device and the server, in seconds.
pub const CONTROL_TIMESTAMP_SKEW_SECS: i64 = 300;

/// Header carrying the authenticating device ID.
pub const HEADER_DEVICE_ID: &str = "x-device-id";

/// Header carrying the unix-seconds timestamp covered by the signature.
pub const HEADER_TIMESTAMP: &str = "x-timestamp";

/// Header carrying the hex-encoded Ed25519 signature.
pub const HEADER_SIGNATURE: &str = "x-signature";

/// Hex-encoded SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The canonical string a device signs for a control-plane request.
///
/// `method` is uppercased; `path` must not contain a query string, so relay
/// WebSocket handshakes sign the bare `/v1/relay/{session_id}` path.
pub fn canonical_request(
    method: &str,
    path: &str,
    timestamp: i64,
    body_sha256_hex: &str,
) -> String {
    format!(
        "{CONTROL_AUTH_CONTEXT}\n{}\n{path}\n{timestamp}\n{body_sha256_hex}\n",
        method.to_ascii_uppercase()
    )
}

/// Convenience over [`canonical_request`] that hashes `body` itself.
pub fn canonical_request_for_body(method: &str, path: &str, timestamp: i64, body: &[u8]) -> String {
    canonical_request(method, path, timestamp, &sha256_hex(body))
}

/// Whether `timestamp` is within [`CONTROL_TIMESTAMP_SKEW_SECS`] of `now`.
///
/// Both stale and far-future timestamps are rejected.
pub fn timestamp_is_fresh(timestamp: i64, now: i64) -> bool {
    timestamp.abs_diff(now) <= CONTROL_TIMESTAMP_SKEW_SECS as u64
}

/// The proof an approving device presents for a pairing request.
///
/// The pairing code itself never reaches the server; the device hashes
/// `request_id` and the code together so the server can verify possession
/// without storing the code.
pub fn pairing_code_proof(request_id: &str, code: &str) -> String {
    sha256_hex(format!("syncengine-pairing-v1\n{request_id}\n{code}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_request_is_stable_and_normalizes_method_case() {
        let expected = "syncengine-control-v1\nPOST\n/v1/devices/register\n1700000000\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n";
        assert_eq!(
            canonical_request(
                "post",
                "/v1/devices/register",
                1_700_000_000,
                &sha256_hex(b"")
            ),
            expected
        );
        assert_eq!(
            canonical_request_for_body("POST", "/v1/devices/register", 1_700_000_000, b""),
            expected
        );
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn timestamp_freshness_covers_boundaries_and_both_directions() {
        let now = 1_700_000_000;
        assert!(timestamp_is_fresh(now, now));
        assert!(timestamp_is_fresh(now - CONTROL_TIMESTAMP_SKEW_SECS, now));
        assert!(timestamp_is_fresh(now + CONTROL_TIMESTAMP_SKEW_SECS, now));
        assert!(!timestamp_is_fresh(
            now - CONTROL_TIMESTAMP_SKEW_SECS - 1,
            now
        ));
        assert!(!timestamp_is_fresh(
            now + CONTROL_TIMESTAMP_SKEW_SECS + 1,
            now
        ));
    }

    #[test]
    fn canonical_request_binds_every_component() {
        let base = canonical_request("GET", "/v1/devices", 10, &sha256_hex(b"x"));
        for variant in [
            canonical_request("POST", "/v1/devices", 10, &sha256_hex(b"x")),
            canonical_request("GET", "/v1/devices/1", 10, &sha256_hex(b"x")),
            canonical_request("GET", "/v1/devices", 11, &sha256_hex(b"x")),
            canonical_request("GET", "/v1/devices", 10, &sha256_hex(b"y")),
        ] {
            assert_ne!(base, variant);
        }
    }

    #[test]
    fn pairing_proof_is_stable_and_binds_request_and_code() {
        let proof = pairing_code_proof("req-1", "ABCD-EFGH");
        assert_eq!(proof, pairing_code_proof("req-1", "ABCD-EFGH"));
        assert_ne!(proof, pairing_code_proof("req-1", "ABCD-EFGI"));
        assert_ne!(proof, pairing_code_proof("req-2", "ABCD-EFGH"));
        assert_eq!(proof.len(), 64);
    }
}
