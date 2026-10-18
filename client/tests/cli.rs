//! End-to-end tests that spawn the real `syncengine` binary.

use std::process::Command;

use tempfile::tempdir;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_syncengine"))
}

#[test]
fn help_prints_usage_and_lists_commands() {
    let out = bin().arg("--help").output().expect("spawn binary");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage:"), "got:\n{stdout}");
    for command in ["init", "pair", "share", "status", "daemon", "doctor"] {
        assert!(
            stdout.contains(command),
            "help missing {command:?}:\n{stdout}"
        );
    }
}

#[test]
fn bare_invocation_shows_help_with_exit_zero() {
    let out = bin().output().expect("spawn binary");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage:"), "got:\n{stdout}");
}

#[test]
fn version_prints_package_version() {
    let out = bin().arg("--version").output().expect("spawn binary");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("syncengine 0.1.0"), "got:\n{stdout}");
}

#[test]
fn subcommand_help_shows_arguments() {
    let out = bin()
        .args(["share", "--help"])
        .output()
        .expect("spawn binary");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.to_lowercase().contains("path"), "got:\n{stdout}");
}

#[test]
fn missing_required_argument_is_a_usage_error() {
    let out = bin().arg("share").output().expect("spawn binary");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("required"), "got:\n{stderr}");
}

#[test]
fn logs_reports_a_missing_file_and_then_shows_agent_output() {
    let dir = tempdir().expect("tempdir");
    let config = temp_config(&dir);

    // Before the agent ever ran, `logs` explains what to do instead of
    // pretending the log is empty.
    let out = run(&config, &["logs"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("daemon start"), "got:\n{stderr}");

    let log_path = dir.path().join("data").join("daemon.log");
    std::fs::create_dir_all(log_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&log_path, b"").expect("write");
    let out = run(&config, &["logs"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("is empty"), "got:\n{stdout}");

    let lines: String = (1..=120)
        .map(|index| format!("log line {index}\n"))
        .collect();
    std::fs::write(&log_path, lines).expect("write");
    let out = run(&config, &["logs", "--lines", "5"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("lines 116-120 of 120"), "got:\n{stdout}");
    assert!(stdout.contains("log line 120"), "got:\n{stdout}");
    assert!(!stdout.contains("log line 115"), "got:\n{stdout}");

    let out = run(&config, &["logs", "--path-only"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.trim().ends_with("daemon.log"), "got:\n{stdout}");
}

#[test]
fn doctor_checks_configuration_state_and_reports_findings() {
    let dir = tempdir().expect("tempdir");
    let config = temp_config(&dir);

    // No shared folder yet: doctor still succeeds but says so.
    let out = run(&config, &["doctor", "--skip-server"]);
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("SyncEngine doctor"), "got:\n{stdout}");
    assert!(stdout.contains("data directory"), "got:\n{stdout}");
    assert!(
        stdout.contains("no `server_url` configured"),
        "got:\n{stdout}"
    );
    assert!(stdout.contains("background agent"), "got:\n{stdout}");
    assert!(stdout.contains("daemon start"), "got:\n{stdout}");

    // With a shared folder the local state checks pass.
    let root = dir.path().join("docs");
    std::fs::create_dir_all(&root).expect("mkdir");
    assert!(
        run(&config, &["share", root.to_str().expect("utf-8")])
            .status
            .success()
    );
    let out = run(&config, &["doctor", "--skip-server"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("local database"), "got:\n{stdout}");
    assert!(stdout.contains("1 configured"), "got:\n{stdout}");
    assert!(stdout.contains("0 trusted of 0"), "got:\n{stdout}");

    // A broken configuration is a hard failure with a clear message.
    let broken = dir.path().join("broken.toml");
    std::fs::write(&broken, b"this is not toml = = =\n").expect("write");
    let out = bin()
        .args(["--config", broken.to_str().expect("utf-8"), "doctor"])
        .output()
        .expect("spawn binary");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("FAILED"), "got:\n{stdout}");
    assert!(stdout.contains("syncengine init"), "got:\n{stdout}");
}

#[test]
fn init_creates_config_and_is_idempotent() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("nested").join("config.toml");
    let path_str = path.to_str().expect("utf-8 path");

    let first = bin()
        .args(["--config", path_str, "init", "--name", "alice"])
        .output()
        .expect("spawn binary");
    assert!(
        first.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(path.exists());
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(stdout.contains("alice"), "got:\n{stdout}");

    let config = syncengine_client::config::load(&path).expect("load written config");
    assert_eq!(config.device_name, "alice");

    let second = bin()
        .args(["--config", path_str, "init", "--name", "bob"])
        .output()
        .expect("spawn binary");
    assert!(second.status.success());
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("already exists"), "got:\n{stdout}");
    let config = syncengine_client::config::load(&path).expect("load");
    assert_eq!(config.device_name, "alice", "existing config must be kept");

    let forced = bin()
        .args(["--config", path_str, "init", "--name", "bob", "--force"])
        .output()
        .expect("spawn binary");
    assert!(forced.status.success());
    let config = syncengine_client::config::load(&path).expect("load");
    assert_eq!(config.device_name, "bob", "--force must replace the file");
}

#[test]
fn status_reports_configuration_and_store_state() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let config = syncengine_client::config::Config {
        device_name: "alice".to_string(),
        data_dir: dir.path().join("data"),
        ..syncengine_client::config::Config::default()
    };
    syncengine_client::config::save(&path, &config).expect("save");

    let out = bin()
        .args(["--config", path.to_str().expect("utf-8"), "status"])
        .output()
        .expect("spawn binary");
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("alice"), "got:\n{stdout}");
    assert!(stdout.contains("syncengine.db"), "got:\n{stdout}");
    assert!(stdout.contains("not initialized"), "got:\n{stdout}");
    assert!(stdout.contains("v1"), "got:\n{stdout}");
    assert!(!stdout.contains("not yet implemented"), "got:\n{stdout}");
    assert!(stdout.contains("doctor"), "got:\n{stdout}");
}

