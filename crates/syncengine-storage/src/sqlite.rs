//! `SqliteStore`: the SQLite implementation of [`Store`](crate::Store).

use rusqlite::{Connection, OptionalExtension, params};
use syncengine_protocol::{
    ChangeId, ChangeRecord, ContentHash, DeviceId, EntryId, FolderId, Operation,
};

use crate::traits::{decode_version, encode_version};
use crate::{
    Conflict, ConflictStatus, ContentObject, EntryState, FileEntry, FolderPeer, LocalDevice, Peer,
    PeerSyncState, Permission, SharedFolder, Store, StoreError, TargetState, Tombstone, Transfer,
    TransferDirection, TransferState, TrustState,
};

/// Schema version applied by this build.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA_V1: &str = r#"
CREATE TABLE local_device (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  device_id TEXT NOT NULL UNIQUE,
  device_name TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE peers (
  device_id TEXT PRIMARY KEY,
  device_name TEXT NOT NULL,
  public_key TEXT NOT NULL,
  encryption_key TEXT NOT NULL,
  platform TEXT NOT NULL DEFAULT '',
  trust TEXT NOT NULL CHECK (trust IN ('pending','trusted','revoked')),
  last_seen_at TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE shared_folders (
  folder_id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  local_root TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL
);

CREATE TABLE folder_peers (
  folder_id TEXT NOT NULL REFERENCES shared_folders(folder_id) ON DELETE CASCADE,
  peer_id TEXT NOT NULL REFERENCES peers(device_id) ON DELETE CASCADE,
  permission TEXT NOT NULL CHECK (permission IN ('read_only','read_write')),
  joined_at TEXT NOT NULL,
  PRIMARY KEY (folder_id, peer_id)
);

CREATE TABLE file_entries (
  entry_id TEXT PRIMARY KEY,
  folder_id TEXT NOT NULL REFERENCES shared_folders(folder_id) ON DELETE CASCADE,
  relative_path TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('present','deleted')),
  content_hash TEXT,
  size INTEGER NOT NULL DEFAULT 0,
  modified_at TEXT,
  version TEXT NOT NULL,
  last_change_id TEXT,
  updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_file_entries_present_path
  ON file_entries(folder_id, relative_path) WHERE state = 'present';
CREATE INDEX idx_file_entries_folder ON file_entries(folder_id);

CREATE TABLE changes (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  change_id TEXT NOT NULL UNIQUE,
  folder_id TEXT NOT NULL,
  entry_id TEXT NOT NULL,
  operation TEXT NOT NULL CHECK (operation IN ('create','update','delete','rename')),
  relative_path TEXT NOT NULL,
  old_relative_path TEXT,
  content_hash TEXT,
  size INTEGER NOT NULL DEFAULT 0,
  base_version TEXT NOT NULL,
  version TEXT NOT NULL,
  origin_device TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX idx_changes_folder ON changes(folder_id);

CREATE TABLE change_targets (
  change_id TEXT NOT NULL REFERENCES changes(change_id) ON DELETE CASCADE,
  peer_id TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending','in_progress','delivered','failed')),
  attempts INTEGER NOT NULL DEFAULT 0,
  next_retry_at TEXT,
  acknowledged_at TEXT,
  last_error TEXT,
  PRIMARY KEY (change_id, peer_id)
);
CREATE INDEX idx_change_targets_peer_state ON change_targets(peer_id, state);

CREATE TABLE tombstones (
  change_id TEXT PRIMARY KEY,
  entry_id TEXT NOT NULL,
  folder_id TEXT NOT NULL,
  last_path TEXT NOT NULL,
  version TEXT NOT NULL,
  deleted_at TEXT NOT NULL
);

CREATE TABLE peer_sync_state (
  folder_id TEXT NOT NULL,
  peer_id TEXT NOT NULL,
  last_acknowledged_seq INTEGER NOT NULL DEFAULT 0,
  last_synced_at TEXT,
  PRIMARY KEY (folder_id, peer_id)
);

CREATE TABLE transfers (
  transfer_id TEXT PRIMARY KEY,
  change_id TEXT NOT NULL,
  peer_id TEXT NOT NULL,
  direction TEXT NOT NULL CHECK (direction IN ('send','receive')),
  state TEXT NOT NULL CHECK (state IN ('queued','active','paused','completed','failed','cancelled')),
  expected_hash TEXT,
  expected_size INTEGER NOT NULL DEFAULT 0,
  acknowledged_bytes INTEGER NOT NULL DEFAULT 0,
  temporary_path TEXT,
  updated_at TEXT NOT NULL
);
CREATE INDEX idx_transfers_change ON transfers(change_id);

CREATE TABLE applied_remote_changes (
  change_id TEXT PRIMARY KEY,
  origin_device TEXT NOT NULL,
  applied_at TEXT NOT NULL
);

CREATE TABLE conflicts (
  conflict_id TEXT PRIMARY KEY,
  folder_id TEXT NOT NULL,
  entry_id TEXT NOT NULL,
  local_change_id TEXT,
  remote_change_id TEXT,
  status TEXT NOT NULL CHECK (status IN ('unresolved','resolved','dismissed')),
  resolution TEXT,
  created_at TEXT NOT NULL,
  resolved_at TEXT
);
CREATE INDEX idx_conflicts_status ON conflicts(status);

CREATE TABLE content_objects (
  content_hash TEXT PRIMARY KEY,
  size INTEGER NOT NULL,
  staged_path TEXT NOT NULL,
  refcount INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL,
  verified_at TEXT
);
"#;

/// A SQLite-backed [`Store`].
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// Opens (creating if needed) a database at `path` and migrates it.
    pub fn open(path: &std::path::Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// An in-memory database for tests.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(mut conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
               version INTEGER PRIMARY KEY,
               applied_at TEXT NOT NULL
            );",
        )?;
        let current: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if current < SCHEMA_VERSION {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA_V1)?;
            let now = syncengine_protocol::now_rfc3339();
            tx.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                params![SCHEMA_VERSION, now],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    fn conn(&self) -> &Connection {
        &self.conn
    }
}

fn parse_operation(s: &str) -> Result<Operation, StoreError> {
    match s {
        "create" => Ok(Operation::Create),
        "update" => Ok(Operation::Update),
        "delete" => Ok(Operation::Delete),
        "rename" => Ok(Operation::Rename),
        other => Err(StoreError::Invalid(format!("unknown operation {other}"))),
    }
}

fn operation_str(op: Operation) -> &'static str {
    match op {
        Operation::Create => "create",
        Operation::Update => "update",
        Operation::Delete => "delete",
        Operation::Rename => "rename",
    }
}

