//! SyncEngine client library: CLI definitions, configuration loading, and
//! command implementations shared by the `syncengine` binary and its tests.

pub mod cli;
pub mod commands;
pub mod config;
pub mod daemon;
pub mod delivery;
pub mod remote;
pub mod state;

use anyhow::Result;
use clap::{CommandFactory, Parser};

use crate::cli::{Cli, Command};

/// Parses `args`, initializes logging, loads configuration per command, and
/// runs it.
///
/// `--help`/`--version` are handled inside clap and terminate the process
/// with their conventional exit codes; this function returns `Err` for
/// runtime failures and unimplemented commands (exit code 1).
pub fn run<I, T>(args: I) -> Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = Cli::try_parse_from(args).unwrap_or_else(|err| err.exit());

    let Some(command) = cli.command.as_ref() else {
        let mut help = Cli::command();
        help.print_help()?;
        return Ok(());
    };

    commands::init_logging(cli.verbose, cli.json_logs);

    match command {
        Command::Init(init_args) => commands::init(init_args, cli.config.as_deref()),
        Command::Status => commands::status(cli.config.as_deref()),
        Command::Share { path, join } => {
            commands::share(path, join.as_deref(), cli.config.as_deref())
        }
        Command::Unshare { folder_id } => commands::unshare(folder_id, cli.config.as_deref()),
        Command::Devices => commands::devices(cli.config.as_deref()),
        Command::Pair(pair_args) => commands::pair(pair_args, cli.config.as_deref()),
        Command::Unpair { device_id } => commands::unpair(device_id, cli.config.as_deref()),
        Command::Sync => commands::sync(cli.config.as_deref()),
        Command::Daemon { action } => commands::daemon(action, cli.config.as_deref()),
        Command::Conflicts(args) => commands::conflicts(cli.config.as_deref(), args),
        Command::Logs(args) => commands::logs(cli.config.as_deref(), args),
        Command::Doctor(args) => commands::doctor(cli.config.as_deref(), args),
    }
}
