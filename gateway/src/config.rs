use std::{env, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use reqwest::redirect::Policy;
use url::Url;

use crate::error::{AppError, Result};

pub const DEFAULT_DATABASE_PATH: &str = "/var/lib/pusula-backup-gateway/gateway.sqlite3";

#[derive(Clone)]
pub struct B2Config {
    pub endpoint: Url,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub key_id: String,
    pub application_key: String,
    pub presign_ttl: Duration,
}

#[derive(Clone)]
pub struct ServiceConfig {
    pub bind: SocketAddr,
    pub database_path: PathBuf,
    pub token_pepper: Arc<[u8]>,
    pub max_backup_bytes: u64,
    pub rate_capacity: u32,
    pub rate_refill: Duration,
    pub max_pending_per_device: u32,
    pub device_byte_quota_24h: u64,
    pub pending_max_age: Duration,
    pub pending_cleanup_limit: u32,
    pub authorization_cleanup_limit: u32,
    pub daily_min_interval: Duration,
    pub monthly_min_interval: Duration,
    pub max_request_concurrency: usize,
    pub max_db_concurrency: usize,
    pub global_request_capacity: u32,
    pub global_request_refill: Duration,
    pub b2: B2Config,
    pub http_client: reqwest::Client,
}

impl ServiceConfig {
    pub fn from_env(database_override: Option<PathBuf>) -> Result<Self> {
        let bind = env_or("PUSULA_GATEWAY_BIND", "127.0.0.1:12741")
            .parse::<SocketAddr>()
            .map_err(|_| AppError::BadRequest("PUSULA_GATEWAY_BIND is invalid"))?;
        if !bind.ip().is_loopback() {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_BIND must use a loopback address",
            ));
        }

        let database_path = database_override.unwrap_or_else(|| {
            PathBuf::from(env_or("PUSULA_GATEWAY_DATABASE", DEFAULT_DATABASE_PATH))
        });
        let pepper = required_env("PUSULA_GATEWAY_TOKEN_PEPPER")?;
        if pepper.len() < 32 {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_TOKEN_PEPPER must contain at least 32 bytes",
            ));
        }

        let endpoint = Url::parse(&required_env("PUSULA_GATEWAY_B2_ENDPOINT")?)
            .map_err(|_| AppError::BadRequest("PUSULA_GATEWAY_B2_ENDPOINT is invalid"))?;
        let allow_insecure = parse_bool_env("PUSULA_GATEWAY_ALLOW_INSECURE_B2_ENDPOINT", false)?;
        if endpoint.scheme() != "https" && !(allow_insecure && endpoint.scheme() == "http") {
            return Err(AppError::BadRequest(
                "B2 endpoint must use HTTPS (HTTP is test-only)",
            ));
        }
        if endpoint.host_str().is_none()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
        {
            return Err(AppError::BadRequest(
                "B2 endpoint must be absolute and contain no credentials, query, or fragment",
            ));
        }

        let bucket = env_or(
            "PUSULA_GATEWAY_B2_BUCKET",
            "stronganchor-pusula-desktop-backups",
        );
        validate_bucket(&bucket)?;
        let prefix = normalize_prefix(&env_or("PUSULA_GATEWAY_B2_PREFIX", "backups/"))?;

        let max_backup_bytes = parse_env("PUSULA_GATEWAY_MAX_BACKUP_BYTES", 268_435_456_u64)?;
        if max_backup_bytes == 0 || max_backup_bytes > 5 * 1024 * 1024 * 1024 {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_MAX_BACKUP_BYTES must be between 1 and 5368709120",
            ));
        }
        let presign_ttl_seconds = parse_env("PUSULA_GATEWAY_PRESIGN_TTL_SECONDS", 900_u64)?;
        if !(60..=3600).contains(&presign_ttl_seconds) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_PRESIGN_TTL_SECONDS must be between 60 and 3600",
            ));
        }
        let rate_capacity = parse_env("PUSULA_GATEWAY_UPLOAD_BURST", 5_u32)?;
        if !(1..=60).contains(&rate_capacity) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_UPLOAD_BURST must be between 1 and 60",
            ));
        }
        let refill_seconds = parse_env("PUSULA_GATEWAY_UPLOAD_REFILL_SECONDS", 60_u64)?;
        if !(1..=3600).contains(&refill_seconds) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_UPLOAD_REFILL_SECONDS must be between 1 and 3600",
            ));
        }
        let max_pending_per_device = parse_env("PUSULA_GATEWAY_MAX_PENDING_PER_DEVICE", 8_u32)?;
        if !(1..=100).contains(&max_pending_per_device) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_MAX_PENDING_PER_DEVICE must be between 1 and 100",
            ));
        }
        let device_byte_quota_24h =
            parse_env("PUSULA_GATEWAY_DEVICE_24H_BYTE_QUOTA", 1_073_741_824_u64)?;
        validate_device_byte_quota(max_backup_bytes, device_byte_quota_24h)?;
        let pending_max_age_seconds = parse_env(
            "PUSULA_GATEWAY_PENDING_MAX_AGE_SECONDS",
            30 * 24 * 60 * 60_u64,
        )?;
        if !(24 * 60 * 60..=180 * 24 * 60 * 60).contains(&pending_max_age_seconds) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_PENDING_MAX_AGE_SECONDS must be between 1 and 180 days",
            ));
        }
        let pending_cleanup_limit = parse_env("PUSULA_GATEWAY_PENDING_CLEANUP_LIMIT", 100_u32)?;
        if !(1..=1000).contains(&pending_cleanup_limit) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_PENDING_CLEANUP_LIMIT must be between 1 and 1000",
            ));
        }
        let authorization_cleanup_limit =
            parse_env("PUSULA_GATEWAY_AUTHORIZATION_CLEANUP_LIMIT", 500_u32)?;
        if !(1..=5000).contains(&authorization_cleanup_limit) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_AUTHORIZATION_CLEANUP_LIMIT must be between 1 and 5000",
            ));
        }
        let daily_min_interval_seconds = parse_env(
            "PUSULA_GATEWAY_DAILY_MIN_INTERVAL_SECONDS",
            20 * 60 * 60_u64,
        )?;
        if !(20 * 60 * 60..=7 * 24 * 60 * 60).contains(&daily_min_interval_seconds) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_DAILY_MIN_INTERVAL_SECONDS must be between 20 hours and 7 days",
            ));
        }
        let monthly_min_interval_seconds = parse_env(
            "PUSULA_GATEWAY_MONTHLY_MIN_INTERVAL_SECONDS",
            25 * 24 * 60 * 60_u64,
        )?;
        if !(25 * 24 * 60 * 60..=90 * 24 * 60 * 60).contains(&monthly_min_interval_seconds) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_MONTHLY_MIN_INTERVAL_SECONDS must be between 25 and 90 days",
            ));
        }
        let max_request_concurrency = parse_env("PUSULA_GATEWAY_MAX_REQUEST_CONCURRENCY", 8_usize)?;
        if !(4..=256).contains(&max_request_concurrency) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_MAX_REQUEST_CONCURRENCY must be between 4 and 256",
            ));
        }
        let max_db_concurrency = parse_env("PUSULA_GATEWAY_MAX_DB_CONCURRENCY", 4_usize)?;
        if !(1..=32).contains(&max_db_concurrency) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_MAX_DB_CONCURRENCY must be between 1 and 32",
            ));
        }
        let global_request_capacity = parse_env("PUSULA_GATEWAY_GLOBAL_REQUEST_BURST", 60_u32)?;
        if !(1..=10_000).contains(&global_request_capacity) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_GLOBAL_REQUEST_BURST must be between 1 and 10000",
            ));
        }
        let global_request_refill_seconds =
            parse_env("PUSULA_GATEWAY_GLOBAL_REQUEST_REFILL_SECONDS", 1_u64)?;
        if !(1..=3600).contains(&global_request_refill_seconds) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_GLOBAL_REQUEST_REFILL_SECONDS must be between 1 and 3600",
            ));
        }

        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .redirect(Policy::none())
            .user_agent(concat!("pusula-backup-gateway/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(AppError::internal)?;

        let region = required_env("PUSULA_GATEWAY_B2_REGION")?;
        if region.len() < 3
            || region.len() > 32
            || region.starts_with('-')
            || region.ends_with('-')
            || !region
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(AppError::BadRequest("PUSULA_GATEWAY_B2_REGION is invalid"));
        }
        let key_id = required_env("PUSULA_GATEWAY_B2_KEY_ID")?;
        let application_key = required_env("PUSULA_GATEWAY_B2_APPLICATION_KEY")?;
        if !(3..=128).contains(&key_id.len())
            || key_id.bytes().any(|byte| byte.is_ascii_whitespace())
            || !(16..=256).contains(&application_key.len())
            || application_key
                .bytes()
                .any(|byte| byte.is_ascii_whitespace())
        {
            return Err(AppError::BadRequest("B2 credentials are malformed"));
        }

        Ok(Self {
            bind,
            database_path,
            token_pepper: Arc::from(pepper.into_bytes()),
            max_backup_bytes,
            rate_capacity,
            rate_refill: Duration::from_secs(refill_seconds),
            max_pending_per_device,
            device_byte_quota_24h,
            pending_max_age: Duration::from_secs(pending_max_age_seconds),
            pending_cleanup_limit,
            authorization_cleanup_limit,
            daily_min_interval: Duration::from_secs(daily_min_interval_seconds),
            monthly_min_interval: Duration::from_secs(monthly_min_interval_seconds),
            max_request_concurrency,
            max_db_concurrency,
            global_request_capacity,
            global_request_refill: Duration::from_secs(global_request_refill_seconds),
            b2: B2Config {
                endpoint,
                region,
                bucket,
                prefix,
                key_id,
                application_key,
                presign_ttl: Duration::from_secs(presign_ttl_seconds),
            },
            http_client,
        })
    }
}