fn conv_err<E>(col: usize, e: E) -> rusqlite::Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(e))
}

fn invalid_col(col: usize, name: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(col, name.to_string(), rusqlite::types::Type::Text)
}

fn row_to_change(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChangeRecord> {
    let change_id: String = row.get("change_id")?;
    let folder_id: String = row.get("folder_id")?;
    let entry_id: String = row.get("entry_id")?;
    let operation: String = row.get("operation")?;
    let relative_path: String = row.get("relative_path")?;
    let old_relative_path: Option<String> = row.get("old_relative_path")?;
    let content_hash: Option<String> = row.get("content_hash")?;
    let size: i64 = row.get("size")?;
    let base_version: String = row.get("base_version")?;
    let version: String = row.get("version")?;
    let origin_device: String = row.get("origin_device")?;
    let created_at: String = row.get("created_at")?;

    Ok(ChangeRecord {
        change_id: change_id.parse().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::fmt::Error),
            )
        })?,
        folder_id: folder_id.parse().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::fmt::Error),
            )
        })?,
        entry_id: entry_id.parse().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(std::fmt::Error),
            )
        })?,
        operation: parse_operation(&operation).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(std::fmt::Error),
            )
        })?,
        relative_path,
        old_relative_path,
        content_hash: content_hash.map(ContentHash),
        size: size.max(0) as u64,
        version: serde_json::from_str(&version).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?,
        base_version: serde_json::from_str(&base_version).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?,
        origin_device: origin_device.parse().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(std::fmt::Error),
            )
        })?,
        timestamp: created_at,
    })
}

