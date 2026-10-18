# SyncEngine — Learning Guide

**Audience:** anyone using this codebase to learn Rust or distributed-systems engineering. Read
this after `docs/ARCHITECTURE.md`; keep `docs/DATA_FLOW.md` and `docs/PROTOCOL.md` open as
references while exploring.

## 1. Pick your track

| Track | Path |
|---|---|
| **User** — just want to run it | Root `README.md` → `docs/DATABASE.md` §1.5 (backups) → `syncengine doctor`. |
| **Contributor** — want to change it | `docs/ARCHITECTURE.md` → `docs/DATA_FLOW.md` → read tests next to each module → `docs/DECISIONS.md`. |
| **Rust learner** — want the concepts | The reading order below, one module at a time, exercises at the end. |

## 2. Suggested reading order

1. **`crates/syncengine-protocol/src/ids.rs`** — the newtype pattern.
   Notice: `DeviceId` is a UUID wrapper, not a type alias. Why? Type safety: you cannot pass a
   `FolderId` where a `DeviceId` is expected, and both get `Display`/`FromStr`/serde for free.
2. **`crates/syncengine-protocol/src/version.rs`** — version vectors and causal ordering.
   Notice how `CausalOrder` reduces to comparing two `BTreeMap`s with four outcomes; every sync
   decision in the system funnels through these ~60 lines.
3. **`crates/syncengine-protocol/src/path.rs`** — untrusted input.
   `RelativePath` validates `../`, absolute paths, and empty segments. This is validation-at-the-
   boundary: once you hold a `RelativePath`, no later code re-checks it.
4. **`crates/syncengine-protocol/src/messages.rs`** — serde enums as a protocol.
   Notice `#[serde(tag = "type", rename_all = "snake_case")]` and the handshake transcript function
   (the "sign exactly these bytes" pattern).
5. **`crates/syncengine-storage/src/traits.rs`** — trait design.
   52 methods, all `Result`. Ask: which ones does the engine actually call? Why is the trait the
   right seam for tests (`open_in_memory()`)?
6. **`crates/syncengine-storage/src/sqlite.rs`** — SQL plumbing.
   Transactions, prepared statements, mapping rows to structs, embedded migrations, WAL pragmas.
   Notice how every mutation is one transaction and why that makes crash points recoverable.
7. **`crates/syncengine-engine/src/crypto.rs`** — real cryptography, ownership of secrets.
   `Identity` is deliberately **not** `Clone`. Notice the composition: X25519 ECDH → HKDF-SHA256
   (domain-separated with `b"syncengine-e2e-v1"`) → ChaCha20-Poly1305 with a 12-byte nonce prefix.
8. **`crates/syncengine-engine/src/staging.rs`** — filesystem discipline.
   Content addressing, streaming BLAKE3, atomic rename, appendable `.recv-*` partials, `Drop`
   cleanup. This file is a masterclass in crash-consistent file I/O.
9. **`crates/syncengine-engine/src/sync.rs`** — the orchestrator (~1500 lines).
   Read `queue_local_changes` (disk → journal) then `plan_remote_change` (journal → decision). The
   `match` arms here *are* the conflict semantics of `docs/DATA_FLOW.md` §4.
10. **`crates/syncengine-transport/src/session.rs`** — an async state machine.
    Four counters (`outstanding_requests`, `outstanding_acks`, `outstanding_fetches`, `incoming`)
    drive the loop; `tokio::time::timeout` implements the idle rule. Notice `PeerChannel` borrows
    the `Identity` with lifetime `'a` instead of cloning keys around.
11. **`client/src/delivery.rs`** and **`client/src/daemon.rs`** — composition.
    How a library-grade engine becomes a product: LAN-first pass logic, inbound windows, pid-file
    supervision, spawn of yourself as a detached process.
12. **`server/src/api.rs`** — axum on the other side of the protocol you just learned.
    Extractors, `State`, error-to-response mapping via one `ApiError` type, and
    `OriginalUri`/query-parameter auth for the WebSocket route.

## 3. Rust concepts by module

| Concept | Where to see it done well |
|---|---|
| Newtypes + `Display`/`FromStr` | `protocol/src/ids.rs` |
| `BTreeMap` ordering logic | `protocol/src/version.rs` |
| Error enums with `thiserror` | every crate's `error.rs`, `protocol/src/path.rs` |
| Serde tagged enums + `skip_serializing_if` | `protocol/src/messages.rs` |
| Traits as test seams, `dyn` vs generic | `storage/src/traits.rs` (`Store`), `transport/src/channel.rs` (`Box<dyn FrameIo>`) |
| RAII / `Drop` for cleanup | `engine/src/staging.rs` (receive files), `server/src/relay.rs` (session rows) |
| Lifetimes used, not fought | `PeerChannel<'a>`, `RemoteClient<'a>`, `IncomingFile<'a>` |
| `async`/`await` + timeouts + `select!` | `transport/src/session.rs`, `server/src/relay.rs` |
| `spawn_blocking` for blocking APIs | `client/src/delivery.rs` (mDNS lookup) |
| Process management | `client/src/daemon.rs` (spawn, signals, pid liveness, `taskkill`) |
| Platform `cfg` code | `config.rs` paths, `daemon.rs` signals, `crypto.rs` file permissions |
| Partial moves / struct destruction | `state.rs::into_parts`, `IncomingTransfer` destructuring |

