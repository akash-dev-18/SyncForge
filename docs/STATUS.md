# SyncEngine — Status (after Milestone 6, release readiness)

**Date:** 2026-10-18  
**Milestones:** M1–M6 complete — M6 (release readiness: `logs`/`doctor`, documentation set, README,
backup/recovery guidance, security review) finished 2026-10-18.  
**Workspace checks:** `cargo fmt --all -- --check` ✅, `cargo clippy --workspace --all-targets -- -D warnings` ✅,
`cargo test --workspace` ✅ — **192 tests, 0 failures** (6 of them are PostgreSQL-backed and were run green
against Docker Postgres with `SYNCENGINE_TEST_DATABASE_URL` set).  
**Dependency audit:** `cargo audit` ✅ — **0 vulnerabilities** across 288 dependencies (RustSec database,
1338 advisories, cargo-audit 0.22.2).

## What works (evidence)

| Item | Verification |
|---|---|
| Protocol crate | 32 tests: IDs, version vectors (dominates/concurrent), relative-path safety, framing, control-plane canonical signing + pairing proof |
| SQLite storage crate | 18 integration tests: schema v1 migrations, journal + targets atomicity, tombstones (retirement + newest-per-path lookup), conflicts, content objects, `delete_peer`, change-target backfill, restart persistence |
| Sync engine crate | 39 tests (24 unit + 15 `sync_flow`): identity/crypto (X25519+ChaCha20-Poly1305, Ed25519), scanner (symlink-safe), staging with **durable resume checkpoints**, chunker, and end-to-end sync flows: create/update/delete/idempotency/stale, concurrent edit conflict + preserved copy, **delete-vs-edit**, **recreate-after-delete**, **rename pair**, **crash-after-publication**, **staged-content GC**, and the three **conflict resolutions** |
| Transport crate | 26 tests (12 unit + 14 integration): framing/limits, sealed handshake, channel, relay-URL helper, real mDNS discovery, two-engine LAN sync over loopback TCP, unknown-device rejection, relay WebSocket echo incl. a 200 KiB frame, and session flows incl. **interrupted-transfer resume**, **concurrent edit-vs-delete over a real session**, **rename propagation**, **apply-then-crash-before-ack recovery**, clean idle close |
| CLI: `init`, `status`, `devices`, `conflicts` | Config creation/validation; read-only status; devices list peer trust state; `conflicts` lists with unresolved counts and `conflicts resolve <id> --keep-local\|--keep-remote\|--dismiss` (clap-enforced: exactly one action) |
| CLI: `share`, `unshare` | Registers canonicalized folder roots; refuses roots containing the data dir; `--join <folder-id>` adopts the same folder id on both devices; best-effort server membership announce (18 client integration tests) |
| CLI: `pair`, `unpair` | Initiate shows request id + one-time code and polls ≤10 min; `--approve --code` completes with the hashed proof; both sides fetch each other's keys; unpair deletes local trust and best-effort server revocation |
| CLI: `sync` | Full pass: scan → link folders to peers → mDNS advertise → dial trusted peers over LAN → relay fallback when `server_url` is set → print per-peer outcomes; changes stay journaled while a peer is unreachable; reclaims unreferenced staged content at the end of a pass |
| CLI: `daemon start/status/stop` | Real background process: fresh-heartbeat + PID-liveness guard against double start, `daemon.json` state, log redirection, SIGTERM/taskkill stop, stale-state cleanup; foreground `daemon run` loop scans + syncs on the scan interval |
| CLI: `logs` (M6) | Tails `daemon.log` (`--lines N`, default 50), `--path-only` prints the path; missing file exits 1 with a clear message; covered by `logs_reports_a_missing_file_and_then_shows_agent_output` |
| CLI: `doctor` (M6) | Checks config, data dir, identity, database, folders, devices, queue, conflicts, agent state, and server `/healthz` (unless `--skip-server`); FAIL findings exit 1; verified against a live server **and** by a full daemon lifecycle smoke run (share → start → status → logs → stop) |
| Engine<->transport LAN e2e | `client/tests/e2e_lan.rs`: real `sync` command against a peer fixture transfers files both ways over mDNS+TCP (server absent), and journaled changes deliver on reconnect after the peer was offline |
| Server: migrations | `0001_control_plane` + `0002_device_encryption_key` applied to Docker Postgres |
| Server: device APIs | Register (TOFU key binding, 409 on key mismatch, `ENCRYPTION_KEY_INVALID`), list, revoke self/relationship, paired-device key exchange (`GET /v1/devices/{id}/keys`) |
| Server: pairing | Code shown once, stored only as `pairing_code_proof` hash; 10-min expiry; 5 attempts; 5 pending/10 min cap; approve creates relationship |
| Server: authorization | Ed25519-signed canonical requests (`x-device-id/x-timestamp/x-signature`), 300s skew, revoked devices rejected on every operation |
| Server: relay | Opaque Text/Binary forwarding between two authorized peers, 16 MiB frame cap, bounded (32-message) per-direction buffering, join timeout, expired-session cleanup loop, participant-scoped session listing — never persisted |
| Relay e2e (client ↔ server ↔ client) | `server/tests/sync_e2e.rs`: two real client installations pair through the control plane, share one folder id, and sync files both ways through the relay WebSocket with sealed frames |
| Error envelope | All errors render `{"error":{"code","message","request_id"}}`; internals never leak (tested) |

