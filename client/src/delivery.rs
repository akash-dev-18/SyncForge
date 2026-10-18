//! One synchronization pass against every trusted peer.
//!
//! Order of transports per peer (SRD §4.1):
//!
//! 1. **LAN**: the peer is discovered via mDNS and dialed directly over TCP.
//!    Frames are sealed end to end; the server is not involved.
//! 2. **Relay**: through the coordination server's WebSocket when a server is
//!    configured, a shared folder exists, and both devices are paired. The
//!    initiator is decided by device id order so both sides agree without
//!    extra coordination.
//!
//! While dialing, inbound LAN connections are accepted in short windows, and
//! one final window runs after all peers were dialed, so two devices running
//! a pass at the same time meet each other.

use std::net::SocketAddr;
use std::time::Duration;

use syncengine_engine::{Identity, SyncEngine};
use syncengine_protocol::{DeviceId, now_rfc3339};
use syncengine_storage::{FolderPeer, Permission, SqliteStore, Store, TrustState};
use syncengine_transport::discovery::{Advertiser, find_peer};
use syncengine_transport::handshake::{Role, verify_peer};
use syncengine_transport::lan::{LanListener, connect_lan};
use syncengine_transport::relay::connect_relay;
use syncengine_transport::session::{SessionOptions, SessionReport, run_session};

use crate::remote::RemoteClient;

/// Tunables of one pass.
#[derive(Debug, Clone)]
pub struct PassOptions {
    /// TCP port to listen on for inbound LAN connections (0 = OS-chosen).
    pub listen_port: u16,
    /// mDNS lookup budget per peer before falling back to the relay.
    pub lan_dial_timeout: Duration,
    /// How long to keep accepting inbound connections after dialing all peers.
    pub inbound_window: Duration,
    /// How long a responder waits for a relay session to appear.
    pub relay_wait: Duration,
    /// Session tunables (chunk size, idle timeout).
    pub session: SessionOptions,
}

impl Default for PassOptions {
    fn default() -> Self {
        Self {
            listen_port: 0,
            lan_dial_timeout: Duration::from_secs(2),
            inbound_window: Duration::from_secs(3),
            relay_wait: Duration::from_secs(10),
            session: SessionOptions::default(),
        }
    }
}

/// What happened with one peer during the pass.
#[derive(Debug, Clone)]
pub struct PeerOutcome {
    pub peer_id: DeviceId,
    /// `"lan"`, `"relay"`, or `"none"` when no transport connected.
    pub transport: &'static str,
    pub report: Option<SessionReport>,
    pub error: Option<String>,
}

/// Summary of a whole pass.
#[derive(Debug, Default, Clone)]
pub struct PassReport {
    /// The LAN listener actually bound (for the daemon log).
    pub listen_addr: Option<SocketAddr>,
    pub outcomes: Vec<PeerOutcome>,
    /// Non-fatal problems (server unreachable, membership registration, …).
    pub warnings: Vec<String>,
}

impl PassReport {
    /// Peers that completed a session.
    pub fn connected(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.report.is_some())
            .count()
    }
}

/// Runs one complete pass: advertise, dial every trusted peer, serve inbound.
pub async fn run_pass(
    engine: &mut SyncEngine<SqliteStore>,
    identity: &Identity,
    device_name: &str,
    remote: Option<&RemoteClient<'_>>,
    options: &PassOptions,
) -> anyhow::Result<PassReport> {
    let mut report = PassReport::default();

    let listener = bind_listener(options.listen_port, &mut report).await?;
    let listen_addr = listener.local_addr()?;
    report.listen_addr = Some(listen_addr);
    let advertiser = Advertiser::start(&engine.device_id(), device_name, listen_addr.port())
        .map_err(|error| anyhow::anyhow!("mDNS advertisement failed: {error}"))?;

    if let Some(remote) = remote {
        register_self(identity, device_name, remote, &mut report).await;
        register_memberships(engine, remote, &mut report).await;
    }

    let peers: Vec<_> = engine
        .store()
        .list_peers()?
        .into_iter()
        .filter(|peer| peer.trust == TrustState::Trusted)
        .collect();

    ensure_folder_peer_links(engine, &peers)?;

    for peer in &peers {
        let outcome = sync_with_peer(engine, identity, &peer.device_id, remote, options).await;
        if let Some(error) = &outcome.error {
            tracing::warn!(peer = %peer.device_id, %error, "peer sync failed");
        }
        report.outcomes.push(outcome);

        serve_inbound(
            engine,
            identity,
            &listener,
            options.inbound_window,
            &options.session,
        )
        .await?;
    }

    serve_inbound(
        engine,
        identity,
        &listener,
        options.inbound_window,
        &options.session,
    )
    .await?;
    drop(advertiser);

    match engine.gc_staging() {
        Ok(gc) if gc.removed > 0 => {
            tracing::debug!(
                removed = gc.removed,
                bytes = gc.bytes,
                "reclaimed staged content"
            );
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "staged-content cleanup failed"),
    }
    Ok(report)
}

