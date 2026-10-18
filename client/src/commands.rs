//! Implementations of the CLI commands.
//!
//! Every command in [`crate::cli::Command`] is implemented here: `init`,
//! `status`, `share` (with `--join`), `unshare`, `devices`, `pair`, `unpair`,
//! `sync`, `conflicts` (list and resolve), `daemon` (`start`/`status`/`stop`
//! plus the internal `run`), `logs`, and `doctor`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use syncengine_protocol::{DeviceId, FolderId, now_rfc3339};
use syncengine_storage::{Peer, SharedFolder, Store, TrustState};

use crate::cli::{
    ConflictsAction, ConflictsArgs, DaemonAction, DoctorArgs, InitArgs, LogsArgs, PairArgs,
};
use crate::config::{self, Config, FALLBACK_DEVICE_NAME};
use crate::daemon;
use crate::delivery::{PassOptions, ensure_folder_peer_links, run_pass};
use crate::remote::RemoteClient;
use crate::state;

/// Initializes structured logging on stderr.
///
/// Precedence: `RUST_LOG` (handled by the env filter) wins; otherwise `-v`
/// maps to `debug` and `-vv` (or more) to `trace`. Command output stays on
/// stdout so it can be piped.
pub fn init_logging(verbose: u8, json_logs: bool) {
    let fallback = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(fallback));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    if json_logs {
        builder.json().init();
    } else {
        builder.init();
    }
}

fn load_config(config_arg: Option<&Path>) -> Result<Config> {
    let path = config::resolve_path(config_arg);
    config::load(&path)
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")
}

fn require_server(config: &Config) -> Result<&str> {
    match config.server_url.as_deref() {
        Some(url) => Ok(url),
        None => bail!(
            "no server is configured; set `server_url` (for example \
             \"http://127.0.0.1:8443\") in {} and retry",
            config::default_config_path().display()
        ),
    }
}

/// `syncengine init`: writes a default configuration file.
///
/// Idempotent: an existing file is kept unless `--force` is given, so rerunning
/// init can never silently discard user edits. Device identity and the local
/// database are created lazily by the first command that needs them.
pub fn init(args: &InitArgs, config_arg: Option<&Path>) -> Result<()> {
    let path = config::resolve_path(config_arg);
    if path.exists() && !args.force {
        println!("Configuration already exists at {}", path.display());
        println!("Edit it directly, or rerun with --force to replace it with defaults.");
        return Ok(());
    }

    let device_name = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(default_device_name)
        .unwrap_or_else(|| FALLBACK_DEVICE_NAME.to_string());
    let config = Config {
        device_name,
        ..Config::default()
    };

    config::save(&path, &config)?;
    tracing::info!(path = %path.display(), "wrote default configuration");

    println!("Wrote default configuration to {}", path.display());
    println!("Device name   : {}", config.device_name);
    println!("Data directory: {}", config.data_dir.display());
    println!("LAN port      : {}", config.lan_port);
    println!();
    println!("Next: run `syncengine share <path>` to share a folder, or");
    println!("`syncengine status` to review the effective configuration.");
    println!("To sync through a coordination server, set `server_url` in the config");
    println!("(for example \"http://127.0.0.1:8443\") and run `syncengine pair`.");
    Ok(())
}

/// `syncengine status`: reports configuration and local state.
///
/// Read-only: it never creates the data directory, identity, or database, so
/// it is safe to run before `init` anywhere.
pub fn status(config_arg: Option<&Path>) -> Result<()> {
    let path = config::resolve_path(config_arg);
    let from_file = path.exists();
    let config = config::load(&path)?;
    let source = if from_file {
        "file"
    } else {
        "missing — using built-in defaults"
    };

    let database = config.database_path();
    let initialized = database.exists();
    let store_state = if initialized {
        "present"
    } else {
        "not initialized"
    };
    let device_name = if config.device_name.trim().is_empty() {
        "(not set)"
    } else {
        config.device_name.trim()
    };

    println!("SyncEngine status");
    println!("  config file      : {} ({source})", path.display());
    println!("  device name      : {device_name}");
    println!("  data directory   : {}", config.data_dir.display());
    println!(
        "  server           : {}",
        config.server_url.as_deref().unwrap_or("(none)")
    );
    println!("  LAN port         : {}", config.lan_port);
    println!(
        "  local store      : {} ({store_state})",
        database.display()
    );
    println!(
        "  protocol version : v{}",
        syncengine_protocol::PROTOCOL_VERSION
    );
    println!(
        "  agent scan       : every {}s",
        config.agent.scan_interval_secs
    );

    if initialized {
        print_initialized_state(&config)?;
    }

    println!();
    println!("  tip              : run `syncengine doctor` to check this setup");
    Ok(())
}