pub fn token_pepper_from_env() -> Result<Arc<[u8]>> {
    let pepper = required_env("PUSULA_GATEWAY_TOKEN_PEPPER")?;
    if pepper.len() < 32 {
        return Err(AppError::BadRequest(
            "PUSULA_GATEWAY_TOKEN_PEPPER must contain at least 32 bytes",
        ));
    }
    Ok(Arc::from(pepper.into_bytes()))
}

fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn required_env(name: &'static str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|value| {
            let value = value.trim();
            !value.is_empty() && !value.contains("REPLACE_")
        })
        .ok_or(AppError::BadRequest(name))
}

fn parse_env<T>(name: &'static str, default: T) -> Result<T>
where
    T: std::str::FromStr,
{
    match env::var(name) {
        Ok(value) => value.parse::<T>().map_err(|_| AppError::BadRequest(name)),
        Err(_) => Ok(default),
    }
}

fn parse_bool_env(name: &'static str, default: bool) -> Result<bool> {
    match env::var(name) {
        Ok(value) => match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Ok(true),
            "0" | "false" | "no" => Ok(false),
            _ => Err(AppError::BadRequest(name)),
        },
        Err(_) => Ok(default),
    }
}

fn validate_bucket(bucket: &str) -> Result<()> {
    if bucket.len() < 3
        || bucket.len() > 63
        || bucket.starts_with(['.', '-'].as_slice())
        || bucket.ends_with(['.', '-'].as_slice())
        || bucket.contains("..")
        || !bucket
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte))
    {
        return Err(AppError::BadRequest("B2 bucket name is invalid"));
    }
    Ok(())
}

fn normalize_prefix(prefix: &str) -> Result<String> {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty()
        || prefix.len() > 200
        || prefix.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(AppError::BadRequest("B2 object prefix is invalid"));
    }
    Ok(format!("{prefix}/"))
}

fn validate_device_byte_quota(max_backup_bytes: u64, quota: u64) -> Result<()> {
    let minimum_fallback_quota = max_backup_bytes.checked_mul(2).ok_or(AppError::BadRequest(
        "PUSULA_GATEWAY_MAX_BACKUP_BYTES is too large for the byte quota",
    ))?;
    if quota < minimum_fallback_quota || quota > 1024 * 1024 * 1024 * 1024 {
        return Err(AppError::BadRequest(
            "PUSULA_GATEWAY_DEVICE_24H_BYTE_QUOTA must cover one direct grant plus one relay and not exceed 1 TiB",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_quota_must_cover_a_direct_grant_and_relay_fallback() {
        assert!(validate_device_byte_quota(256, 512).is_ok());
        assert!(matches!(
            validate_device_byte_quota(256, 511),
            Err(AppError::BadRequest(_))
        ));
    }
}
