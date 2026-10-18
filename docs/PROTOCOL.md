# SyncEngine — Protocol

**Audience:** implementers and learners. This documents what actually goes over the wire, as
implemented. Read `docs/DATA_FLOW.md` for the narrative and `docs/ARCHITECTURE.md` for the map.

Wire protocol version: **1** (`syncengine_protocol::messages::PROTOCOL_VERSION`). Peers must match
exactly; there is no negotiation yet.

## 1. Layers

| Layer | What |
|---|---|
| Transport | Plain TCP on the LAN, or a WebSocket (binary frames) through the relay. |
| Framing | 4-byte big-endian length prefix + payload. Frames larger than `MAX_FRAME_BYTES` (16 MiB) are rejected before allocation. |
| Payload | JSON of one `PeerMessage`, internally tagged: `{"type":"changes", …}` (snake_case). |
| Sealing | `Hello`/`HelloAccepted` are plaintext; **after** the handshake every frame is `nonce(12) || ciphertext` (ChaCha20-Poly1305). |

Sealing key: static-static X25519 ECDH (sender's private key × the peer's announced `encryption_key`)
hashing into HKDF-SHA256 with info `b"syncengine-e2e-v1"`; a fresh random 12-byte nonce is generated
per frame and prepended. The relay sees only framed sealed bytes.

## 2. Handshake

1. Dialer sends `Hello`; listener answers `HelloAccepted`; both messages have identical shape:

```json
{
  "type": "hello",
  "protocol_version": 1,
  "device_id": "…uuid…",
  "signing_key": "…ed25519-hex…",
  "encryption_key": "…x25519-hex…",
  "signature": "…ed25519-hex…"
}
```

2. The signature is over the **hello transcript**:

```
syncengine-hello-v1\n{device_id}\n{signing_key}\n{encryption_key}
```

This binds the id and both keys together, proving possession of the signing key.
3. Receiver checks: protocol version equals 1; signature verifies; then `verify_peer` requires the
   presented `device_id` and both keys to equal the stored trusted `Peer` row exactly (keys are
   lowercased before comparison). Discovery alone never grants trust.
4. `HELLO_TIMEOUT` is 15 seconds per hello frame. Sealing is enabled immediately after the
   `HelloAccepted` is verified.

## 3. Message catalog (`PeerMessage`, `crates/syncengine-protocol/src/messages.rs`)

| Type | Direction | Purpose |
|---|---|---|
| `hello` / `hello_accepted` | ↔ | Handshake (plaintext only). |
| `changes_request` | → | "Send me what you have pending for me in folder X." |
| `changes` | → | Batch of `ChangeRecord` metadata (never content). |
| `content_request` | → | Request bytes for a change: `expected_hash`, `expected_size`, resume `offset`. |
| `chunk` | → | File bytes: absolute `offset`, base64 `data`, `last` flag. |
| `ack` | → | Durable acceptance of a change; echoes the applied `version` vector. |
| `conflict` | → | "I preserved a conflicting revision of your change at `conflict_path`." |
| `error` | ↔ | Application-level error; connection stays usable. |
| `done` | ↔ | "I sent everything and got all acks." Session ends when both sides are done. |
| `ping` / `pong` | ↔ | Keepalive; the session answers `pong` automatically. |
| — | — | `hello` inside an established session is ignored with a warning. |

## 4. Session flow

- Both sides start by sending one `ChangesRequest` per common folder; `outstanding_requests`
  counts them.
- On a request, the peer replies with `Changes` built from `pending_for_peer` (changes whose
  per-peer target is not yet acknowledged), and raises `outstanding_acks`.
- Each received change is planned (see `docs/DATA_FLOW.md` §3): immediate ack for
  stale/already-applied/deletes; otherwise a `ContentRequest` opening a transfer.
- Each `Ack` decrements `outstanding_acks` and marks the target delivered.
- When `outstanding_requests == 0 && outstanding_acks <= 0 && outstanding_fetches <= 0` (and no
  transfer is mid-flight), a side sends `Done`; the session ends when both sides are done and no
  `ok`-message is in flight. Silence longer than `max_idle` (default 120 s) is a timeout.