fn print_initialized_state(config: &Config) -> Result<()> {
    let store = syncengine_storage::SqliteStore::open(&config.database_path())
        .context("failed to open the local database")?;

    println!();
    match store.local_device()? {
        Some(device) => {
            println!("  device id        : {}", device.device_id);
            println!("  device name      : {}", device.device_name);
            println!("  registered       : {}", device.created_at);
        }
        None => println!("  device id        : (not registered)"),
    }

    let folders = store.list_folders()?;
    println!("  shared folders   : {}", folders.len());
    for folder in &folders {
        let enabled = if folder.enabled {
            "enabled"
        } else {
            "disabled"
        };
        println!(
            "    - {} {} -> {} ({enabled})",
            folder.folder_id,
            folder.name,
            folder.local_root.display()
        );
    }

    let peers = store.list_peers()?;
    println!("  paired devices   : {}", peers.len());
    for peer in &peers {
        println!(
            "    - {} {} [{}]",
            peer.device_id,
            peer.device_name,
            peer.trust.as_str()
        );
    }

    println!("  journal entries  : {}", store.max_change_seq()?);
    let unresolved = store
        .list_conflicts(Some(syncengine_storage::ConflictStatus::Unresolved))?
        .len();
    println!("  open conflicts   : {unresolved}");
    Ok(())
}

/// `syncengine share <path> [--join <folder-id>]`: registers a local folder
/// for sharing. With `--join`, adopts the folder id announced by another
/// device so both sides sync the same logical folder.
pub fn share(path: &Path, join: Option<&str>, config_arg: Option<&Path>) -> Result<()> {
    let config = load_config(config_arg)?;
    let mut state = state::open(&config)?;

    let root = std::fs::canonicalize(path)
        .with_context(|| format!("cannot share {} — path does not exist", path.display()))?;
    if !root.is_dir() {
        bail!("cannot share {} — not a directory", root.display());
    }
    if state::root_contains_data_dir(&root, &config.data_dir).unwrap_or(false) {
        bail!(
            "refusing to share {} — it contains SyncEngine's data directory ({})",
            root.display(),
            config.data_dir.display()
        );
    }

    let joined: Option<FolderId> =
        match join {
            Some(text) => Some(text.parse().map_err(|_| {
                anyhow::anyhow!("`{text}` is not a valid folder id (expected a UUID)")
            })?),
            None => None,
        };

    let root_canonical = root.clone();
    for existing in state.store.list_folders()? {
        let existing_root = std::fs::canonicalize(&existing.local_root)
            .unwrap_or_else(|_| existing.local_root.clone());
        if let Some(joined_id) = joined {
            if existing.folder_id == joined_id {
                if existing_root == root_canonical {
                    println!(
                        "Already sharing {} as {} ({})",
                        root.display(),
                        existing.name,
                        existing.folder_id
                    );
                    return Ok(());
                }
                bail!(
                    "folder {joined_id} is already shared from {} on this device; unshare it first to point it at {}",
                    existing_root.display(),
                    root.display()
                );
            }
        } else if existing_root == root_canonical {
            println!(
                "Already shared as {} ({})",
                existing.folder_id, existing.name
            );
            return Ok(());
        }
    }

    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| root.display().to_string());
    let folder_id = match joined {
        Some(id) => id,
        None => FolderId::new(),
    };
    state.store.upsert_folder(&SharedFolder {
        folder_id,
        name: name.clone(),
        local_root: root.clone(),
        enabled: true,
        created_at: now_rfc3339(),
    })?;

    if joined.is_some() {
        println!("Joined {name}");
    } else {
        println!("Shared {name}");
    }
    println!("  folder id : {folder_id}");
    println!("  root      : {}", root.display());

    if let Some(server) = config.server_url.as_deref() {
        let result = runtime()?.block_on(announce_folder(
            server,
            &state.identity,
            state.device_id,
            &state::effective_device_name(&config),
            folder_id,
        ));
        match result {
            Ok(()) => {
                println!();
                println!("Registered the folder with the server for remote access.");
            }
            Err(error) => {
                println!();
                println!("Warning: could not register the folder with the server: {error:#}");
                println!("Run `syncengine sync` once the server is reachable to retry.");
            }
        }
    } else {
        println!();
        println!("No server_url configured: this folder will sync over the LAN with");
        println!("paired devices only, once both devices run `syncengine sync` or the daemon.");
    }
    Ok(())
}

/// Registers the device and the folder membership with the server, so relay
/// sessions are authorized. Callers decide whether failures are fatal;
/// `sync` retries the same registration on every pass anyway.
pub async fn announce_folder(
    server_url: &str,
    identity: &syncengine_engine::Identity,
    device_id: DeviceId,
    device_name: &str,
    folder_id: FolderId,
) -> Result<()> {
    let remote = RemoteClient::new(server_url, identity, device_id)?;
    remote
        .register(
            device_name,
            std::env::consts::OS,
            &identity.ed25519_public_hex(),
            &identity.x25519_public_hex(),
        )
        .await?;
    remote.upsert_membership(folder_id, "read-write").await
}