**Test breakdown (192):** protocol 32 · storage 18 · engine 39 (24+15) · transport 26 (12+14) ·
client 50 (30+18+2) · server 27 (14 unit + 7 binary + 5 PostgreSQL API + 1 PostgreSQL relay e2e).

## M6 — Release readiness (evidence)

- **CLI completeness.** All commands in `AGENT.md` §7 exist and work; the placeholder
  `not_implemented`/`milestone()` machinery was deleted — nothing pretends to succeed.
- **Documentation set** (per `AGENT.md` §9): `docs/ARCHITECTURE.md`, `docs/DATA_FLOW.md`,
  `docs/DATABASE.md`, `docs/PROTOCOL.md`, `docs/DECISIONS.md` (ADR style), `docs/LEARNING_GUIDE.md`
  (reading order, Rust concepts, exercises), plus a root `README.md` with Linux/Windows build
  instructions, a two-device quickstart, a commented config example (reserved keys marked), data
  locations, troubleshooting, and the honest limitations list.
- **Backup/recovery guidance** for both databases: `docs/DATABASE.md` §1.5–1.6 (SQLite: stop
  writer, `.backup` + `identity.json`, staging caveats, recovery table) and §2.4–2.5 (PostgreSQL:
  `pg_dump`/restore, re-registration path).
- **Security/dependency review** (manual + automated, 2026-10-18):
  - `cargo audit` 0.22.2: **0 vulnerabilities** across 288 dependencies.
  - Zero `unsafe` in first-party sources (`client/`, `server/`, `crates/`).
  - TLS stack is rustls-only — no OpenSSL or native-tls anywhere in the dependency tree.
  - All SQL is parameterized (rusqlite and tokio-postgres); no string-built SQL.
  - File publication re-validates `RelativePath` before joining the folder root (defense in
    depth), the scanner skips symlinks, and publication is an atomic rename.
  - `identity.json` is written `0600` on Unix via `restrict_permissions` (`crypto.rs:215`, called
    from the identity save path); Windows relies on directory ACLs (documented limitation).
  - No key material, pairing codes, or message plaintext appears in logs; the relay never parses
    frames.
  - Control plane requires Ed25519-signed canonical requests (±300 s skew) and rejects revoked
    devices on every route; pairing codes exist server-side only as SHA-256 proofs with
    10-minute/5-attempt/5-pending caps.
  - Frame cap (16 MiB) is enforced before allocation; HTTP body cap is 64 KiB.
  - Manifest hygiene: unused `notify` and `tower` workspace dependency entries removed.
- **Verification rerun (2026-10-18):** `cargo fmt --all -- --check` ✅,
  `cargo clippy --workspace --all-targets -- -D warnings` ✅,
  `SYNCENGINE_TEST_DATABASE_URL=… cargo test --workspace` ✅ 192/192, `cargo audit` ✅ 0 findings.

## Conflict and crash-recovery semantics (M5)

- **Concurrent edit vs edit** → the remote revision is preserved as `<name> (conflict from <device>)`
  (suffix `-2`, `-3`, … if the name is taken), a conflict row is recorded, and the local path is left
  untouched. The change stays retryable until the copy is actually written, so a crash mid-preservation
  cannot silently drop a revision.
- **Remote delete vs local edit** → the local revision is preserved as a conflict copy, the conflict is
  recorded, and the delete is applied to the shared path.
- **Remote create/update vs local delete** → decided against the tombstone version: a change that
  dominates it resurrects the file, an older/equal change is stale, a concurrent one is a conflict whose
  remote revision is preserved while the deletion stands.
- **Rename** → journaled conservatively as delete + create (SRD §10); the receiver ends up with the new
  path only, and no conflict is raised.
- **Retry safety** → applying is guarded by `applied_remote_changes`; a redelivery after a crash between
  publication and DB commit re-plans as stale and is acknowledged rather than rewritten.
- **Tombstone retirement** → a tombstone is retired once every trusted peer currently sharing the folder
  has both delivered and acknowledged its deletion target; unpairing a device removes it from the folder,
  which excuses its outstanding targets. Retirement runs opportunistically at the end of each session.
