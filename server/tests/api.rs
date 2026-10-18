//! End-to-end tests of the `/v1` API and the relay WebSocket against a real
//! PostgreSQL instance.
//!
//! These tests are skipped (with a clear SKIP line) unless
//! `SYNCENGINE_TEST_DATABASE_URL` is set, for example:
//!
//! ```text
//! docker compose up -d --wait postgres
//! SYNCENGINE_TEST_DATABASE_URL=postgres://syncengine:syncengine@localhost:5432/syncengine \
//!   cargo test -p syncengine-server
//! ```

use std::sync::LazyLock;

use ed25519_dalek::Signer;
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use syncengine_protocol::control::{
    HEADER_DEVICE_ID, HEADER_SIGNATURE, HEADER_TIMESTAMP, canonical_request,
    canonical_request_for_body, pairing_code_proof, sha256_hex,
};
use syncengine_server::db::{PgPool, pool_from_url, run_migrations};
use syncengine_server::{AppState, api};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Serializes tests that share one database.
static DB_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn test_database_url() -> Option<String> {
    match std::env::var("SYNCENGINE_TEST_DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!(
                "SKIP: set SYNCENGINE_TEST_DATABASE_URL (docker compose up -d --wait postgres) \
                 to run the DB-backed server tests"
            );
            None
        }
    }
}

struct TestServer {
    base: String,
    pool: PgPool,
}

async fn spawn_server(url: &str) -> TestServer {
    let pool = pool_from_url(url).expect("pool");
    run_migrations(&pool).await.expect("migrations");
    reset_database(&pool).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = api::app(AppState::new(pool.clone()));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    TestServer {
        base: format!("http://{addr}"),
        pool,
    }
}

async fn reset_database(pool: &PgPool) {
    let client = pool.get().await.expect("connection");
    client
        .batch_execute(
            "TRUNCATE devices, pairing_requests, device_relationships, \
             folder_memberships, relay_sessions CASCADE",
        )
        .await
        .expect("truncate");
}

struct TestDevice {
    id: Uuid,
    key: ed25519_dalek::SigningKey,
    encryption_key: [u8; 32],
    client: reqwest::Client,
}

impl TestDevice {
    fn new() -> Self {
        Self {
            id: Uuid::new_v4(),
            key: ed25519_dalek::SigningKey::generate(&mut rand::rng()),
            encryption_key: rand::random(),
            client: reqwest::Client::new(),
        }
    }

    fn public_key_hex(&self) -> String {
        hex::encode(self.key.verifying_key().to_bytes())
    }

    fn encryption_key_hex(&self) -> String {
        hex::encode(self.encryption_key)
    }

    fn auth_headers(&self, method: &str, path: &str, body: &[u8]) -> [(String, String); 3] {
        let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
        let canonical = canonical_request_for_body(method, path, timestamp, body);
        let signature = self.key.sign(canonical.as_bytes());
        [
            (HEADER_DEVICE_ID.to_string(), self.id.to_string()),
            (HEADER_TIMESTAMP.to_string(), timestamp.to_string()),
            (
                HEADER_SIGNATURE.to_string(),
                hex::encode(signature.to_bytes()),
            ),
        ]
    }

    async fn post(&self, base: &str, path: &str, body: &Value) -> reqwest::Response {
        let bytes = serde_json::to_vec(body).expect("json");
        let headers = self.auth_headers("POST", path, &bytes);
        self.client
            .post(format!("{base}{path}"))
            .header(headers[0].0.clone(), headers[0].1.clone())
            .header(headers[1].0.clone(), headers[1].1.clone())
            .header(headers[2].0.clone(), headers[2].1.clone())
            .header("content-type", "application/json")
            .body(bytes)
            .send()
            .await
            .expect("request")
    }

