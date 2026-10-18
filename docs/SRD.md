# SyncEngine — Software Requirements Document (SRD)

**Version:** 1.0  
**Status:** Baseline design  
**Product type:** Cross-platform file synchronization agent  
**Platforms:** Linux and Windows

## 1. Product overview

SyncEngine is a CLI-first application that synchronizes files bidirectionally between a user's paired devices. It supports online synchronization through an encrypted relay and offline synchronization between devices on the same local network.

The server coordinates devices and relays encrypted data. It does not persistently store users' file contents. Each client maintains its own local state and durable change journal.

### 1.1 Product goals

- Keep selected folders synchronized across paired devices.
- Support Linux and Windows clients.
- Work online through a coordination-and-relay server.
- Work offline between paired devices on the same LAN.
- Preserve pending changes across disconnections and restarts.
- Verify file integrity and avoid silent data loss.
- Detect concurrent edits and surface conflicts.
- Keep the initial architecture understandable and practical for a solo developer.

### 1.2 Non-goals for v1

- Cloud file storage or a cloud drive UI.
- A graphical desktop interface.
- Internet P2P/NAT traversal as the primary online transport.
- Public anonymous file sharing.
- A fleet of microservices.
- Custom cryptographic algorithms.

## 2. Users and use cases

### 2.1 Target users

Developers and technically comfortable users who want selected folders synchronized across their own Linux and Windows devices.

### 2.2 Main use cases

1. Initialize SyncEngine on a device.
2. Pair another device and explicitly trust it.
3. Share a local folder with selected paired devices.
4. Automatically detect and journal file changes.
5. Synchronize changes while online through a relay.
6. Synchronize directly over a LAN when the server is unavailable.
7. Resume interrupted transfers.
8. View device, folder, sync, and conflict status.
9. Revoke a device's access.
10. Recover safely after a process or machine crash.

## 3. High-level architecture

The repository is a monorepo containing two executable applications:

```text
syncengine/
├── client/       # Rust CLI and background agent
├── server/       # Rust coordination API and relay
├── docs/         # SRD, architecture, database, protocol, security
├── docker-compose.yml
├── Cargo.toml    # Rust workspace root
└── README.md
```

### 3.1 Client application

Logical modules:

- **CLI:** accepts commands and communicates with the background agent.
- **Background agent:** performs ongoing synchronization after the CLI exits.
- **Filesystem watcher:** observes create, modify, rename, and delete events.
- **Reconciliation scanner:** periodically compares disk state with indexed state to repair missed watcher events.
- **Sync engine:** schedules changes, tracks peers, compares versions, and decides when conflicts exist.
- **Local storage:** SQLite metadata, journal, checkpoints, transfer state, and tombstones.
- **Content staging:** preserves immutable content for pending revisions.
- **Transfer manager:** chunking, resume, retry, verification, and safe publication.
- **Network manager:** relay connections, reconnects, and LAN discovery.
- **Security:** device identity, peer authentication, authorization, and encryption.

These are logical boundaries. They do not all need to be separate Rust crates in the first implementation.

### 3.2 Server application

One Rust server application with these logical modules:

- **Coordination API:** device registration, pairing, permissions, presence, and relay-session authorization.
- **Relay:** forwards encrypted payloads between authorized devices and handles connection backpressure and failures.
- **Persistence:** PostgreSQL metadata and migrations.

The coordination API and relay should initially be modules in one deployable server, not separate microservices.

### 3.3 Data storage boundaries

- Each client has its own local SQLite database.
- The server uses PostgreSQL for device, relationship, pairing, and relay-session metadata.
- The server must not persistently store file contents or device-local absolute paths.
- A relay may temporarily buffer bounded data in memory, but it must not become a file-storage service.

## 4. Operating modes

### 4.1 Online mode

1. The watcher observes a filesystem change.
2. The client records the change durably in SQLite.
3. The client stabilizes and stages the exact content revision when needed.
4. The sync engine identifies authorized target devices.
5. The sender requests or uses an authorized relay session.
6. The sender transmits protected chunks through the relay.
7. The receiver verifies the content, checks for conflicts, and safely publishes it.
8. The receiver commits the applied-change record and required local state.
9. The receiver acknowledges durable acceptance.
10. The sender records delivery only after receiving the required acknowledgement.

