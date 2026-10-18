# How To Use — Commands Only

## One-time setup (on EACH device)

```sh
syncengine init --name laptop
syncengine doctor
```

Edit config to add the server URL:

```toml
server_url = "http://192.168.1.10:8443"
```

## Start the server (once, on one machine) — needed for pairing

```sh
docker compose up -d --wait postgres
export DATABASE_URL=postgres://syncengine:syncengine@localhost:5432/syncengine
cargo run -p syncengine-server -- serve --bind 0.0.0.0:8443
```

## Connect the two devices

```sh
# Device A: share a folder (prints folder id)
syncengine share ~/Documents/notes

# Device B: join the same folder
syncengine share ~/Documents/notes --join <FOLDER-ID>

# Device B: find its own device id
syncengine status

# Device A: start pairing (prints request id + code)
syncengine pair <B-DEVICE-ID>

# Device B: approve
syncengine pair --approve <REQUEST-ID> --code <CODE>
```

## Use it

```sh
syncengine sync            # one sync pass now
syncengine daemon start    # sync in background every 30s
syncengine daemon status
syncengine daemon stop
syncengine status          # folders, peers, queue, conflicts
syncengine logs -n 100     # agent log
```

## Conflicts

```sh
syncengine conflicts
syncengine conflicts resolve <CONFLICT-ID> --keep-local
syncengine conflicts resolve <CONFLICT-ID> --keep-remote
syncengine conflicts resolve <CONFLICT-ID> --dismiss
```

## Wi-Fi only (no internet)

```sh
# Pair once with a temporary server (above), then stop the server:
# Ctrl+C the server, then daily use needs no server at all:
syncengine daemon start    # on both devices, same Wi-Fi — direct sync
syncengine sync            # output should say "via lan"
```

## Remove

```sh
syncengine unpair <DEVICE-ID>
syncengine unshare <FOLDER-ID>
```

## If something is wrong

```sh
syncengine doctor
syncengine doctor --skip-server
syncengine sync
```

Ports to allow: TCP 41641 (LAN sync), UDP 5353 (discovery), TCP 8443 (server).
