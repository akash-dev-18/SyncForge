use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use syncengine_server::db;
use syncengine_server::{AppState, api};
use tokio::net::TcpListener;

/// Command-line interface of the SyncEngine coordination and relay server.
#[derive(Debug, Parser)]
#[command(
    name = "syncengine-server",
    version,
    about = "SyncEngine coordination API and encrypted relay",
    long_about = "Coordinates paired devices and forwards end-to-end-protected payloads.\n\nThe server stores metadata only; it never persistently stores file contents."
)]
struct Cli {
    /// Increase log verbosity on stderr (-v: debug, -vv: trace; RUST_LOG wins).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,

    /// Write logs as newline-delimited JSON to stderr.
    #[arg(long, global = true)]
    json_logs: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the coordination API and encrypted relay.
    Serve {
        /// Address to bind, for example 127.0.0.1:8443 or 0.0.0.0:8443.
        #[arg(long, default_value = "127.0.0.1:8443")]
        bind: SocketAddr,

        /// PostgreSQL connection URL (env: DATABASE_URL).
        #[arg(long, env = "DATABASE_URL")]
        database_url: Option<String>,
    },

    /// Apply PostgreSQL database migrations.
    Migrate {
        /// PostgreSQL connection URL (env: DATABASE_URL).
        #[arg(long, env = "DATABASE_URL")]
        database_url: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let Some(command) = cli.command.as_ref() else {
        let mut help = Cli::command();
        let _ = help.print_help();
        return ExitCode::SUCCESS;
    };

    init_logging(cli.verbose, cli.json_logs);

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    let result = match command {
        Command::Serve { bind, database_url } => {
            runtime.block_on(serve(*bind, database_url.as_deref()))
        }
        Command::Migrate { database_url } => runtime.block_on(migrate(database_url.as_deref())),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn require_database_url(database_url: Option<&str>) -> anyhow::Result<String> {
    database_url.map(str::to_string).ok_or_else(|| {
        anyhow::anyhow!(
            "no database configured: pass --database-url or set DATABASE_URL \
             (for local development: docker compose up -d postgres)"
        )
    })
}

/// Connects to PostgreSQL and applies migrations. The URL is never logged:
/// it contains credentials.
async fn connect_and_migrate(database_url: Option<&str>) -> anyhow::Result<db::PgPool> {
    let url = require_database_url(database_url)?;
    let pool = db::pool_from_url(&url)?;
    let applied = db::run_migrations(&pool).await?;
    if applied > 0 {
        println!("Applied {applied} migration(s).");
    }
    Ok(pool)
}

async fn migrate(database_url: Option<&str>) -> anyhow::Result<()> {
    let _pool = connect_and_migrate(database_url).await?;
    println!("Migrations applied.");
    Ok(())
}

async fn serve(bind: SocketAddr, database_url: Option<&str>) -> anyhow::Result<()> {
    let pool = connect_and_migrate(database_url).await?;

    println!("Listening on http://{bind}");
    println!("Note: terminate TLS in front of this server; see docs/STATUS.md.");

    tokio::spawn(syncengine_server::relay::cleanup_loop(
        pool.clone(),
        Duration::from_secs(60),
    ));

    let listener = TcpListener::bind(bind)
        .await
        .map_err(|error| anyhow::anyhow!("failed to bind {bind}: {error}"))?;
    tracing::info!(%bind, "serving the control-plane API");

    axum::serve(listener, api::app(AppState::new(pool)))
        .await
        .map_err(|error| anyhow::anyhow!("server error: {error}"))?;
    Ok(())
}

fn init_logging(verbose: u8, json_logs: bool) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_serve_with_custom_bind() {
        let cli = Cli::try_parse_from(["syncengine-server", "serve", "--bind", "0.0.0.0:9000"])
            .expect("parse");
        match cli.command {
            Some(Command::Serve { bind, .. }) => {
                assert_eq!(bind.port(), 9000);
                assert!(bind.ip().is_unspecified());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_bind_address() {
        let err = Cli::try_parse_from(["syncengine-server", "serve", "--bind", "not-an-addr"])
            .unwrap_err();
        assert!(err.to_string().contains("not-an-addr"), "got: {err}");
    }

    #[test]
    fn migrate_accepts_database_url() {
        let cli = Cli::try_parse_from([
            "syncengine-server",
            "migrate",
            "--database-url",
            "postgres://localhost/syncengine",
        ])
        .expect("parse");
        match cli.command {
            Some(Command::Migrate { database_url }) => {
                assert_eq!(
                    database_url.as_deref(),
                    Some("postgres://localhost/syncengine")
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn serve_accepts_database_url() {
        let cli = Cli::try_parse_from([
            "syncengine-server",
            "serve",
            "--database-url",
            "postgres://localhost/syncengine",
        ])
        .expect("parse");
        match cli.command {
            Some(Command::Serve { database_url, .. }) => {
                assert!(database_url.is_some());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn missing_database_url_is_an_explicit_error() {
        let err = require_database_url(None).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("DATABASE_URL"), "got: {text}");
        assert!(text.contains("docker compose"), "got: {text}");
    }

    #[test]
    fn help_lists_serve_and_migrate() {
        let err = Cli::try_parse_from(["syncengine-server", "--help"]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("serve"), "got: {text}");
        assert!(text.contains("migrate"), "got: {text}");
        assert!(
            text.contains("never persistently stores file contents"),
            "got: {text}"
        );
    }

    #[test]
    fn bare_invocation_has_no_command() {
        let cli = Cli::try_parse_from(["syncengine-server"]).expect("parse");
        assert!(cli.command.is_none());
    }
}