    async fn put(&self, base: &str, path: &str, body: &Value) -> reqwest::Response {
        let bytes = serde_json::to_vec(body).expect("json");
        let headers = self.auth_headers("PUT", path, &bytes);
        self.client
            .put(format!("{base}{path}"))
            .header(headers[0].0.clone(), headers[0].1.clone())
            .header(headers[1].0.clone(), headers[1].1.clone())
            .header(headers[2].0.clone(), headers[2].1.clone())
            .header("content-type", "application/json")
            .body(bytes)
            .send()
            .await
            .expect("request")
    }

    async fn get(&self, base: &str, path: &str) -> reqwest::Response {
        let headers = self.auth_headers("GET", path, b"");
        self.client
            .get(format!("{base}{path}"))
            .header(headers[0].0.clone(), headers[0].1.clone())
            .header(headers[1].0.clone(), headers[1].1.clone())
            .header(headers[2].0.clone(), headers[2].1.clone())
            .send()
            .await
            .expect("request")
    }

    async fn delete(&self, base: &str, path: &str) -> reqwest::Response {
        let headers = self.auth_headers("DELETE", path, b"");
        self.client
            .delete(format!("{base}{path}"))
            .header(headers[0].0.clone(), headers[0].1.clone())
            .header(headers[1].0.clone(), headers[1].1.clone())
            .header(headers[2].0.clone(), headers[2].1.clone())
            .send()
            .await
            .expect("request")
    }

    async fn register(&self, base: &str, name: &str) -> reqwest::Response {
        self.post(
            base,
            "/v1/devices/register",
            &json!({
                "device_id": self.id,
                "device_name": name,
                "platform": "linux",
                "public_key": self.public_key_hex(),
                "encryption_key": self.encryption_key_hex(),
            }),
        )
        .await
    }

    /// The relay WebSocket URL authenticated entirely through query params.
    fn relay_ws_url(&self, base: &str, session_id: Uuid) -> String {
        let path = format!("/v1/relay/{session_id}");
        let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
        let canonical = canonical_request("GET", &path, timestamp, &sha256_hex(b""));
        let signature = self.key.sign(canonical.as_bytes());
        format!(
            "{}{path}?{HEADER_DEVICE_ID}={}&{HEADER_TIMESTAMP}={timestamp}&{HEADER_SIGNATURE}={}",
            base.replace("http://", "ws://"),
            self.id,
            hex::encode(signature.to_bytes())
        )
    }
}

async fn error_code(response: reqwest::Response) -> (StatusCode, String) {
    let status = response.status();
    let value: Value = response.json().await.expect("error json");
    let code = value["error"]["code"]
        .as_str()
        .expect("error code")
        .to_string();
    (status, code)
}

