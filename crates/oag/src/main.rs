#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! `oag` — the gateway binary.

mod admin;
mod catalog;
mod settings;

use clap::{Parser, Subcommand};
use oag_core::Result;
use oag_server::AppState;
use oag_store::{Cache, Db};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "oag", version, about = "An internal AI gateway")]
struct Cli {
    /// Path to config.yaml. Optional: everything can come from the environment.
    #[arg(short, long, env = "OAG_CONFIG")]
    config: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the gateway.
    Serve,
    /// Apply migrations and exit.
    ///
    /// Safe to run from every replica at once: an advisory lock serialises
    /// them, so one applies and the rest wait and no-op.
    Migrate,
    /// Show the resolved configuration, with secrets redacted.
    ///
    /// The fastest way to answer "is this replica actually reading the
    /// environment variable I think it is".
    Config,
    /// Operator commands: principals, keys, credentials, catalog.
    /// Boxed: the nested noun-verb tree is large, and clap only ever holds one
    /// command. Leaving it inline blew past `large_enum_variant` on this enum.
    #[command(subcommand)]
    Admin(Box<admin::AdminCommand>),
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // Before telemetry is up this is the only channel there is, so
            // print as well as log rather than losing a config error to a
            // subscriber that was never installed.
            eprintln!("error: {e}");
            tracing::error!(error = %e, "fatal");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = settings::load(cli.config.as_deref())?;
    init_telemetry(&config);

    match cli.command {
        Command::Config => {
            // `Config`'s Debug redacts the secret fields by hand.
            println!("{config:#?}");
            Ok(())
        }
        Command::Migrate => {
            let db = Db::connect(&config.database.url, config.database.max_connections)?;
            db.migrate().await?;
            tracing::info!("migrations applied");
            Ok(())
        }
        Command::Admin(cmd) => {
            let db = Db::connect(&config.database.url, config.database.max_connections)?;
            let kek = oag_core::Kek::from_base64(&config.security.credential_kek)?;
            admin::run(*cmd, &db, &kek, &config.redis.url, &config).await
        }
        Command::Serve => {
            let handle = oag_server::metrics::install()?;
            oag_server::metrics::describe();

            let db = Db::connect(&config.database.url, config.database.max_connections)?;
            let cache = Cache::connect(&config.redis.url)?;

            let state = Arc::new(AppState::new(config, db, cache)?);
            state.lifecycle.set_metrics(handle);

            match state.reload_catalog().await {
                Ok(0) => tracing::warn!(
                    interval_secs = state.config.gateway.catalog_refresh_interval.as_secs(),
                    "model catalog is empty; every request will fail to route until it is \
                     seeded. Run `oag admin catalog seed` — the change is picked up on the \
                     refresh interval, no restart needed."
                ),
                Ok(n) => tracing::info!(models = n, "catalog loaded"),
                Err(e) => tracing::warn!(error = %e, "could not load catalog"),
            }

            // Report dependency health once at boot rather than waiting for the
            // first readiness probe: an operator watching the logs of a replica
            // that will never become ready should be told why immediately.
            let r = oag_store::readiness(&state.db, &state.cache).await;
            if r.ready {
                tracing::info!("postgres and redis reachable");
            } else {
                tracing::warn!(
                    database = r.database,
                    redis = r.redis,
                    "starting without all dependencies; readiness will fail until they recover"
                );
            }

            oag_server::serve(state).await
        }
    }
}

fn init_telemetry(config: &oag_core::config::Config) {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.telemetry.log_filter));

    if config.telemetry.log_json {
        fmt().json().with_env_filter(filter).init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}