- A clean socket close before both `Done`s is a session error (`Closed`), not success.

## 5. Content transfer rules

- Chunk size: 256 KiB default, clamped to 4 KiB – 8 MiB (`MAX_FRAME_BYTES / 2`).
- `chunk.offset` is absolute; the receiver enforces that chunks continue exactly where the partial
  file left off. The receiver's staging partial is `.recv-<hash>`; the resume offset is the partial
  file's current length (re-hashed prefix assumed good, final BLAKE3 catches corruption).
- The sender (`serve_content`) validates, in order: change exists; folder/entry/change ids match;
  size matches; staging holds verified content; offset ≤ size; then seeks and streams.
- The receiver verifies the final BLAKE3. On mismatch the partial is discarded (clean restart next
  session). After `finish()`, the file is committed and acked; a `Conflict` transfer preserves the
  local revision first and then acks.
- `PeerMessage::Error` codes used by the session layer:

| Code | Meaning |
|---|---|
| `FOLDER_MISMATCH` / `UNKNOWN_FOLDER` | `Changes` entry or request does not match a shared folder. |
| `UNKNOWN_CHANGE` | `ContentRequest` references a change the sender does not have. |
| `CHANGE_MISMATCH` | `ContentRequest` ids inconsistent with the change. |
| `SIZE_MISMATCH` / `NO_CONTENT` | Change metadata does not match staged content. |
| `CONTENT_UNAVAILABLE` | Staged object missing/unreadable. |
| `BAD_OFFSET` | Resume offset out of bounds. |
| `CONTENT_SHORT` | Staging shrank mid-transfer. |
| `HASH_MISMATCH` | Staged content re-hash failed at send time. |
| `BAD_CHUNK` / `UNEXPECTED_CHUNK` | Receiver-side chunk validation failures. |

Error frames are non-fatal: the loop logs, counts them in the report, and continues.

## 6. Control-plane API (client ↔ server)

### 6.1 Authentication

Every HTTP request carries three headers:

| Header | Value |
|---|---|
| `x-device-id` | registered device UUID |
| `x-timestamp` | unix seconds; accepted within ±300 s |
| `x-signature` | hex Ed25519 signature over the canonical string |

Canonical string (note the trailing newline; method is uppercased):

```
syncengine-control-v1\n{METHOD}\n{path}\n{timestamp}\n{sha256_hex(body)}\n
```

The signature therefore binds method, path, time, and body hash — a captured signature cannot be
replayed against a different request or payload. The relay WebSocket cannot send headers, so it
passes `x-device-id`, `x-timestamp`, `x-signature` as **query parameters** and signs
`GET /v1/relay/{session_id}` with the empty-body hash. The server builds the canonical string from
`uri.path()` (query excluded), so client and server agree.

Registration is trust-on-first-use: `POST /v1/devices/register` stores the device name, platform,
and both public keys. All later calls are verified against the stored signing key, and revoked
devices are rejected (`DEVICE_REVOKED`).

### 6.2 Pairing

The pairing code never reaches the server. The approving device sends

```
sha256_hex("syncengine-pairing-v1\n{request_id}\n{code}")
```

which the server compares against the requester's stored proof. Codes expire after 10 minutes,
allow 5 attempts, and a device may hold at most 5 pending requests.

### 6.3 Endpoints

| Method & path | Auth | Purpose |
|---|---|---|
| `GET /healthz` | none | Liveness probe (`{"status":"ok"}`); used by `syncengine doctor`. |
| `POST /v1/devices/register` | self-signed | Register (TOFU) or refresh own registration. |
| `GET /v1/devices` | yes | List devices known to the caller (pairing workflows). |
| `DELETE /v1/devices/{device_id}` | yes | Revoke a device (self or a relationship). |
| `GET /v1/devices/{device_id}/keys` | yes | Fetch a paired device's public keys (relay setup). |
| `POST /v1/pairing-requests` | yes | Create a pairing request (code proof, target device). |
| `GET /v1/pairing-requests/{request_id}` | yes | Poll request status. |
| `POST /v1/pairing-requests/{request_id}/approve` | yes | Approve with the code proof. |
| `PUT /v1/folders/{folder_id}/memberships` | yes | Declare folder membership for a relationship. |
| `GET /v1/relay-sessions` | yes | List own open/active relay sessions (bounded). |
| `POST /v1/relay-sessions` | yes | Open a relay session (folder membership + paired relationship required). |
| `GET /v1/relay/{session_id}` | query params | WebSocket upgrade; server joins the two participants and forwards frames. |

