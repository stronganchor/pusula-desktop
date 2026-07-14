use std::{path::PathBuf, sync::Arc};

use clap::{Parser, Subcommand};
use pusula_backup_gateway::{
    build_gateway,
    config::{token_pepper_from_env, ServiceConfig, DEFAULT_DATABASE_PATH},
    db::Database,
    error::{AppError, Result},
};
use serde_json::json;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    #[arg(
        long,
        env = "PUSULA_GATEWAY_DATABASE",
        default_value = DEFAULT_DATABASE_PATH,
        global = true
    )]
    database: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run pending database migrations and exit.
    Migrate,
    /// Start the loopback HTTP service.
    Serve,
    /// Create a one-time enrollment code and print it once as JSON.
    IssueEnrollment {
        #[arg(long)]
        label: String,
        #[arg(long, default_value_t = 24)]
        expires_hours: u64,
    },
    /// Revoke an unused enrollment code by its public ID.
    RevokeEnrollment {
        #[arg(long)]
        enrollment_id: String,
    },
    /// Create a device credential directly and print its token once as JSON.
    IssueDevice {
        #[arg(long)]
        name: String,
    },
    /// Revoke a device and all future use of its token.
    RevokeDevice {
        #[arg(long)]
        device_id: String,
    },
}

#[tokio::main]
async fn main() {
    init_tracing();
    if let Err(error) = run(Cli::parse()).await {
        tracing::error!(error = %error, "command failed");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Migrate => {
            migration_database(cli.database).migrate()?;
            println!("{}", json!({ "status": "ok", "migration": "current" }));
        }
        Command::Serve => {
            let config = ServiceConfig::from_env(Some(cli.database))?;
            let bind = config.bind;
            let router = build_gateway(&config)?;
            let listener = tokio::net::TcpListener::bind(bind)
                .await
                .map_err(AppError::internal)?;
            tracing::info!(%bind, "Pusula backup gateway listening");
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown_signal())
                .await
                .map_err(AppError::internal)?;
        }
        Command::IssueEnrollment {
            label,
            expires_hours,
        } => {
            let database = admin_database(cli.database)?;
            database.migrate()?;
            let seconds = expires_hours
                .checked_mul(60 * 60)
                .ok_or(AppError::BadRequest("expires_hours is too large"))?;
            let credential = database.issue_enrollment(&label, seconds)?;
            println!(
                "{}",
                serde_json::to_string(&credential).map_err(AppError::internal)?
            );
        }
        Command::RevokeEnrollment { enrollment_id } => {
            let database = admin_database(cli.database)?;
            database.migrate()?;
            database.revoke_enrollment(&enrollment_id)?;
            println!(
                "{}",
                json!({ "status": "revoked", "enrollment_id": enrollment_id })
            );
        }
        Command::IssueDevice { name } => {
            let database = admin_database(cli.database)?;
            database.migrate()?;
            let capacity = std::env::var("PUSULA_GATEWAY_UPLOAD_BURST")
                .ok()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|_| AppError::BadRequest("PUSULA_GATEWAY_UPLOAD_BURST"))?
                .unwrap_or(5);
            if !(1..=60).contains(&capacity) {
                return Err(AppError::BadRequest(
                    "PUSULA_GATEWAY_UPLOAD_BURST must be between 1 and 60",
                ));
            }
            let credential = database.issue_device(&name, capacity)?;
            println!(
                "{}",
                serde_json::to_string(&credential).map_err(AppError::internal)?
            );
        }
        Command::RevokeDevice { device_id } => {
            let database = admin_database(cli.database)?;
            database.migrate()?;
            database.revoke_device(&device_id)?;
            println!("{}", json!({ "status": "revoked", "device_id": device_id }));
        }
    }
    Ok(())
}

fn migration_database(path: PathBuf) -> Database {
    Database::new(path, Arc::from([0_u8; 32]))
}

fn admin_database(path: PathBuf) -> Result<Database> {
    Ok(Database::new(path, token_pepper_from_env()?))
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("shutdown signal received");
}
