//! Lazily-created local state shared by commands that need it.
//!
//! `init` writes configuration only; the data directory, device identity, and
//! SQLite database come into existence the first time a command actually needs
//! them (`share`, `devices`, `sync`, `conflicts`). This keeps read-only
//! commands like `status` free of side effects.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use syncengine_engine::{Identity, StagingStore, SyncEngine};
use syncengine_protocol::DeviceId;
use syncengine_storage::{LocalDevice, SqliteStore, Store};

use crate::config::{Config, FALLBACK_DEVICE_NAME};

/// File name of the device identity inside the data directory.
pub const IDENTITY_FILE_NAME: &str = "identity.json";

/// Directory name of the staging cache inside the data directory.
pub const STAGING_DIR_NAME: &str = "staging";

/// Opened local state: config, identity, store, and device ID.
pub struct State {
    pub config: Config,
    pub data_dir: PathBuf,
    pub identity: Identity,
    pub device_id: DeviceId,
    pub store: SqliteStore,
}

/// Opens (creating on first use) the local state for `config`.
pub fn open(config: &Config) -> Result<State> {
    let data_dir = config.data_dir.clone();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create data directory {}", data_dir.display()))?;

    let identity = Identity::load_or_create(&data_dir.join(IDENTITY_FILE_NAME))
        .context("failed to load or create the device identity")?;

    let mut store =
        SqliteStore::open(&config.database_path()).context("failed to open the local database")?;

    let device_name = effective_device_name(config);
    let device_id = ensure_local_device(&mut store, &device_name)?;

    Ok(State {
        config: config.clone(),
        data_dir,
        identity,
        device_id,
        store,
    })
}

fn ensure_local_device(store: &mut SqliteStore, device_name: &str) -> Result<DeviceId> {
    if let Some(existing) = store.local_device()? {
        return Ok(existing.device_id);
    }
    let device_id = DeviceId::new();
    store.set_local_device(device_id, device_name)?;
    Ok(device_id)
}

/// The device name to record: configured name, hostname, or fallback.
pub fn effective_device_name(config: &Config) -> String {
    let configured = config.device_name.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    if let Ok(host) = hostname::get() {
        let host = host.to_string_lossy().trim().to_string();
        if !host.is_empty() {
            return host;
        }
    }
    FALLBACK_DEVICE_NAME.to_string()
}

impl State {
    /// A staging store rooted inside the data directory.
    pub fn staging(&self) -> Result<StagingStore> {
        StagingStore::new(self.data_dir.join(STAGING_DIR_NAME))
            .context("failed to open the staging directory")
    }

    /// Consumes the state and builds a sync engine over the same store.
    pub fn into_engine(self) -> Result<SyncEngine<SqliteStore>> {
        let staging = self.staging()?;
        Ok(SyncEngine::new(self.device_id, self.store, staging))
    }

    /// Splits the state so callers can hold the identity (borrowed by
    /// transports) while moving the store into an engine.
    pub fn into_parts(self) -> (Identity, DeviceId, SqliteStore, PathBuf) {
        (self.identity, self.device_id, self.store, self.data_dir)
    }

    /// Convenience: the local device row, which exists after [`open`].
    pub fn local_device(&self) -> Result<Option<LocalDevice>> {
        Ok(self.store.local_device()?)
    }
}

/// Refuses to share a root that contains SyncEngine's own data directory,
/// which would otherwise index the database, identity, and staging cache.
pub fn root_contains_data_dir(root: &Path, data_dir: &Path) -> Result<bool> {
    let root = canonical_or_self(root);
    let data = canonical_or_self(data_dir);
    Ok(data.starts_with(&root))
}

fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