async fn register_pair_and_share(
    server: &TestServer,
    alice: &TestDevice,
    bob: &TestDevice,
    folder_id: Uuid,
) {
    assert_eq!(
        alice.register(&server.base, "alice").await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        bob.register(&server.base, "bob").await.status(),
        StatusCode::CREATED
    );

    let response = alice
        .post(
            &server.base,
            "/v1/pairing-requests",
            &json!({"target_device_id": bob.id}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Value = response.json().await.expect("json");
    let request_id = created["request_id"].as_str().expect("request id");
    let code = created["code"].as_str().expect("code");

    let proof = pairing_code_proof(request_id, code);
    let response = bob
        .post(
            &server.base,
            &format!("/v1/pairing-requests/{request_id}/approve"),
            &json!({"proof": proof}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    for device in [alice, bob] {
        let response = device
            .put(
                &server.base,
                &format!("/v1/folders/{folder_id}/memberships"),
                &json!({"permission": "read-write"}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn registration_pairing_authorization_and_revocation_flow() {
    let Some(url) = test_database_url() else {
        return;
    };
    let _guard = DB_LOCK.lock().await;
    let server = spawn_server(&url).await;

    let alice = TestDevice::new();
    let bob = TestDevice::new();
    let mallory = TestDevice::new();

    // Registration: created, idempotent, then conflicting key rejected.
    assert_eq!(
        alice.register(&server.base, "alice").await.status(),
        StatusCode::CREATED
    );
    let response = alice.register(&server.base, "alice-renamed").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["device"]["device_name"], "alice-renamed");

    let impostor = TestDevice {
        id: alice.id,
        key: ed25519_dalek::SigningKey::generate(&mut rand::rng()),
        encryption_key: rand::random(),
        client: reqwest::Client::new(),
    };
    let (status, code) = error_code(impostor.register(&server.base, "impostor").await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code, "DEVICE_KEY_MISMATCH");

    // Bad signature is rejected.
    let mut bad_headers = alice.auth_headers("GET", "/v1/devices", b"");
    bad_headers[2].1 = "00".repeat(64);
    let response = alice
        .client
        .get(format!("{}/v1/devices", server.base))
        .header(bad_headers[0].0.clone(), bad_headers[0].1.clone())
        .header(bad_headers[1].0.clone(), bad_headers[1].1.clone())
        .header(bad_headers[2].0.clone(), bad_headers[2].1.clone())
        .send()
        .await
        .expect("request");
    let (status, code) = error_code(response).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code, "INVALID_SIGNATURE");

    // Stale timestamp is rejected.
    let stale = time::OffsetDateTime::now_utc().unix_timestamp() - 3600;
    let canonical = canonical_request_for_body("GET", "/v1/devices", stale, b"");
    let signature = alice.key.sign(canonical.as_bytes());
    let response = alice
        .client
        .get(format!("{}/v1/devices", server.base))
        .header(HEADER_DEVICE_ID, alice.id.to_string())
        .header(HEADER_TIMESTAMP, stale.to_string())
        .header(HEADER_SIGNATURE, hex::encode(signature.to_bytes()))
        .send()
        .await
        .expect("request");
    let (status, code) = error_code(response).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code, "INVALID_TIMESTAMP");

    assert_eq!(
        bob.register(&server.base, "bob").await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        mallory.register(&server.base, "mallory").await.status(),
        StatusCode::CREATED
    );

    // Pairing: wrong proof rejected, right proof approved, no double approval.
    let response = alice
        .post(
            &server.base,
            "/v1/pairing-requests",
            &json!({"target_device_id": bob.id}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Value = response.json().await.expect("json");
    let request_id = created["request_id"]
        .as_str()
        .expect("request id")
        .to_string();
    let code = created["code"].as_str().expect("code").to_string();
    let expires_at = created["expires_at"].as_str().expect("expiry").to_string();
    assert!(expires_at.contains('T'), "got {expires_at}");

    let wrong = pairing_code_proof(&request_id, "WRNG-CODE");
    let response = bob
        .post(
            &server.base,
            &format!("/v1/pairing-requests/{request_id}/approve"),
            &json!({"proof": wrong}),
        )
        .await;
    let (status, code_err) = error_code(response).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_err, "INVALID_PAIRING_CODE");

    let response = mallory
        .post(
            &server.base,
            &format!("/v1/pairing-requests/{request_id}/approve"),
            &json!({"proof": pairing_code_proof(&request_id, &code)}),
        )
        .await;
    let (status, code_err) = error_code(response).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code_err, "NOT_PAIRING_TARGET");

    let proof = pairing_code_proof(&request_id, &code);
    let response = bob
        .post(
            &server.base,
            &format!("/v1/pairing-requests/{request_id}/approve"),
            &json!({"proof": proof}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = alice
        .get(&server.base, &format!("/v1/pairing-requests/{request_id}"))
        .await;
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["status"], "approved");

    let response = bob
        .post(
            &server.base,
            &format!("/v1/pairing-requests/{request_id}/approve"),
            &json!({"proof": pairing_code_proof(&request_id, &code)}),
        )
        .await;
    let (status, code_err) = error_code(response).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code_err, "PAIRING_NOT_PENDING");

    // Devices list now shows the partner.
    let response = alice.get(&server.base, "/v1/devices").await;
    let body: Value = response.json().await.expect("json");
    let devices = body["devices"].as_array().expect("devices");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["device_id"], bob.id.to_string());

    // Relay requires pairing and both folder memberships.
    let folder_id = Uuid::new_v4();
    let response = alice
        .post(
            &server.base,
            "/v1/relay-sessions",
            &json!({"receiver_device_id": bob.id, "folder_id": folder_id}),
        )
        .await;
    let (status, code_err) = error_code(response).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code_err, "FOLDER_NOT_AUTHORIZED");

    for device in [&alice, &bob] {
        let response = device
            .put(
                &server.base,
                &format!("/v1/folders/{folder_id}/memberships"),
                &json!({"permission": "read-write"}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    let response = mallory
        .post(
            &server.base,
            "/v1/relay-sessions",
            &json!({"receiver_device_id": bob.id, "folder_id": folder_id}),
        )
        .await;
    let (status, code_err) = error_code(response).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code_err, "NOT_PAIRED");

    let response = alice
        .post(
            &server.base,
            "/v1/relay-sessions",
            &json!({"receiver_device_id": bob.id, "folder_id": folder_id}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created_session: Value = response.json().await.expect("json");
    let session_id = created_session["session_id"].as_str().expect("session id");

    // Both participants see the open session in the list endpoint; others do
    // not.
    for device in [&alice, &bob] {
        let response = device.get(&server.base, "/v1/relay-sessions").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.expect("json");
        let sessions = body["sessions"].as_array().expect("sessions");
        assert_eq!(sessions.len(), 1, "participant must see the session");
        assert_eq!(sessions[0]["session_id"], session_id);
        assert_eq!(sessions[0]["sender_device_id"], alice.id.to_string());
        assert_eq!(sessions[0]["receiver_device_id"], bob.id.to_string());
        assert_eq!(sessions[0]["folder_id"], folder_id.to_string());
        assert_eq!(sessions[0]["status"], "open");
    }
    let response = mallory.get(&server.base, "/v1/relay-sessions").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert!(
        body["sessions"].as_array().expect("sessions").is_empty(),
        "a non-participant must not see the session"
    );

    // Revocation: relationship first, then the device itself.
    assert_eq!(
        alice
            .delete(&server.base, &format!("/v1/devices/{}", bob.id))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let response = alice
        .post(
            &server.base,
            "/v1/relay-sessions",
            &json!({"receiver_device_id": bob.id, "folder_id": folder_id}),
        )
        .await;
    let (status, code_err) = error_code(response).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code_err, "NOT_PAIRED");

    assert_eq!(
        bob.delete(&server.base, &format!("/v1/devices/{}", bob.id))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let (status, code_err) = error_code(bob.register(&server.base, "bob").await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code_err, "DEVICE_REVOKED");
    let (status, code_err) = error_code(bob.get(&server.base, "/v1/devices").await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code_err, "DEVICE_REVOKED");
}

#[tokio::test]
async fn device_keys_are_visible_to_paired_devices_only() {
    let Some(url) = test_database_url() else {
        return;
    };
    let _guard = DB_LOCK.lock().await;
    let server = spawn_server(&url).await;

    let alice = TestDevice::new();
    let bob = TestDevice::new();
    let mallory = TestDevice::new();
    let folder_id = Uuid::new_v4();
    register_pair_and_share(&server, &alice, &bob, folder_id).await;
    assert_eq!(
        mallory.register(&server.base, "mallory").await.status(),
        StatusCode::CREATED
    );

    // A device can always read its own keys.
    let response = alice
        .get(&server.base, &format!("/v1/devices/{}/keys", alice.id))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["device_id"], alice.id.to_string());
    assert_eq!(body["signing_key"], alice.public_key_hex());
    assert_eq!(body["encryption_key"], alice.encryption_key_hex());

    // The paired partner can read them too; an unrelated device cannot.
    let response = bob
        .get(&server.base, &format!("/v1/devices/{}/keys", alice.id))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["encryption_key"], alice.encryption_key_hex());

    let response = mallory
        .get(&server.base, &format!("/v1/devices/{}/keys", alice.id))
        .await;
    let (status, code_err) = error_code(response).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code_err, "NOT_PAIRED");

    let response = alice
        .get(
            &server.base,
            &format!("/v1/devices/{}/keys", Uuid::new_v4()),
        )
        .await;
    let (status, code_err) = error_code(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_err, "DEVICE_NOT_FOUND");
}

#[tokio::test]
async fn expired_pairing_requests_cannot_be_approved() {
    let Some(url) = test_database_url() else {
        return;
    };
    let _guard = DB_LOCK.lock().await;
    let server = spawn_server(&url).await;

    let alice = TestDevice::new();
    let bob = TestDevice::new();
    assert_eq!(
        alice.register(&server.base, "alice").await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        bob.register(&server.base, "bob").await.status(),
        StatusCode::CREATED
    );

    let response = alice
        .post(
            &server.base,
            "/v1/pairing-requests",
            &json!({"target_device_id": bob.id}),
        )
        .await;
    let created: Value = response.json().await.expect("json");
    let request_id = created["request_id"].as_str().expect("id").to_string();
    let code = created["code"].as_str().expect("code").to_string();

    let client = server.pool.get().await.expect("connection");
    client
        .execute(
            "UPDATE pairing_requests SET expires_at = now() - interval '1 minute' \
             WHERE request_id = $1",
            &[&Uuid::parse_str(&request_id).expect("uuid")],
        )
        .await
        .expect("expire");

    let proof = pairing_code_proof(&request_id, &code);
    let response = bob
        .post(
            &server.base,
            &format!("/v1/pairing-requests/{request_id}/approve"),
            &json!({"proof": proof}),
        )
        .await;
    let (status, code_err) = error_code(response).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(code_err, "PAIRING_EXPIRED");

    let response = alice
        .get(&server.base, &format!("/v1/pairing-requests/{request_id}"))
        .await;
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["status"], "expired");
}

#[tokio::test]
async fn relay_forwards_opaque_frames_and_rejects_intruders() {
    let Some(url) = test_database_url() else {
        return;
    };
    let _guard = DB_LOCK.lock().await;
    let server = spawn_server(&url).await;

    let alice = TestDevice::new();
    let bob = TestDevice::new();
    let mallory = TestDevice::new();
    let folder_id = Uuid::new_v4();
    register_pair_and_share(&server, &alice, &bob, folder_id).await;
    assert_eq!(
        mallory.register(&server.base, "mallory").await.status(),
        StatusCode::CREATED
    );

    let response = alice
        .post(
            &server.base,
            "/v1/relay-sessions",
            &json!({"receiver_device_id": bob.id, "folder_id": folder_id}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Value = response.json().await.expect("json");
    let session_id = Uuid::parse_str(created["session_id"].as_str().expect("id")).expect("uuid");
    assert_eq!(
        created["relay_path"].as_str().expect("path"),
        format!("/v1/relay/{session_id}")
    );

    // Intruders and unauthenticated peers are rejected before the upgrade.
    let intruder_url = mallory.relay_ws_url(&server.base, session_id);
    let error = tokio_tungstenite::connect_async(&intruder_url)
        .await
        .expect_err("intruder must be rejected");
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected an HTTP rejection, got {error:?}");
    };
    assert_eq!(response.status(), StatusCode::FORBIDDEN.as_u16());

    let unauthenticated_url = format!(
        "{}/v1/relay/{session_id}?{HEADER_DEVICE_ID}={}&{HEADER_TIMESTAMP}=1&{HEADER_SIGNATURE}={}",
        server.base.replace("http://", "ws://"),
        alice.id,
        "00".repeat(64)
    );
    let error = tokio_tungstenite::connect_async(&unauthenticated_url)
        .await
        .expect_err("bad signature must be rejected");
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected an HTTP rejection, got {error:?}");
    };
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED.as_u16());

    // Both authorized peers connect and have frames forwarded verbatim.
    let alice_url = alice.relay_ws_url(&server.base, session_id);
    let bob_url = bob.relay_ws_url(&server.base, session_id);

    let (mut alice_ws, _) = tokio_tungstenite::connect_async(&alice_url)
        .await
        .expect("alice connects");
    let (mut bob_ws, _) = tokio_tungstenite::connect_async(&bob_url)
        .await
        .expect("bob connects");

    alice_ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            "ping-from-alice".into(),
        ))
        .await
        .expect("send");
    let received = tokio::time::timeout(std::time::Duration::from_secs(5), bob_ws.next())
        .await
        .expect("timeout")
        .expect("message")
        .expect("ok message");
    assert_eq!(received.to_text().expect("text"), "ping-from-alice");

    bob_ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            "pong-from-bob".into(),
        ))
        .await
        .expect("send");
    let received = tokio::time::timeout(std::time::Duration::from_secs(5), alice_ws.next())
        .await
        .expect("timeout")
        .expect("message")
        .expect("ok message");
    assert_eq!(received.to_text().expect("text"), "pong-from-bob");

    // The relay never persists frame content: the DB only knows status.
    let client = server.pool.get().await.expect("connection");
    let row = client
        .query_one(
            "SELECT status FROM relay_sessions WHERE session_id = $1",
            &[&session_id],
        )
        .await
        .expect("session row");
    let status: String = row.get(0);
    assert!(status == "active" || status == "closed", "got {status}");

    drop(alice_ws);
    drop(bob_ws);
}

