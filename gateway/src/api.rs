#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path as RoutePath, State},
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, sync::Semaphore};
use uuid::Uuid;

use crate::{
    crypto::normalize_sha256,
    db::{
        AdmissionPolicy, AuthenticatedDevice, BackupRecord, Database, RetentionClass,
        RetentionPolicy,
    },
    error::{AppError, Result},
    storage::LocalObjectStore,
};

#[derive(Clone)]
pub struct GatewayState {
    database: Database,
    storage: LocalObjectStore,
    object_prefix: Arc<str>,
    relay_directory: Arc<PathBuf>,
    relay_slots: Arc<Semaphore>,
    request_slots: Arc<Semaphore>,
    db_slots: Arc<Semaphore>,
    retention_slots: Arc<Semaphore>,
    aggregate_rate: Arc<AggregateRateLimiter>,
    max_backup_bytes: u64,
    reservation_ttl: Duration,
    admission_policy: AdmissionPolicy,
    retention_policy: RetentionPolicy,
}

#[derive(Clone, Copy, Debug)]
pub struct GatewayLimits {
    pub max_backup_bytes: u64,
    pub reservation_ttl: Duration,
    pub admission_policy: AdmissionPolicy,
    pub retention_policy: RetentionPolicy,
    pub max_request_concurrency: usize,
    pub max_db_concurrency: usize,
    pub global_request_capacity: u32,
    pub global_request_refill: Duration,
}

struct AggregateRateLimiter {
    capacity: f64,
    refill: Duration,
    state: Mutex<AggregateRateState>,
}

struct AggregateRateState {
    tokens: f64,
    updated_at: Instant,
}

impl AggregateRateLimiter {
    fn new(capacity: u32, refill: Duration) -> Result<Self> {
        if capacity == 0 || refill.is_zero() {
            return Err(AppError::BadRequest(
                "aggregate request rate policy is invalid",
            ));
        }
        Ok(Self {
            capacity: f64::from(capacity),
            refill,
            state: Mutex::new(AggregateRateState {
                tokens: f64::from(capacity),
                updated_at: Instant::now(),
            }),
        })
    }

    fn try_admit(&self) -> Result<()> {
        let now = Instant::now();
        let mut state = self.state.lock().map_err(|_| {
            AppError::Internal("aggregate rate limiter lock was poisoned".to_owned())
        })?;
        let elapsed = now
            .saturating_duration_since(state.updated_at)
            .as_secs_f64();
        state.tokens = (state.tokens + elapsed / self.refill.as_secs_f64()).min(self.capacity);
        state.updated_at = now;
        if state.tokens < 1.0 {
            let retry_after_seconds = ((1.0 - state.tokens) * self.refill.as_secs_f64())
                .ceil()
                .max(1.0) as u64;
            return Err(AppError::RateLimited {
                retry_after_seconds,
            });
        }
        state.tokens -= 1.0;
        Ok(())
    }
}

impl GatewayState {
    pub fn new(
        database: Database,
        storage: LocalObjectStore,
        object_prefix: impl Into<Arc<str>>,
        limits: GatewayLimits,
    ) -> Result<Self> {
        if limits.max_backup_bytes == 0
            || limits.reservation_ttl.is_zero()
            || limits.max_request_concurrency == 0
            || limits.max_db_concurrency == 0
        {
            return Err(AppError::BadRequest("gateway limits are invalid"));
        }
        let relay_directory = relay_directory(database.path());
        let aggregate_rate = AggregateRateLimiter::new(
            limits.global_request_capacity,
            limits.global_request_refill,
        )?;
        Ok(Self {
            database,
            storage,
            object_prefix: object_prefix.into(),
            relay_directory: Arc::new(relay_directory),
            relay_slots: Arc::new(Semaphore::new(1)),
            request_slots: Arc::new(Semaphore::new(limits.max_request_concurrency)),
            db_slots: Arc::new(Semaphore::new(limits.max_db_concurrency)),
            retention_slots: Arc::new(Semaphore::new(1)),
            aggregate_rate: Arc::new(aggregate_rate),
            max_backup_bytes: limits.max_backup_bytes,
            reservation_ttl: limits.reservation_ttl,
            admission_policy: limits.admission_policy,
            retention_policy: limits.retention_policy,
        })
    }
}

