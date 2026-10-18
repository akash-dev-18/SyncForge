//! Configuration loading for the SyncEngine client.
//!
//! Precedence: built-in defaults, then the TOML config file when present.
//! Unknown keys are rejected so typos fail loudly instead of being ignored.
//! The background agent honors `log_level` from this file; the CLI itself is
//! controlled by `-v/-vv` or `RUST_LOG`.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// File name of the SQLite database inside the data directory.
pub const DATABASE_FILE_NAME: &str = "syncengine.db";

/// Device name used when neither `--name` nor a hostname is available.
pub const FALLBACK_DEVICE_NAME: &str = "syncengine-device";

/// Maximum accepted `device_name` length.
const MAX_DEVICE_NAME_CHARS: usize = 64;

/// Default TCP port for direct LAN connections.
pub const DEFAULT_LAN_PORT: u16 = 41641;

const VALID_LOG_LEVELS: [&str; 6] = ["off", "error", "warn", "info", "debug", "trace"];

/// Root client configuration, mirroring `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Human-readable name shown to paired devices.
    pub device_name: String,
    /// Directory for local state (database, staging, keys).
    pub data_dir: PathBuf,
    /// Default log level of the background agent process.
    pub log_level: String,
    /// Base URL of the coordination server (`http://host:port`), if any.
    /// Without it, devices can still sync over the LAN but not over relays.
    pub server_url: Option<String>,
    /// TCP port advertised and bound for direct LAN connections.
    pub lan_port: u16,
    /// Background agent behavior.
    pub agent: AgentConfig,
    /// Resource limits applied by the transfer engine.
    pub limits: LimitsConfig,
}

/// Background agent settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// Seconds between full reconciliation scans of shared folders.
    pub scan_interval_secs: u64,
    /// Milliseconds to coalesce filesystem watcher events.
    pub debounce_ms: u64,
}

/// Transfer and staging limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Target chunk size for file transfers.
    pub chunk_size_bytes: u64,
    /// Maximum simultaneous transfers.
    pub max_concurrent_transfers: u32,
    /// Maximum bytes of staged immutable content kept on disk.
    pub staging_max_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: String::new(),
            data_dir: default_data_dir(),
            log_level: "info".to_string(),
            server_url: None,
            lan_port: DEFAULT_LAN_PORT,
            agent: AgentConfig::default(),
            limits: LimitsConfig::default(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            scan_interval_secs: 30,
            debounce_ms: 500,
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            chunk_size_bytes: 256 * 1024,
            max_concurrent_transfers: 4,
            staging_max_bytes: 1024 * 1024 * 1024,
        }
    }
}

impl Config {
    /// Path of the client SQLite database derived from `data_dir`.
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join(DATABASE_FILE_NAME)
    }

    /// Checks every field against its documented bounds.
    pub fn validate(&self) -> anyhow::Result<()> {
        let name = self.device_name.trim();
        if name.chars().count() > MAX_DEVICE_NAME_CHARS {
            bail!("device_name must be {MAX_DEVICE_NAME_CHARS} characters or fewer");
        }
        if !VALID_LOG_LEVELS.contains(&self.log_level.as_str()) {
            bail!(
                "log_level must be one of {} (got {})",
                VALID_LOG_LEVELS.join(", "),
                self.log_level
            );
        }
        if let Some(url) = &self.server_url {
            let trimmed = url.trim();
            if trimmed.is_empty() {
                bail!("server_url must be a URL like http://host:port, or removed entirely");
            }
            if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
                bail!("server_url must start with http:// or https:// (got {trimmed})");
            }
            if trimmed.chars().any(char::is_whitespace) {
                bail!("server_url must not contain whitespace (got {trimmed})");
            }
        }
        if self.lan_port < 1024 {
            bail!(
                "lan_port must be 1024 or higher (got {}); ports below are privileged",
                self.lan_port
            );
        }
        if self.agent.scan_interval_secs == 0 {
            bail!("agent.scan_interval_secs must be at least 1");
        }
        if self.agent.scan_interval_secs > 86_400 {
            bail!("agent.scan_interval_secs must not exceed 86400 (one day)");
        }
        if self.agent.debounce_ms > 60_000 {
            bail!("agent.debounce_ms must not exceed 60000");
        }
        if !(4_096..=16 * 1024 * 1024).contains(&self.limits.chunk_size_bytes) {
            bail!("limits.chunk_size_bytes must be between 4096 and 16777216");
        }
        if self.limits.max_concurrent_transfers == 0 {
            bail!("limits.max_concurrent_transfers must be at least 1");
        }
        if self.limits.max_concurrent_transfers > 64 {
            bail!("limits.max_concurrent_transfers must not exceed 64");
        }
        if self.limits.staging_max_bytes < self.limits.chunk_size_bytes {
            bail!("limits.staging_max_bytes must be at least limits.chunk_size_bytes");
        }
        Ok(())
    }
}

/// Resolves the configuration file path: explicit argument (which includes
/// values picked up from `SYNCENGINE_CONFIG` by clap) or the platform default.
pub fn resolve_path(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path)
}

/// Default configuration file location per platform.
pub fn default_config_path() -> PathBuf {
    if cfg!(windows) {
        windows_appdata().join("syncengine").join("config.toml")
    } else {
        config_home_from(
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        )
        .join("syncengine")
        .join("config.toml")
    }
}

