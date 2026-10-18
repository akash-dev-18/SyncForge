# SyncEngine

CLI-first, cross-platform file synchronization between your own paired devices.

- **No accounts.** Every device has an Ed25519 + X25519 identity; devices pair explicitly with a
  short code.
- **LAN first.** Direct sealed TCP between devices when they can see each other (mDNS discovery);
  the server relay is a fallback for when they cannot.
- **End-to-end encrypted.** File frames are sealed with per-session ECDH keys; the relay forwards
  opaque bytes and stores no file contents.
- **Conflict-safe.** Nothing is ever silently overwritten; the losing revision is kept as a
  conflict copy and every deletion is acknowledged before its tombstone is retired.

Feature-complete for the v1 scope in `docs/SRD.md`. The honest, current verification status
(what was tested and where) lives in `docs/STATUS.md` — read that before trusting any claim.

## Requirements

| | |
|---|---|
| Rust | 1.85+ (edition 2024), `rustup` recommended; developed with 1.95 |
| Linux | any recent distro; no system libraries beyond a C compiler for the bundled SQLite |
| Windows | Windows 10/11 with the MSVC toolchain (Visual Studio Build Tools, "Desktop development with C++") |
| TLS | rustls-based; **no OpenSSL/vcpkg needed** on either platform |
| Optional | Docker (local PostgreSQL) if you want the coordination server |

The client is developed and tested on Linux. Windows code paths exist (paths, process control,
file permissions) but have not been exercised on real Windows hardware — see "Known limitations".

## Build

```sh
git clone <this repo> && cd SyncForge

# debug build (fast)
cargo build

# release binaries
cargo build --release
```

Binaries:

| Binary | Path |
|---|---|
| Client | `target/release/syncengine` (`syncengine.exe` on Windows) |
| Server | `target/release/syncengine-server` |

### Linux

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # if rustup is not installed
cargo build --release
install -m755 target/release/syncengine ~/.local/bin/syncengine
```

### Windows (PowerShell)

```powershell
# Install rustup (MSVC toolchain) — https://rustup.rs
winget install Rustlang.Rustup   # or: irm https://win.rustup.rs/x86_64 | iex

cargo build --release
# optional: copy target\release\syncengine.exe somewhere on PATH
```

## Quick start (two devices)

A server is needed for **pairing** and for **relay**. LAN sync itself works without one.

### 1. Server (optional, for pairing/relay)

```sh
docker compose up -d --wait postgres
export DATABASE_URL=postgres://syncengine:syncengine@localhost:5432/syncengine
cargo run -p syncengine-server -- serve --bind 0.0.0.0:8443
```

There is no TLS in the server process: expose it behind a TLS-terminating reverse proxy (or keep
it on a trusted LAN). Migrations run automatically on `serve`.

### 2. First-time setup on each device

```sh
syncengine init --name laptop          # writes config + creates data dir
# edit the config to add: server_url = "http://192.168.1.10:8443"
syncengine doctor                      # verify config, identity, server reachability
```

### 3. Share a folder on both devices

Sharing also registers the device with the server, which pairing needs.

```sh
# device A
syncengine share ~/Documents/notes     # prints the folder id

# device B — join the same folder id A printed
syncengine share ~/Documents/notes --join <FOLDER-ID>
```

### 4. Pair the devices

Pairing needs the **target** device to be registered on the server first, so do this after B has
run `share` (or any server-visible command) while the server was reachable.

```sh
# device B: find your own device id
syncengine status                      # "device id : xxxxxxxx-…"

# device A: initiate with B's device id (prints a request id and a short code)
syncengine pair <B-DEVICE-ID>

# device B: approve using the request id and code A printed
syncengine pair --approve <REQUEST-ID> --code <CODE>   # e.g. BKUS-KFZ5
```

### 5. Sync

```sh
syncengine sync                        # one pass now
syncengine daemon start                # or sync continuously in the background
syncengine status                      # devices, folders, queue, conflicts
```

## Everyday commands

```sh
syncengine status                  # what is shared, what is queued, what conflicts exist
syncengine sync                    # run one synchronization pass
syncengine daemon start|status|stop
syncengine logs -n 100             # recent agent log; --path-only for the file path
syncengine conflicts               # list unresolved conflicts
syncengine conflicts resolve <ID> --keep-local | --keep-remote | --dismiss
syncengine unpair <DEVICE-ID>
syncengine unshare <FOLDER-ID>
```

Global flags: `-c/--config <FILE>` (env `SYNCENGINE_CONFIG`), `-v`/`-vv` (debug/trace; `RUST_LOG`
wins), `--json-logs`. Data goes to stdout, logs to stderr; exit codes: 0 ok, 1 failure, 2 CLI
misuse.

To run the daemon at login, add your own autostart entry (systemd user unit on Linux, Task
Scheduler on Windows) that runs `syncengine daemon start`; there is no installer yet.

## Configuration

Config file location: `%APPDATA%\syncengine\config.toml` (Windows) or
`$XDG_CONFIG_HOME/syncengine/config.toml` (Linux, typically `~/.config/syncengine/config.toml`).
Unknown keys are rejected so typos fail loudly.

```toml
device_name = "laptop"                      # shown to paired devices
# data_dir = "/home/you/.local/share/syncengine"   # default: XDG data dir / %APPDATA%
log_level = "info"                          # off | error | warn | info | debug | trace
server_url = "http://192.168.1.10:8443"     # needed for pairing and relay
lan_port = 41641                            # TCP port for direct LAN connections

