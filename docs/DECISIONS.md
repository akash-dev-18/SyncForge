# SyncEngine — Decisions

Architecture decisions, in the order they shaped the system. Each entry lists the decision, why it
was made, and the honest trade-off. This complements the SRD (§21 baseline ADR set) and
`docs/syncengine_lld/README.md` (design decisions tied to diagrams).

## ADR-01 — Rust 2024 workspace, shared protocol crate
**Decision:** one repo, one `Cargo.lock`, six packages; `syncengine-protocol` has no I/O
dependencies and is shared by client and server.
**Why:** the server and client must agree on IDs, version vectors, message shapes, and canonical
signing strings — sharing types makes drift impossible at compile time.
**Trade-off:** server builds pull the whole crate topology (unused types ship in the server binary).

## ADR-02 — CLI-first with an optional daemon
**Decision:** every operation is a subcommand; `daemon start` spawns the same binary in foreground
`daemon run` mode. There is no service/tray/systemd integration in v1.
**Why:** scriptable, testable, no platform service plumbing; the SRD's primary persona is a
CLI-capable user with two machines.
**Trade-off:** "just works in the background at login" requires the user to add their own autostart
entry (documented in the README).

## ADR-03 — SQLite for client state (WAL, embedded migrations) over an embedded KV store
**Decision:** one SQLite file per device (`syncengine.db`), one connection, WAL mode, forward-only
migrations embedded in the binary (`sqlite.rs`), all access behind a `Store` trait.
**Why:** relational queries fit the journal + targets + entries model; SQLite is transactional and
crash-safe; the trait keeps the engine testable with in-memory stores.
**Trade-off:** single-writer semantics constrain concurrency (acceptable: one daemon writes).

## ADR-04 — Hand-rolled rusqlite store; no sqlx; Postgres via deadpool/tokio-postgres
**Decision:** `rusqlite` (bundled) for the client; `deadpool-postgres` + `tokio-postgres` for the
server; SQL migrations as embedded files; no ORM.
**Why:** `sqlx` was removed from the workspace: it pulled `native-tls`/OpenSSL and conflicted with
rusqlite over the `libsqlite3-sys` `links` attribute, and the query macro was unusable without a
live DB at build time. The remaining stack is explicit and dependency-light.
**Trade-off:** more manual SQL mapping code; migrations are hand-maintained.

## ADR-05 — Version vectors, not mtimes or timestamps
**Decision:** every change carries a `VersionVector`; causality (`Dominates/Equal/Concurrent`)
decides what to apply. Timestamps are informational (`unix-ms:` strings).
**Why:** wall clocks lie across machines (skew, NTP, dual-boot). Vectors give correct convergence
and make "concurrent edit" decidable (SRD §10).
**Trade-off:** vectors grow with the number of devices that ever wrote a path (fine at this scale).

## ADR-06 — BLAKE3 content addressing with immutable staging and refcounts
**Decision:** staged objects are `staging/<blake3-hex>`; a `content_objects` refcount tracks
referencing paths; GC removes refcount ≤ 0 objects not referenced by pending deliveries.
**Why:** content-addressed copies make "the bytes that were hashed are the bytes that are sent"
true by construction; dedupe comes free (identical files share one object); immutability makes
crash recovery trivial.
**Trade-off:** disk use for staged objects until GC runs; refcounts must be kept honest.

## ADR-07 — Renames are journaled as delete + create (the scanner never detects renames)
**Decision:** `Operation::Rename` exists in the type system but no scan emits it; the delete
creates a tombstone that ties old and new paths.
**Why:** reliable rename detection needs heuristics (hash history, near-match) and buys little at
this scale; delete+create is trivially correct under the lattice, and the tombstone preserves the
race semantics (SRD §10 explicitly blesses this).
**Trade-off:** a renamed large file re-transfers content (the content object dedupes staging, not
bytes-on-wire).

## ADR-08 — Conflicts become ordinary sibling files
**Decision:** the losing revision is written next to the winner as
`"{path} (conflict from {label})"`, scanned and synced like any other file; the `conflicts` table
is a review aid, not the storage of the conflict.
**Why:** conflict copies must converge to *every* device — making them normal files reuses the
whole sync path instead of inventing a second data type to replicate.
**Trade-off:** copies pollute the folder listing (Syncthing-style, intentionally).

## ADR-09 — Receivers never journal remote changes
**Decision:** applied remote changes are recorded only in `applied_remote_changes`; they are not
inserted into `changes`/`change_targets` for re-advertisement.
**Why:** re-journaling would echo changes back and forth (and `ensure_change_targets` backfills
targets for every row, making the echo worse). The original originator already tracks delivery to
all peers; other devices learn the change through their own sessions with the originator (or by
converging later — remote-derived content still converges because the *file* is on disk and the
next local scan journals genuinely new local state only).
**Trade-off:** a device B that got content via C does not forward it to D; D must reach A or C
directly. Acceptable for a personal mesh; documented honestly.

## ADR-10 — Tombstones arbitrate delete races; retirement is acked
**Decision:** incoming creates/renames with no local entry consult `tombstones`; a delete-vs-edit
race resolves as `Stale` (the edit survives); an edit-vs-concurrent-delete preserves the edited
revision as a conflict copy before deleting. A tombstone is retired only when every trusted peer
that shares the folder delivered *and* acknowledged it.
**Why:** "delete wins" or "edit wins" blanket rules each destroy data in one of the directions;
the tombstone makes the decision per-path and per-version. Acked retirement prevents resurrecting
deleted files on a stale device.
**Trade-off:** tombstones linger until every peer sees the delete (unpaired peers stop counting).