The relay forwards opaque, end-to-end-protected payloads and does not need access to file contents.

### 4.2 Offline LAN mode

1. Paired devices discover each other using mDNS.
2. Devices authenticate one another using their previously trusted identities and keys.
3. First-time trust requires explicit local approval; discovery alone never grants access.
4. Devices exchange pending-change metadata.
5. They transfer content directly over the LAN using the same transfer, integrity, conflict, and recovery rules as online mode.
6. Pending changes remain journaled if the other device is unavailable.

The remote coordination/relay server is not required for LAN transfer between already paired devices. Revocation and policy freshness must be handled carefully when a device has been offline; see Security.

## 5. Functional requirements

### FR-01: Device identity

Each installation must have a stable device ID and a cryptographic key pair. Private keys must not be stored in plaintext in SQLite.

### FR-02: Pairing and trust

A user must be able to pair devices and explicitly approve trust. Pairing requests must expire and be rate-limited. Revoked devices must not be allowed to start new authorized sessions.

### FR-03: Shared folders

A user must be able to register and remove a local folder from synchronization. Each shared folder has a logical folder ID that is independent of its local root path.

### FR-04: Relative paths

Files are identified within a shared folder by relative path and stable entry identity where possible. Different devices may map the same logical folder to different local roots. For example, Linux `/home/user/Projects/app` and Windows `D:\\Work\\App` may represent the same folder ID.

### FR-05: Filesystem observation

The client must detect create, update, rename, and delete operations. Watcher events are hints and must be debounced. Periodic reconciliation must repair missed events. An incomplete scan must never be treated as proof that all unobserved files were deleted.

### FR-06: Durable change journal

Changes must be persisted before the system relies on asynchronous delivery. Pending work must survive application restart, machine restart, network loss, and receiver unavailability.

### FR-07: Bidirectional synchronization

Changes made on any authorized writable device must be eligible to propagate to other authorized writable devices.

### FR-08: Content staging

If a revision is pending delivery, the client must preserve the exact bytes associated with its recorded content hash. A later edit or deletion must not silently change the bytes of an earlier pending revision.

### FR-09: Transfer integrity

Transfers must support chunking, bounded memory use, retry, resumability, and final content-hash verification. The receiver must not publish a partially received or unverified file.

### FR-10: Idempotency

Repeated messages, retries, and acknowledgements must not apply the same logical change more than once. Stable change IDs must be used across retries.

### FR-11: Conflict handling

Concurrent edits must not silently overwrite each other. The client must preserve conflicting content and expose the conflict for inspection or resolution.

### FR-12: Deletions

Deletions must be represented as tombstones so stale peers do not resurrect deleted files. Tombstones may be retired only under an explicit peer acknowledgement or device-removal policy.

### FR-13: Status and diagnostics

The CLI must show device state, shared folders, pending changes, active transfers, errors, and unresolved conflicts. A diagnostic command should identify common configuration and connectivity problems.

### FR-14: Revocation

Users must be able to revoke device trust. The server must reject new sessions for revoked devices and invalidate active sessions where possible. Offline peers must revalidate authorization when they reconnect.

### FR-15: Safe publication and recovery

The receiver must write to a temporary path, verify size and hash, check conflicts, and publish safely. Recovery logic must handle crashes between filesystem publication, database commit, and acknowledgement.

## 6. Non-functional requirements

### NFR-01: Integrity

No file is considered successfully transferred until the receiver verifies its declared size and cryptographic content hash.

### NFR-02: Durability

Pending changes and transfer checkpoints must survive process restarts and recoverable machine failures, subject to the documented filesystem and flush guarantees.

### NFR-03: Security

All protected control-plane requests must use TLS. Device-to-device content must use an established authenticated encryption protocol. Do not implement custom cryptography.

### NFR-04: Resource limits

Network transfer must use bounded memory and configurable limits for concurrent transfers, chunk sizes, file sizes, staging capacity, and retries.

### NFR-05: Portability

The client must support Linux and Windows. Path handling, rename behavior, file locks, case sensitivity, symlinks, permissions, and atomic publication must be tested per platform.

