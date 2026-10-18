use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};

/// Command-line interface of the `syncengine` binary.
#[derive(Debug, Parser)]
#[command(
    name = "syncengine",
    version,
    about = "SyncEngine — cross-platform file synchronization",
    long_about = "Synchronize folders between your own paired, trusted devices.\n\nEvery command has its own help: `syncengine <command> --help`."
)]
pub struct Cli {
    /// Path to the configuration file (env: SYNCENGINE_CONFIG).
    #[arg(
        short,
        long,
        global = true,
        env = "SYNCENGINE_CONFIG",
        value_name = "FILE"
    )]
    pub config: Option<PathBuf>,

    /// Increase log verbosity on stderr (-v: debug, -vv: trace; RUST_LOG wins).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Write logs as newline-delimited JSON to stderr.
    #[arg(long, global = true)]
    pub json_logs: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommands accepted by `syncengine`.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the configuration directory and write a default config file.
    Init(InitArgs),

    /// Pair this device with another device using a pairing code.
    Pair(PairArgs),

    /// Remove trust for a paired device.
    Unpair {
        /// Device id to unpair (as shown by `devices`).
        device_id: String,
    },

    /// List known devices and their trust state.
    Devices,

    /// Share a local folder with paired devices.
    Share {
        /// Local directory to share.
        path: PathBuf,

        /// Join an existing folder announced on another device
        /// (the folder id printed by `share` or `status` there).
        #[arg(long, value_name = "FOLDER_ID")]
        join: Option<String>,
    },

    /// Stop sharing a folder.
    Unshare {
        /// Logical folder identifier shown by `status`/`devices`.
        folder_id: String,
    },

    /// Show device, folder, transfer, and conflict status.
    Status,

    /// Run a synchronization pass now.
    Sync,

    /// Manage the background synchronization agent.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// List conflicts, or resolve one by id.
    Conflicts(ConflictsArgs),

    /// Show recent agent logs.
    Logs(LogsArgs),

    /// Diagnose common configuration and connectivity problems.
    Doctor(DoctorArgs),
}

/// Arguments of the `logs` subcommand.
#[derive(Debug, Args)]
pub struct LogsArgs {
    /// How many lines from the end of the log to print.
    #[arg(long, default_value_t = 50, value_name = "N")]
    pub lines: usize,

    /// Print the path of the log file instead of its contents.
    #[arg(long)]
    pub path_only: bool,
}

/// Arguments of the `doctor` subcommand.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Skip the coordination-server reachability check.
    #[arg(long)]
    pub skip_server: bool,
}

/// Actions of the `daemon` subcommand.
#[derive(Debug, Subcommand)]
pub enum DaemonAction {
    /// Start the background agent.
    Start,
    /// Show whether the background agent is running.
    Status,
    /// Stop the background agent.
    Stop,
    /// Run the agent in the foreground (used by `start`).
    #[command(hide = true)]
    Run,
}

/// Arguments of the `conflicts` subcommand.
#[derive(Debug, Args)]
pub struct ConflictsArgs {
    #[command(subcommand)]
    pub action: Option<ConflictsAction>,
}

/// Actions of the `conflicts` subcommand.
#[derive(Debug, Subcommand)]
pub enum ConflictsAction {
    /// Resolve one conflict by id (as listed by `conflicts`).
    Resolve(ResolveArgs),
}

/// Arguments of `conflicts resolve`: exactly one resolution action.
#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("action")
        .required(true)
        .multiple(false)
        .args(["keep_local", "keep_remote", "dismiss"])
))]
pub struct ResolveArgs {
    /// Conflict id shown by `conflicts`.
    pub conflict_id: String,

    /// Keep the current shared-path revision; the preserved copy stays on disk.
    #[arg(long)]
    pub keep_local: bool,

    /// Overwrite the shared path with the preserved remote revision.
    #[arg(long)]
    pub keep_remote: bool,

    /// Mark the conflict dismissed without touching any files.
    #[arg(long)]
    pub dismiss: bool,
}

/// Arguments of the `pair` subcommand.
#[derive(Debug, Args)]
pub struct PairArgs {
    /// Device id of the device to pair with (initiator side).
    #[arg(value_name = "DEVICE_ID", conflicts_with = "approve")]
    pub device_id: Option<String>,

    /// Approve an incoming pairing request with this request id.
    #[arg(long, value_name = "REQUEST_ID")]
    pub approve: Option<String>,

    /// The pairing code shown by the initiating device.
    #[arg(long, value_name = "CODE", requires = "approve")]
    pub code: Option<String>,
}

/// Arguments of the `init` subcommand.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Device name to record (defaults to this machine's hostname).
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Replace an existing configuration file with defaults.
    #[arg(long)]
    pub force: bool,
}