/// Every enabled folder syncs with every trusted peer (SRD M3: `share` means
/// "sync with my paired devices"). Creates the membership row when missing and
/// backfills delivery targets for changes journaled before the link existed,
/// so pre-existing files are sent to newly paired devices too.
pub fn ensure_folder_peer_links(
    engine: &mut SyncEngine<SqliteStore>,
    peers: &[syncengine_storage::Peer],
) -> anyhow::Result<()> {
    let folders = engine.store().list_folders()?;
    for folder in folders.iter().filter(|folder| folder.enabled) {
        let existing: Vec<DeviceId> = engine
            .store()
            .list_folder_peers(&folder.folder_id)?
            .into_iter()
            .map(|membership| membership.peer_id)
            .collect();
        for peer in peers {
            if !existing.contains(&peer.device_id) {
                engine.store_mut().set_folder_peer(&FolderPeer {
                    folder_id: folder.folder_id,
                    peer_id: peer.device_id,
                    permission: Permission::ReadWrite,
                    joined_at: now_rfc3339(),
                })?;
            }
            let backfilled = engine
                .store_mut()
                .ensure_change_targets(&peer.device_id, &folder.folder_id)?;
            if backfilled > 0 {
                tracing::debug!(
                    folder = %folder.folder_id,
                    peer = %peer.device_id,
                    backfilled,
                    "queued existing changes for the peer"
                );
            }
        }
    }
    Ok(())
}

async fn bind_listener(port: u16, report: &mut PassReport) -> anyhow::Result<LanListener> {
    let wanted: SocketAddr = format!("0.0.0.0:{port}").parse().expect("valid address");
    match LanListener::bind(wanted).await {
        Ok(listener) => Ok(listener),
        Err(error) if port != 0 => {
            report.warnings.push(format!(
                "could not bind LAN port {port} ({error}); using an OS-assigned port"
            ));
            Ok(LanListener::bind("0.0.0.0:0".parse().expect("valid address")).await?)
        }
        Err(error) => Err(error.into()),
    }
}

async fn register_self(
    identity: &Identity,
    device_name: &str,
    remote: &RemoteClient<'_>,
    report: &mut PassReport,
) {
    let result = remote
        .register(
            device_name,
            std::env::consts::OS,
            &identity.ed25519_public_hex(),
            &identity.x25519_public_hex(),
        )
        .await;
    if let Err(error) = result {
        report.warnings.push(format!(
            "device registration with the server failed: {error:#}"
        ));
    }
}

async fn register_memberships(
    engine: &SyncEngine<SqliteStore>,
    remote: &RemoteClient<'_>,
    report: &mut PassReport,
) {
    let folders = match engine.store().list_folders() {
        Ok(folders) => folders,
        Err(error) => {
            report
                .warnings
                .push(format!("failed to list local folders: {error}"));
            return;
        }
    };
    for folder in folders.iter().filter(|folder| folder.enabled) {
        if let Err(error) = remote
            .upsert_membership(folder.folder_id, "read-write")
            .await
        {
            report.warnings.push(format!(
                "folder {} is not registered with the server: {error:#}",
                folder.folder_id
            ));
        }
    }
}

/// Tries LAN first, then relay. Never fails: problems land in the outcome.
async fn sync_with_peer(
    engine: &mut SyncEngine<SqliteStore>,
    identity: &Identity,
    peer: &DeviceId,
    remote: Option<&RemoteClient<'_>>,
    options: &PassOptions,
) -> PeerOutcome {
    let mut outcome = PeerOutcome {
        peer_id: *peer,
        transport: "none",
        report: None,
        error: None,
    };

    match try_lan(engine, identity, peer, options).await {
        Ok(report) => {
            outcome.transport = "lan";
            outcome.report = Some(report);
            return outcome;
        }
        Err(error) => outcome.error = Some(format!("LAN: {error}")),
    }

    match try_relay(engine, identity, remote, peer, options).await {
        Ok(report) => {
            outcome.transport = "relay";
            outcome.report = Some(report);
            outcome.error = None;
        }
        Err(error) => {
            let existing = outcome.error.take();
            outcome.error = Some(match existing {
                Some(lan_error) => format!("{lan_error}; relay: {error}"),
                None => format!("relay: {error}"),
            });
        }
    }

    outcome
}

async fn try_lan(
    engine: &mut SyncEngine<SqliteStore>,
    identity: &Identity,
    peer: &DeviceId,
    options: &PassOptions,
) -> Result<SessionReport, String> {
    let peer_id = *peer;
    let timeout = options.lan_dial_timeout;
    let candidates = tokio::task::spawn_blocking(move || find_peer(&peer_id, timeout))
        .await
        .map_err(|error| format!("mDNS task failed: {error}"))?
        .map_err(|error| format!("mDNS lookup failed: {error}"))?;
    if candidates.is_empty() {
        return Err(format!(
            "not discovered via mDNS within {}s",
            options.lan_dial_timeout.as_secs_f32()
        ));
    }

    let peer_record = engine
        .store()
        .get_peer(peer)
        .map_err(|error| format!("failed to load the peer: {error}"))?
        .ok_or_else(|| "peer vanished from the store".to_string())?;

    let mut last_error = String::new();
    for addr in candidates {
        let (mut channel, info) = match connect_lan(addr, identity, engine.device_id()).await {
            Ok(connected) => connected,
            Err(error) => {
                tracing::debug!(%addr, %error, "lan dial attempt failed");
                last_error = format!("connect to {addr} failed: {error}");
                continue;
            }
        };
        verify_peer(&info, &peer_record)
            .map_err(|error| format!("verification failed: {error}"))?;
        return run_session(engine, &mut channel, &peer_record, &options.session)
            .await
            .map_err(|error| format!("session failed: {error}"));
    }
    Err(last_error)
}

