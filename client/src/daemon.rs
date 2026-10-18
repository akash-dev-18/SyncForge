//! Background agent process management and run loop.
//!
//! `daemon start` launches a detached `daemon run` child that writes a small
//! state file (pid, start time, heartbeat) each tick. `daemon status` reads
//! that file and measures how fresh the heartbeat is; `daemon stop` sends a
//! termination signal and waits for the child to clean up. This works without
//! platform-specific process APIs and is honest about stale files.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{self, Config};
use crate::delivery::{PassOptions, ensure_folder_peer_links, run_pass};
use crate::remote::RemoteClient;
use crate::state;
use syncengine_storage::{Store as _, TrustState};

/// File name of the daemon state (pid + heartbeat) inside the data directory.
pub const DAEMON_STATE_FILE: &str = "daemon.json";

/// File name of the daemon log inside the data directory.
pub const DAEMON_LOG_FILE: &str = "daemon.log";

/// State file contents of the last agent start.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DaemonState {
    pub(crate) pid: u32,
    pub(crate) started_at: String,
    pub(crate) last_beat_unix: i64,
    pub(crate) scan_interval_secs: u64,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs() as i64
}

fn state_path(config: &Config) -> PathBuf {
    config.data_dir.join(DAEMON_STATE_FILE)
}

/// Reads the agent state file, if one exists.
pub(crate) fn read_state(config: &Config) -> Option<DaemonState> {
    let text = std::fs::read_to_string(state_path(config)).ok()?;
    serde_json::from_str(&text).ok()
}

fn freshness_window(state: &DaemonState) -> i64 {
    (state.scan_interval_secs as i64 * 3).max(45)
}