### NFR-06: Observability

Use structured logs and actionable errors. Logs must not expose private keys, pairing secrets, or file contents.

### NFR-07: Maintainability

Use versioned database migrations, versioned protocol messages, clear module boundaries, and automated tests for critical invariants.

### NFR-08: Server storage

The server must not persistently store file contents. Server-side data is limited to metadata required for identity, authorization, pairing, presence, and relay sessions.

## 7. CLI requirements

Initial command set:

```text
syncengine init
syncengine pair
syncengine devices
syncengine share <path>
syncengine unshare <folder-id>
syncengine status
syncengine daemon start
syncengine daemon status
syncengine daemon stop
syncengine conflicts
syncengine logs
syncengine doctor
```

The CLI is a control surface, not the long-running sync worker. Commands must validate inputs, provide help, return consistent exit codes, and communicate with the background agent through local IPC or another well-defined local interface.

Exact flags and pairing UX are implementation decisions.

## 8. Client database design (SQLite)

Use versioned migrations, foreign-key enforcement on every connection, and an appropriate SQLite durability configuration. WAL mode may be used for suitable local workloads. Do not assume the database is safe on arbitrary network-mounted filesystems.

### 8.1 `schema_migrations`

Tracks applied schema versions and application timestamps.

### 8.2 `local_device`

Singleton record containing stable device ID, device name, and creation timestamp.

### 8.3 `peers`

Stores peer ID, device name, public key, platform, trust state (`pending`, `trusted`, `revoked`), last-seen time, and creation time.

### 8.4 `shared_folders`

Stores logical folder UUID, name, device-specific local root path, enabled state, and creation time.

### 8.5 `folder_peers`

Maps folder ID to peer ID and permission (`read-only` or `read-write`), with join timestamp.

### 8.6 `file_entries`

Stores stable entry ID, folder ID, relative path, state (`present` or `deleted`), content hash, size, modification metadata, version vector, last change ID, and update timestamp. Use appropriate uniqueness constraints, including a partial unique index for currently present paths.

### 8.7 `changes`

Append-oriented journal containing local sequence number, globally unique change ID, folder ID, entry ID, operation (`create`, `update`, `delete`, `rename`), current and old relative paths, content hash, size, base and current version vectors, origin device ID, and timestamp.

### 8.8 `change_targets`

Tracks delivery for each change and peer: state (`pending`, `in-progress`, `delivered`, `failed`), attempt count, retry time, acknowledgement timestamp, and last error.

### 8.9 `tombstones`

Stores deletion records including **delete change ID as the primary key**, entry ID, folder ID, last path, version vector, and deletion timestamp. Do not use entry ID alone as the primary key because a file can be deleted, recreated, and deleted again.

### 8.10 `peer_sync_state`

Stores folder ID, peer ID, last acknowledged sequence, and last successful synchronization time.

### 8.11 `transfers`

Stores transfer ID, change ID, peer ID, direction (`send` or `receive`), state (`queued`, `active`, `paused`, `completed`, `failed`, `cancelled`), expected hash and size, acknowledged bytes, temporary path, and update timestamp.

`acknowledged_bytes` means the contiguous durable offset. If out-of-order chunk acknowledgement is supported, add a chunk table or bitmap instead of treating one offset as sufficient.

### 8.12 `applied_remote_changes`

Uses change ID as the primary key and records origin device ID and application timestamp. This supports idempotent remote application.

### 8.13 `conflicts`

Stores conflict ID, folder ID, entry ID, local change ID, remote change ID, status (`unresolved`, `resolved`, `dismissed`), resolution details, and creation/resolution timestamps.

### 8.14 `content_objects`

Stores immutable staged local content by content hash, size, staged path, reference tracking, and created/verified timestamps. It is required because a source file can change or disappear while an older revision is still pending delivery. This is local staging, not cloud storage.

### 8.15 Database invariants

