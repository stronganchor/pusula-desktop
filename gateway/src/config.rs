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