## 4. Systems concepts this repo demonstrates

- **Causal consistency via version vectors** — `docs/DATA_FLOW.md` §3 table maps one-to-one onto
  `RemotePlan`.
- **At-least-once delivery + idempotency** — every change may be re-sent; `applied_remote_changes`
  and `is_applied` make application exactly-once *effective*.
- **Content addressing** — hashing defines identity; staging is immutable; resume offsets are
  trusted only until the final hash check.
- **Crash consistency** — one transaction per mutation; partials with clean restart; the crash
  table in `docs/DATA_FLOW.md` §5 is the checklist.
- **Trust-on-first-use + out-of-band secrets** — pairing in `docs/PROTOCOL.md` §6.2.
- **ECDH + HKDF + AEAD composition** — `crypto.rs`; the relay is a byte pipe by design.
- **Framing hostile input** — 4-byte cap check before allocation; body limits; base64 validation.

## 5. Exercises (roughly in difficulty order)

1. **Follow one change.** Run two devices over LAN, edit a file, then with
   `RUST_LOG=debug syncengine sync` and `sqlite3` (queries in `docs/DATABASE.md` §1.4) correlate:
   journal row → target row → staged object → peer's entry row.
2. **Add a doctor check.** Add a staging-directory size check to `commands::doctor` in
   `client/src/commands.rs` (pattern: return a `DoctorCheck`). Add a CLI test like the existing
   ones in `client/tests/cli.rs`.
3. **New conflict-copy naming test.** In `crates/syncengine-engine/src/sync.rs` tests, create two
   conflicts for one path and assert the `-2` suffix. Find the existing naming helper first.
4. **Version vector property test.** In `protocol/src/version.rs` tests, loop over small
   hand-written vectors and assert `Dominates` implies a merge equals self (idempotence).
5. **Error-frame test.** In `transport/src/session.rs` tests (two in-memory channels), send a
   `ContentRequest` for an unknown change and assert an `UNKNOWN_CHANGE` error frame comes back.
6. **`logs --follow`.** Implement tailing (poll with `tokio::time::sleep`) in
   `commands::logs`; keep exit codes consistent.
7. **`status` shows relay usage.** If `server_url` is set, list `GET /v1/relay-sessions` in
   `status` (extend `RemoteClient`); skip gracefully when the server is unreachable.
8. **Journal compaction.** Add a `Store` method that deletes `changes` rows whose targets are all
   acknowledged and older than N days (mind `applied_remote_changes` on other devices only; local
   pruning is safe). Wire it into daemon tick; test with the storage test harness.
9. **Watcher.** Add the `notify` crate (workspace dependency) to the client, watch shared folders,
   and make events just trigger "scan now" with `agent.debounce_ms` coalescing. Never let events
   replace the reconciliation scan.
10. **Concurrency.** Honor `limits.max_concurrent_transfers` by pipelining multiple folders'
    transfers in one session (currently strictly sequential); measure with two LAN devices.
11. **Forward secrecy.** Design a per-session ratchet (ephemeral X25519 + rekey after N frames)
    behind a protocol version bump; document the change in `docs/PROTOCOL.md` and ADR style.
12. **Server test.** In `server/tests`, add a case where a revoked device's relay WebSocket join
    is rejected (`DEVICE_REVOKED`); it requires `SYNCENGINE_TEST_DATABASE_URL`.

## 6. Debugging toolbox

| Goal | Tool |
|---|---|
| What is the daemon doing? | `syncengine logs -n 200`, `--json-logs` on any command |
| Is config/env sane? | `syncengine doctor`, `--skip-server` when offline |
| What does the DB think? | `sqlite3` queries in `docs/DATABASE.md` §1.4 |
| Wire-level debugging | `RUST_LOG=syncengine_transport=trace` (frames are logged, not contents) |
| Two-node test quickly | `SYNCENGINE_TEST_DATABASE_URL=… cargo test -p syncengine --test e2e_lan -- --nocapture` |
| Postgres state | `docker compose exec postgres psql -U syncengine -c '\dt'` |

## 7. Glossary

- **Change** — one journaled mutation (create/update/delete) with a version vector.
- **Target** — the per-peer delivery row for a change; "pending for a peer" means its target row is
  not acknowledged.
- **Tombstone** — durable deletion record used to arbitrate races; retired when all peers ack.
- **Staging** — content-addressed immutable cache of file bytes (`staging/<blake3>`).
- **Conflict copy** — the preserved losing revision, stored as a normal sibling file.
- **Relay** — the server's opaque WebSocket forwarder between two paired devices.
- **TOFU** — trust on first use: registration is unauthenticated, trust comes from pairing.
- **Sealing** — per-frame ChaCha20-Poly1305 encryption under the session's ECDH-derived key.
