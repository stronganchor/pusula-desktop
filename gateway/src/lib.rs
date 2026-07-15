pub mod api;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod storage;

use api::{GatewayLimits, GatewayState};
use config::ServiceConfig;
use db::{AdmissionPolicy, Database};
use error::Result;
use storage::LocalObjectStore;

pub fn build_gateway(config: &ServiceConfig) -> Result<axum::Router> {
    let database = Database::new(config.database_path.clone(), config.token_pepper.clone());
    database.migrate()?;
    let storage = LocalObjectStore::new(config.object_root.clone(), config.min_free_bytes)?;
    Ok(api::router(GatewayState::new(
        database,
        storage,
        "backups/",
        GatewayLimits {
            max_backup_bytes: config.max_backup_bytes,
            reservation_ttl: config.reservation_ttl,
            admission_policy: AdmissionPolicy {
                rate_capacity: config.rate_capacity,
                rate_refill_seconds: config.rate_refill.as_secs(),
                max_pending_per_device: config.max_pending_per_device,
                byte_quota_24h: config.device_byte_quota_24h,
                pending_max_age_seconds: config.pending_max_age.as_secs(),
                pending_cleanup_limit: config.pending_cleanup_limit,
                authorization_cleanup_limit: config.authorization_cleanup_limit,
                daily_min_interval_seconds: config.daily_min_interval.as_secs(),
                monthly_min_interval_seconds: config.monthly_min_interval.as_secs(),
            },
            retention_policy: config.retention_policy,
            max_request_concurrency: config.max_request_concurrency,
            max_db_concurrency: config.max_db_concurrency,
            global_request_capacity: config.global_request_capacity,
            global_request_refill: config.global_request_refill,
        },
    )?))
}
