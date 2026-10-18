# SyncEngine User Manual

A plain-language guide to setting up, connecting, and using SyncEngine.

**Contents**

1. [What SyncEngine is](#1-what-syncengine-is)
2. [Install](#2-install)
3. [Set up a device](#3-set-up-a-device)
4. [Connect two devices (pairing)](#4-connect-two-devices-pairing)
5. [Share folders](#5-share-folders)
6. [Sync: manual and background](#6-sync-manual-and-background)
7. [Using it Wi-Fi-only (no internet)](#7-using-it-wi-fi-only-no-internet)
8. [Conflicts: nothing is ever lost](#8-conflicts-nothing-is-ever-lost)
9. [Managing devices and folders](#9-managing-devices-and-folders)
10. [Files, data locations, backups](#10-files-data-locations-backups)
11. [Troubleshooting](#11-troubleshooting)
12. [Command cheat sheet](#12-command-cheat-sheet)
13. [Where to learn more](#13-where-to-learn-more)

---

## 1. What SyncEngine is

SyncEngine keeps folders synchronized between **your own devices** (laptop ↔ desktop, laptop ↔
laptop, …). There are no accounts and no cloud storage:

- Each device has its own identity (a key file on disk).
- You **pair** two devices once, with a short code — after that they trust each other.
- Every **shared folder** syncs with every paired device: directly over your **LAN/Wi-Fi** when
  the devices can reach each other, or through a small **coordination server's relay** when they
  cannot.
- File contents are **end-to-end encrypted** between devices; the server only ever sees
  encrypted frames.
- If both sides change the same file, **both versions are kept** and the conflict is reported —
  nothing is silently overwritten.

Words you will see:

| Term | Meaning |
|---|---|
| **Device ID** | UUID that identifies one of your machines. Shown by `syncengine status`. |
| **Pairing** | One-time approval step that makes two devices trust each other. Needs the server. |
| **Folder ID** | UUID for one shared folder; both devices use the same ID to sync together. |
| **Sync pass** | One round of "scan my folders, exchange changes with every paired device". |
| **Daemon** | Background process that runs a sync pass every 30 s (configurable). |
| **Conflict copy** | A sibling file holding the losing revision of a simultaneous edit. |
| **Server** | Small coordination service: pairing + optional relay. Not needed for LAN sync. |

---

## 2. Install

Build once per machine (see `README.md` for platform details):

```sh
git clone <this repo> && cd SyncForge
cargo build --release
# binary: target/release/syncengine        (Windows: target\release\syncengine.exe)
```

Put the binary somewhere on your `PATH` (e.g. `~/.local/bin` on Linux) or call it by full path.
The server binary `syncengine-server` is only needed on the machine that runs the coordination
server (section 4).

---

## 3. Set up a device

Do this on **each** device:

```sh
syncengine init --name laptop
```

This writes a config file:

- Linux: `~/.config/syncengine/config.toml`
- Windows: `%APPDATA%\syncengine\config.toml`

Open it and set the server address (only needed for pairing and relay — see section 7 if you
want Wi-Fi-only):

```toml
device_name = "laptop"
server_url  = "http://192.168.1.10:8443"   # the machine running the server
# lan_port = 41641                          # default; change only if it clashes
```

Then check everything:

```sh
syncengine doctor
```

`doctor` verifies your config, identity, database, folders, devices, queue, conflicts and server
reachability (use `--skip-server` when the server is off). Anything marked `FAIL` exits non-zero.

---

## 4. Connect two devices (pairing)

Pairing is a one-time, two-sided approval:

- Device A creates a **pairing request** and shows a **request id** and a **short code**.
- Device B approves with both — proving to the server (and to A) that you control B.
- Both devices store each other's public keys locally. **The server never sees the code** (only a
  hash) and never gets file access.

Pairing requires the coordination server, because the request/approval exchange goes through it.
See section 7 for how to run a server just for pairing in a Wi-Fi-only setup.

### 4.1 Start the server (once, on one machine)

On any always-on or temporary machine (it can be one of your two devices):

```sh
docker compose up -d --wait postgres
export DATABASE_URL=postgres://syncengine:syncengine@localhost:5432/syncengine
cargo run -p syncengine-server -- serve --bind 0.0.0.0:8443
```

The other device must reach it at `http://<that machine's LAN IP>:8443` — find the IP with
`hostname -I` (Linux) or `ipconfig` (Windows), and make sure the firewall allows TCP 8443.

### 4.2 Pair

```sh
# device B first: register it with the server by sharing a folder (section 5), then:
syncengine status          # copy B's "device id"

# device A: start pairing using B's device id
syncengine pair <B-DEVICE-ID>
# → prints: request id + code, and "Waiting for approval (up to 10 minutes)…"

# device B: approve (within 10 minutes; 5 attempts allowed)
syncengine pair --approve <REQUEST-ID> --code <CODE>   # code looks like BKUS-KFZ5
```

Both commands finish with `Paired with <name>`. If you see `DEVICE_NOT_FOUND`, the target device
has not registered with the server yet — run `syncengine share <folder>` on it while the server is
reachable, then try again.

> Tip: device B can already be registered automatically if you do section 5 (share) **before**
> pairing.

---

## 5. Share folders

Sharing registers the folder locally and (when the server is reachable) announces it for relay
use.

```sh
# device A
syncengine share ~/Documents/notes
# → Shared notes
#     folder id : 8d3a…

# device B — join the SAME folder id so both devices sync one logical folder:
syncengine share ~/Documents/notes --join 8d3a…
```

Notes:

- The path may differ on each device (`~/notes` on Linux, `C:\Users\you\notes` on Windows) — the
  **folder id** is what links them.
- `share` refuses to share a folder that contains SyncEngine's own data directory.
- One device can share many folders; every shared folder syncs with every paired device.
- If `share` prints `Warning: could not register the folder with the server`, the local share
  still happened; the server part is retried on every `syncengine sync` while it is reachable.

## 6. Sync: manual and background

```sh
syncengine sync          # one pass now
syncengine daemon start  # keep syncing every 30 s in the background
syncengine daemon status # is it running?
syncengine daemon stop   # stop it
syncengine logs -n 100   # what has the agent been doing?
```

What a `sync` pass prints:

```
notes (8d3a…)
  created 1  updated 0  deleted 0  unchanged 42
Journaled 1 change(s): 1 created, 0 updated, 0 deleted (42 unchanged).

LAN listener on port 41641 for this pass.
Sync with 1 paired device(s):
  b7c1… via lan: delivered 1 applied 0 conflicts 0
```

- `via lan` = direct connection (fast, private). `via relay` = through the server.
- `not connected (…)` means the peer was unreachable; nothing is lost — its changes stay queued
  and go out on the next pass.
- Unreachable peers do **not** make the command fail (exit code stays 0).

The daemon repeats the same pass every `agent.scan_interval_secs` (default 30 s). To start it at
login, add your own autostart entry (systemd user unit, Task Scheduler, …) that runs
`syncengine daemon start`.

---

## 7. Using it Wi-Fi-only (no internet)

**What works without any internet/server access:** everything except *pairing* and *relay*.

- Devices that are already paired sync **directly over your LAN/Wi-Fi** (mDNS discovery + TCP).
  The server can be switched off completely.
- Sharing, syncing, the daemon, conflicts, logs — all work offline.
- **Pairing is the one-time exception**: the request/approval handshake goes through the server,
  so you need a reachable server once per new device pair. After that, it is never needed again
  for LAN sync.

### Scenario A — permanent home server (recommended if you have an always-on machine)

Run the server on a desktop, NAS, or Raspberry Pi on your home network (section 4.1, binding
`0.0.0.0:8443`). Point both devices' `server_url` at it. Benefits: pairing works any time, relay
works for devices outside the house, and you never think about it again.

### Scenario B — temporary server just for pairing (truly Wi-Fi-only after)

1. On one of the two devices (or any laptop on the same Wi-Fi), start the server for a few
   minutes (section 4.1).
2. Set `server_url` on both devices to that machine's LAN IP, e.g.
   `http://192.168.1.23:8443`.
3. Share and pair (sections 4–5).
4. Stop the server (`Ctrl+C`, `docker compose down`).

From now on the two devices sync over Wi-Fi on their own. Two options for the config:

- **Keep `server_url`** (recommended): when the server is off you will see a warning like
  `device registration with the server failed …` on each pass, but LAN sync still works. Keeping
  it means relay works again whenever the server is reachable.
- **Remove `server_url`** to silence the warnings: strictly LAN-only, warnings gone. You will
  need to re-add it (and start a server) to pair another device.

### Scenario C — no server even once

Not possible in this release: two devices cannot establish trust without the pairing exchange,
and pairing needs a reachable server. Use Scenario B once (it only takes a few minutes) — the
server is then gone for good, and daily syncing is pure Wi-Fi.

### Wi-Fi/LAN checklist

| Requirement | Why / fix |
|---|---|
| Same network/subnet | LAN sync uses mDNS + TCP on the local network. |
| AP/client isolation OFF | Guest Wi-Fi and some routers block device-to-device traffic. |
| UDP 5353 (mDNS) allowed | Used to discover peers. Allow it through host firewalls. |
| TCP port `lan_port` (41641) allowed | The direct connection port; allow it on both devices. |
| VPNs | A VPN can hide your LAN interface from mDNS or capture peer traffic. Disable it while testing, or expect the relay path instead. |
| Two devices on ONE computer | Give each instance its own `data_dir` **and** a different `lan_port` in its config. |

Verify with `syncengine sync` — a working LAN pass prints `via lan`. `syncengine doctor` checks
the local setup (use `--skip-server`).

---

## 8. Conflicts: nothing is ever lost

When the same file is edited on two devices before they sync (or an edit races a delete), both
revisions are kept:

```
notes/
  shopping.txt
  shopping.txt (conflict from f81c…)     ← the losing revision, as a normal file
```

- The conflict copy is labeled with the device that produced the other revision and syncs to all
  devices like any other file.
- A second conflict for the same file gets `-2`, `-3`, … suffixes.
- Review them with:

```sh
syncengine conflicts          # list conflicts (xxxx ids shown)
syncengine conflicts resolve <CONFLICT-ID> --keep-remote   # put the other revision at the main path
syncengine conflicts resolve <CONFLICT-ID> --keep-local    # keep the current file; copy stays on disk
syncengine conflicts resolve <CONFLICT-ID> --dismiss       # just mark it reviewed
```

Resolution is optional housekeeping: the data is always safe either way. When in doubt, open both
files, merge by hand, then `--dismiss` and delete the copy you do not want (deleting it propagates
normally).

---

## 9. Managing devices and folders

```sh
syncengine devices            # your device id + all paired devices
syncengine status             # everything at a glance: folders, queues, conflicts

syncengine unpair <DEVICE-ID> # remove trust (local; server revocation is best-effort)
syncengine unshare <FOLDER-ID># stop sharing/syncing a folder
```

After `unpair`, the two devices stop syncing entirely until you pair again. After `unshare`, the
folder stays on disk untouched but stops taking part in sync.

---

## 10. Files, data locations, backups

Default data directory:

- Linux: `~/.local/share/syncengine/`
- Windows: `%APPDATA%\syncengine\`

| File | What it is | Back it up? |
|---|---|---|
| `identity.json` | **Device keys.** Losing it = re-pair everything. | **Yes** |
| `syncengine.db` | Local state: folders, journal, conflicts. | Yes |
| `staging/` | Temporary content cache. | No (recoverable) |
| `daemon.json`, `daemon.log` | Agent state and log. | No |

Quick backup (while the daemon is stopped):

```sh
syncengine daemon stop
sqlite3 ~/.local/share/syncengine/syncengine.db ".backup ~/syncengine-backup.db"
cp ~/.local/share/syncengine/identity.json ~/
syncengine daemon start
```

Full backup/recovery procedures (including the server's PostgreSQL) are in `docs/DATABASE.md`.

---

## 11. Troubleshooting

Start with `syncengine doctor` (and `syncengine logs -n 100` when the daemon is involved).

| Symptom | Likely cause / fix |
|---|---|
| Devices never connect on Wi-Fi | mDNS blocked (VPN, guest Wi-Fi, AP isolation) or firewall. See the checklist in section 7; check `syncengine sync` output for `not connected (…)`. |
| `pair` fails with `DEVICE_NOT_FOUND` | The other device is not registered with the server yet: run `syncengine share <path>` on it while the server is reachable, then retry. |
| `pair` immediately errors about the server | `server_url` is missing/unreachable. Set it in the config (section 3) or start the server (section 4.1). |
| Pairing code expired | Requests last 10 minutes; rerun `syncengine pair <ID>` on the initiator for a fresh code. |
| `unauthorized` / `INVALID_TIMESTAMP` from the server | Your clock is off by more than 5 minutes. Sync time on both devices. |
| `Warning: could not register the folder with the server` | Server unreachable during `share` — harmless for LAN; rerun `syncengine sync` when it is back. |
| `Warning: device registration with the server failed` every pass | You kept `server_url` but the server is off. Expected in Wi-Fi-only mode — sync still works over LAN, or remove `server_url` (section 7). |
| `CONTENT_UNAVAILABLE` in logs | The staging cache lost content a pending change still references. Touch/rewrite the affected file to re-stage it. |
| Conflict files keep appearing | Both devices edit the same file between syncs. Sync more often (`daemon start` on both), or work on different files. |
| Nothing syncs after moving a folder | The share points at the old path. `unshare <FOLDER-ID>` and `share` again (use `--join` to reattach the same folder id). |
| Two copies of SyncEngine on one PC don't connect | Give each instance its own `data_dir` and `lan_port` (section 7 checklist). |

---

## 12. Command cheat sheet

| Command | What it does |
|---|---|
| `syncengine init --name <name> [--force]` | Create config + data dir. |
| `syncengine doctor [--skip-server]` | Diagnose config, identity, DB, queues, server. |
| `syncengine status` | Device id, folders, peers, journal, conflicts. |
| `syncengine devices` | Your keys/ID and all paired devices. |
| `syncengine share <path> [--join <folder-id>]` | Share (or join) a folder. |
| `syncengine unshare <folder-id>` | Stop sharing a folder. |
| `syncengine pair <device-id>` | Start pairing; prints request id + code. |
| `syncengine pair --approve <request-id> --code <code>` | Approve a pairing request. |
| `syncengine unpair <device-id>` | Remove trust for a device. |
| `syncengine sync` | Run one sync pass now. |
| `syncengine daemon start\|status\|stop` | Background agent control. |
| `syncengine logs [-n N] [--path-only]` | Tail the agent log (default 50 lines). |
| `syncengine conflicts` | List conflicts. |
| `syncengine conflicts resolve <id> --keep-local\|--keep-remote\|--dismiss` | Settle one conflict. |

Global flags: `-c/--config <file>` (or `SYNCENGINE_CONFIG`), `-v`/`-vv`, `--json-logs`.
Exit codes: `0` success, `1` failure, `2` command-line misuse.

---

## 13. Where to learn more

| Document | Contents |
|---|---|
| `README.md` | Build instructions, quickstart, config reference. |
| `docs/STATUS.md` | What is verified, known limitations. |
| `docs/DATABASE.md` | Inspecting, backing up, and recovering both databases. |
| `docs/PROTOCOL.md` | What actually goes over the wire (for the curious). |
| `docs/LEARNING_GUIDE.md` | Reading path for learning from the codebase. |
