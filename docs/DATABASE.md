# SyncEngine — Databases

SyncEngine uses two independent stores:

| Store | Owner | Holds | Versioning |
|---|---|---|---|
| SQLite (`syncengine.db`) | each device (client) | identity-adjacent state, file index, change journal, delivery ledger, conflicts, content cache metadata | `SCHEMA_VERSION` + `schema_migrations` table, migrations embedded in `crates/syncengine-storage/src/sqlite.rs` |
| PostgreSQL | coordination server | device registrations, pairing requests, folder memberships, relay-session bookkeeping | ordered SQL files in `server/migrations/`, applied via `schema_migrations` |

The two schemas share nothing at runtime except the ID types from `syncengine-protocol`. The server
never stores file contents, absolute local paths, or the client's file index.

---

## 1. SQLite (client)

### 1.1 Location and pragmas

- Default path: `{data_dir}/syncengine.db`
  - Linux: `$XDG_DATA_HOME/syncengine/` (fallback `~/.local/share/syncengine/`)
  - Windows: `%APPDATA%\syncengine\`
- Opened by `SqliteStore::open` with: WAL journal mode, `foreign_keys = ON`, `synchronous = NORMAL`,
  and a 5 s busy timeout. WAL means `syncengine.db-wal` / `-shm` files appear next to the database;
  all three belong together when copying.
- Migrations run automatically on open (`run_migrations`): each entry is `(version, [sql…])` and is
  applied inside a transaction, recorded in `schema_migrations`. Current `SCHEMA_VERSION = 1`.

### 1.2 Tables

| Table | Purpose | Notes |
|---|---|---|
| `local_device` | this device's id, name, platform, public keys | single logically-singular row |
| `peers` | trusted peers: id, name, platform, Ed25519 + X25519 public keys, trust state | handshake keys must match these rows exactly |
| `shared_folders` | folders this device syncs: id, absolute root, enabled flag | relative paths in all other tables are relative to `root_path` |
| `folder_peers` | which trusted peer participates in which folder | drives change targeting and tombstone retirement |
| `file_entries` | current state per (folder, relative path): present/deleted, hash, size, entry id, version | unique per folder+path; indexed by folder |
| `changes` | append-only journal: one row per local mutation (op, paths, hash, size, versions, origin, timestamps) | never compacted in this milestone |
| `change_targets` | per-peer delivery ledger for each change (pending → delivered/acknowledged, error text) | a change is "pending for a peer" iff its target row is not acknowledged |
| `tombstones` | durable deletion records (folder, last path, version, change id) | used to arbitrate delete races; retired once all peers ack |
| `peer_sync_state` | optional per (folder, peer) sync cursor | maintained via the Store API; not yet driven by the transport loop |
| `transfers` | optional per-change transfer bookkeeping (direction, state, checkpoint) | exercised by storage tests; live resume uses `.recv-*` partials instead |
| `applied_remote_changes` | change ids of remote changes already applied | the idempotency guard (`is_applied`) |
| `conflicts` | recorded conflicts: path, local/remote versions + hashes, resolved/unresolved | `--dismiss`/`resolve` update status only; the copy files are the data |
| `content_objects` | staged blobs: content hash → size, refcount | refcount incremented per referencing path; GC removes ≤ 0 unused objects |

Indexes: `idx_file_entries_folder`, `idx_changes_folder`, `idx_change_targets_peer_state`,
`idx_transfers_change`, `idx_conflicts_status`.

Relationships: `changes.folder_id → shared_folders`, `change_targets.change_id → changes` (cascade),
`file_entries.folder_id → shared_folders`, `folder_peers → ({peers}, {shared_folders})`.

### 1.3 Retention behavior

- Deletions leave `tombstones` behind until every trusted folder peer acknowledged the delete;
  then `retire_tombstones` removes them.
- Staged content (`staging/<hash>` files and `content_objects` rows) is garbage-collected when the
  refcount drops to zero and no pending delivery references the hash.
- The change journal (`changes` + `change_targets`) and `applied_remote_changes` grow without bound
  in this milestone — see "Known limitations" in `docs/STATUS.md`.

### 1.4 Inspecting it yourself

```sh
sqlite3 ~/.local/share/syncengine/syncengine.db \
  "SELECT relative_path, state, size FROM file_entries ORDER BY relative_path LIMIT 20;"
sqlite3 ~/.local/share/syncengine/syncengine.db \
  "SELECT COUNT(*) FROM change_targets WHERE acknowledged_at IS NULL;"