/// `syncengine unshare <folder-id>`: stops sharing a folder.
pub fn unshare(folder_id: &str, config_arg: Option<&Path>) -> Result<()> {
    let folder_id: FolderId = folder_id
        .parse()
        .map_err(|_| anyhow::anyhow!("`{folder_id}` is not a valid folder id (expected a UUID)"))?;
    let config = load_config(config_arg)?;
    let mut state = state::open(&config)?;

    let folder = state
        .store
        .get_folder(&folder_id)?
        .ok_or_else(|| anyhow::anyhow!("no shared folder with id {folder_id}"))?;
    state.store.delete_folder(&folder_id)?;

    println!("Stopped sharing {} ({})", folder.name, folder.folder_id);
    println!("Local files were left untouched.");
    Ok(())
}

/// `syncengine devices`: lists this device and known peers.
pub fn devices(config_arg: Option<&Path>) -> Result<()> {
    let config = load_config(config_arg)?;
    let state = state::open(&config)?;

    println!("This device");
    match state.store.local_device()? {
        Some(device) => println!("  {}  {}", device.device_id, device.device_name),
        None => println!("  (not registered)"),
    }
    println!("  public key  : {}", state.identity.ed25519_public_hex());
    println!("  encrypt key : {}", state.identity.x25519_public_hex());
    println!(
        "  server      : {}",
        config.server_url.as_deref().unwrap_or("(none)")
    );

    let peers = state.store.list_peers()?;
    println!();
    if peers.is_empty() {
        println!("No paired devices yet.");
        println!("Run `syncengine pair <device-id>` to start pairing.");
        return Ok(());
    }
    println!("Paired devices ({})", peers.len());
    for peer in peers {
        let seen = peer.last_seen_at.as_deref().unwrap_or("never");
        println!(
            "  {}  {}  [{}]  last seen: {seen}",
            peer.device_id,
            peer.device_name,
            peer.trust.as_str()
        );
    }
    Ok(())
}

/// `syncengine pair <device-id>` / `pair --approve <request-id> --code <code>`.
pub fn pair(args: &PairArgs, config_arg: Option<&Path>) -> Result<()> {
    let config = load_config(config_arg)?;
    let server = require_server(&config)?.to_string();
    let mut state = state::open(&config)?;
    let device_name = state::effective_device_name(&config);
    let remote = RemoteClient::new(&server, &state.identity, state.device_id)?;
    let rt = runtime()?;

    let peer = rt.block_on(async {
        remote
            .register(
                &device_name,
                std::env::consts::OS,
                &state.identity.ed25519_public_hex(),
                &state.identity.x25519_public_hex(),
            )
            .await
            .context("registration with the server failed")?;

        match (&args.approve, &args.device_id) {
            (Some(request_id), _) => {
                let code = args
                    .code
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("`pair --approve` requires `--code <CODE>`"))?;
                approve_pairing(&remote, request_id, code, state.device_id).await
            }
            (None, Some(target)) => {
                let target: DeviceId = target.parse().map_err(|_| {
                    anyhow::anyhow!("`{target}` is not a valid device id (expected a UUID)")
                })?;
                if target == state.device_id {
                    bail!("cannot pair a device with itself");
                }
                initiate_pairing(&remote, target).await
            }
            (None, None) => bail!(
                "usage: `syncengine pair <DEVICE_ID>` to start pairing, or \
                 `syncengine pair --approve <REQUEST_ID> --code <CODE>` to approve"
            ),
        }
    })?;

    if peer.device_id == state.device_id {
        bail!("the server returned this device's own id as the pairing partner");
    }
    state.store.upsert_peer(&Peer {
        device_id: peer.device_id,
        device_name: peer.device_name.clone(),
        public_key: peer.signing_key.clone(),
        encryption_key: peer.encryption_key.clone(),
        platform: peer.platform.clone(),
        trust: TrustState::Trusted,
        last_seen_at: None,
        created_at: now_rfc3339(),
    })?;

    println!("Paired with {}", peer.device_name);
    println!("  device id : {}", peer.device_id);
    println!("  platform  : {}", peer.platform);
    println!();
    println!("Next: `syncengine share <path>` on one device and");
    println!("`syncengine share <path> --join <folder-id>` on the other, then run");
    println!("`syncengine sync` (or `syncengine daemon start`).");
    Ok(())
}