async fn try_relay(
    engine: &mut SyncEngine<SqliteStore>,
    identity: &Identity,
    remote: Option<&RemoteClient<'_>>,
    peer: &DeviceId,
    options: &PassOptions,
) -> Result<SessionReport, String> {
    let Some(remote) = remote else {
        return Err("no server_url configured".to_string());
    };
    let peer_record = engine
        .store()
        .get_peer(peer)
        .map_err(|error| format!("failed to load the peer: {error}"))?
        .ok_or_else(|| "peer vanished from the store".to_string())?;

    let folder_id = common_folder(engine, peer)?.ok_or_else(|| {
        "no shared enabled folder with this peer (needed for relay authorization)".to_string()
    })?;

    let i_initiate = engine.device_id().to_string() < peer.to_string();
    let session_id = if i_initiate {
        remote
            .create_relay_session(*peer, folder_id)
            .await
            .map_err(|error| format!("creating relay session failed: {error:#}"))?
    } else {
        wait_for_session(remote, peer, folder_id, options.relay_wait).await?
    };

    let role = if i_initiate {
        Role::Initiator
    } else {
        Role::Responder
    };
    let (mut channel, info) = connect_relay(
        remote.base(),
        &session_id,
        identity,
        engine.device_id(),
        role,
    )
    .await
    .map_err(|error| format!("relay connect failed: {error}"))?;
    verify_peer(&info, &peer_record).map_err(|error| format!("verification failed: {error}"))?;
    run_session(engine, &mut channel, &peer_record, &options.session)
        .await
        .map_err(|error| format!("session failed: {error}"))
}

async fn wait_for_session(
    remote: &RemoteClient<'_>,
    peer: &DeviceId,
    folder_id: syncengine_protocol::FolderId,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let sessions = remote
            .list_relay_sessions()
            .await
            .map_err(|error| format!("listing relay sessions failed: {error:#}"))?;
        if let Some(session) = sessions
            .iter()
            .find(|session| session.sender_device_id == *peer && session.folder_id == folder_id)
        {
            return Ok(session.session_id.clone());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "no relay session from that peer appeared within {}s (the peer must run a sync pass too)",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn common_folder(
    engine: &SyncEngine<SqliteStore>,
    peer: &DeviceId,
) -> Result<Option<syncengine_protocol::FolderId>, String> {
    for membership in engine
        .store()
        .list_peer_folders(peer)
        .map_err(|error| format!("failed to list peer folders: {error}"))?
    {
        let enabled = engine
            .store()
            .get_folder(&membership.folder_id)
            .map_err(|error| format!("failed to load folder: {error}"))?
            .map(|folder| folder.enabled)
            .unwrap_or(false);
        if enabled {
            return Ok(Some(membership.folder_id));
        }
    }
    Ok(None)
}

/// Accepts inbound LAN connections until the window closes.
async fn serve_inbound(
    engine: &mut SyncEngine<SqliteStore>,
    identity: &Identity,
    listener: &LanListener,
    window: Duration,
    session_options: &SessionOptions,
) -> anyhow::Result<()> {
    if window.is_zero() {
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let accepted =
            tokio::time::timeout(remaining, listener.accept(identity, engine.device_id())).await;
        let Ok(Ok((mut channel, info, addr))) = accepted else {
            return Ok(());
        };

        let Some(peer) = engine.store().get_peer(&info.device_id)? else {
            tracing::warn!(%addr, device = %info.device_id, "rejecting inbound connection from unknown device");
            continue;
        };
        if peer.trust != TrustState::Trusted {
            tracing::warn!(%addr, device = %info.device_id, "rejecting inbound connection from untrusted device");
            continue;
        }
        if let Err(error) = verify_peer(&info, &peer) {
            tracing::warn!(%addr, device = %info.device_id, %error, "rejecting inbound connection");
            continue;
        }

        tracing::info!(%addr, peer = %peer.device_id, "serving inbound LAN session");
        match run_session(engine, &mut channel, &peer, session_options).await {
            Ok(report) => tracing::info!(
                peer = %peer.device_id,
                delivered = report.delivered,
                applied = report.applied,
                "inbound session finished"
            ),
            Err(error) => {
                tracing::warn!(peer = %peer.device_id, %error, "inbound session failed");
            }
        }
    }
}