#[tokio::test]
async fn expired_relay_sessions_are_rejected_and_cleaned_up() {
    let Some(url) = test_database_url() else {
        return;
    };
    let _guard = DB_LOCK.lock().await;
    let server = spawn_server(&url).await;

    let alice = TestDevice::new();
    let bob = TestDevice::new();
    let folder_id = Uuid::new_v4();
    register_pair_and_share(&server, &alice, &bob, folder_id).await;

    let response = alice
        .post(
            &server.base,
            "/v1/relay-sessions",
            &json!({"receiver_device_id": bob.id, "folder_id": folder_id}),
        )
        .await;
    let created: Value = response.json().await.expect("json");
    let session_id = Uuid::parse_str(created["session_id"].as_str().expect("id")).expect("uuid");

    let client = server.pool.get().await.expect("connection");
    client
        .execute(
            "UPDATE relay_sessions SET expires_at = now() - interval '1 minute' \
             WHERE session_id = $1",
            &[&session_id],
        )
        .await
        .expect("expire");

    let error = tokio_tungstenite::connect_async(&alice.relay_ws_url(&server.base, session_id))
        .await
        .expect_err("expired session must be rejected");
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected an HTTP rejection, got {error:?}");
    };
    assert_eq!(response.status(), StatusCode::GONE.as_u16());

    let row = client
        .query_one(
            "SELECT status FROM relay_sessions WHERE session_id = $1",
            &[&session_id],
        )
        .await
        .expect("row");
    assert_eq!(row.get::<_, String>(0), "closed");

    // A second expired session that nobody connects to is closed by the
    // periodic cleanup pass.
    let response = alice
        .post(
            &server.base,
            "/v1/relay-sessions",
            &json!({"receiver_device_id": bob.id, "folder_id": folder_id}),
        )
        .await;
    let created: Value = response.json().await.expect("json");
    let second_id = Uuid::parse_str(created["session_id"].as_str().expect("id")).expect("uuid");
    client
        .execute(
            "UPDATE relay_sessions SET expires_at = now() - interval '1 minute' \
             WHERE session_id = $1",
            &[&second_id],
        )
        .await
        .expect("expire");

    let closed = syncengine_server::relay::cleanup_expired_sessions(&server.pool)
        .await
        .expect("cleanup");
    assert!(closed >= 1);
    let row = client
        .query_one(
            "SELECT status FROM relay_sessions WHERE session_id = $1",
            &[&second_id],
        )
        .await
        .expect("row");
    assert_eq!(row.get::<_, String>(0), "closed");
}
