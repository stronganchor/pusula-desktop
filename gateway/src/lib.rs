pub mod api;
pub mod b2;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;

use api::{GatewayLimits, GatewayState};
use b2::B2Client;
use config::ServiceConfig;
use db::{AdmissionPolicy, Database};
use error::Result;

pub fn build_gateway(config: &ServiceConfig) -> Result<axum::Router> {
    let database = Database::new(config.database_path.clone(), config.token_pepper.clone());
    database.migrate()?;
    let b2 = B2Client::new(config.b2.clone(), config.http_client.clone());
    Ok(api::router(GatewayState::new(
        database,
        b2,
        config.b2.prefix.clone(),
        GatewayLimits {
            max_backup_bytes: config.max_backup_bytes,
            upload_ttl: config.b2.presign_ttl,
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
            max_request_concurrency: config.max_request_concurrency,
            max_db_concurrency: config.max_db_concurrency,
            global_request_capacity: config.global_request_capacity,
            global_request_refill: config.global_request_refill,
        },
    )?))
}
