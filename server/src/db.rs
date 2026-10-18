//! PostgreSQL connection pooling and versioned migrations.
//!
//! Migrations are plain SQL files embedded at compile time and tracked in a
//! `schema_migrations` table, mirroring the client's approach.

use std::collections::HashSet;
use std::str::FromStr;

use anyhow::Context;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use tokio_postgres::NoTls;

/// The shared connection pool type.
pub type PgPool = Pool;

/// Embedded, ordered migrations: `(version, sql)`.
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_control_plane",
        include_str!("../migrations/0001_control_plane.sql"),
    ),
    (
        "0002_device_encryption_key",
        include_str!("../migrations/0002_device_encryption_key.sql"),
    ),
];

/// Builds a connection pool from a PostgreSQL URL.
///
/// The URL is never logged: it contains credentials.
pub fn pool_from_url(database_url: &str) -> anyhow::Result<PgPool> {
    let config = tokio_postgres::Config::from_str(database_url).context("invalid database URL")?;
    let manager = Manager::from_config(
        config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(10)
        .build()
        .context("failed to build the connection pool")
}

/// Applies all pending migrations and returns how many ran.
pub async fn run_migrations(pool: &PgPool) -> anyhow::Result<usize> {
    let mut client = pool
        .get()
        .await
        .context("failed to connect to PostgreSQL for migrations")?;
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations ( \
               version TEXT PRIMARY KEY, \
               applied_at TIMESTAMPTZ NOT NULL DEFAULT now() \
             )",
        )
        .await
        .context("failed to create schema_migrations")?;

    let applied: HashSet<String> = client
        .query("SELECT version FROM schema_migrations", &[])
        .await
        .context("failed to read applied migrations")?
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect();

    let mut applied_now = 0;
    for (version, sql) in MIGRATIONS.iter().copied() {
        if applied.contains(version) {
            continue;
        }
        let tx = client.transaction().await?;
        tx.batch_execute(sql)
            .await
            .with_context(|| format!("migration {version} failed"))?;
        tx.execute(
            "INSERT INTO schema_migrations (version) VALUES ($1)",
            &[&version],
        )
        .await?;
        tx.commit().await?;
        tracing::info!(version, "applied migration");
        applied_now += 1;
    }
    Ok(applied_now)
}

/// Formats a timestamp the way the API returns it.
pub fn to_rfc3339(timestamp: time::OffsetDateTime) -> String {
    timestamp
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| timestamp.unix_timestamp().to_string())
}