impl Store for SqliteStore {
    fn local_device(&self) -> Result<Option<LocalDevice>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT device_id, device_name, created_at FROM local_device WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((device_id, device_name, created_at)) => Ok(Some(LocalDevice {
                device_id: device_id
                    .parse()
                    .map_err(|_| StoreError::Invalid("bad device_id".into()))?,
                device_name,
                created_at,
            })),
        }
    }

    fn set_local_device(&mut self, device_id: DeviceId, name: &str) -> Result<(), StoreError> {
        let now = syncengine_protocol::now_rfc3339();
        self.conn().execute(
            "INSERT INTO local_device (id, device_id, device_name, created_at)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET device_id = ?1, device_name = ?2",
            params![device_id.to_string(), name, now],
        )?;
        Ok(())
    }

    fn upsert_peer(&mut self, peer: &Peer) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO peers (device_id, device_name, public_key, encryption_key, platform, trust, last_seen_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(device_id) DO UPDATE SET
               device_name = ?2, public_key = ?3, encryption_key = ?4,
               platform = ?5, trust = ?6, last_seen_at = COALESCE(?7, last_seen_at)",
            params![
                peer.device_id.to_string(),
                peer.device_name,
                peer.public_key,
                peer.encryption_key,
                peer.platform,
                peer.trust.as_str(),
                peer.last_seen_at,
                peer.created_at,
            ],
        )?;
        Ok(())
    }

    fn get_peer(&self, device_id: &DeviceId) -> Result<Option<Peer>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT device_id, device_name, public_key, encryption_key, platform, trust, last_seen_at, created_at
                 FROM peers WHERE device_id = ?1",
                params![device_id.to_string()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;
        row.map(|t| {
            Ok(Peer {
                device_id: t
                    .0
                    .parse()
                    .map_err(|_| StoreError::Invalid("bad id".into()))?,
                device_name: t.1,
                public_key: t.2,
                encryption_key: t.3,
                platform: t.4,
                trust: TrustState::parse(&t.5)
                    .ok_or_else(|| StoreError::Invalid("bad trust".into()))?,
                last_seen_at: t.6,
                created_at: t.7,
            })
        })
        .transpose()
    }

    fn delete_peer(&mut self, device_id: &DeviceId) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM folder_peers WHERE peer_id = ?1",
            params![device_id.to_string()],
        )?;
        tx.execute(
            "DELETE FROM peers WHERE device_id = ?1",
            params![device_id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn list_peers(&self) -> Result<Vec<Peer>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT device_id, device_name, public_key, encryption_key, platform, trust, last_seen_at, created_at
             FROM peers ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        let mut peers = Vec::new();
        for row in rows {
            let t = row?;
            peers.push(Peer {
                device_id: t
                    .0
                    .parse()
                    .map_err(|_| StoreError::Invalid("bad id".into()))?,
                device_name: t.1,
                public_key: t.2,
                encryption_key: t.3,
                platform: t.4,
                trust: TrustState::parse(&t.5)
                    .ok_or_else(|| StoreError::Invalid("bad trust".into()))?,
                last_seen_at: t.6,
                created_at: t.7,
            });
        }
        Ok(peers)
    }

    fn set_peer_trust(
        &mut self,
        device_id: &DeviceId,
        trust: TrustState,
    ) -> Result<(), StoreError> {
        let n = self.conn().execute(
            "UPDATE peers SET trust = ?2 WHERE device_id = ?1",
            params![device_id.to_string(), trust.as_str()],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("peer {device_id}")));
        }
        Ok(())
    }

    fn touch_peer_seen(&mut self, device_id: &DeviceId, seen_at: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE peers SET last_seen_at = ?2 WHERE device_id = ?1",
            params![device_id.to_string(), seen_at],
        )?;
        Ok(())
    }

    fn upsert_folder(&mut self, folder: &SharedFolder) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO shared_folders (folder_id, name, local_root, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(folder_id) DO UPDATE SET name = ?2, local_root = ?3, enabled = ?4",
            params![
                folder.folder_id.to_string(),
                folder.name,
                folder.local_root.to_string_lossy(),
                folder.enabled as i64,
                folder.created_at,
            ],
        )?;
        Ok(())
    }

    fn get_folder(&self, folder_id: &FolderId) -> Result<Option<SharedFolder>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT folder_id, name, local_root, enabled, created_at FROM shared_folders WHERE folder_id = ?1",
                params![folder_id.to_string()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        row.map(|t| {
            Ok(SharedFolder {
                folder_id: t
                    .0
                    .parse()
                    .map_err(|_| StoreError::Invalid("bad id".into()))?,
                name: t.1,
                local_root: t.2.into(),
                enabled: t.3 != 0,
                created_at: t.4,
            })
        })
        .transpose()
    }

    fn list_folders(&self) -> Result<Vec<SharedFolder>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT folder_id, name, local_root, enabled, created_at FROM shared_folders ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut folders = Vec::new();
        for row in rows {
            let t = row?;
            folders.push(SharedFolder {
                folder_id: t
                    .0
                    .parse()
                    .map_err(|_| StoreError::Invalid("bad id".into()))?,
                name: t.1,
                local_root: t.2.into(),
                enabled: t.3 != 0,
                created_at: t.4,
            });
        }
        Ok(folders)
    }

    fn delete_folder(&mut self, folder_id: &FolderId) -> Result<(), StoreError> {
        self.conn().execute(
            "DELETE FROM shared_folders WHERE folder_id = ?1",
            params![folder_id.to_string()],
        )?;
        Ok(())
    }

    fn set_folder_peer(&mut self, membership: &FolderPeer) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO folder_peers (folder_id, peer_id, permission, joined_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(folder_id, peer_id) DO UPDATE SET permission = ?3",
            params![
                membership.folder_id.to_string(),
                membership.peer_id.to_string(),
                membership.permission.as_str(),
                membership.joined_at,
            ],
        )?;
        Ok(())
    }

    fn list_folder_peers(&self, folder_id: &FolderId) -> Result<Vec<FolderPeer>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT folder_id, peer_id, permission, joined_at FROM folder_peers WHERE folder_id = ?1",
        )?;
        let rows = stmt.query_map(params![folder_id.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let t = row?;
            out.push(FolderPeer {
                folder_id: t
                    .0
                    .parse()
                    .map_err(|_| StoreError::Invalid("bad id".into()))?,
                peer_id: t
                    .1
                    .parse()
                    .map_err(|_| StoreError::Invalid("bad id".into()))?,
                permission: Permission::parse(&t.2)
                    .ok_or_else(|| StoreError::Invalid("bad permission".into()))?,
                joined_at: t.3,
            });
        }
        Ok(out)
    }

    fn list_peer_folders(&self, peer_id: &DeviceId) -> Result<Vec<FolderPeer>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT folder_id, peer_id, permission, joined_at FROM folder_peers WHERE peer_id = ?1",
        )?;
        let rows = stmt.query_map(params![peer_id.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let t = row?;
            out.push(FolderPeer {
                folder_id: t
                    .0
                    .parse()
                    .map_err(|_| StoreError::Invalid("bad id".into()))?,
                peer_id: t
                    .1
                    .parse()
                    .map_err(|_| StoreError::Invalid("bad id".into()))?,
                permission: Permission::parse(&t.2)
                    .ok_or_else(|| StoreError::Invalid("bad permission".into()))?,
                joined_at: t.3,
            });
        }
        Ok(out)
    }

    fn remove_folder_peer(
        &mut self,
        folder_id: &FolderId,
        peer_id: &DeviceId,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "DELETE FROM folder_peers WHERE folder_id = ?1 AND peer_id = ?2",
            params![folder_id.to_string(), peer_id.to_string()],
        )?;
        Ok(())
    }

    fn upsert_entry(&mut self, entry: &FileEntry) -> Result<(), StoreError> {
        let version = encode_version(&entry.version)?;
        self.conn().execute(
            "INSERT INTO file_entries (entry_id, folder_id, relative_path, state, content_hash, size, modified_at, version, last_change_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(entry_id) DO UPDATE SET
               relative_path = ?3, state = ?4, content_hash = ?5, size = ?6,
               modified_at = ?7, version = ?8, last_change_id = ?9, updated_at = ?10",
            params![
                entry.entry_id.to_string(),
                entry.folder_id.to_string(),
                entry.relative_path,
                entry.state.as_str(),
                entry.content_hash.as_ref().map(|h| h.0.clone()),
                entry.size as i64,
                entry.modified_at,
                version,
                entry.last_change_id.map(|c| c.to_string()),
                entry.updated_at,
            ],
        )?;
        Ok(())
    }

    fn get_entry(
        &self,
        folder_id: &FolderId,
        relative_path: &str,
    ) -> Result<Option<FileEntry>, StoreError> {
        let row = self.conn().query_row(
            "SELECT entry_id, folder_id, relative_path, state, content_hash, size, modified_at, version, last_change_id, updated_at
             FROM file_entries WHERE folder_id = ?1 AND relative_path = ?2 AND state = 'present'",
            params![folder_id.to_string(), relative_path],
            |r| {
                Ok(FileEntry {
                    entry_id: r.get::<_, String>(0)?.parse().map_err(|_| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(std::fmt::Error),
                        )
                    })?,
                    folder_id: r.get::<_, String>(1)?.parse().map_err(|_| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(std::fmt::Error),
                        )
                    })?,
                    relative_path: r.get(2)?,
                    state: EntryState::parse(&r.get::<_, String>(3)?).ok_or(
                        invalid_col(3, "state"),
                    )?,
                    content_hash: r.get::<_, Option<String>>(4)?.map(ContentHash),
                    size: r.get::<_, i64>(5)?.max(0) as u64,
                    modified_at: r.get(6)?,
                    version: decode_version(&r.get::<_, String>(7)?)
                        .map_err(|e| conv_err(7, e))?,
                    last_change_id: r
                        .get::<_, Option<String>>(8)?
                        .map(|s| s.parse().map_err(|_| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Text,
                                Box::new(std::fmt::Error),
                            )
                        }))
                        .transpose()?,
                    updated_at: r.get(9)?,
                })
            },
        ).optional()?;
        Ok(row)
    }

    fn get_entry_by_id(&self, entry_id: &EntryId) -> Result<Option<FileEntry>, StoreError> {
        let row = self.conn().query_row(
            "SELECT entry_id, folder_id, relative_path, state, content_hash, size, modified_at, version, last_change_id, updated_at
             FROM file_entries WHERE entry_id = ?1",
            params![entry_id.to_string()],
            |r| {
                Ok(FileEntry {
                    entry_id: r.get::<_, String>(0)?.parse().map_err(|_| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(std::fmt::Error),
                        )
                    })?,
                    folder_id: r.get::<_, String>(1)?.parse().map_err(|_| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(std::fmt::Error),
                        )
                    })?,
                    relative_path: r.get(2)?,
                    state: EntryState::parse(&r.get::<_, String>(3)?).ok_or(
                        invalid_col(3, "state"),
                    )?,
                    content_hash: r.get::<_, Option<String>>(4)?.map(ContentHash),
                    size: r.get::<_, i64>(5)?.max(0) as u64,
                    modified_at: r.get(6)?,
                    version: decode_version(&r.get::<_, String>(7)?)
                        .map_err(|e| conv_err(7, e))?,
                    last_change_id: r
                        .get::<_, Option<String>>(8)?
                        .map(|s| s.parse().map_err(|_| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Text,
                                Box::new(std::fmt::Error),
                            )
                        }))
                        .transpose()?,
                    updated_at: r.get(9)?,
                })
            },
        ).optional()?;
        Ok(row)
    }

    fn list_entries(&self, folder_id: &FolderId) -> Result<Vec<FileEntry>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT entry_id, folder_id, relative_path, state, content_hash, size, modified_at, version, last_change_id, updated_at
             FROM file_entries WHERE folder_id = ?1 ORDER BY relative_path",
        )?;
        let rows = stmt.query_map(params![folder_id.to_string()], |r| {
            Ok(FileEntry {
                entry_id: r.get::<_, String>(0)?.parse().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(std::fmt::Error),
                    )
                })?,
                folder_id: r.get::<_, String>(1)?.parse().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(std::fmt::Error),
                    )
                })?,
                relative_path: r.get(2)?,
                state: EntryState::parse(&r.get::<_, String>(3)?).ok_or(invalid_col(3, "state"))?,
                content_hash: r.get::<_, Option<String>>(4)?.map(ContentHash),
                size: r.get::<_, i64>(5)?.max(0) as u64,
                modified_at: r.get(6)?,
                version: decode_version(&r.get::<_, String>(7)?).map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        Box::new(std::fmt::Error),
                    )
                })?,
                last_change_id: r
                    .get::<_, Option<String>>(8)?
                    .map(|s| {
                        s.parse().map_err(|_| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Text,
                                Box::new(std::fmt::Error),
                            )
                        })
                    })
                    .transpose()?,
                updated_at: r.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn list_present_entries(&self, folder_id: &FolderId) -> Result<Vec<FileEntry>, StoreError> {
        Ok(self
            .list_entries(folder_id)?
            .into_iter()
            .filter(|e| e.state == EntryState::Present)
            .collect())
    }

    fn append_change(
        &mut self,
        record: &ChangeRecord,
        peer_ids: &[DeviceId],
    ) -> Result<(), StoreError> {
        let base_version = encode_version(&record.base_version)?;
        let version = encode_version(&record.version)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO changes (change_id, folder_id, entry_id, operation, relative_path, old_relative_path, content_hash, size, base_version, version, origin_device, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                record.change_id.to_string(),
                record.folder_id.to_string(),
                record.entry_id.to_string(),
                operation_str(record.operation),
                record.relative_path,
                record.old_relative_path,
                record.content_hash.as_ref().map(|h| h.0.clone()),
                record.size as i64,
                base_version,
                version,
                record.origin_device.to_string(),
                record.timestamp,
            ],
        )?;
        for peer in peer_ids {
            tx.execute(
                "INSERT INTO change_targets (change_id, peer_id, state) VALUES (?1, ?2, 'pending')
                 ON CONFLICT(change_id, peer_id) DO NOTHING",
                params![record.change_id.to_string(), peer.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn get_change(&self, change_id: &ChangeId) -> Result<Option<ChangeRecord>, StoreError> {
        let row = self.conn().query_row(
            "SELECT change_id, folder_id, entry_id, operation, relative_path, old_relative_path, content_hash, size, base_version, version, origin_device, created_at
             FROM changes WHERE change_id = ?1",
            params![change_id.to_string()],
            row_to_change,
        ).optional()?;
        Ok(row)
    }

    fn changes_after(&self, after_seq: i64) -> Result<Vec<(i64, ChangeRecord)>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT seq, change_id, folder_id, entry_id, operation, relative_path, old_relative_path, content_hash, size, base_version, version, origin_device, created_at
             FROM changes WHERE seq > ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![after_seq], |r| {
            let seq: i64 = r.get(0)?;
            let record = row_to_change(r)?;
            Ok((seq, record))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn ensure_change_targets(
        &mut self,
        peer_id: &DeviceId,
        folder_id: &FolderId,
    ) -> Result<usize, StoreError> {
        let created = self.conn.execute(
            "INSERT INTO change_targets (change_id, peer_id, state)
             SELECT c.change_id, ?1, 'pending' FROM changes c
             LEFT JOIN change_targets t
               ON t.change_id = c.change_id AND t.peer_id = ?1
             WHERE c.folder_id = ?2 AND t.change_id IS NULL",
            params![peer_id.to_string(), folder_id.to_string()],
        )?;
        Ok(created)
    }

    fn pending_for_peer(&self, peer_id: &DeviceId) -> Result<Vec<ChangeRecord>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT c.change_id, c.folder_id, c.entry_id, c.operation, c.relative_path, c.old_relative_path, c.content_hash, c.size, c.base_version, c.version, c.origin_device, c.created_at
             FROM changes c
             JOIN change_targets t ON t.change_id = c.change_id
             WHERE t.peer_id = ?1 AND t.state != 'delivered'
             ORDER BY c.seq",
        )?;
        let rows = stmt.query_map(params![peer_id.to_string()], row_to_change)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn mark_target(
        &mut self,
        change_id: &ChangeId,
        peer_id: &DeviceId,
        state: TargetState,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE change_targets SET state = ?3, last_error = ?4, attempts = attempts + 1
             WHERE change_id = ?1 AND peer_id = ?2",
            params![
                change_id.to_string(),
                peer_id.to_string(),
                state.as_str(),
                error
            ],
        )?;
        Ok(())
    }

    fn acknowledge_change(
        &mut self,
        change_id: &ChangeId,
        peer_id: &DeviceId,
        at: &str,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE change_targets SET state = 'delivered', acknowledged_at = ?3
             WHERE change_id = ?1 AND peer_id = ?2",
            params![change_id.to_string(), peer_id.to_string(), at],
        )?;
        Ok(())
    }

    fn insert_tombstone(&mut self, tombstone: &Tombstone) -> Result<(), StoreError> {
        let version = encode_version(&tombstone.version)?;
        self.conn().execute(
            "INSERT INTO tombstones (change_id, entry_id, folder_id, last_path, version, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(change_id) DO NOTHING",
            params![
                tombstone.change_id.to_string(),
                tombstone.entry_id.to_string(),
                tombstone.folder_id.to_string(),
                tombstone.last_path,
                version,
                tombstone.deleted_at,
            ],
        )?;
        Ok(())
    }

    fn list_tombstones(&self, folder_id: &FolderId) -> Result<Vec<Tombstone>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT change_id, entry_id, folder_id, last_path, version, deleted_at
             FROM tombstones WHERE folder_id = ?1 ORDER BY deleted_at",
        )?;
        let rows = stmt.query_map(params![folder_id.to_string()], |r| {
            Ok(Tombstone {
                change_id: r.get::<_, String>(0)?.parse().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(std::fmt::Error),
                    )
                })?,
                entry_id: r.get::<_, String>(1)?.parse().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(std::fmt::Error),
                    )
                })?,
                folder_id: r.get::<_, String>(2)?.parse().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(std::fmt::Error),
                    )
                })?,
                last_path: r.get(3)?,
                version: decode_version(&r.get::<_, String>(4)?).map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(std::fmt::Error),
                    )
                })?,
                deleted_at: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn get_tombstone(
        &self,
        folder_id: &FolderId,
        last_path: &str,
    ) -> Result<Option<Tombstone>, StoreError> {
        let mut found = None;
        for tombstone in self.list_tombstones(folder_id)? {
            if tombstone.last_path == last_path {
                found = Some(tombstone);
            }
        }
        Ok(found)
    }

    fn peer_sync_state(
        &self,
        folder_id: &FolderId,
        peer_id: &DeviceId,
    ) -> Result<Option<PeerSyncState>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT folder_id, peer_id, last_acknowledged_seq, last_synced_at
                 FROM peer_sync_state WHERE folder_id = ?1 AND peer_id = ?2",
                params![folder_id.to_string(), peer_id.to_string()],
                |r| {
                    Ok(PeerSyncState {
                        folder_id: r.get::<_, String>(0)?.parse().map_err(|_| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(std::fmt::Error),
                            )
                        })?,
                        peer_id: r.get::<_, String>(1)?.parse().map_err(|_| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Text,
                                Box::new(std::fmt::Error),
                            )
                        })?,
                        last_acknowledged_seq: r.get(2)?,
                        last_synced_at: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    fn set_peer_sync_state(&mut self, state: &PeerSyncState) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO peer_sync_state (folder_id, peer_id, last_acknowledged_seq, last_synced_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(folder_id, peer_id) DO UPDATE SET
               last_acknowledged_seq = ?3, last_synced_at = ?4",
            params![
                state.folder_id.to_string(),
                state.peer_id.to_string(),
                state.last_acknowledged_seq,
                state.last_synced_at,
            ],
        )?;
        Ok(())
    }

    fn upsert_transfer(&mut self, transfer: &Transfer) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO transfers (transfer_id, change_id, peer_id, direction, state, expected_hash, expected_size, acknowledged_bytes, temporary_path, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(transfer_id) DO UPDATE SET
               state = ?5, acknowledged_bytes = ?8, temporary_path = COALESCE(?9, temporary_path), updated_at = ?10",
            params![
                transfer.transfer_id,
                transfer.change_id.to_string(),
                transfer.peer_id.to_string(),
                transfer.direction.as_str(),
                transfer.state.as_str(),
                transfer.expected_hash.as_ref().map(|h| h.0.clone()),
                transfer.expected_size as i64,
                transfer.acknowledged_bytes as i64,
                transfer.temporary_path,
                transfer.updated_at,
            ],
        )?;
        Ok(())
    }

    fn get_transfer(&self, transfer_id: &str) -> Result<Option<Transfer>, StoreError> {
        let row = self.conn().query_row(
            "SELECT transfer_id, change_id, peer_id, direction, state, expected_hash, expected_size, acknowledged_bytes, temporary_path, updated_at
             FROM transfers WHERE transfer_id = ?1",
            params![transfer_id],
            |r| {
                Ok(Transfer {
                    transfer_id: r.get(0)?,
                    change_id: r.get::<_, String>(1)?.parse().map_err(|_| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(std::fmt::Error),
                        )
                    })?,
                    peer_id: r.get::<_, String>(2)?.parse().map_err(|_| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(std::fmt::Error),
                        )
                    })?,
                    direction: TransferDirection::parse(&r.get::<_, String>(3)?).ok_or(
                        invalid_col(3, "direction"),
                    )?,
                    state: TransferState::parse(&r.get::<_, String>(4)?).ok_or(
                        invalid_col(4, "state"),
                    )?,
                    expected_hash: r.get::<_, Option<String>>(5)?.map(ContentHash),
                    expected_size: r.get::<_, i64>(6)?.max(0) as u64,
                    acknowledged_bytes: r.get::<_, i64>(7)?.max(0) as u64,
                    temporary_path: r.get(8)?,
                    updated_at: r.get(9)?,
                })
            },
        ).optional()?;
        Ok(row)
    }

    fn get_transfer_for_change(
        &self,
        change_id: &ChangeId,
        direction: TransferDirection,
    ) -> Result<Option<Transfer>, StoreError> {
        let row = self.conn().query_row(
            "SELECT transfer_id, change_id, peer_id, direction, state, expected_hash, expected_size, acknowledged_bytes, temporary_path, updated_at
             FROM transfers WHERE change_id = ?1 AND direction = ?2
             ORDER BY updated_at DESC LIMIT 1",
            params![change_id.to_string(), direction.as_str()],
            |r| {
                Ok(Transfer {
                    transfer_id: r.get(0)?,
                    change_id: r.get::<_, String>(1)?.parse().map_err(|_| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(std::fmt::Error),
                        )
                    })?,
                    peer_id: r.get::<_, String>(2)?.parse().map_err(|_| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(std::fmt::Error),
                        )
                    })?,
                    direction: TransferDirection::parse(&r.get::<_, String>(3)?).ok_or(
                        invalid_col(3, "direction"),
                    )?,
                    state: TransferState::parse(&r.get::<_, String>(4)?).ok_or(
                        invalid_col(4, "state"),
                    )?,
                    expected_hash: r.get::<_, Option<String>>(5)?.map(ContentHash),
                    expected_size: r.get::<_, i64>(6)?.max(0) as u64,
                    acknowledged_bytes: r.get::<_, i64>(7)?.max(0) as u64,
                    temporary_path: r.get(8)?,
                    updated_at: r.get(9)?,
                })
            },
        ).optional()?;
        Ok(row)
    }

    fn save_checkpoint(
        &mut self,
        transfer_id: &str,
        acknowledged_bytes: u64,
        state: TransferState,
    ) -> Result<(), StoreError> {
        let now = syncengine_protocol::now_rfc3339();
        let n = self.conn().execute(
            "UPDATE transfers SET acknowledged_bytes = ?2, state = ?3, updated_at = ?4
             WHERE transfer_id = ?1",
            params![transfer_id, acknowledged_bytes as i64, state.as_str(), now],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("transfer {transfer_id}")));
        }
        Ok(())
    }

    fn try_mark_applied(
        &mut self,
        change_id: &ChangeId,
        origin_device: &DeviceId,
        at: &str,
    ) -> Result<bool, StoreError> {
        let n = self.conn().execute(
            "INSERT INTO applied_remote_changes (change_id, origin_device, applied_at)
             VALUES (?1, ?2, ?3) ON CONFLICT(change_id) DO NOTHING",
            params![change_id.to_string(), origin_device.to_string(), at],
        )?;
        Ok(n > 0)
    }

    fn is_applied(&self, change_id: &ChangeId) -> Result<bool, StoreError> {
        let found: Option<i64> = self
            .conn()
            .query_row(
                "SELECT 1 FROM applied_remote_changes WHERE change_id = ?1",
                params![change_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    fn insert_conflict(&mut self, conflict: &Conflict) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO conflicts (conflict_id, folder_id, entry_id, local_change_id, remote_change_id, status, resolution, created_at, resolved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                conflict.conflict_id,
                conflict.folder_id.to_string(),
                conflict.entry_id.to_string(),
                conflict.local_change_id.map(|c| c.to_string()),
                conflict.remote_change_id.map(|c| c.to_string()),
                conflict.status.as_str(),
                conflict.resolution,
                conflict.created_at,
                conflict.resolved_at,
            ],
        )?;
        Ok(())
    }

    fn list_conflicts(&self, status: Option<ConflictStatus>) -> Result<Vec<Conflict>, StoreError> {
        let (sql, param) = match status {
            Some(s) => (
                "SELECT conflict_id, folder_id, entry_id, local_change_id, remote_change_id, status, resolution, created_at, resolved_at
                 FROM conflicts WHERE status = ?1 ORDER BY created_at",
                Some(s.as_str().to_string()),
            ),
            None => (
                "SELECT conflict_id, folder_id, entry_id, local_change_id, remote_change_id, status, resolution, created_at, resolved_at
                 FROM conflicts ORDER BY created_at",
                None,
            ),
        };
        let mut stmt = self.conn().prepare(sql)?;
        let mapper = |r: &rusqlite::Row<'_>| {
            Ok(Conflict {
                conflict_id: r.get(0)?,
                folder_id: r.get::<_, String>(1)?.parse().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(std::fmt::Error),
                    )
                })?,
                entry_id: r.get::<_, String>(2)?.parse().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(std::fmt::Error),
                    )
                })?,
                local_change_id: r
                    .get::<_, Option<String>>(3)?
                    .map(|s| {
                        s.parse().map_err(|_| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(std::fmt::Error),
                            )
                        })
                    })
                    .transpose()?,
                remote_change_id: r
                    .get::<_, Option<String>>(4)?
                    .map(|s| {
                        s.parse().map_err(|_| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                Box::new(std::fmt::Error),
                            )
                        })
                    })
                    .transpose()?,
                status: ConflictStatus::parse(&r.get::<_, String>(5)?)
                    .ok_or(invalid_col(5, "status"))?,
                resolution: r.get(6)?,
                created_at: r.get(7)?,
                resolved_at: r.get(8)?,
            })
        };
        let rows = match param {
            Some(p) => stmt
                .query_map(params![p], mapper)?
                .collect::<Result<Vec<_>, _>>()?,
            None => stmt.query_map([], mapper)?.collect::<Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    fn resolve_conflict(
        &mut self,
        conflict_id: &str,
        status: ConflictStatus,
        resolution: Option<&str>,
        at: &str,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE conflicts SET status = ?2, resolution = ?3, resolved_at = ?4
             WHERE conflict_id = ?1",
            params![conflict_id, status.as_str(), resolution, at],
        )?;
        Ok(())
    }

    fn put_content_object(&mut self, object: &ContentObject) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO content_objects (content_hash, size, staged_path, refcount, created_at, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(content_hash) DO UPDATE SET staged_path = ?3",
            params![
                object.content_hash.0,
                object.size as i64,
                object.staged_path,
                object.refcount,
                object.created_at,
                object.verified_at,
            ],
        )?;
        Ok(())
    }

    fn get_content_object(&self, hash: &ContentHash) -> Result<Option<ContentObject>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT content_hash, size, staged_path, refcount, created_at, verified_at
             FROM content_objects WHERE content_hash = ?1",
                params![hash.0.clone()],
                |r| {
                    Ok(ContentObject {
                        content_hash: ContentHash(r.get(0)?),
                        size: r.get::<_, i64>(1)?.max(0) as u64,
                        staged_path: r.get(2)?,
                        refcount: r.get(3)?,
                        created_at: r.get(4)?,
                        verified_at: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    fn bump_content_refcount(&mut self, hash: &ContentHash, delta: i64) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE content_objects SET refcount = MAX(refcount + ?2, 0) WHERE content_hash = ?1",
            params![hash.0.clone(), delta],
        )?;
        Ok(())
    }

    fn list_content_objects(&self) -> Result<Vec<ContentObject>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT content_hash, size, staged_path, refcount, created_at, verified_at FROM content_objects",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ContentObject {
                content_hash: ContentHash(r.get(0)?),
                size: r.get::<_, i64>(1)?.max(0) as u64,
                staged_path: r.get(2)?,
                refcount: r.get(3)?,
                created_at: r.get(4)?,
                verified_at: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn retire_tombstones(&mut self) -> Result<usize, StoreError> {
        let retired = self.conn().execute(
            "DELETE FROM tombstones
             WHERE change_id IN (
               SELECT t.change_id FROM tombstones t
               WHERE NOT EXISTS (
                 SELECT 1 FROM folder_peers fp
                 JOIN peers p ON p.device_id = fp.peer_id
                 WHERE fp.folder_id = t.folder_id AND p.trust = 'trusted'
                   AND NOT EXISTS (
                     SELECT 1 FROM change_targets ct
                     WHERE ct.change_id = t.change_id AND ct.peer_id = fp.peer_id
                       AND ct.state = 'delivered' AND ct.acknowledged_at IS NOT NULL
                   )
               )
             )",
            [],
        )?;
        Ok(retired)
    }

    fn pending_content_hashes(&self) -> Result<Vec<ContentHash>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT DISTINCT c.content_hash
             FROM changes c
             JOIN change_targets t ON t.change_id = c.change_id
             WHERE t.state != 'delivered' AND c.content_hash IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(ContentHash(row?));
        }
        Ok(out)
    }

    fn delete_content_object(&mut self, hash: &ContentHash) -> Result<(), StoreError> {
        self.conn().execute(
            "DELETE FROM content_objects WHERE content_hash = ?1",
            params![hash.as_str()],
        )?;
        Ok(())
    }

    fn max_change_seq(&self) -> Result<i64, StoreError> {
        let seq: i64 =
            self.conn()
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM changes", [], |r| {
                    r.get(0)
                })?;
        Ok(seq)
    }
}
