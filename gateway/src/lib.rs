pub mod api;
pub mod b2;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;

use api::GatewayState;
use b2::B2Client;
use config::ServiceConfig;
use db::Database;
use error::Result;

pub fn build_gateway(config: &ServiceConfig) -> Result<axum::Router> {
    let database = Database::new(config.database_path.clone(), config.token_pepper.clone());
    database.migrate()?;
    let b2 = B2Client::new(config.b2.clone(), config.http_client.clone());
    Ok(api::router(GatewayState::new(
        database,
        b2,
        config.b2.prefix.clone(),
        config.max_backup_bytes,
        config.rate_capacity,
        config.rate_refill,
    )))
}