#[test]
fn status_without_config_falls_back_to_defaults() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("missing.toml");
    let out = bin()
        .args(["-c", path.to_str().expect("utf-8"), "status"])
        .output()
        .expect("spawn binary");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("using built-in defaults"), "got:\n{stdout}");
    assert!(stdout.contains("not initialized"), "got:\n{stdout}");
}

#[test]
fn status_rejects_invalid_config() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "log_level = \"loud\"\n").expect("write");
    let out = bin()
        .args(["-c", path.to_str().expect("utf-8"), "status"])
        .output()
        .expect("spawn binary");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("log_level"), "got:\n{stderr}");
}

#[test]
fn verbose_flag_emits_debug_logs_on_stderr() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let out = bin()
        .args(["-v", "--config", path.to_str().expect("utf-8"), "status"])
        .env_remove("RUST_LOG")
        .output()
        .expect("spawn binary");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("DEBUG"), "got:\n{stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("SyncEngine status"),
        "logs must not pollute stdout"
    );
}

#[test]
fn json_logs_are_newline_delimited_json_on_stderr() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let out = bin()
        .args([
            "-v",
            "--json-logs",
            "--config",
            path.to_str().expect("utf-8"),
            "status",
        ])
        .env_remove("RUST_LOG")
        .output()
        .expect("spawn binary");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stderr.lines().next().expect("at least one log line");
    assert!(line.contains("\"level\":\"DEBUG\""), "got:\n{stderr}");
}

fn temp_config(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("config.toml");
    let config = syncengine_client::config::Config {
        device_name: "alice".to_string(),
        data_dir: dir.path().join("data"),
        ..syncengine_client::config::Config::default()
    };
    syncengine_client::config::save(&path, &config).expect("save config");
    path
}

fn run(config: &std::path::Path, args: &[&str]) -> std::process::Output {
    bin()
        .args(["--config", config.to_str().expect("utf-8")])
        .args(args)
        .output()
        .expect("spawn binary")
}

