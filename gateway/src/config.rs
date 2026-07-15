use std::{
    env,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{
    db::RetentionPolicy,
    error::{AppError, Result},
};

pub const DEFAULT_DATABASE_PATH: &str = "/var/lib/pusula-backup-gateway/gateway.sqlite3";

#[derive(Clone)]
pub struct ServiceConfig {
    pub bind: SocketAddr,
    pub database_path: PathBuf,
    pub object_root: PathBuf,
    pub token_pepper: Arc<[u8]>,
    pub max_backup_bytes: u64,
    pub min_free_bytes: u64,
    pub reservation_ttl: Duration,
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
    pub retention_policy: RetentionPolicy,
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
        let object_root = object_root_from_env(&database_path)?;

        let pepper = required_env("PUSULA_GATEWAY_TOKEN_PEPPER")?;
        if pepper.len() < 32 {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_TOKEN_PEPPER must contain at least 32 bytes",
            ));
        }

        let max_backup_bytes = parse_env("PUSULA_GATEWAY_MAX_BACKUP_BYTES", 268_435_456_u64)?;
        if max_backup_bytes == 0 || max_backup_bytes > 5 * 1024 * 1024 * 1024 {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_MAX_BACKUP_BYTES must be between 1 and 5368709120",
            ));
        }
        let min_free_bytes = parse_env("PUSULA_GATEWAY_MIN_FREE_BYTES", 1_073_741_824_u64)?;
        if !(64 * 1024 * 1024..=1024 * 1024 * 1024 * 1024).contains(&min_free_bytes) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_MIN_FREE_BYTES must be between 64 MiB and 1 TiB",
            ));
        }
        let reservation_ttl_seconds = parse_env("PUSULA_GATEWAY_RESERVATION_TTL_SECONDS", 900_u64)?;
        if !(60..=3600).contains(&reservation_ttl_seconds) {
            return Err(AppError::BadRequest(
                "PUSULA_GATEWAY_RESERVATION_TTL_SECONDS must be between 60 and 3600",
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

        let retention_policy = retention_policy_from_env()?;

        Ok(Self {
            bind,
            database_path,
            object_root,
            token_pepper: Arc::from(pepper.into_bytes()),
            max_backup_bytes,
            min_free_bytes,
            reservation_ttl: Duration::from_secs(reservation_ttl_seconds),
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
            retention_policy,
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

pub fn object_root_from_env(database_path: &Path) -> Result<PathBuf> {
    validate_absolute_normalized_path(database_path, "PUSULA_GATEWAY_DATABASE is invalid")?;
    let default_object_root = database_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("/var/lib/pusula-backup-gateway"))
        .join("objects");
    let object_root = env::var("PUSULA_GATEWAY_OBJECT_ROOT")
        .map(PathBuf::from)
        .unwrap_or(default_object_root);
    validate_absolute_normalized_path(&object_root, "PUSULA_GATEWAY_OBJECT_ROOT is invalid")?;
    if object_root == database_path {
        return Err(AppError::BadRequest(
            "PUSULA_GATEWAY_OBJECT_ROOT must not equal the database path",
        ));
    }
    Ok(object_root)
}

pub fn retention_policy_from_env() -> Result<RetentionPolicy> {
    let rolling_days = parse_retention_days("PUSULA_GATEWAY_RETENTION_ROLLING_DAYS", 14)?;
    let daily_days = parse_retention_days("PUSULA_GATEWAY_RETENTION_DAILY_DAYS", 60)?;
    let monthly_days = parse_retention_days("PUSULA_GATEWAY_RETENTION_MONTHLY_DAYS", 400)?;
    if rolling_days > daily_days || daily_days > monthly_days {
        return Err(AppError::BadRequest(
            "storage retention days must satisfy rolling <= daily <= monthly",
        ));
    }
    let cleanup_limit = parse_env("PUSULA_GATEWAY_RETENTION_CLEANUP_LIMIT", 100_u32)?;
    if !(1..=1000).contains(&cleanup_limit) {
        return Err(AppError::BadRequest(
            "PUSULA_GATEWAY_RETENTION_CLEANUP_LIMIT must be between 1 and 1000",
        ));
    }
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
    Ok(RetentionPolicy {
        rolling_seconds: rolling_days * 24 * 60 * 60,
        daily_seconds: daily_days * 24 * 60 * 60,
        monthly_seconds: monthly_days * 24 * 60 * 60,
        pending_max_age_seconds,
        pending_cleanup_limit,
        cleanup_limit,
    })
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

fn parse_retention_days(name: &'static str, default: u64) -> Result<u64> {
    let days = parse_env(name, default)?;
    if !(1..=3650).contains(&days) {
        return Err(AppError::BadRequest(name));
    }
    Ok(days)
}

fn validate_absolute_normalized_path(path: &Path, message: &'static str) -> Result<()> {
    if !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(AppError::BadRequest(message));
    }
    Ok(())
}

fn validate_device_byte_quota(max_backup_bytes: u64, quota: u64) -> Result<()> {
    let minimum_fallback_quota = max_backup_bytes.checked_mul(2).ok_or(AppError::BadRequest(
        "PUSULA_GATEWAY_MAX_BACKUP_BYTES is too large for the byte quota",
    ))?;
    if quota < minimum_fallback_quota || quota > 1024 * 1024 * 1024 * 1024 {
        return Err(AppError::BadRequest(
            "PUSULA_GATEWAY_DEVICE_24H_BYTE_QUOTA must cover one reservation plus one relay and not exceed 1 TiB",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_quota_must_cover_a_reservation_and_relay() {
        assert!(validate_device_byte_quota(256, 512).is_ok());
        assert!(matches!(
            validate_device_byte_quota(256, 511),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn storage_paths_must_be_absolute_and_normalized() {
        assert!(
            validate_absolute_normalized_path(Path::new("relative/objects"), "invalid").is_err()
        );
        assert!(validate_absolute_normalized_path(
            Path::new("/var/lib/pusula/../objects"),
            "invalid"
        )
        .is_err());
        let current = std::env::current_dir().unwrap();
        let filesystem_root = current.ancestors().last().unwrap();
        assert!(validate_absolute_normalized_path(filesystem_root, "invalid").is_err());
    }
}
