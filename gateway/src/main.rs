use std::{path::PathBuf, sync::Arc};

use clap::{Parser, Subcommand};
use pusula_backup_gateway::{
    api::cleanup_stale_relay_spools,
    build_gateway,
    config::{
        object_root_from_env, retention_policy_from_env, token_pepper_from_env, ServiceConfig,
        DEFAULT_DATABASE_PATH,
    },
    db::Database,
    error::{AppError, Result},
    storage::{prune_retention, LocalObjectStore},
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
    /// List completed backup records (Unix root only).
    ListBackups {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Look up one completed backup record by ID (Unix root only).
    LookupBackup {
        #[arg(long)]
        backup_id: String,
    },
    /// Download and verify one exact completed local object version (Unix root only).
    DownloadBackup {
        #[arg(long)]
        backup_id: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Run bounded local object retention immediately.
    PruneStorage,
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
            let removed_spools = cleanup_stale_relay_spools(&config.database_path).await?;
            if removed_spools > 0 {
                tracing::warn!(
                    removed_spools,
                    "removed stale encrypted relay spool files before startup"
                );
            }
            let retention_database =
                Database::new(config.database_path.clone(), config.token_pepper.clone());
            retention_database.migrate()?;
            let retention_storage =
                LocalObjectStore::new(config.object_root.clone(), config.min_free_bytes)?;
            let removed_objects = prune_retention(
                retention_database,
                retention_storage,
                config.retention_policy,
            )
            .await?;
            if removed_objects > 0 {
                tracing::info!(removed_objects, "expired encrypted backup objects removed");
            }
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
        Command::ListBackups { limit } => {
            require_root()?;
            let database = migration_database(cli.database);
            let backups = database.list_completed_backups(limit)?;
            println!(
                "{}",
                serde_json::to_string(&backups).map_err(AppError::internal)?
            );
        }
        Command::LookupBackup { backup_id } => {
            require_root()?;
            validate_backup_id(&backup_id)?;
            let database = migration_database(cli.database);
            let backup = database.completed_backup(&backup_id)?;
            println!(
                "{}",
                serde_json::to_string(&backup).map_err(AppError::internal)?
            );
        }
        Command::DownloadBackup { backup_id, output } => {
            require_root()?;
            validate_backup_id(&backup_id)?;
            let object_root = object_root_from_env(&cli.database)?;
            let database = migration_database(cli.database);
            let backup = database.completed_backup(&backup_id)?;
            let version_id = backup.version_id.as_deref().ok_or(AppError::Conflict(
                "completed backup has no verified storage version",
            ))?;
            let storage = LocalObjectStore::open_existing(object_root)?;
            let verified = storage
                .download_verified(
                    &backup.object_key,
                    version_id,
                    backup.size_bytes,
                    &backup.sha256,
                    &output,
                )
                .await?;
            println!(
                "{}",
                json!({
                    "status": "downloaded",
                    "backup_id": backup.id,
                    "version_id": verified.version_id,
                    "size_bytes": verified.size_bytes,
                    "sha256": verified.sha256
                })
            );
        }
        Command::PruneStorage => {
            let object_root = object_root_from_env(&cli.database)?;
            let retention_policy = retention_policy_from_env()?;
            let database = migration_database(cli.database);
            database.migrate()?;
            let storage = LocalObjectStore::new(object_root, 0)?;
            let removed = prune_retention(database, storage, retention_policy).await?;
            println!("{}", json!({ "status": "ok", "removed": removed }));
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

fn validate_backup_id(value: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(value)
        .map_err(|_| AppError::BadRequest("backup_id must be a UUID"))?;
    if parsed.to_string() != value {
        return Err(AppError::BadRequest(
            "backup_id must use canonical lowercase UUID form",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn require_root() -> Result<()> {
    // SAFETY: geteuid has no preconditions and does not dereference memory.
    if unsafe { libc::geteuid() } != 0 {
        return Err(AppError::Unauthorized);
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_root() -> Result<()> {
    Err(AppError::BadRequest(
        "backup recovery commands must run as Unix root",
    ))
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