async fn initiate_pairing(
    remote: &RemoteClient<'_>,
    target: DeviceId,
) -> Result<crate::remote::DeviceKeys> {
    let created = remote
        .create_pairing_request(target)
        .await
        .context("creating the pairing request failed")?;
    println!("Pairing request created for device {target}");
    println!("  request id : {}", created.request_id);
    println!("  code       : {}", created.code);
    if !created.expires_at.is_empty() {
        println!("  expires at : {}", created.expires_at);
    }
    println!();
    println!("On the other device run:");
    println!(
        "  syncengine pair --approve {} --code {}",
        created.request_id, created.code
    );
    println!();
    println!("Waiting for approval (up to 10 minutes)...");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10 * 60);
    loop {
        let status = remote
            .pairing_status(&created.request_id)
            .await
            .context("checking the pairing request failed")?;
        match status.status.as_str() {
            "approved" => break,
            "expired" => bail!("the pairing request expired before it was approved"),
            "pending" => {}
            other => bail!("unexpected pairing status `{other}`"),
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("the pairing request was not approved within 10 minutes");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    remote
        .device_keys(target)
        .await
        .context("fetching the partner's keys failed")
}

async fn approve_pairing(
    remote: &RemoteClient<'_>,
    request_id: &str,
    code: &str,
    self_id: DeviceId,
) -> Result<crate::remote::DeviceKeys> {
    remote
        .approve_pairing_request(request_id, code)
        .await
        .context("approving the pairing request failed")?;
    let status = remote
        .pairing_status(request_id)
        .await
        .context("checking the pairing request failed")?;
    let partner = if status.initiator_device_id == self_id {
        status.target_device_id
    } else {
        status.initiator_device_id
    };
    remote
        .device_keys(partner)
        .await
        .context("fetching the partner's keys failed")
}

/// `syncengine unpair <device-id>`: removes trust locally and, when a server
/// is configured, revokes the relationship server-side.
pub fn unpair(device_id: &str, config_arg: Option<&Path>) -> Result<()> {
    let device_id: DeviceId = device_id
        .parse()
        .map_err(|_| anyhow::anyhow!("`{device_id}` is not a valid device id (expected a UUID)"))?;
    let config = load_config(config_arg)?;
    let mut state = state::open(&config)?;

    let peer = state
        .store
        .get_peer(&device_id)?
        .ok_or_else(|| anyhow::anyhow!("no paired device with id {device_id}"))?;
    state.store.delete_peer(&device_id)?;
    println!("Unpaired {} ({})", peer.device_name, peer.device_id);
    println!("Local files were left untouched.");

    if let Some(server) = config.server_url.as_deref() {
        let remote = RemoteClient::new(server, &state.identity, state.device_id)?;
        let rt = runtime()?;
        match rt.block_on(remote.revoke_relationship(device_id)) {
            Ok(()) => println!("Revoked the relationship on the server."),
            Err(error) => {
                println!();
                println!("Warning: the server relationship was not revoked: {error:#}");
                println!("Run `syncengine unpair {device_id}` again once the server is reachable.");
            }
        }
    }
    Ok(())
}

/// `syncengine sync`: scans shared folders and synchronizes with every
/// trusted peer — LAN first, relay as fallback.
pub fn sync(config_arg: Option<&Path>) -> Result<()> {
    let config = load_config(config_arg)?;
    let state = state::open(&config)?;

    let device_name = state::effective_device_name(&config);
    let (identity, device_id, store, data_dir) = state.into_parts();
    let staging = syncengine_engine::StagingStore::new(data_dir.join(state::STAGING_DIR_NAME))
        .context("failed to open the staging directory")?;
    let mut engine = syncengine_engine::SyncEngine::new(device_id, store, staging);

    let folders = engine.store().list_folders()?;
    if folders.is_empty() {
        println!("No shared folders. Run `syncengine share <path>` first.");
        return Ok(());
    }

    let trusted: Vec<_> = engine
        .store()
        .list_peers()?
        .into_iter()
        .filter(|peer| peer.trust == TrustState::Trusted)
        .collect();
    ensure_folder_peer_links(&mut engine, &trusted)?;

    let mut total = syncengine_engine::QueueReport::default();
    let mut any_incomplete = false;
    for folder in &folders {
        if !folder.enabled {
            println!("Skipping {} ({}) — disabled", folder.name, folder.folder_id);
            continue;
        }
        let report = engine
            .queue_local_changes(&folder.folder_id)
            .with_context(|| format!("failed to scan {}", folder.local_root.display()))?;
        println!("{} ({})", folder.name, folder.folder_id);
        println!(
            "  created {}  updated {}  deleted {}  unchanged {}",
            report.created, report.updated, report.deleted, report.unchanged
        );
        if report.deletions_skipped {
            any_incomplete = true;
            println!("  scan was incomplete — deletions were not processed");
        }
        total.created += report.created;
        total.updated += report.updated;
        total.deleted += report.deleted;
        total.unchanged += report.unchanged;
        total.deletions_skipped |= report.deletions_skipped;
    }

    if any_incomplete {
        println!();
        println!("Warning: at least one scan was incomplete; nothing was treated as deleted.");
    }
    println!();
    println!(
        "Journaled {} change(s): {} created, {} updated, {} deleted ({unchanged} unchanged).",
        total.created + total.updated + total.deleted,
        total.created,
        total.updated,
        total.deleted,
        unchanged = total.unchanged
    );

    let peers = trusted;
    if peers.is_empty() {
        println!();
        println!("No paired devices yet. Run `syncengine pair <device-id>` first.");
        return Ok(());
    }

    let remote = config
        .server_url
        .as_deref()
        .map(|base| RemoteClient::new(base, &identity, device_id))
        .transpose()?;
    let options = PassOptions {
        listen_port: config.lan_port,
        session: syncengine_transport::session::SessionOptions {
            chunk_size: usize::try_from(config.limits.chunk_size_bytes).unwrap_or(256 * 1024),
            max_idle: Duration::from_secs(120),
        },
        ..PassOptions::default()
    };
    let rt = runtime()?;
    let report = rt.block_on(run_pass(
        &mut engine,
        &identity,
        &device_name,
        remote.as_ref(),
        &options,
    ))?;

    println!();
    if let Some(addr) = report.listen_addr {
        println!("LAN listener on port {} for this pass.", addr.port());
    }
    println!("Sync with {} paired device(s):", report.outcomes.len());
    for outcome in &report.outcomes {
        match &outcome.report {
            Some(session) => println!(
                "  {} via {}: delivered {} applied {} conflicts {}",
                outcome.peer_id,
                outcome.transport,
                session.delivered,
                session.applied,
                session.conflicts
            ),
            None => println!(
                "  {} not connected ({})",
                outcome.peer_id,
                outcome.error.as_deref().unwrap_or("no transport available")
            ),
        }
    }
    for warning in &report.warnings {
        println!("Warning: {warning}");
    }
    Ok(())
}

/// `syncengine conflicts [resolve]`: lists or settles detected conflicts.
pub fn conflicts(config_arg: Option<&Path>, args: &ConflictsArgs) -> Result<()> {
    let Some(action) = &args.action else {
        return list_conflicts(config_arg);
    };
    match action {
        ConflictsAction::Resolve(args) => resolve_conflict(config_arg, args),
    }
}

fn list_conflicts(config_arg: Option<&Path>) -> Result<()> {
    let config = load_config(config_arg)?;
    let state = state::open(&config)?;

    let all = state.store.list_conflicts(None)?;
    if all.is_empty() {
        println!("No conflicts.");
        return Ok(());
    }
    let unresolved = all
        .iter()
        .filter(|c| c.status == syncengine_storage::ConflictStatus::Unresolved)
        .count();
    println!("Conflicts ({unresolved} unresolved of {})", all.len());
    for conflict in &all {
        println!(
            "  {}  [{:?}]  folder {}  entry {}  created {}",
            conflict.conflict_id,
            conflict.status,
            conflict.folder_id,
            conflict.entry_id,
            conflict.created_at
        );
        if let Some(resolution) = &conflict.resolution {
            println!("      resolution: {resolution}");
        }
    }
    if unresolved > 0 {
        println!();
        println!(
            "Resolve one with `syncengine conflicts resolve <conflict-id> \
             --keep-local|--keep-remote|--dismiss`."
        );
    }
    Ok(())
}

fn resolve_conflict(config_arg: Option<&Path>, args: &crate::cli::ResolveArgs) -> Result<()> {
    use syncengine_engine::ConflictResolution;

    let config = load_config(config_arg)?;
    let state = state::open(&config)?;
    let (_, device_id, store, data_dir) = state.into_parts();
    let staging = syncengine_engine::StagingStore::new(data_dir.join(state::STAGING_DIR_NAME))
        .context("failed to open the staging directory")?;
    let mut engine = syncengine_engine::SyncEngine::new(device_id, store, staging);

    let resolution = if args.keep_remote {
        ConflictResolution::KeepRemote
    } else if args.keep_local {
        ConflictResolution::KeepLocal
    } else {
        ConflictResolution::Dismiss
    };

    let outcome = engine.resolve_conflict(&args.conflict_id, resolution)?;
    println!(
        "Conflict {} is now {}.",
        outcome.conflict_id,
        outcome.status.as_str()
    );
    if let Some(path) = &outcome.relative_path {
        println!("  path: {path}");
    }
    match resolution {
        ConflictResolution::KeepRemote => {
            println!("The preserved remote revision now lives at the shared path.");
            println!("The resolution is queued and propagates on `syncengine sync`.");
        }
        ConflictResolution::KeepLocal => {
            println!("The preserved copy was left on disk for inspection.");
        }
        ConflictResolution::Dismiss => {
            println!("No files were changed; the conflict was only marked dismissed.");
        }
    }
    Ok(())
}

/// `syncengine logs`: shows the tail of the background agent's log file.
pub fn logs(config_arg: Option<&Path>, args: &LogsArgs) -> Result<()> {
    let config = load_config(config_arg)?;
    let path = config.data_dir.join(crate::daemon::DAEMON_LOG_FILE);

    if args.path_only {
        println!("{}", path.display());
        return Ok(());
    }

    let contents = std::fs::read_to_string(&path).map_err(|error| {
        anyhow::anyhow!(
            "no agent log at {} ({error}); start the agent with `syncengine daemon start`",
            path.display()
        )
    })?;
    let total = contents.lines().count();
    let start = total.saturating_sub(args.lines);
    if total == 0 {
        println!("Agent log {} is empty.", path.display());
        return Ok(());
    }
    println!(
        "Agent log {} (lines {}-{} of {}):",
        path.display(),
        start + 1,
        total,
        total
    );
    for line in contents.lines().skip(start) {
        println!("{line}");
    }
    Ok(())
}

/// Severity of one `doctor` finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Finding {
    Ok,
    Warn,
    Fail,
}