/// Default data directory per platform.
pub fn default_data_dir() -> PathBuf {
    if cfg!(windows) {
        windows_appdata().join("syncengine")
    } else {
        data_home_from(
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        )
        .join("syncengine")
    }
}

/// Loads the configuration from `path`; a missing file yields defaults.
pub fn load(path: &Path) -> anyhow::Result<Config> {
    if !path.exists() {
        tracing::debug!(path = %path.display(), "config file not found, using built-in defaults");
        let config = Config::default();
        config.validate()?;
        return Ok(config);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config: Config = toml::from_str(&text)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;
    config
        .validate()
        .with_context(|| format!("invalid config file {}", path.display()))?;
    tracing::debug!(path = %path.display(), "loaded configuration");
    Ok(config)
}

/// Validates and writes `config` to `path`, creating parent directories.
pub fn save(path: &Path, config: &Config) -> anyhow::Result<()> {
    config.validate()?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }
    let text = toml::to_string_pretty(config).context("failed to serialize configuration")?;
    std::fs::write(path, text)
        .with_context(|| format!("failed to write config file {}", path.display()))?;
    Ok(())
}

fn config_home_from(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    xdg_config_home
        .or_else(|| home.map(|home| home.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn data_home_from(xdg_data_home: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    xdg_data_home
        .or_else(|| home.map(|home| home.join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn windows_appdata() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_config_is_valid_and_round_trips() {
        let config = Config::default();
        config.validate().expect("defaults must validate");
        let text = toml::to_string_pretty(&config).expect("serialize");
        let parsed: Config = toml::from_str(&text).expect("deserialize");
        assert_eq!(parsed, config);
    }

    #[test]
    fn partial_file_fills_missing_fields_from_defaults() {
        let parsed: Config = toml::from_str("device_name = \"bob\"").expect("parse");
        let expected = Config {
            device_name: "bob".to_string(),
            ..Config::default()
        };
        assert_eq!(parsed, expected);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Config>("device_nam = \"typo\"").unwrap_err();
        assert!(err.to_string().contains("device_nam"), "got: {err}");
    }

    #[test]
    fn validation_rejects_out_of_bounds_values() {
        let config = Config {
            limits: LimitsConfig {
                chunk_size_bytes: 10,
                ..LimitsConfig::default()
            },
            ..Config::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("chunk_size_bytes"), "got: {err}");

        let config = Config {
            log_level: "loud".to_string(),
            ..Config::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("log_level"), "got: {err}");

        let config = Config {
            agent: AgentConfig {
                scan_interval_secs: 0,
                ..AgentConfig::default()
            },
            ..Config::default()
        };
        assert!(config.validate().is_err());

        let config = Config {
            device_name: "x".repeat(MAX_DEVICE_NAME_CHARS + 1),
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validation_rejects_bad_server_url_and_privileged_lan_port() {
        let config = Config {
            server_url: Some("ftp://example.com".to_string()),
            ..Config::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("server_url"), "got: {err}");

        let config = Config {
            server_url: Some("http://host:8443".to_string()),
            ..Config::default()
        };
        config.validate().expect("well-formed http url");

        let config = Config {
            lan_port: 80,
            ..Config::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("lan_port"), "got: {err}");
    }

    #[test]
    fn server_url_survives_a_toml_round_trip() {
        let config = Config {
            server_url: Some("http://127.0.0.1:8443".to_string()),
            lan_port: 51999,
            ..Config::default()
        };
        let text = toml::to_string_pretty(&config).expect("serialize");
        let parsed: Config = toml::from_str(&text).expect("deserialize");
        assert_eq!(parsed, config);
    }

    #[test]
    fn save_and_load_round_trip_through_a_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("config.toml");
        let config = Config {
            device_name: "alice".to_string(),
            ..Config::default()
        };
        save(&path, &config).expect("save");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded, config);
    }

    #[test]
    fn load_of_missing_file_returns_defaults() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("missing.toml");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded, Config::default());
    }

    #[test]
    fn load_reports_parse_errors_with_the_file_name() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not = valid toml").expect("write");
        let err = load(&path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("failed to parse"), "got: {text}");
        assert!(text.contains("config.toml"), "got: {text}");
    }

    #[test]
    fn database_path_lives_inside_data_dir() {
        let config = Config {
            data_dir: PathBuf::from("/tmp/sync-data"),
            ..Config::default()
        };
        assert_eq!(
            config.database_path(),
            PathBuf::from("/tmp/sync-data").join(DATABASE_FILE_NAME)
        );
    }

    #[test]
    fn xdg_env_vars_override_home_fallbacks() {
        let home = Some(PathBuf::from("/home/alice"));
        assert_eq!(
            config_home_from(None, home.clone()),
            PathBuf::from("/home/alice/.config")
        );
        assert_eq!(
            data_home_from(None, home),
            PathBuf::from("/home/alice/.local/share")
        );
        assert_eq!(
            config_home_from(Some(PathBuf::from("/custom/cfg")), None),
            PathBuf::from("/custom/cfg")
        );
        assert_eq!(
            data_home_from(Some(PathBuf::from("/custom/data")), None),
            PathBuf::from("/custom/data")
        );
        assert_eq!(config_home_from(None, None), PathBuf::from("."));
        assert_eq!(data_home_from(None, None), PathBuf::from("."));
    }
}