## ADR-11 — Conservative scanning
**Decision:** any read error makes the whole scan incomplete and deletions are skipped for that
pass; symlinks are refused; re-hashing is skipped only when size+mtime are unchanged.
**Why:** a half-readable folder must never be interpreted as mass deletion.
**Trade-off:** permission-broken subtrees delay deletions until fixed (safe direction).

## ADR-12 — LAN first, relay fallback; lexicographic initiator
**Decision:** dial the LAN (mDNS lookup) first, fall back to the relay only when `server_url` is
configured; the initiator is the device whose id sorts smaller.
**Why:** LAN is faster, private, and server-free; the sort rule is a deterministic tie-break that
needs no coordination — both sides compute the same answer.
**Trade-off:** both devices must know a server URL for relay to help; NAT situations that break
TCP still depend on the relay.

## ADR-13 — Hand-rolled framing + sealed JSON instead of TLS/Noise/QUIC
**Decision:** 4-byte length prefix + JSON, then ChaCha20-Poly1305 frames under a static-static
ECDH key; the relay stays a dumb byte forwarder.
**Why:** debuggable (readable handshakes, JSON errors), minimal dependencies, and the cryptography
is exactly as strong as it needs to be for an opaque relay.
**Trade-off:** static-static ECDH has **no forward secrecy** — a leaked device private key decrypts
recorded sessions. No per-session ratchet is implemented. Documented limitation, see STATUS.

## ADR-14 — Control-plane auth: Ed25519 over method+path+time+body-hash
**Decision:** one canonical string for every request, signed hex, ±300 s skew, replay-bound.
**Why:** stateless server verification, no sessions/tokens to store, protects both mutable and
immutable endpoints; the WebSocket variant reuses the same construction with query parameters.
**Trade-off:** clients must have roughly-synced clocks (±5 min is generous).

## ADR-15 — Pairing: out-of-band code, hash proof, TOFU registration
**Decision:** registration is trust-on-first-use; pairing requires the target device to approve
with a short code whose hash — not the code — reaches the server; approval creates the mutual trust
rows.
**Why:** no accounts, no passwords, no code in server logs/DB; a compromised server still cannot
pair itself to a device without the code.
**Trade-off:** the code is only 5-attempt/10-minute protected against online guessing; it is meant
to be typed quickly over a trusted channel.

## ADR-16 — Server is a coordinator, not a data store
**Decision:** the server stores identities, pairing state, memberships, and 5-minute relay session
rows only. Relay frames are opaque.
**Why:** trust minimization — a fully compromised server cannot read file contents or enumerate
paths; it can only deny service.
**Trade-off:** the server is a hard dependency for pairing and relay (LAN sync never needs it).

## ADR-17 — Postgres pooling + embedded forward-only migrations
**Decision:** `deadpool-postgres` with max 10 connections; migrations are ordered `.sql` files
embedded with `include_str!`, tracked in `schema_migrations`, run at `serve` startup or via
`migrate`.
**Why:** the binary can never be deployed with missing migration files; ordering is explicit in
the file name.
**Trade-off:** no down-migrations — rolling back a schema requires restoring a dump.

## ADR-18 — Daemon supervision by pid file + liveness, not a service manager
**Decision:** `daemon.json` holds pid/heartbeat/interval; `daemon status` verifies pid liveness
(plus heartbeat freshness `max(3×interval, 45s)`); `stop` sends SIGTERM (Unix) / `taskkill /F`
(Windows).
**Why:** portable, no root, works identically in tests; the state file doubles as `doctor` input.
**Trade-off:** a crash can leave a stale state file — which liveness checks handle by design.

## ADR-19 — CLI contract: stdout for data, stderr for logs, exit 0/1/2
**Decision:** machine-readable output on stdout; tracing on stderr (`--json-logs` for structured);
exit 2 for CLI misuse (clap), 1 for runtime failures, 0 success.
**Why:** scriptability (`syncengine status | jq`-style piping) and CI-friendly failure semantics.
**Trade-off:** none significant; it constrains future output formats to stay stdout-clean.

## ADR-20 — Periodic scans now, filesystem watcher later
**Decision:** the daemon scans on an interval (`agent.scan_interval_secs`); `agent.debounce_ms` is
reserved. The unused `notify` workspace entry was removed during the M6 review; a watcher would
add its own dependency when it lands.
**Why:** correctness of reconciliation scans is required regardless; watchers are a latency
optimization, not a mechanism of record. Walking before running.
**Trade-off:** changes sync on the next tick (seconds), not instantly.

## ADR-21 — No placeholder CLI commands
**Decision:** M6 removed the `not_implemented` machinery; every command in the CLI exists and
either works or does not exist. `logs`, `doctor`, and `conflicts resolve` complete the surface.
**Why:** "commands that pretend" erode trust in a sync tool; the CLI is the product.
**Trade-off:** as features are added they must ship with real behavior.

## ADR-22 — In-memory SQLite for engine tests; env-gated Postgres integration tests
**Decision:** engine/transport tests use `SqliteStore::open_in_memory()`; the few server DB tests
run only when `SYNCENGINE_TEST_DATABASE_URL` is set (docker compose provides it locally); a
two-node LAN e2e test exercises real mDNS/TCP.
**Why:** `cargo test` must pass on a fresh checkout without Docker, while integration coverage
still exists where it matters.
**Trade-off:** CI must set the env var to get Postgres coverage; unset runs silently skip those
tests (they print a skip reason).