struct Check {
    name: &'static str,
    finding: Finding,
    detail: String,
    hint: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            finding: Finding::Ok,
            detail: detail.into(),
            hint: None,
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            finding: Finding::Warn,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            finding: Finding::Fail,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
}

/// `syncengine doctor`: checks configuration, local state, and connectivity.
pub fn doctor(config_arg: Option<&Path>, args: &DoctorArgs) -> Result<()> {
    let path = crate::config::resolve_path(config_arg);
    let config = match config::load(&path) {
        Ok(config) => config,
        Err(error) => {
            println!("Configuration: FAILED");
            println!("  {} could not be loaded: {error}", path.display());
            println!();
            println!("Fix the file or run `syncengine init` to create a new one.");
            std::process::exit(1);
        }
    };

    let mut checks: Vec<Check> = Vec::new();
    checks.push(Check::ok(
        "configuration",
        format!(
            "{} (device name {:?}, LAN port {}, scan every {}s)",
            path.display(),
            config.device_name,
            config.lan_port,
            config.agent.scan_interval_secs
        ),
    ));

    checks.push(match std::fs::metadata(&config.data_dir) {
        Ok(meta) if !meta.is_dir() => Check::fail(
            "data directory",
            format!("{} is not a directory", config.data_dir.display()),
            "remove the path or point `data_dir` at a directory",
        ),
        Ok(_) => {
            // Opening a directory for writing is not portable (EISDIR on Unix),
            // so writability is checked by writing and removing a probe file.
            let probe = config
                .data_dir
                .join(format!(".doctor-probe-{}", std::process::id()));
            match std::fs::write(&probe, b"") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                    Check::ok(
                        "data directory",
                        format!("{} exists and is writable", config.data_dir.display()),
                    )
                }
                Err(error) => Check::fail(
                    "data directory",
                    format!("{} is not writable: {error}", config.data_dir.display()),
                    "fix the directory permissions for the current user",
                ),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Check::warn(
            "data directory",
            format!("{} does not exist yet", config.data_dir.display()),
            "it is created automatically by the first command that needs state",
        ),
        Err(error) => Check::fail(
            "data directory",
            format!("{} is unavailable: {error}", config.data_dir.display()),
            "create it, or fix `data_dir` in the configuration file",
        ),
    });

    let identity_path = config.data_dir.join(state::IDENTITY_FILE_NAME);
    match std::fs::metadata(&identity_path) {
        Ok(_) => {
            let mut check = Check::ok(
                "device identity",
                format!("{} present", identity_path.display()),
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&identity_path) {
                    let mode = meta.permissions().mode() & 0o777;
                    if mode & 0o077 != 0 {
                        check = Check::warn(
                            "device identity",
                            format!("{} has permissions {mode:o}", identity_path.display()),
                            "restrict it: chmod 600 <identity file>",
                        );
                    }
                }
            }
            checks.push(check);
        }
        Err(error) => checks.push(Check::warn(
            "device identity",
            format!("{} not found ({error})", identity_path.display()),
            "it is created automatically by the next command that needs state",
        )),
    }

    let state = match state::open(&config) {
        Ok(state) => {
            checks.push(Check::ok(
                "local database",
                format!("{} opened", config.database_path().display()),
            ));
            Some(state)
        }
        Err(error) => {
            checks.push(Check::fail(
                "local database",
                format!("{} unusable: {error}", config.database_path().display()),
                "check permissions and disk space; restore a backup if the file is corrupt",
            ));
            None
        }
    };

    if let Some(state) = &state {
        let folders = state.store.list_folders()?;
        let peers = state.store.list_peers()?;
        let trusted = peers
            .iter()
            .filter(|peer| peer.trust == TrustState::Trusted)
            .count();
        let conflicts = state
            .store
            .list_conflicts(Some(syncengine_storage::ConflictStatus::Unresolved))?;
        let queued: usize = peers
            .iter()
            .map(|peer| state.store.pending_for_peer(&peer.device_id))
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .map(|changes| changes.len())
            .sum();

        checks.push(Check::ok(
            "shared folders",
            format!("{} configured", folders.len()),
        ));
        for folder in &folders {
            if !folder.local_root.is_dir() {
                checks.push(Check::warn(
                    "folder root",
                    format!("{} is missing", folder.local_root.display()),
                    "restore the folder or run `syncengine unshare <folder-id>`",
                ));
            }
        }
        checks.push(Check::ok(
            "paired devices",
            format!("{trusted} trusted of {}", peers.len()),
        ));
        checks.push(Check::ok(
            "change queue",
            format!("{queued} change(s) awaiting delivery"),
        ));
        checks.push(if conflicts.is_empty() {
            Check::ok("conflicts", "none unresolved")
        } else {
            Check::warn(
                "conflicts",
                format!("{} unresolved", conflicts.len()),
                "review them with `syncengine conflicts`",
            )
        });
    }

    let daemon_running = match daemon::read_state(&config) {
        Some(state) if daemon::pid_alive(state.pid) => {
            Check::ok("background agent", format!("running (pid {})", state.pid))
        }
        Some(_) => Check::warn(
            "background agent",
            "not running",
            "automatic synchronization needs `syncengine daemon start`",
        ),
        None => Check::warn(
            "background agent",
            "not running",
            "automatic synchronization needs `syncengine daemon start`",
        ),
    };
    checks.push(daemon_running);

    match &config.server_url {
        None => checks.push(Check::warn(
            "coordination server",
            "no `server_url` configured",
            "LAN-only synchronization works; relay mode needs a server URL",
        )),
        Some(base) if args.skip_server => checks.push(Check::ok(
            "coordination server",
            format!("{base} configured (reachability check skipped)"),
        )),
        Some(base) => {
            let health = format!("{}/healthz", base.trim_end_matches('/'));
            let result = runtime()?.block_on(async {
                reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .map_err(anyhow::Error::from)?
                    .get(&health)
                    .send()
                    .await
                    .map_err(anyhow::Error::from)
            });
            match result {
                Ok(response) if response.status().is_success() => {
                    checks.push(Check::ok(
                        "coordination server",
                        format!("{health} reachable"),
                    ));
                }
                Ok(response) => checks.push(Check::warn(
                    "coordination server",
                    format!("{health} answered {}", response.status()),
                    "check the server version and TLS/proxy configuration",
                )),
                Err(error) => checks.push(Check::warn(
                    "coordination server",
                    format!("{health} unreachable: {error}"),
                    "LAN synchronization still works; check the URL, the server, and the network",
                )),
            }
        }
    }

    println!("SyncEngine doctor");
    println!();
    let mut failures = 0usize;
    let mut warnings = 0usize;
    for check in &checks {
        let label = match check.finding {
            Finding::Ok => "ok     ",
            Finding::Warn => {
                warnings += 1;
                "warning"
            }
            Finding::Fail => {
                failures += 1;
                "FAILED "
            }
        };
        println!("{label}  {:<20} {}", check.name, check.detail);
        if let Some(hint) = &check.hint {
            println!("        {:<20} → {hint}", "");
        }
    }
    println!();
    if failures > 0 {
        println!("{failures} check(s) failed, {warnings} warning(s).");
        std::process::exit(1);
    }
    if warnings > 0 {
        println!("Everything essential works; {warnings} warning(s) above are informational.");
    } else {
        println!("All checks passed.");
    }
    Ok(())
}

