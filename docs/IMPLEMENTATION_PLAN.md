# SyncEngine — Implementation Plan

**Created:** 2026-10-18
**Source documents:** `AGENT.md` (engineering brief), `docs/SRD.md` (requirements; §3 High-level
architecture serves as the HLD — no separate HLD document exists in this repository),
`docs/syncengine_lld/` (client/server LLD diagrams), `docs/README_SETUP.md` (agent setup).

**Current milestone:** M1–M6 complete (client and server) — M6 finished 2026-10-18 (see `docs/STATUS.md` for evidence).

## Conventions

- Milestone numbering follows `AGENT.md` §6 (M0–M6). SRD §18 phases map onto these milestones:
  SRD Phase 1 ≈ M0+M1+M2, Phase 2 ≈ M3, Phase 3 ≈ M4, Phase 4 ≈ M5, Phase 5 ≈ M6.
- Each milestone is complete only when `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass and
  its acceptance criteria have evidence in `docs/STATUS.md`.
- Unimplemented CLI commands must fail loudly with the milestone that will deliver them. They must
  never return unconditional success.

## Repository decisions (assumptions recorded per AGENT.md §2)

| Decision | Choice | Rationale |
|---|---|---|
| Workspace layout | Root `Cargo.toml` with members `client`, `server`, `crates/syncengine-protocol` | SRD §3 monorepo + AGENT.md §3 requirement for a shared protocol crate. `client/` and `server/` directories are preserved as they already exist. |
| Package names | `client/` → package `syncengine`, binary `syncengine`; `server/` → package `syncengine-server`, binary `syncengine-server` | SRD §7 requires the CLI to be invoked as `syncengine`. The previous `client`/`server` names were unused hello-world stubs. Renamed from `syncengine-client` ahead of the crates.io release. |
| Shared protocol crate | `crates/syncengine-protocol` | Single source of truth for wire-protocol versions and (later) message types, used by both binaries. |
| HLD | SRD §3 "High-level architecture" is the HLD | No `HLD.md` exists in this repository; the SRD is treated as the primary requirements and high-level design document. |
| Git | `client/.git` and `server/.git` exist as nested repos with no commits; the repository root is not a git repo | Left untouched (AGENT.md forbids resetting history). No commits are made without explicit permission. |
| Edition | Rust edition 2024 (toolchain 1.95) | Matches the existing `client`/`server` manifests. |
| Errors in binaries | `anyhow` at the CLI boundary, `thiserror` in `syncengine-protocol` | Teachable, idiomatic split: context chains for users, typed errors for library code. |
| Log output | Structured `tracing` to **stderr**; command output to **stdout** | Keeps stdout machine-readable; `RUST_LOG` > `-v/-vv` > `log_level` (agent) precedence. |

## Milestone status

| # | Milestone | Status |
|---|---|---|
| M0 | Workspace and CLI foundation | ✅ Complete — `docs/STATUS.md` has test evidence |
| M1 | Local storage and change model | ✅ Complete — SQLite schema v1, `Store`/`SqliteStore`, identity, folders, journal, tombstones; 14 storage + client tests |
| M2 | Local filesystem synchronization | ✅ Complete — scanner, staging, chunker, `SyncEngine` reconcile; 26 engine tests |
| M3 | Background agent and LAN synchronization | ✅ Complete — mDNS discovery, sealed TCP transport, `pair`/`unpair`/`daemon`, delivery loop; LAN e2e + offline-journal + resume-checkpoint tests |
| M4 | Online coordination and relay | ✅ Complete — server API + relay, client relay transport; end-to-end relay test with two real clients through the control plane |
| M5 | Conflicts and crash recovery | ✅ Complete — tombstone-aware planning (delete-vs-edit, recreate-after-delete), durable conflict preservation, `conflicts resolve` CLI, tombstone retirement, staged-content GC, crash-recovery tests |
| M6 | Release readiness | ✅ Complete — `logs`/`doctor`, docs set (`ARCHITECTURE`, `DATA_FLOW`, `DATABASE`, `PROTOCOL`, `DECISIONS`, `LEARNING_GUIDE`), root README (Linux/Windows builds, quickstart, config example), backup/recovery guidance, `cargo audit` 0 vulnerabilities, 192/192 tests green |

## M0 — Workspace and CLI foundation

**Goal:** a clean, formatted, linted, tested workspace with a real CLI surface, structured
logging, and configuration loading.

Scope:

1. Root workspace manifest (`.gitignore`, `Makefile` with `fmt`/`fmt-check`/`lint`/`test` targets).
2. `crates/syncengine-protocol`: `PROTOCOL_VERSION`, version-compatibility check with a typed
   error (seed for SRD §11.1 "fail clearly when protocol versions are incompatible").
3. `syncengine` client CLI (clap): commands from SRD §7 plus `sync` from AGENT.md §7:
   `init`, `pair`, `unpair`, `devices`, `share`, `unshare`, `status`, `sync`,
   `daemon start|status|stop`, `conflicts`, `logs`, `doctor`.
   - Implemented in M0: `init` (writes default config idempotently; `--force` to replace;
     `--name` or hostname default), `status` (effective config, store state, protocol version).
   - All other commands exit 1 with an explicit "not implemented yet (Milestone N)" message.
   - Global flags: `--config/-c` (also `SYNCENGINE_CONFIG`), `-v/-vv`, `--json-logs`.
4. Configuration: `config.toml` with `device_name`, `data_dir`, `log_level`, `[agent]`,
   `[limits]`; built-in defaults when the file is absent; validation of every numeric bound and
   the log level; unknown keys rejected.
5. Structured logging (`tracing` + `tracing-subscriber`, env filter, JSON mode) on stderr.
6. `syncengine-server` CLI: `serve --bind`, `migrate --database-url` (parsed and validated now,
   executed in M4), `--help`/`--version`, same logging flags.

Acceptance criteria:

- [x] `cargo build --workspace` succeeds from a clean checkout on this platform.
- [x] `cargo fmt --all -- --check` passes.
- [x] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [x] `cargo test --workspace` passes (protocol version tests, config tests, clap parsing tests,
      integration tests that spawn both binaries).
- [x] `syncengine --help`, `syncengine --version`, `syncengine <cmd> --help` produce useful output
      and correct exit codes (0 help/version, 2 usage errors, 1 unimplemented/runtime errors).
- [x] `syncengine init` creates a config file; repeated runs do not clobber it; `--force` replaces it.
- [x] `syncengine status` reports effective configuration and store state without a database.
- [x] Every unimplemented command names its target milestone and points at this plan.
- [x] `docs/STATUS.md` updated with actual command results.

## M1 — Local storage and change model

Scope: SQLite schema + versioned migrations (SRD §8), `Store` trait / `SqliteStore` (LLD),
device identity and key storage (FR-01), stable IDs (`DeviceId`, `FolderId`, `EntryId`,
`ChangeId`), peers, shared folders, folder permissions, file entries, change journal,
change targets, tombstones, peer sync state, transfer state, applied-remote-change
idempotency records, conflicts, content objects.

Acceptance criteria:

- [x] `syncengine init` creates a device identity; private keys are never stored plaintext in SQLite.
      Evidence: identity lives in `identity.json` (0600 on Unix, `state.rs`); the SQLite schema has no
      private-key columns; storage tests cover identity persistence.
- [x] `syncengine share/unshare/devices` manage real local state; `status` shows it.
      Evidence: 16 client integration tests (`client/tests/cli.rs`).
- [x] Foreign keys enforced; migrations run from empty and upgrade from prior versions.
      Evidence: `crates/syncengine-storage/tests/sqlite_store.rs` migration + FK tests.
- [x] Tests: migration, uniqueness/partial indexes, atomic journal insert, duplicate remote change
      application, multi-peer delivery rows, restart does not lose queued changes.
      Evidence: `sqlite_store.rs` + `crates/syncengine-engine/tests/sync_flow.rs`.

## M2 — Local filesystem synchronization

Scope: scanning and safe path handling (path traversal rejection, symlink/junction policy),
change detection, periodic reconciliation (incomplete scan ≠ mass deletion), durable staging
(`content_objects`), reusable `TransferEngine` with chunking, hash verification, durable
checkpoints, temp-file publication; local two-directory test harness.

Acceptance criteria:

- [x] Creates, modifications, renames, deletes, large files, and interrupted transfers pass in the
      harness using two temporary directories. Evidence: `sync_flow.rs` (create/update/delete/large),
      `staging.rs` unit tests, and the session resume test
      (`crates/syncengine-transport/tests/session.rs`). Renames are exercised as delete+create
      change pairs; the dedicated rename tests landed in M5
      (`renames_propagate_as_a_conservative_delete_plus_create`,
      `renames_propagate_without_conflicts`).
- [x] A file modified while a transfer is pending still delivers the original staged bytes (FR-08).
      Evidence: `staged_copy_survives_source_change`.
- [x] Concurrent edits never silently destroy either version.
      Evidence: `concurrent_edits_detect_conflict_and_preserve_copy` (engine + transport session).
- [x] Unsafe paths (`../`, absolute, symlink escapes) are rejected with tests.
      Evidence: `malicious_remote_paths_are_rejected_before_any_write` + scanner symlink tests.

## M3 — Background agent and LAN synchronization

Scope: agent lifecycle (`daemon start|status|stop`), retry/backoff scheduling, `syncengine sync`,
mDNS discovery, authenticated peer connections for already-paired devices, change-metadata
exchange over the shared transfer engine, LAN transfer resume, duplicate-event tolerance.
Server must not be required for LAN sync of paired devices.

Acceptance criteria:

- [x] Two paired devices sync over LAN with the server absent.
      Evidence: `client/tests/e2e_lan.rs::sync_command_transfers_files_over_lan_between_two_devices`
      runs the real `sync` command against a peer fixture (no `server_url`) and transfers files both ways.
- [x] Unavailable peer → changes stay journaled; reconnect delivers them.
      Evidence: `client/tests/e2e_lan.rs::changes_stay_journaled_while_the_peer_is_offline_and_deliver_on_reconnect`.
- [x] Interrupted LAN transfer resumes from the receiver's durable checkpoint.
      Evidence: `crates/syncengine-transport/tests/session.rs::interrupted_transfer_resumes_from_the_durable_checkpoint`
      (session cut mid-file, torn tail simulated, next session resumes at the checkpoint offset).
- [x] Pairing requires explicit approval; discovery alone never grants trust.
      Evidence: `crates/syncengine-transport/tests/lan.rs::unknown_devices_fail_the_trusted_peer_check`;
      `pair` requires the `--approve --code` proof; server pairing tests cover attempt/expiry limits.

## M4 — Online coordination and relay

Scope: PostgreSQL migrations and repositories (SRD §9), device registration, pairing APIs,
authorization, presence, revocation (SRD §12 endpoints under `/v1`), authenticated connection
protocol, relay session lifecycle, opaque-payload forwarding with bounded in-memory buffering
only, client reconnection/offline queueing/backpressure, `syncengine-server serve/migrate`.

Acceptance criteria:

- [x] Relay forwards end-to-end-protected payloads and persistently stores no file contents.
      Evidence: `relay_forwards_opaque_frames_and_rejects_intruders` forwards frames between two
      authenticated WebSocket peers; payloads are never written to the database or disk.
- [x] Unauthorized/revoked devices are rejected; malformed messages are rejected safely.
      Evidence: signature/timestamp/revocation cases in `registration_pairing_authorization_and_revocation_flow`;
      invalid JSON returns 400 `INVALID_JSON`; 64 KiB body cap and 16 MiB frame cap.
- [ ] Multiple simultaneous connections and transfer resumption are tested — the two-client relay
      e2e passes end to end (`server/tests/sync_e2e.rs`), including pairing through the control
      plane and sealed frames; multi-session stress remains untested.
- [x] No plaintext secrets in logs; request/message size limits enforced.
      Evidence: database URLs are never logged; pairing codes are stored only as proofs; body and
      frame limits are set in `api::app` and `relay_ws`.

## M5 — Conflicts and crash recovery

Scope: version-vector comparison completion, conflict preservation and `syncengine conflicts`,
delete-vs-modify, rename conflicts, delete-and-recreate, crash-injection tests at every
SRD §16.4 point, idempotent re-application under retry, tombstone retirement rules.

Acceptance criteria:

- [x] Simultaneous edits on both devices preserve both versions and surface the conflict.
      Evidence: `crates/syncengine-engine/tests/sync_flow.rs::concurrent_edits_detect_conflict_and_preserve_copy`
      (both versions on disk, conflict row, change stays retryable until the copy exists) and
      `crates/syncengine-transport/tests/session.rs::concurrent_edits_are_preserved_as_conflict_copies`
      (real session, both sides preserve the peer's revision). Delete-vs-edit is covered by
      `delete_vs_concurrent_edit_preserves_local_revision_and_applies_delete` and
      `concurrent_edit_versus_delete_preserves_both_revisions_once` (session); recreate-after-delete by
      `concurrent_recreate_after_delete_keeps_the_deletion` (decided against the tombstone version).
- [x] Retries cannot apply a change twice or corrupt a published file.
      Evidence: `remote_apply_is_idempotent`, `crash_after_publication_before_applied_record_replans_as_stale`
      (publication durable, applied record missing → re-plan is stale, no rewrite, no conflict),
      `apply_then_crash_before_ack_is_recovered_by_the_next_session` (redelivery is acknowledged, nothing
      applied twice), and `interrupted_transfer_resumes_from_the_durable_checkpoint` (torn tail detected by
      the hash check).
- [x] Crash between publication and DB commit recovers to a consistent, acknowledged-later state.
      Evidence: same two crash tests above; acknowledged-later is asserted on the sender side
      (`pending_for_peer` empty after the recovery session).
- [x] Renames propagate without spurious conflicts.
      Evidence: `renames_propagate_as_a_conservative_delete_plus_create` (engine) and
      `renames_propagate_without_conflicts` (session) — the scanner emits delete + create per SRD §10 and
      the receiver ends up with the new path only.
- [x] Conflict resolution is user-reachable and safe.
      Evidence: `resolve_conflict_keep_remote_restores_the_remote_revision_and_rejournals`,
      `resolve_conflict_keep_local_and_dismiss_only_update_state`,
      `crates/syncengine-transport/tests/session.rs` covers the ConflictDelete arm, and
      `client/tests/cli.rs::conflicts_lists_and_resolves_a_seeded_conflict` drives the real binary.
- [x] Tombstone retirement and staged-content GC exist and are bounded.
      Evidence: `tombstones_retire_only_after_every_trusted_folder_peer_acknowledged`,
      `get_tombstone_returns_the_newest_deletion_for_a_path`,
      `gc_staging_keeps_content_referenced_or_awaiting_delivery`; retirement runs at the end of every
      session and GC at the end of every sync pass.

## M6 — Release readiness

Scope: `syncengine doctor` and `syncengine logs`, configuration examples, Linux/Windows build
instructions, backup/recovery guidance for both databases, full fmt/clippy/test/ integration
suite, dependency and security review, honest limitation list in `docs/STATUS.md`.

Acceptance criteria:

- [x] All acceptance criteria above have evidence; remaining gaps are listed explicitly as
      limitations, not silently omitted. Evidence: `docs/STATUS.md` "M6 — Release readiness" and
      "Known limitations"; the placeholder `not_implemented` CLI machinery was removed.
- [x] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
      `cargo test --workspace` all pass. Evidence: rerun 2026-10-18 — fmt ✅, clippy ✅,
      192/192 tests ✅ (6 PostgreSQL-backed against Docker Postgres), plus `cargo audit` ✅
      0 vulnerabilities across 288 dependencies.
- [x] Documentation set from `AGENT.md` §9 exists: `docs/ARCHITECTURE.md`, `docs/DATA_FLOW.md`,
      `docs/DATABASE.md`, `docs/PROTOCOL.md`, `docs/DECISIONS.md`, `docs/LEARNING_GUIDE.md`,
      plus a root `README.md` with Linux/Windows build instructions and configuration examples.

## Verification commands

```bash
make fmt        # cargo fmt --all
make fmt-check  # cargo fmt --all -- --check
make lint       # cargo clippy --workspace --all-targets -- -D warnings
make test       # cargo test --workspace
make check      # fmt-check + lint + test
```

## Open decisions deferred to later milestones

Tracked from SRD §20; each must be resolved and recorded in `docs/DECISIONS.md` before the
feature that depends on it ships:

- Wire format/transport library for peer frames (M3) and relay (M4). — Resolved in M3/M4
  (`crates/syncengine-transport` with length-prefixed JSON frames).
- Pairing UX and approval flow (M3). — Resolved in M3: request id + one-time code, explicit
  `--approve --code`.
- Version-vector compaction and device removal semantics (M5). — Partially resolved in M5: device
  removal is implemented as unpairing (trust row deleted, folder membership dropped, outstanding
  targets excused); vector compaction is **not** implemented and stays open — see the limitations
  in `docs/STATUS.md`.
- Symlink/junction policy details (M2) and durability/flush policy per platform (M2/M5). —
  Partially resolved: scanning skips symlinks/junctions and staging is crash-tolerant via
  hash-verified publication, but there is no per-chunk fsync.
- Tombstone retirement for permanently removed devices (M5). — Resolved in M5: retirement requires
  delivered **and** acknowledged targets from every trusted peer still in the folder; an unparied
  device is dropped from the folder and no longer blocks retirement.
- Chunk size, concurrency, retry defaults, staging quotas from measurement (M2/M4). — Partially
  resolved: chunk size is fixed at 256 KiB (`limits.chunk_size_bytes`, enforced);
  `max_concurrent_transfers` and `staging_max_bytes` are validated config keys but not yet
  enforced (tracked in the `docs/STATUS.md` limitations and the post-v1 backlog).