[agent]
scan_interval_secs = 30                     # reconciliation scan frequency
debounce_ms = 500                           # reserved: filesystem watcher is not implemented yet

[limits]
chunk_size_bytes = 262144                   # 256 KiB transfer chunks (4 KiB – 16 MiB)
max_concurrent_transfers = 4                # reserved: transfers are currently sequential
staging_max_bytes = 1073741824              # reserved: staging GC uses refcounts, not a quota
```

Default data directory contents: `identity.json` (device keys — back this up), `syncengine.db`
(state; see `docs/DATABASE.md` for backup/recovery), `staging/` (content cache), `daemon.json` +
`daemon.log` (agent state).

## Troubleshooting

| Symptom | Likely cause / fix |
|---|---|
| Devices never find each other on the LAN | mDNS blocked (VPN, Docker bridge, some Wi-Fi APs) or firewall. Allow UDP 5353 (mDNS) and TCP `lan_port` through the firewall, or set `server_url` and rely on the relay. |
| `sync` says unauthorized/timeout against the server | Clock skew beyond ±5 minutes breaks control-plane signatures; check `timedatectl` / Windows time sync. |
| `CONTENT_UNAVAILABLE` in logs | The staging cache lost an object that a pending change still references. Touch/rewrite the affected file to re-stage it, or restore `staging/` from backup. |
| Two devices on one machine cannot sync | They share one IP; give each instance its own `data_dir` **and** `lan_port`. |
| Unexpected `(conflict from …)` files | Both sides changed the same file (or an edit raced a delete). That is the design: no data was lost. Review with `syncengine conflicts`. |
| Daemon says running but nothing happens | `syncengine logs -n 100` first; `daemon status` verifies pid liveness + heartbeat. A stale `daemon.json` is ignored automatically. |
| Server won't start | Is Postgres up (`docker compose ps`) and is `DATABASE_URL` set? `syncengine-server migrate` runs migrations without serving. |

Run `syncengine doctor` first for almost anything — it checks config, identity, store, queue,
conflicts, and (unless `--skip-server`) server reachability.

## Known limitations (v1)

- No in-process TLS on the server (use a reverse proxy); sealed frames have **no forward secrecy**
  (static-static ECDH, no ratchet).
- The change journal and `applied_remote_changes` grow without compaction; staging is refcount-GC'd
  but has no size quota (`staging_max_bytes` reserved).
- Transfers are sequential per peer (`max_concurrent_transfers` reserved); no bandwidth limits.
- No filesystem watcher yet (`debounce_ms` reserved): changes sync on the next scan tick.
- Windows support is implemented but unverified on real Windows hardware.
- The daemon is not integrated with service managers; add your own autostart entry.
- Losing `identity.json` or unpairing requires re-pairing; the server database is metadata-only but
  losing it requires re-registering devices.

## Documentation

| Doc | Contents |
|---|---|
| `docs/SRD.md` | Requirements (the contract this repo implements) |
| `docs/STATUS.md` | What is verified, what is limited, milestone history |
| `docs/ARCHITECTURE.md` | Components, crate layout, invariants |
| `docs/DATA_FLOW.md` | One change end to end, conflict flows, crash points |
| `docs/DATABASE.md` | SQLite + PostgreSQL schemas, backup/recovery, inspection queries |
| `docs/PROTOCOL.md` | Wire messages, handshake, sealing, control API, limits |
| `docs/DECISIONS.md` | Why things are the way they are (ADR style) |
| `docs/LEARNING_GUIDE.md` | Reading order, Rust concepts, exercises |
| `AGENT.md`, `docs/IMPLEMENTATION_PLAN.md` | Engineering brief and milestone plan |

## Development

```sh
make check   # fmt --check + clippy -D warnings
make test    # cargo test --workspace

# full suite including Postgres-backed tests:
docker compose up -d --wait postgres
SYNCENGINE_TEST_DATABASE_URL=postgres://syncengine:syncengine@localhost:5432/syncengine cargo test --workspace
```

## Deploy the server (Docker)

```sh
docker compose up -d --build postgres server
curl http://127.0.0.1:8443/healthz   # {"status":"ok"}
```

The image builds `syncengine-server` (multi-stage, ~30 MB runtime) and runs
`serve --bind 0.0.0.0:8443` against the bundled `postgres` service. For public use, put a
TLS-terminating reverse proxy (Caddy, nginx) in front and set `server_url = "https://your.domain"`
in each client config. The server stores metadata only; file contents stay end-to-end encrypted.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

Crate layout for publishing (planned/possible on crates.io):

| Crate | Install | Binary |
|---|---|---|
| `syncengine` | `cargo install syncengine` | `syncengine` (client CLI + daemon) |
| `syncengine-server` | `cargo install syncengine-server` | `syncengine-server` (coordination API + relay) |

Supporting libraries published alongside: `syncengine-protocol`, `syncengine-storage`,
`syncengine-engine`, `syncengine-transport`.