/// `syncengine daemon start|status|stop` and the internal `daemon run`.
pub fn daemon(action: &DaemonAction, config_arg: Option<&Path>) -> Result<()> {
    match action {
        DaemonAction::Start => {
            let config = load_config(config_arg)?;
            daemon::start(&config, config_arg)
        }
        DaemonAction::Status => {
            let config = load_config(config_arg)?;
            daemon::status(&config)
        }
        DaemonAction::Stop => {
            let config = load_config(config_arg)?;
            daemon::stop(&config)
        }
        DaemonAction::Run => {
            let path = config::resolve_path(config_arg);
            let rt = runtime()?;
            rt.block_on(daemon::run(path))
        }
    }
}

fn default_device_name() -> Option<String> {
    let name = hostname::get().ok()?;
    let name = name.to_string_lossy().trim().to_string();
    if name.is_empty() { None } else { Some(name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_defaults_device_name_to_hostname_or_fallback() {
        let args = InitArgs {
            name: None,
            force: false,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        init(&args, Some(&path)).expect("init");
        let config = config::load(&path).expect("load");
        assert!(!config.device_name.is_empty());
    }

    #[test]
    fn init_keeps_existing_file_without_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let first = InitArgs {
            name: Some("alice".to_string()),
            force: false,
        };
        init(&first, Some(&path)).expect("first init");
        let second = InitArgs {
            name: Some("bob".to_string()),
            force: false,
        };
        init(&second, Some(&path)).expect("second init");
        assert_eq!(config::load(&path).expect("load").device_name, "alice");

        let third = InitArgs {
            name: Some("bob".to_string()),
            force: true,
        };
        init(&third, Some(&path)).expect("forced init");
        assert_eq!(config::load(&path).expect("load").device_name, "bob");
    }

    #[test]
    fn init_rejects_overlong_device_name() {
        let args = InitArgs {
            name: Some("x".repeat(100)),
            force: false,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let err = init(&args, Some(&path)).unwrap_err();
        assert!(format!("{err:#}").contains("device_name"), "got: {err:#}");
        assert!(!path.exists());
    }

    fn config_with_temp_data(dir: &Path) -> (std::path::PathBuf, Config) {
        let path = dir.join("config.toml");
        let config = Config {
            device_name: "tester".to_string(),
            data_dir: dir.join("data"),
            lan_port: 51999,
            ..Config::default()
        };
        config::save(&path, &config).expect("save config");
        (path, config)
    }

    #[test]
    fn share_registers_folder_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, _config) = config_with_temp_data(dir.path());
        let root = dir.path().join("docs");
        std::fs::create_dir_all(root.join("nested")).expect("mkdir");
        std::fs::write(root.join("nested").join("a.txt"), b"a").expect("write");

        share(&root, None, Some(&path)).expect("share");
        share(&root, None, Some(&path)).expect("share again");

        let config = config::load(&path).expect("load");
        let state = state::open(&config).expect("state");
        let folders = state.store.list_folders().expect("folders");
        assert_eq!(folders.len(), 1, "duplicate share must not add a folder");
        assert_eq!(folders[0].local_root, std::fs::canonicalize(&root).unwrap());
    }

    #[test]
    fn share_rejects_missing_path_and_data_dir_overlap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, config) = config_with_temp_data(dir.path());
        assert!(share(&dir.path().join("missing"), None, Some(&path)).is_err());
        // Sharing the parent that contains the data directory is refused.
        assert!(share(dir.path(), None, Some(&path)).is_err());
        let _ = config;
    }

    #[test]
    fn share_join_adopts_the_given_folder_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, _config) = config_with_temp_data(dir.path());
        let root = dir.path().join("docs");
        std::fs::create_dir_all(&root).expect("mkdir");

        let folder_id = FolderId::new();
        share(&root, Some(&folder_id.to_string()), Some(&path)).expect("join");
        share(&root, Some(&folder_id.to_string()), Some(&path)).expect("join again");

        let config = config::load(&path).expect("load");
        let state = state::open(&config).expect("state");
        let folders = state.store.list_folders().expect("folders");
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].folder_id, folder_id);

        assert!(share(&root, Some("not-a-uuid"), Some(&path)).is_err());
        let other = dir.path().join("elsewhere");
        std::fs::create_dir_all(&other).expect("mkdir");
        assert!(
            share(&other, Some(&folder_id.to_string()), Some(&path)).is_err(),
            "the same joined id cannot point at two roots"
        );
    }

    #[test]
    fn unshare_removes_known_folder_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, _) = config_with_temp_data(dir.path());
        let root = dir.path().join("docs");
        std::fs::create_dir_all(&root).expect("mkdir");
        share(&root, None, Some(&path)).expect("share");

        let config = config::load(&path).expect("load");
        let folder_id = state::open(&config)
            .expect("state")
            .store
            .list_folders()
            .expect("folders")[0]
            .folder_id
            .to_string();

        unshare(&folder_id, Some(&path)).expect("unshare");
        let state = state::open(&config).expect("state");
        assert!(state.store.list_folders().expect("folders").is_empty());

        assert!(unshare("not-a-uuid", Some(&path)).is_err());
        assert!(unshare(&folder_id, Some(&path)).is_err(), "already removed");
    }

    #[test]
    fn sync_journals_local_changes_and_reports_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, config) = config_with_temp_data(dir.path());
        let root = dir.path().join("docs");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join("a.txt"), b"hello").expect("write");
        share(&root, None, Some(&path)).expect("share");

        sync(Some(&path)).expect("sync without peers still journals");

        let state = state::open(&config).expect("state");
        assert!(state.store.max_change_seq().expect("seq") >= 1);
    }

    #[test]
    fn pairing_requires_a_server_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, _) = config_with_temp_data(dir.path());
        let args = PairArgs {
            device_id: Some(DeviceId::new().to_string()),
            approve: None,
            code: None,
        };
        let err = pair(&args, Some(&path)).unwrap_err();
        assert!(format!("{err:#}").contains("server_url"), "got: {err:#}");
    }

    #[test]
    fn unpair_requires_a_known_peer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, _) = config_with_temp_data(dir.path());
        let err = unpair(&DeviceId::new().to_string(), Some(&path)).unwrap_err();
        assert!(
            format!("{err:#}").contains("no paired device"),
            "got: {err:#}"
        );
    }
}