sqlite3 ~/.local/share/syncengine/syncengine.db \
  "SELECT relative_path, status FROM conflicts WHERE status = 'unresolved';"
```

### 1.5 Backup

1. Stop the writer: `syncengine daemon stop` (CLI commands are short-lived writers and finish on
   their own).
2. Back up **together**: `syncengine.db`, `-wal`, `-shm` (or use `sqlite3 … ".backup out.db"`), and
   `identity.json`. Losing `identity.json` loses the device identity: every peer would have to
   re-pair.
3. `staging/` is a cache: it can be excluded, with the caveat below. `daemon.json` / `daemon.log`
   are disposable.

```sh
syncengine daemon stop
sqlite3 "$DATA/syncengine.db" ".backup /backup/syncengine-$(date +%F).db"
cp "$DATA/identity.json" /backup/
```

### 1.6 Recovery

| Situation | Guidance |
|---|---|
| DB corrupted, identity intact, healthy backup | Stop the daemon, replace `syncengine.db` (+ remove stale `-wal`/`-shm`), restart. |
| DB corrupted, no backup | Last resort: stop daemon, move `syncengine.db*` aside and let a fresh DB be created, then run `syncengine sync`. A fresh scan re-journals every file as a new revision; peers' version vectors are already ahead, so expect conflict copies rather than silent adoption. Re-check `syncengine conflicts`. |
| `staging/` lost but DB kept | Scans do not re-stage unchanged files, so previously-journaled-but-undelivered content can fail with `CONTENT_UNAVAILABLE`. Touch/rewrite the affected files (or restore staging from backup) to re-stage; pending deliveries then complete. |
| Stale `daemon.json` after a crash | Harmless: `daemon status` and `doctor` verify pid liveness before trusting it; `daemon start` overwrites it. |
| Accidental `unpair` | The peer's trust row is gone; re-pair with `syncengine pair`. Version vectors of the peer remain in journaled changes, so re-sync converges. |

---

## 2. PostgreSQL (server)

### 2.1 Local development

```sh
docker compose up -d --wait postgres
export DATABASE_URL=postgres://syncengine:syncengine@localhost:5432/syncengine
cargo run -p syncengine-server -- serve --bind 127.0.0.1:8443
```

`serve` runs migrations automatically at startup; `syncengine-server migrate` does the same and
exits (useful in deployment pipelines). Migrations are embedded into the binary (`include_str!`), so
there are no missing-file surprises.

### 2.2 Tables

| Migration | Table | Purpose |
|---|---|---|
| `0001_control_plane.sql` | `devices` | registration: id, name, platform, public keys, revocation, `last_seen_at` (refreshed on authenticated calls) |
| `0001_control_plane.sql` | `pairing_requests` | one pending pairing per request: requester/target, hashed code proof, expiry, attempt counter, status |
| `0001_control_plane.sql` | `device_relationships` | paired device pairs (one row per ordered pair) |
| `0001_control_plane.sql` | `folder_memberships` | which relationship may relay which folder id; folder ids are opaque to the server |
| `0001_control_plane.sql` | `relay_sessions` | short-lived session rows: id, folder, participants, expiry, consumed flag |
| `0002_device_encryption_key.sql` | `devices.encryption_key` | X25519 public key column (relay-side key distribution) |

(Names above match the embedded SQL; the file is authoritative — read
`server/migrations/0001_control_plane.sql` for exact columns and partial indexes.)

### 2.3 What the server must never hold

- File contents or content hashes tied to plaintext — relay frames are sealed by the clients and
  forwarded opaquely.
- Absolute local paths — memberships store only client-generated folder ids.
- Long-term message history — relay sessions expire after 5 minutes and rows are cleaned up.

### 2.4 Backup

```sh
docker compose exec -T postgres pg_dump -U syncengine syncengine > backup.sql
```

The data is metadata-only and re-creatable in principle, but losing it means devices must re-register
and re-pair, and memberships must be re-established — so back it up like real state.

### 2.5 Recovery

```sh
docker compose down
docker compose up -d --wait postgres
cat backup.sql | docker compose exec -T postgres psql -U syncengine -d syncengine
```

Forward-only migrations: restoring an old dump and starting a newer server simply replays the newer
migrations. Rolling back a schema version is not supported.

If the database is lost entirely: bring up an empty one, restart the server, and re-pair devices
(`syncengine pair`). Relay paths recover automatically; LAN sync was never dependent on the server.