#[test]
fn share_then_status_lists_folder_and_devices_shows_identity() {
    let dir = tempdir().expect("tempdir");
    let config = temp_config(&dir);
    let root = dir.path().join("docs");
    std::fs::create_dir_all(&root).expect("mkdir");

    let out = run(&config, &["share", root.to_str().expect("utf-8")]);
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Shared"), "got:\n{stdout}");
    assert!(stdout.contains("folder id"), "got:\n{stdout}");

    let out = run(&config, &["share", root.to_str().expect("utf-8")]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Already shared"), "got:\n{stdout}");

    let out = run(&config, &["status"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("present"), "got:\n{stdout}");
    assert!(stdout.contains("docs"), "got:\n{stdout}");
    assert!(stdout.contains("shared folders   : 1"), "got:\n{stdout}");
    assert!(!stdout.contains("not initialized"), "got:\n{stdout}");

    let out = run(&config, &["devices"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("This device"), "got:\n{stdout}");
    assert!(stdout.contains("No paired devices yet."), "got:\n{stdout}");
}

#[test]
fn unshare_removes_folder_but_keeps_files() {
    let dir = tempdir().expect("tempdir");
    let config = temp_config(&dir);
    let root = dir.path().join("docs");
    std::fs::create_dir_all(&root).expect("mkdir");
    std::fs::write(root.join("keep.txt"), b"keep").expect("write");

    let out = run(&config, &["share", root.to_str().expect("utf-8")]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let folder_id = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("folder id : "))
        .expect("folder id line")
        .to_string();

    let out = run(&config, &["unshare", &folder_id]);
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(root.join("keep.txt").exists(), "files must be untouched");

    let out = run(&config, &["unshare", &folder_id]);
    assert_eq!(out.status.code(), Some(1));
    let out = run(&config, &["unshare", "not-a-uuid"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn sync_journals_changes_and_conflicts_lists_none() {
    let dir = tempdir().expect("tempdir");
    let config = temp_config(&dir);
    let root = dir.path().join("docs");
    std::fs::create_dir_all(&root).expect("mkdir");
    std::fs::write(root.join("a.txt"), b"alpha").expect("write");
    assert!(
        run(&config, &["share", root.to_str().expect("utf-8")])
            .status
            .success()
    );

    let out = run(&config, &["sync"]);
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("created 1"), "got:\n{stdout}");
    assert!(stdout.contains("No paired devices yet"), "got:\n{stdout}");

    let out = run(&config, &["conflicts"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No conflicts."), "got:\n{stdout}");
}

#[test]
fn sync_without_folders_explains_the_next_step() {
    let dir = tempdir().expect("tempdir");
    let config = temp_config(&dir);
    let out = run(&config, &["sync"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("No shared folders"), "got:\n{stdout}");
}

#[test]
fn conflicts_lists_and_resolves_a_seeded_conflict() {
    use syncengine_storage::{Conflict, ConflictStatus, SqliteStore, Store};

    let dir = tempdir().expect("tempdir");
    let config = temp_config(&dir);
    let root = dir.path().join("docs");
    std::fs::create_dir_all(&root).expect("mkdir");
    assert!(
        run(&config, &["share", root.to_str().expect("utf-8")])
            .status
            .success()
    );

    let loaded = syncengine_client::config::load(&config).expect("load config");
    let mut store = SqliteStore::open(&loaded.database_path()).expect("open store");
    let folder = store.list_folders().expect("folders").remove(0);
    let conflict_id = "cf-cli-test".to_string();
    store
        .insert_conflict(&Conflict {
            conflict_id: conflict_id.clone(),
            folder_id: folder.folder_id,
            entry_id: syncengine_protocol::EntryId::new(),
            local_change_id: None,
            remote_change_id: None,
            status: ConflictStatus::Unresolved,
            resolution: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            resolved_at: None,
        })
        .expect("insert conflict");
    drop(store);

    let out = run(&config, &["conflicts"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&conflict_id), "got:\n{stdout}");
    assert!(stdout.contains("unresolved"), "got:\n{stdout}");

    // A resolution action is required: none and several are usage errors.
    let out = run(&config, &["conflicts", "resolve", &conflict_id]);
    assert_eq!(out.status.code(), Some(2), "missing action flag");
    let out = run(
        &config,
        &[
            "conflicts",
            "resolve",
            &conflict_id,
            "--keep-local",
            "--dismiss",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "mutually exclusive actions");

    let out = run(
        &config,
        &["conflicts", "resolve", &conflict_id, "--dismiss"],
    );
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("now dismissed"), "got:\n{stdout}");
    assert!(stdout.contains("No files were changed"), "got:\n{stdout}");

    let out = run(&config, &["conflicts"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Dismissed"), "got:\n{stdout}");
    assert!(stdout.contains("0 unresolved"), "got:\n{stdout}");

    // Unknown conflict ids fail loudly.
    let out = run(
        &config,
        &["conflicts", "resolve", "cf-missing", "--dismiss"],
    );
    assert_eq!(out.status.code(), Some(1));
}