fn relay_directory(database_path: &FsPath) -> PathBuf {
    database_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| FsPath::new("."))
        .join("relay-spool")
}

/// Remove encrypted relay fragments left by a process or host crash. Production
/// calls this before binding the listener, so no active request can be touched.
pub async fn cleanup_stale_relay_spools(database_path: &FsPath) -> Result<u64> {
    let directory = relay_directory(database_path);
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(AppError::internal(error)),
    };
    let mut removed = 0_u64;
    while let Some(entry) = entries.next_entry().await.map_err(AppError::internal)? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(".relay-") || !name.ends_with(".sqlite3.age.part") {
            continue;
        }
        let file_type = entry.file_type().await.map_err(AppError::internal)?;
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }
        tokio::fs::remove_file(entry.path())
            .await
            .map_err(AppError::internal)?;
        removed = removed.saturating_add(1);
    }
    Ok(removed)
}

pub fn router(state: GatewayState) -> Router {
    let admission_state = state.clone();
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/enroll", post(enroll))
        .route("/v1/backups/upload-url", post(upload_url))
        .route("/v1/backups/relay/{backup_id}", put(relay_backup))
        .route("/v1/backups/complete", post(complete_backup))
        .route("/v1/backups/status", get(backup_status))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(
            admission_state,
            request_admission,
        ))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn request_admission(
    State(state): State<GatewayState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if request.uri().path() == "/healthz" {
        return next.run(request).await;
    }
    let _request_permit = match state.request_slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return AppError::ServiceUnavailable {
                retry_after_seconds: 1,
            }
            .into_response()
        }
    };
    if let Err(error) = state.aggregate_rate.try_admit() {
        return error.into_response();
    }
    next.run(request).await
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollRequest {
    enrollment_code: String,
    device_name: String,
}

#[derive(Serialize)]
struct EnrollResponse {
    device_id: String,
    device_token: String,
    created_at: String,
}