- Use transactions to commit journal changes, current state, and delivery targets together where appropriate.
- Apply a remote change and insert its `applied_remote_changes` record in one database transaction.
- The filesystem and SQLite cannot be committed atomically together; explicit crash recovery is mandatory.
- Do not garbage-collect staged content while journal, transfer, or recovery work still references it.
- Use UTC timestamps; UUIDs may be stored as text; version vectors may be stored as validated canonical JSON.
- Enable foreign keys on every SQLite connection.

## 9. Server database design (PostgreSQL)

### 9.1 `devices`

Device ID, public key, device name, status, creation timestamp, and last-seen timestamp.

### 9.2 `pairing_requests`

Request ID, initiating device ID, pairing-code hash or equivalent protected secret representation, expiry, status, and creation timestamp.

### 9.3 `device_relationships`

Canonical pair of device IDs, relationship status, and creation timestamp.

### 9.4 `folder_memberships`

Folder ID, device ID, permission, and creation timestamp.

### 9.5 `relay_sessions`

Session ID, sender and receiver device IDs, folder ID, expiry, status, and creation timestamp.

No server table should contain persistent file content or device-local absolute folder paths. An account table is unnecessary unless account-based login becomes a real requirement.

Pairing codes must expire and be rate-limited. Bind public keys to authenticated device identities. Relay sessions must authorize the sender, receiver, folder, operation, expiry, and applicable limits. Presence is transient and does not guarantee successful delivery. Expired sessions require cleanup.

## 10. Versioning and conflict model

Use version vectors to reason about causality rather than relying on modification time alone.

For vectors A and B:

- A dominates B if every counter in A is greater than or equal to the corresponding counter in B and at least one is greater.
- If B dominates A, B is causally newer.
- If neither dominates the other, the changes are concurrent and must be handled as a conflict.
- Equal vectors represent the same causal position; verify content identity where relevant.

Vector format, device-counter lifecycle, compaction, and behavior after device removal must be specified and tested before production use.

Rename detection should preserve entry identity when reliably correlated. Otherwise use a conservative delete-plus-create representation.

## 11. Transfer protocol (logical design)

Initial logical messages:

- **`TransferStart`:** protocol version, transfer ID, change ID, folder ID, entry ID, relative path, operation, expected hash/size, version vector, and required metadata.
- **`Chunk`:** transfer ID, offset or index, payload, and integrity metadata.
- **`TransferResume`:** transfer ID, expected hash, and receiver's durable checkpoint or chunk state.
- **`TransferComplete`:** sender indicates all declared content has been sent.
- **`TransferAcknowledged`:** receiver confirms verified and durably accepted content/change according to defined semantics.
- **`ConflictDetected`:** receiver reports a concurrent change.
- **`ChangeApplied`:** confirms application of a non-content operation such as deletion.

The final wire encoding, framing, authentication handshake, size limits, message versioning, and transport library remain open design decisions. Use an established secure protocol and library. Do not design custom cryptography.

### 11.1 Retry and acknowledgement rules

- Keep change IDs stable across retries.
- Make duplicate messages and acknowledgements idempotent.
- Acknowledgements must identify the exact operation or transfer.
- Verify content hash and declared size before publication.
- Keep pending work durable through failures.
- Use bounded exponential backoff with retry limits and visible error states.
- Fail clearly when protocol versions are incompatible.
- Do not mark delivery complete merely because the sender transmitted bytes.

## 12. API requirements

Use versioned endpoints under `/v1`, TLS, an OpenAPI specification, consistent error responses, authorization on protected operations, input validation, rate limits, request-size limits, and idempotency for retried state-changing requests.

Initial endpoints:

| Method | Endpoint | Purpose |
|---|---|---|
| `POST` | `/v1/devices/register` | Register device identity, public key, name, and platform |
| `GET` | `/v1/devices` | List devices visible to the authenticated device |
| `POST` | `/v1/pairing-requests` | Start pairing |
| `POST` | `/v1/pairing-requests/{request_id}/approve` | Approve pairing after required user confirmation |
| `GET` | `/v1/pairing-requests/{request_id}` | Query pairing status |
| `POST` | `/v1/relay-sessions` | Request an authorized relay session |
| `DELETE` | `/v1/devices/{device_id}` | Revoke device access/relationship |

Example error response:

```json
{
  "error": {
    "code": "DEVICE_NOT_AUTHORIZED",
    "message": "The device is not authorized for this operation.",
    "request_id": "req_123"
  }
}
```