Request bodies are capped at 64 KiB. Errors use the envelope
`{"error": {"code": "…", "message": "…", "request_id": "…"}}`.

### 6.4 Error codes

`INVALID_SIGNATURE`, `MISSING_AUTH`, `INVALID_TIMESTAMP`, `DEVICE_ID_MISMATCH`, `DEVICE_NOT_FOUND`,
`DEVICE_NOT_REGISTERED`, `DEVICE_REVOKED`, `DEVICE_KEY_MISMATCH`, `DEVICE_ALREADY_REGISTERED`,
`DEVICE_NAME_INVALID`, `PLATFORM_INVALID`, `PUBLIC_KEY_INVALID`, `ENCRYPTION_KEY_INVALID`,
`ENCRYPTION_KEY_MISSING`, `NOT_PAIRED`, `RELATIONSHIP_NOT_FOUND`, `SELF_PAIRING`, `ALREADY_PAIRED`,
`PAIRING_REQUEST_NOT_FOUND`, `NOT_PAIRING_TARGET`, `NOT_PAIRING_PARTICIPANT`, `PAIRING_NOT_PENDING`,
`PAIRING_EXPIRED`, `TOO_MANY_ATTEMPTS`, `INVALID_PAIRING_CODE`, `TOO_MANY_PAIRING_REQUESTS`,
`PERMISSION_INVALID`, `FOLDER_NOT_AUTHORIZED`, `SELF_RELAY`, `TOO_MANY_SESSIONS`,
`SESSION_NOT_FOUND`, `SESSION_NOT_PARTICIPANT`, `SESSION_NOT_OPEN`, `SESSION_EXPIRED`.

## 7. Relay behavior (server side)

- `POST /v1/relay-sessions` checks: caller ≠ target (`SELF_RELAY`), a paired relationship exists,
  folder membership exists (`FOLDER_NOT_AUTHORIZED`), and the caller has fewer than 8 open
  sessions (`TOO_MANY_SESSIONS`). Sessions expire after 5 minutes.
- `GET /v1/relay/{session_id}` upgrades both participants; only the two registered device ids may
  join (`SESSION_NOT_PARTICIPANT`). Each side gets a bounded queue (32 frames); frames are
  forwarded opaquely in both directions. Join timeout: 30 s. The server never parses sealed frames
  and closes idle sessions on its 60-second cleanup sweep.

## 8. Limits summary

| Limit | Value |
|---|---|
| Protocol version | 1 (exact match) |
| Framed message | 16 MiB max |
| HTTP body | 64 KiB max |
| Hello handshake | 15 s per frame |
| Session idle timeout | 120 s |
| Chunk size | 256 KiB default; 4 KiB – 8 MiB effective |
| Control signature skew | ±300 s |
| Pairing code TTL / attempts / pending | 10 min / 5 / 5 |
| Relay session TTL | 5 min |
| Open relay sessions per device | 8 |
| Relay participant join timeout | 30 s |
| Relay per-direction buffer | 32 frames |

## 9. Compatibility notes

- Any protocol-version mismatch fails the handshake with a clear error; upgrade clients together.
- JSON payloads use serde defaults (no `deny_unknown_fields`), so additive fields are
  forward-readable — but there is no capability negotiation yet, so treat version 1 as frozen.
- The control API is versioned by path prefix (`/v1`); the two SQLite/Postgres schemas version
  independently (see `docs/DATABASE.md`).
- There is no in-process TLS on the server: deploy behind a TLS-terminating reverse proxy. Signing
  still protects authenticity end-to-end to the origin's private key.