impl Command {
    /// The name used in help text and error messages.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Init(_) => "init",
            Command::Pair(_) => "pair",
            Command::Unpair { .. } => "unpair",
            Command::Devices => "devices",
            Command::Share { .. } => "share",
            Command::Unshare { .. } => "unshare",
            Command::Status => "status",
            Command::Sync => "sync",
            Command::Daemon { .. } => "daemon",
            Command::Conflicts(_) => "conflicts",
            Command::Logs(_) => "logs",
            Command::Doctor(_) => "doctor",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_invocation_has_no_command() {
        let cli = Cli::try_parse_from(["syncengine"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.verbose, 0);
    }

    #[test]
    fn parses_global_flags_on_subcommand() {
        let cli = Cli::try_parse_from(["syncengine", "-vv", "--json-logs", "status"]).unwrap();
        assert_eq!(cli.verbose, 2);
        assert!(cli.json_logs);
        assert!(matches!(cli.command, Some(Command::Status)));
    }

    #[test]
    fn parses_share_path_and_unshare_id() {
        let cli = Cli::try_parse_from(["syncengine", "share", "/data/photos"]).unwrap();
        match cli.command {
            Some(Command::Share { path, join }) => {
                assert_eq!(path, PathBuf::from("/data/photos"));
                assert!(join.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        let cli = Cli::try_parse_from(["syncengine", "unshare", "f-123"]).unwrap();
        match cli.command {
            Some(Command::Unshare { folder_id }) => assert_eq!(folder_id, "f-123"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_share_join_and_pair_forms() {
        let cli = Cli::try_parse_from([
            "syncengine",
            "share",
            "/data/photos",
            "--join",
            "11111111-2222-3333-4444-555555555555",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Share { join, .. }) => {
                assert_eq!(
                    join.as_deref(),
                    Some("11111111-2222-3333-4444-555555555555")
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from(["syncengine", "pair", "abc"]).unwrap();
        match cli.command {
            Some(Command::Pair(args)) => {
                assert_eq!(args.device_id.as_deref(), Some("abc"));
                assert!(args.approve.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "syncengine",
            "pair",
            "--approve",
            "req-1",
            "--code",
            "123456",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Pair(args)) => {
                assert_eq!(args.approve.as_deref(), Some("req-1"));
                assert_eq!(args.code.as_deref(), Some("123456"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn pair_code_requires_approve_and_approve_conflicts_with_device_id() {
        let err =
            Cli::try_parse_from(["syncengine", "pair", "abc", "--approve", "req"]).unwrap_err();
        assert!(err.to_string().contains("cannot be used"), "got: {err}");

        let err = Cli::try_parse_from(["syncengine", "pair", "--code", "123456"]).unwrap_err();
        assert!(err.to_string().contains("--approve"), "got: {err}");
    }

    #[test]
    fn share_requires_a_path() {
        let err = Cli::try_parse_from(["syncengine", "share"]).unwrap_err();
        assert!(err.to_string().contains("required"));
    }

    #[test]
    fn help_lists_the_main_commands() {
        let err = Cli::try_parse_from(["syncengine", "--help"]).unwrap_err();
        let text = err.to_string();
        for command in ["init", "pair", "share", "status", "daemon", "doctor"] {
            assert!(text.contains(command), "help missing {command:?}:\n{text}");
        }
    }

    #[test]
    fn daemon_actions_are_parsed() {
        let cli = Cli::try_parse_from(["syncengine", "daemon", "stop"]).unwrap();
        match cli.command {
            Some(Command::Daemon {
                action: DaemonAction::Stop,
            }) => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn conflicts_resolve_requires_exactly_one_action() {
        let err = Cli::try_parse_from(["syncengine", "conflicts", "resolve", "cf-1"]).unwrap_err();
        assert!(err.to_string().contains("required arguments"), "got: {err}");

        let err = Cli::try_parse_from([
            "syncengine",
            "conflicts",
            "resolve",
            "cf-1",
            "--keep-local",
            "--dismiss",
        ])
        .unwrap_err();
        assert!(err.to_string().contains("cannot be used"), "got: {err}");

        let cli = Cli::try_parse_from([
            "syncengine",
            "conflicts",
            "resolve",
            "cf-1",
            "--keep-remote",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Conflicts(ConflictsArgs {
                action: Some(ConflictsAction::Resolve(args)),
            })) => {
                assert_eq!(args.conflict_id, "cf-1");
                assert!(args.keep_remote);
                assert!(!args.keep_local && !args.dismiss);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from(["syncengine", "conflicts"]).unwrap();
        match cli.command {
            Some(Command::Conflicts(ConflictsArgs { action: None })) => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