The relay must verify sender identity, receiver identity, folder permissions, session expiry, and limits. Add pagination where needed. Avoid leaking secrets or sensitive details in errors.

## 13. Security and threat model

### 13.1 Threats

- Unauthorized LAN peers.
- Malicious or compromised relay clients.
- Stolen device credentials.
- Replay or duplicated messages.
- Malformed protocol messages.
- Resource exhaustion and abusive transfer requests.
- Path traversal and unsafe symlink/junction behavior.
- Crashes during transfer or publication.
- Stale peers and concurrent edits.
- Revoked devices attempting to reconnect.

### 13.2 Required controls

- Generate a secure device key pair.
- Protect private keys with an OS credential store or protected encrypted key file; never store them in plaintext in SQLite.
- Use TLS for the control plane.
- Use established authenticated encryption for device-to-device content so the relay cannot read payloads.
- Authenticate and authorize every protected operation.
- Require explicit approval for first-time pairing.
- Expire and rate-limit pairing requests.
- Validate relative paths and prevent path traversal.
- Define symlink/junction policy and guard against writes escaping the shared root.
- Apply size, concurrency, memory, staging, and request limits.
- Revoke trust and invalidate active sessions where possible.
- Revalidate authorization after offline periods.
- Keep secrets and file contents out of logs.

### 13.3 Filesystem publication

Receiver flow:

1. Write content to a temporary file within a safe destination area.
2. Verify declared size and hash.
3. Check version/conflict state.
4. Safely publish using platform-appropriate atomic operations where available.
5. Commit applied-change state.
6. Acknowledge only when the documented durability condition is met.

Because the filesystem and SQLite are separate transaction domains, recovery must detect and repair crashes between publication, database commit, and acknowledgement.

## 14. Reliability and recovery

- Filesystem watcher events are hints; periodic reconciliation is required.
- An incomplete scan must not trigger mass deletions.
- Journal every change before relying on delivery.
- Stage immutable content for revisions that must remain available to pending transfers.
- Resume from the receiver's durable checkpoint.
- Retry transient failures with backoff.
- Retain conflicts until explicitly resolved or dismissed.
- Retire tombstones only after relevant peers have observed deletion or an explicit device-removal policy permits retirement.
- Garbage-collect staged content only after checking all journal, transfer, and recovery references.
- Document what each acknowledgement means and which data has been durably stored.

## 15. Observability and operations

- Structured tracing/logging on client and server.
- Actionable status and diagnostic output.
- Configurable log retention and redaction of sensitive fields.
- Server monitoring for active sessions, connection failures, relay throughput, quotas, and authorization failures.
- Database migrations and a documented upgrade procedure.
- Staging cleanup and disk quota handling.
- Troubleshooting guidance for firewall, discovery, permissions, and protocol compatibility.

## 16. Testing strategy

### 16.1 Unit tests

- Version-vector comparison.
- Conflict detection.
- Path validation and root containment.
- Change classification and rename handling.
- Content hashing and integrity checks.
- Retry/backoff and transfer state transitions.
- Tombstone retirement.
- Protocol validation and authorization decisions.

### 16.2 Database tests

- Migration upgrades.
- Foreign-key constraints and uniqueness.
- Atomic journal creation.
- Duplicate remote change handling.
- Multiple peers per change.
- Content-staging cleanup safety.
- Crash-recovery state transitions.

### 16.3 Integration tests

- Linux-to-Linux, Linux-to-Windows, and Windows-to-Linux.
- Online relay and offline LAN modes.
- Empty files, large files, nested directories, renames, and deletions.
- Disconnect/restart and transfer resume.
- Duplicate messages and acknowledgements.
- Concurrent edits and conflict resolution.
- Device revocation and unauthorized access.
- Missed watcher events repaired by reconciliation.

### 16.4 Crash-injection points

Test failures:

- After journal insert.
- During content staging.
- Halfway through transfer.
- After filesystem publication but before applied-change database commit.
- After apply but before acknowledgement.
- During staged-content garbage collection.

## 17. Acceptance criteria

The v1 release is acceptable when tests demonstrate that:

1. Authorized devices synchronize shared folders in online and supported offline LAN modes.
2. Pending changes survive application restarts and network outages.
3. Interrupted transfers resume from verified durable progress.
4. Files are verified before publication.
5. Concurrent edits are preserved and conflicts are visible.
6. Deletions do not resurrect because of stale peers.
7. Unauthorized peers are denied.
8. Revoked peers cannot start new authorized sessions after revocation propagates.
9. Reconciliation repairs missed watcher events without interpreting incomplete scans as deletions.
10. Linux and Windows platform tests pass.
11. The server does not persistently store file contents.
12. Resource limits and failure states are observable and tested.

## 18. Implementation roadmap

### Phase 1 — Local foundation

- Rust workspace and CLI.
- Device identity and configuration.
- SQLite migrations and shared-folder registry.
- Filesystem watcher and reconciliation.
- Hashing, immutable staging, and durable journal.
- Local status and diagnostics.

### Phase 2 — Offline LAN synchronization

- mDNS discovery.
- Explicit local pairing and authenticated connections.
- Change metadata exchange.
- Chunked transfer and integrity verification.
- Resume/checkpoints, conflict handling, and deletion tombstones.

### Phase 3 — Online coordination and relay

- Device registration and pairing APIs.
- PostgreSQL persistence.
- Authenticated client connections and authorized relay sessions.
- Encrypted payload forwarding.
- Reconnect and pending-change recovery.

### Phase 4 — Reliability and security hardening

- Crash recovery and fault injection.
- Reconciliation improvements.
- Conflict workflow.
- Tombstone retirement and staged-content garbage collection.
- Revocation and resource limits.
- Cross-platform integration tests.

### Phase 5 — Release readiness

- Linux and Windows packaging.
- Installation and upgrade documentation.
- API and protocol specifications.
- Benchmarks and limits based on measurements.
- Security review and release checklist.

## 19. Documentation deliverables

- `README.md`: overview, features, limitations, architecture, installation, quick start, CLI, privacy/security, and development setup.
- `docs/SRD.md`: this requirements document.
- `docs/architecture.md`: components, boundaries, flows, failure boundaries, trust boundaries, and architecture decisions.
- `docs/database.md`: ER diagram, schema, constraints, indexes, transactions, migrations, state transitions, tombstones, and recovery.
- `docs/api/openapi.yaml`: endpoints, schemas, auth, errors, idempotency, pagination, rate limits, and versioning.
- `docs/protocol.md`: wire encoding, versions, authentication, chunks, resume, acknowledgements, retries, conflicts, and disconnect recovery.
- `docs/security.md`: threat model, identity, key storage/rotation, pairing/revocation, encryption, authorization, path safety, and limits.
- `docs/operations.md`: agent lifecycle, configuration, logs, backup/restore, staging cleanup, troubleshooting, upgrades, and relay monitoring.
- `docs/testing.md`: commands, test strategy, platform matrix, crash tests, benchmarks, and release checklist.
- `docs/adr/`: decisions for relay rather than internet P2P, SQLite/PostgreSQL, durable journal, version vectors, content staging, mDNS, and the monolithic server.

## 20. Open decisions before production

- Final transport library and wire format.
- Exact pairing UX, approval flow, and expiry.
- Version-vector compaction and device-counter lifecycle.
- Symlink/junction policy, permissions, case-only renames, and unsupported file types.
- Tombstone retirement for long-offline or permanently removed devices.
- Durability and flush policy per platform/filesystem.
- Chunk size, concurrency, retry defaults, and staging quotas based on benchmarks.
- Protocol compatibility and upgrade strategy.

## 21. Baseline architecture decision

SyncEngine v1.0 consists of:

- A modular Rust client agent with CLI.
- SQLite local state and a durable change journal.
- Immutable local content staging for pending revisions.
- A Rust coordination-and-relay server backed by PostgreSQL.
- Direct authenticated LAN synchronization.
- A versioned synchronization protocol.
- Integrity verification, recoverability, conflict preservation, and security as first-class requirements.

The design is a baseline, not proof of correctness. Reliability and security claims must be demonstrated through implementation, tests, and review.