async fn enroll(
    State(state): State<GatewayState>,
    Json(request): Json<EnrollRequest>,
) -> Result<impl IntoResponse> {
    let capacity = state.admission_policy.rate_capacity;
    let credential = run_db(&state, move |database| {
        database.enroll_device(&request.enrollment_code, &request.device_name, capacity)
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(EnrollResponse {
            device_id: credential.device_id,
            device_token: credential.device_token,
            created_at: timestamp(credential.created_at)?,
        }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadUrlRequest {
    content_length: u64,
    sha256: String,
    #[serde(default)]
    retention_class: RetentionClass,
}

#[derive(Serialize)]
struct UploadUrlResponse {
    backup_id: String,
    retention_class: RetentionClass,
    transport: &'static str,
    expires_at: String,
}

async fn upload_url(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(request): Json<UploadUrlRequest>,
) -> Result<Json<UploadUrlResponse>> {
    let device = authenticate(&state, &headers).await?;
    if request.content_length == 0 || request.content_length > state.max_backup_bytes {
        return Err(AppError::BadRequest(
            "content_length is zero or exceeds the configured backup limit",
        ));
    }
    let checksum = normalize_sha256(&request.sha256)?;
    run_retention_cleanup(&state).await?;
    let now = Utc::now();
    let backup_id = Uuid::new_v4().to_string();
    let object_key = format!(
        "{}{}/{}/{}/{:02}/{:02}/{}.sqlite3.age",
        state.object_prefix,
        request.retention_class.as_str(),
        device.id,
        now.format("%Y"),
        now.format("%m"),
        now.format("%d"),
        backup_id
    );
    let expires_at =
        now + chrono::Duration::from_std(state.reservation_ttl).map_err(AppError::internal)?;
    let expires_epoch = expires_at.timestamp();
    let admission_policy = state.admission_policy;
    let device_id = device.id;
    let db_backup_id = backup_id.clone();
    let db_object_key = object_key;
    let db_checksum = checksum.clone();
    let retention_class = request.retention_class;
    let backup = run_db(&state, move |database| {
        database.reserve_or_reuse_backup(
            &device_id,
            &db_backup_id,
            &db_object_key,
            request.content_length,
            &db_checksum,
            retention_class,
            expires_epoch,
            admission_policy,
        )
    })
    .await?;
    Ok(Json(UploadUrlResponse {
        backup_id: backup.id,
        retention_class: backup.retention_class,
        transport: "gateway_relay",
        expires_at: expires_at.to_rfc3339_opts(SecondsFormat::Secs, true),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteRequest {
    backup_id: String,
}

#[derive(Serialize)]
struct CompleteResponse {
    backup_id: String,
    status: String,
    completed_at: String,
    etag: Option<String>,
    version_id: String,
}

async fn complete_backup(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(request): Json<CompleteRequest>,
) -> Result<Json<CompleteResponse>> {
    validate_uuid(&request.backup_id)?;
    let device = authenticate(&state, &headers).await?;
    let _relay_slot =
        state
            .relay_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| AppError::RateLimited {
                retry_after_seconds: 5,
            })?;
    let device_id = device.id;
    let backup_id = request.backup_id;
    let lookup_device_id = device_id.clone();
    let lookup_backup_id = backup_id.clone();
    let backup = run_db(&state, move |database| {
        database.backup_for_device(&lookup_device_id, &lookup_backup_id)
    })
    .await?;

    if backup.status == "completed" {
        verify_completed_storage(&state, &backup).await?;
        return Ok(Json(completion_response(backup)?));
    }
    let verified = state
        .storage
        .verify_object_if_present(&backup.object_key, backup.size_bytes, &backup.sha256)
        .await?
        .ok_or(AppError::ObjectNotPresent {
            retry_after_seconds: 5,
        })?;
    let completed = run_db(&state, move |database| {
        database.mark_backup_completed(
            &device_id,
            &backup_id,
            verified.etag.as_deref(),
            &verified.version_id,
            verified.size_bytes,
            &verified.sha256,
        )
    })
    .await?;
    Ok(Json(completion_response(completed)?))
}

async fn relay_backup(
    State(state): State<GatewayState>,
    RoutePath(backup_id): RoutePath<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<CompleteResponse>> {
    validate_uuid(&backup_id)?;
    let device = authenticate(&state, &headers).await?;
    let _relay_slot =
        state
            .relay_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| AppError::RateLimited {
                retry_after_seconds: 5,
            })?;
    let device_id = device.id;
    let lookup_device_id = device_id.clone();
    let lookup_backup_id = backup_id.clone();
    let backup = run_db(&state, move |database| {
        database.backup_for_device(&lookup_device_id, &lookup_backup_id)
    })
    .await?;

    validate_relay_headers(&headers, backup.size_bytes)?;
    if backup.status == "completed" {
        verify_completed_storage(&state, &backup).await?;
        return Ok(Json(completion_response(backup)?));
    }
    if backup.status != "pending" {
        return Err(AppError::Conflict("backup is not pending"));
    }
    if backup.size_bytes > state.max_backup_bytes {
        return Err(AppError::Conflict(
            "backup exceeds the configured relay limit",
        ));
    }

    if let Some(verified) = state
        .storage
        .verify_object_if_present(&backup.object_key, backup.size_bytes, &backup.sha256)
        .await?
    {
        let confirmed_device_id = device_id.clone();
        let confirmed_backup_id = backup_id.clone();
        let completed = run_db(&state, move |database| {
            database.mark_backup_completed(
                &confirmed_device_id,
                &confirmed_backup_id,
                verified.etag.as_deref(),
                &verified.version_id,
                verified.size_bytes,
                &verified.sha256,
            )
        })
        .await?;
        return Ok(Json(completion_response(completed)?));
    }

    let admission_device_id = device_id.clone();
    let admission_backup_id = backup_id.clone();
    let admission_policy = state.admission_policy;
    state.storage.ensure_capacity(backup.size_bytes)?;
    run_db(&state, move |database| {
        database.begin_relay_attempt(&admission_device_id, &admission_backup_id, admission_policy)
    })
    .await?;
    let spool = spool_relay_body(
        state.relay_directory.as_ref(),
        &backup_id,
        body,
        backup.size_bytes,
        &backup.sha256,
    )
    .await?;
    let upload_result = state
        .storage
        .store_verified_spool(
            &backup.object_key,
            backup.size_bytes,
            &backup.sha256,
            spool.path(),
        )
        .await;
    spool.cleanup().await?;
    let verified = upload_result?;

    let completed = run_db(&state, move |database| {
        database.mark_backup_completed(
            &device_id,
            &backup_id,
            verified.etag.as_deref(),
            &verified.version_id,
            verified.size_bytes,
            &verified.sha256,
        )
    })
    .await?;
    Ok(Json(completion_response(completed)?))
}

fn validate_relay_headers(headers: &HeaderMap, expected_size: u64) -> Result<()> {
    if headers.contains_key(header::TRANSFER_ENCODING) {
        return Err(AppError::BadRequest(
            "relay requests must not use transfer encoding",
        ));
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or(AppError::BadRequest(
            "relay content type must be application/octet-stream",
        ))?;
    if content_type != "application/octet-stream" {
        return Err(AppError::BadRequest(
            "relay content type must be application/octet-stream",
        ));
    }
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(AppError::BadRequest(
            "relay content length is required and must be numeric",
        ))?;
    if content_length != expected_size {
        return Err(AppError::BadRequest(
            "relay content length does not match the reservation",
        ));
    }
    Ok(())
}

struct RelaySpool {
    path: PathBuf,
    armed: bool,
}

impl RelaySpool {
    fn path(&self) -> &FsPath {
        &self.path
    }

    async fn cleanup(mut self) -> Result<()> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => {
                self.armed = false;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.armed = false;
                Ok(())
            }
            Err(error) => Err(AppError::internal(error)),
        }
    }
}

impl Drop for RelaySpool {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn spool_relay_body(
    directory: &FsPath,
    backup_id: &str,
    body: Body,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<RelaySpool> {
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(AppError::internal)?;
    #[cfg(unix)]
    tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(AppError::internal)?;

    let path = directory.join(format!(
        ".relay-{backup_id}-{}.sqlite3.age.part",
        Uuid::new_v4()
    ));
    let spool = RelaySpool { path, armed: true };
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(spool.path())
        .await
        .map_err(AppError::internal)?;
    #[cfg(unix)]
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(AppError::internal)?;

    let mut stream = body.into_data_stream();
    let mut received = 0_u64;
    let mut hasher = Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| AppError::BadRequest("relay body stream failed"))?;
        received = received
            .checked_add(u64::try_from(chunk.len()).map_err(AppError::internal)?)
            .ok_or(AppError::BadRequest("relay body is too large"))?;
        if received > expected_size {
            return Err(AppError::BadRequest(
                "relay body exceeds the reserved content length",
            ));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(AppError::internal)?;
    }
    if received != expected_size {
        return Err(AppError::BadRequest(
            "relay body is shorter than the reserved content length",
        ));
    }
    if hex::encode(hasher.finalize()) != expected_sha256 {
        return Err(AppError::BadRequest(
            "relay ciphertext checksum does not match the reservation",
        ));
    }
    file.flush().await.map_err(AppError::internal)?;
    file.sync_all().await.map_err(AppError::internal)?;
    drop(file);
    Ok(spool)
}

async fn verify_completed_storage(state: &GatewayState, backup: &BackupRecord) -> Result<()> {
    let verified = state
        .storage
        .verify_object_if_present(&backup.object_key, backup.size_bytes, &backup.sha256)
        .await?
        .ok_or_else(|| AppError::Upstream("completed storage object was missing".to_owned()))?;
    if backup.version_id.as_deref() != Some(verified.version_id.as_str())
        || backup.verified_size_bytes != Some(verified.size_bytes)
        || backup.verified_sha256.as_deref() != Some(verified.sha256.as_str())
    {
        return Err(AppError::Upstream(
            "completed storage evidence did not match the immutable object".to_owned(),
        ));
    }
    Ok(())
}

fn completion_response(backup: BackupRecord) -> Result<CompleteResponse> {
    let completed_at = backup
        .completed_at
        .ok_or_else(|| AppError::Internal("completed backup has no timestamp".to_owned()))?;
    let version_id = backup
        .version_id
        .clone()
        .filter(|value| {
            !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| {
            AppError::Upstream("completed backup has no exact storage version".to_owned())
        })?;
    if backup.verified_size_bytes != Some(backup.size_bytes)
        || backup.verified_sha256.as_deref() != Some(backup.sha256.as_str())
        || backup.verified_at.is_none()
    {
        return Err(AppError::Upstream(
            "completed backup lacks actual-body verification evidence".to_owned(),
        ));
    }
    Ok(CompleteResponse {
        backup_id: backup.id,
        status: backup.status,
        completed_at: timestamp(completed_at)?,
        etag: backup.etag,
        version_id,
    })
}

#[derive(Serialize)]
struct BackupStatusResponse {
    device_id: String,
    server_time: String,
    active_pending_uploads: u64,
    expired_pending_uploads: u64,
    latest_completed: Option<CompletedBackupSummary>,
}

#[derive(Serialize)]
struct CompletedBackupSummary {
    backup_id: String,
    size_bytes: u64,
    sha256: String,
    completed_at: String,
}

async fn backup_status(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Result<Json<BackupStatusResponse>> {
    let device = authenticate(&state, &headers).await?;
    let device_id = device.id;
    let lookup_id = device_id.clone();
    let summary = run_db(&state, move |database| database.backup_summary(&lookup_id)).await?;
    let latest_completed = summary
        .latest_completed
        .map(|backup| {
            Ok(CompletedBackupSummary {
                backup_id: backup.id,
                size_bytes: backup.size_bytes,
                sha256: backup.sha256,
                completed_at: timestamp(backup.completed_at.ok_or_else(|| {
                    AppError::Internal("completed backup has no timestamp".to_owned())
                })?)?,
            })
        })
        .transpose()?;
    Ok(Json(BackupStatusResponse {
        device_id,
        server_time: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        active_pending_uploads: summary.active_pending,
        expired_pending_uploads: summary.expired_pending,
        latest_completed,
    }))
}

async fn authenticate(state: &GatewayState, headers: &HeaderMap) -> Result<AuthenticatedDevice> {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    let token = header
        .strip_prefix("Bearer ")
        .filter(|token| !token.contains(char::is_whitespace))
        .ok_or(AppError::Unauthorized)?
        .to_owned();
    run_db(state, move |database| database.authenticate(&token)).await
}

async fn run_retention_cleanup(state: &GatewayState) -> Result<()> {
    let Ok(_permit) = state.retention_slots.clone().try_acquire_owned() else {
        return Ok(());
    };
    let Ok(_relay_permit) = state.relay_slots.clone().try_acquire_owned() else {
        return Ok(());
    };
    let policy = state.retention_policy;
    let candidates = run_db(state, move |database| database.claim_storage_purges(policy)).await?;
    let mut removed = 0_u64;
    for candidate in candidates {
        state.storage.remove_object(&candidate.object_key).await?;
        let backup_id = candidate.backup_id;
        run_db(state, move |database| {
            database.finish_storage_purge(&backup_id)
        })
        .await?;
        removed = removed.saturating_add(1);
    }
    if removed > 0 {
        tracing::info!(removed, "expired encrypted backup objects removed");
    }
    Ok(())
}

fn validate_uuid(value: &str) -> Result<()> {
    let parsed =
        Uuid::parse_str(value).map_err(|_| AppError::BadRequest("backup_id must be a UUID"))?;
    if parsed.to_string() != value {
        return Err(AppError::BadRequest(
            "backup_id must use canonical lowercase UUID form",
        ));
    }
    Ok(())
}

fn timestamp(epoch: i64) -> Result<String> {
    DateTime::<Utc>::from_timestamp(epoch, 0)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Secs, true))
        .ok_or_else(|| AppError::Internal("database timestamp was out of range".to_owned()))
}

async fn run_db<T, F>(state: &GatewayState, operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Database) -> Result<T> + Send + 'static,
{
    let database = state.database.clone();
    run_db_with_slots(database, state.db_slots.clone(), operation).await
}

async fn run_db_with_slots<T, F>(
    database: Database,
    db_slots: Arc<Semaphore>,
    operation: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Database) -> Result<T> + Send + 'static,
{
    let permit = db_slots
        .try_acquire_owned()
        .map_err(|_| AppError::ServiceUnavailable {
            retry_after_seconds: 1,
        })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation(&database)
    })
    .await
    .map_err(AppError::internal)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn db_concurrency_is_fail_fast() {
        let directory = tempfile::TempDir::new().unwrap();
        let database = Database::new(
            directory.path().join("gateway.sqlite3"),
            Arc::from(b"test-only-pepper-at-least-32-bytes".as_slice()),
        );
        let slots = Arc::new(Semaphore::new(1));
        let _held = slots.clone().try_acquire_owned().unwrap();
        let error = run_db_with_slots(database, slots, |_| Ok(()))
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::ServiceUnavailable { .. }));
    }
}