/// Whether the recorded process still exists. A fresh heartbeat alone is not
/// enough: a crashed child leaves one behind.
pub(crate) fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        output
            .map(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

/// Starts a detached background agent. Errors when one is already running.
pub fn start(config: &Config, config_arg: Option<&Path>) -> Result<()> {
    if let Some(existing) = read_state(config) {
        if pid_alive(existing.pid)
            && now_unix() - existing.last_beat_unix <= freshness_window(&existing)
        {
            bail!(
                "the background agent is already running (pid {}); use `daemon stop` first",
                existing.pid
            );
        }
    }

    std::fs::create_dir_all(&config.data_dir).with_context(|| {
        format!(
            "failed to create data directory {}",
            config.data_dir.display()
        )
    })?;
    let log_path = config.data_dir.join(DAEMON_LOG_FILE);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let log_err = log.try_clone().context("failed to clone the log handle")?;

    let exe = std::env::current_exe().context("failed to locate the syncengine binary")?;
    let mut command = std::process::Command::new(exe);
    if let Some(path) = config_arg {
        command.arg("--config").arg(path);
    }
    command
        .arg("daemon")
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    let child = command
        .spawn()
        .context("failed to spawn the background agent")?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    if !pid_alive(child.id()) {
        bail!(
            "the background agent exited immediately after start; check {}",
            log_path.display()
        );
    }
    let state = DaemonState {
        pid: child.id(),
        started_at: syncengine_protocol::now_rfc3339(),
        last_beat_unix: now_unix(),
        scan_interval_secs: config.agent.scan_interval_secs,
    };
    std::fs::write(
        state_path(config),
        serde_json::to_vec_pretty(&state).expect("state serializes"),
    )
    .with_context(|| format!("failed to write {}", state_path(config).display()))?;

    println!("Background agent started (pid {}).", child.id());
    println!("  log  : {}", log_path.display());
    println!("  state: {}", state_path(config).display());
    Ok(())
}

/// Reports whether the agent is running, based on heartbeat freshness.
pub fn status(config: &Config) -> Result<()> {
    match read_state(config) {
        None => {
            println!("Background agent: not running");
            Ok(())
        }
        Some(state) => {
            let age = now_unix() - state.last_beat_unix;
            let running = pid_alive(state.pid) && age <= freshness_window(&state);
            if running {
                println!("Background agent: running (pid {})", state.pid);
            } else {
                println!(
                    "Background agent: not running (stale state from pid {}, last beat {}s ago)",
                    state.pid, age
                );
            }
            println!("  started : {}", state.started_at);
            println!("  heartbeat: {}s ago", age.max(0));
            println!("  scan interval: every {}s", state.scan_interval_secs);
            Ok(())
        }
    }
}

/// Stops the agent and waits for it to clean up its state file.
pub fn stop(config: &Config) -> Result<()> {
    let Some(state) = read_state(config) else {
        println!("Background agent: not running");
        return Ok(());
    };
    if !pid_alive(state.pid) || now_unix() - state.last_beat_unix > freshness_window(&state) {
        println!(
            "Background agent: not running (removing stale state file from pid {})",
            state.pid
        );
        let _ = std::fs::remove_file(state_path(config));
        return Ok(());
    }

    signal_terminate(state.pid)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if read_state(config).is_none() {
            println!("Background agent stopped (pid {}).", state.pid);
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    bail!(
        "the background agent (pid {}) did not stop within 10s; check it manually",
        state.pid
    );
}

#[cfg(unix)]
fn signal_terminate(pid: u32) -> Result<()> {
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .context("failed to run `kill`")?;
    if !status.success() {
        bail!("`kill -TERM {pid}` failed");
    }
    Ok(())
}

#[cfg(windows)]
fn signal_terminate(pid: u32) -> Result<()> {
    let status = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .context("failed to run `taskkill`")?;
    if !status.success() {
        bail!("`taskkill /PID {pid} /F` failed");
    }
    Ok(())
}

/// The foreground loop executed by `daemon run` (spawned by `daemon start`).
///
/// Each tick scans every enabled folder, then runs one delivery pass; the
/// pass's inbound window doubles as the inter-scan sleep so inbound LAN
/// connections are always being served.
pub async fn run(config_path: PathBuf) -> Result<()> {
    let config = config::load(&config_path)?;
    let device_name = state::effective_device_name(&config);
    let interval = std::time::Duration::from_secs(config.agent.scan_interval_secs);
    let pid = std::process::id();
    let started_at = syncengine_protocol::now_rfc3339();

    write_beat(&config, pid, &started_at)?;
    tracing::info!(
        pid,
        interval_secs = config.agent.scan_interval_secs,
        "background agent started"
    );

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        let tick = async {
            match tick(&config, &device_name, interval).await {
                Ok(()) => {}
                Err(error) => tracing::warn!(%error, "sync pass failed"),
            }
            let _ = write_beat(&config, pid, &started_at);
        };
        tokio::select! {
            _ = tick => {}
            _ = &mut shutdown => break,
        }
    }

    let _ = std::fs::remove_file(state_path(&config));
    tracing::info!(pid, "background agent stopped");
    Ok(())
}

fn write_beat(config: &Config, pid: u32, started_at: &str) -> Result<()> {
    let state = DaemonState {
        pid,
        started_at: started_at.to_string(),
        last_beat_unix: now_unix(),
        scan_interval_secs: config.agent.scan_interval_secs,
    };
    std::fs::write(
        state_path(config),
        serde_json::to_vec_pretty(&state).expect("state serializes"),
    )
    .with_context(|| format!("failed to write {}", state_path(config).display()))
}

async fn tick(config: &Config, device_name: &str, interval: std::time::Duration) -> Result<()> {
    let opened = state::open(config)?;
    let (identity, device_id, store, data_dir) = opened.into_parts();
    let staging = syncengine_engine::StagingStore::new(data_dir.join(state::STAGING_DIR_NAME))
        .context("failed to open the staging directory")?;
    let mut engine = syncengine_engine::SyncEngine::new(device_id, store, staging);

    let trusted: Vec<_> = engine
        .store()
        .list_peers()?
        .into_iter()
        .filter(|peer| peer.trust == TrustState::Trusted)
        .collect();
    ensure_folder_peer_links(&mut engine, &trusted)?;

    for folder in engine.store().list_folders()? {
        if !folder.enabled {
            continue;
        }
        match engine.queue_local_changes(&folder.folder_id) {
            Ok(report) => tracing::debug!(
                folder = %folder.folder_id,
                created = report.created,
                updated = report.updated,
                deleted = report.deleted,
                "scanned"
            ),
            Err(error) => tracing::warn!(
                folder = %folder.folder_id,
                %error,
                "scan failed"
            ),
        }
    }

    let remote = config
        .server_url
        .as_deref()
        .map(|base| RemoteClient::new(base, &identity, device_id))
        .transpose()?;
    let options = PassOptions {
        listen_port: config.lan_port,
        inbound_window: interval.min(std::time::Duration::from_secs(300)),
        session: syncengine_transport::session::SessionOptions {
            chunk_size: usize::try_from(config.limits.chunk_size_bytes).unwrap_or(256 * 1024),
            max_idle: std::time::Duration::from_secs(120),
        },
        ..PassOptions::default()
    };
    let report = run_pass(
        &mut engine,
        &identity,
        device_name,
        remote.as_ref(),
        &options,
    )
    .await?;
    tracing::info!(
        connected = report.connected(),
        peers = report.outcomes.len(),
        "sync pass finished"
    );
    for warning in &report.warnings {
        tracing::warn!("{warning}");
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