- **Staged-content GC** → objects with refcount 0 that are not queued for any peer are deleted at the end
  of a sync pass; anything still referenced by a present entry or pending delivery is kept.
- **Resolution** → `syncengine conflicts resolve <id> --keep-remote` copies the newest preserved copy back
  to the shared path and journals it so the decision propagates; `--keep-local` keeps the current
  revision (the copy stays on disk); `--dismiss` only updates the conflict record.

## Incomplete (honest)

| Milestone | State |
|---|---|
| M6 — Release readiness | ✅ Complete (2026-10-18): `logs`, `doctor`, docs set, README, backup/recovery, security review, full verification. No known open acceptance criteria; the remaining gaps are the limitations below. |

## Known limitations (honest)

- **Resume semantics**: an interrupted LAN transfer resumes from a content-addressed
  `.recv-<hash>` checkpoint in staging (the transfer is restarted at the checkpoint length, and
  the final BLAKE3 check validates everything). A crash can leave a torn tail; the next transfer
  either completes or fails the hash check and restarts cleanly. There is no per-chunk fsync.
- **Crash-injection coverage** is test-driven rather than exhaustive: interrupted transfer, crash after
  publication before the applied record, and apply-before-ack are covered; the remaining SRD §16.4 points
  (e.g. torn journal writes mid-`append_change`) rely on SQLite transactions rather than dedicated tests.
- **Conflict copies are ordinary files**: once written, `foo (conflict from B).txt` syncs to peers like
  any other file (Syncthing-style) rather than staying device-local.
- **Version-vector compaction** is not implemented; entries keep their counters per device, and device
  removal relies on unpairing (trust row deleted, folder membership dropped) rather than vector
  rewriting. A permanently lost device keeps its counters, so its vector can never be "beaten".
- **Journal growth**: `changes`, `change_targets`, and `applied_remote_changes` are kept forever — no
  compaction job runs (it is exercise #8 in `docs/LEARNING_GUIDE.md`).
- **Reserved config keys**: `agent.debounce_ms` (no filesystem watcher yet) and
  `limits.max_concurrent_transfers` / `limits.staging_max_bytes` (transfers are sequential and staging
  GC is refcount-based) are validated but not yet enforced. `limits.chunk_size_bytes` **is** enforced.
- **Tombstones are retired only for trusted peers still in the folder**; a device that is unreachable
  forever (never unparied) keeps its targets outstanding.
- **TLS is not terminated in-process**: `syncengine-server serve` speaks plain HTTP and binds loopback
  by default; a TLS-terminating reverse proxy is required before any real deployment.
- **No forward secrecy**: sealed frames use static-static X25519 ECDH keys (no ratchet), so a leaked
  device private key could decrypt recorded sessions. Recorded in `docs/DECISIONS.md` ADR-13.
- **Private keys**: stored as `identity.json` in the data directory with `0600` on Unix.
  An OS credential store / encrypted key file (SRD §13.2) is not implemented; on Windows there are no
  permission restrictions.
- **Rate limiting** is per-request-class (pairing attempts, pending requests, open relay sessions).
  There is no global per-IP rate limiter.
- **Presence** is just `devices.last_seen_at`; no session/presence protocol.
- **Multi-session stress** on the relay (many concurrent sessions, backpressure under load) is not
  tested; the end-to-end relay path with two clients is.
- **mDNS needs a multicast-capable interface**; a network that blocks multicast falls back to the
  relay only when `server_url` is configured. Discovery prefers IPv4 and re-browses when only a
  link-local IPv6 record has resolved so far.
- **Windows is untested**; symlink/junction policy is implemented for scanning (skip), and the
  daemon has a `taskkill /F` stop path, but neither has run on Windows. Case-only renames are
  untested (they are journaled as delete + create).
- **Pairing initiation** polls status every 2s for up to 10 minutes in the foreground; there is no
  notification mechanism.
- DB-backed server tests print a `SKIP:` line to stderr and return when
  `SYNCENGINE_TEST_DATABASE_URL` is unset; run them with the variable set for real coverage.
- No global account model (per SRD §9, none is required).

## Next (post-v1 backlog)

1. Filesystem watcher (`notify`) triggering early scans with `debounce_ms` coalescing.
2. Parallel transfers honoring `max_concurrent_transfers`, plus a staging size quota.
3. Journal compaction + version-vector pruning for long-lived deployments.
4. Forward-secret session ratchet (protocol version 2).
5. Verification on real Windows hardware (paths, daemon, permissions, case-only renames).
6. OS credential-store integration for `identity.json` (SRD §13.2).
7. Global rate limiting and relay multi-session stress tests.
