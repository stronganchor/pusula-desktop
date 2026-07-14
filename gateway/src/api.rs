#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::Duration,
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
    b2::B2Client,
    crypto::normalize_sha256,
    db::{AuthenticatedDevice, BackupRecord, Database},
    error::{AppError, Result},
};

#[derive(Clone)]
pub struct GatewayState {
    database: Database,
    b2: B2Client,
    object_prefix: Arc<str>,
    relay_directory: Arc<PathBuf>,
    relay_slots: Arc<Semaphore>,
    max_backup_bytes: u64,
    rate_capacity: u32,
    rate_refill: Duration,
}

impl GatewayState {
    pub fn new(
        database: Database,
        b2: B2Client,
        object_prefix: impl Into<Arc<str>>,
        max_backup_bytes: u64,
        rate_capacity: u32,
        rate_refill: Duration,
    ) -> Self {
        let relay_directory = relay_directory(database.path());
        Self {
            database,
            b2,
            object_prefix: object_prefix.into(),
            relay_directory: Arc::new(relay_directory),
            relay_slots: Arc::new(Semaphore::new(1)),
            max_backup_bytes,
            rate_capacity,
            rate_refill,
        }
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
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/enroll", post(enroll))
        .route("/v1/backups/upload-url", post(upload_url))
        .route("/v1/backups/relay/{backup_id}", put(relay_backup))
        .route("/v1/backups/complete", post(complete_backup))
        .route("/v1/backups/status", get(backup_status))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
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
    let capacity = state.rate_capacity;
    let credential = run_db(state.database, move |database| {
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

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum RetentionClass {
    #[default]
    Rolling,
    Daily,
    Monthly,
}

impl RetentionClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rolling => "rolling",
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }
}

#[derive(Serialize)]
struct UploadUrlResponse {
    backup_id: String,
    retention_class: RetentionClass,
    method: &'static str,
    upload_url: String,
    required_headers: std::collections::BTreeMap<String, String>,
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
    let presigned = state
        .b2
        .presign_put(&object_key, request.content_length, &checksum, now)?;
    let expires_at = now
        + chrono::Duration::from_std(presigned_ttl(&presigned, now)?)
            .map_err(AppError::internal)?;
    let expires_epoch = expires_at.timestamp();
    let refill_seconds = state.rate_refill.as_secs();
    let capacity = state.rate_capacity;
    let device_id = device.id;
    let db_backup_id = backup_id.clone();
    let db_object_key = object_key;
    let db_checksum = checksum;
    run_db(state.database, move |database| {
        database.reserve_backup(
            &device_id,
            &db_backup_id,
            &db_object_key,
            request.content_length,
            &db_checksum,
            expires_epoch,
            capacity,
            refill_seconds,
        )
    })
    .await?;

    Ok(Json(UploadUrlResponse {
        backup_id,
        retention_class: request.retention_class,
        method: "PUT",
        upload_url: presigned.url,
        required_headers: presigned.headers,
        expires_at: presigned.expires_at,
    }))
}

fn presigned_ttl(presigned: &crate::b2::PresignedUpload, now: DateTime<Utc>) -> Result<Duration> {
    let expires_at = DateTime::parse_from_rfc3339(&presigned.expires_at)
        .map_err(AppError::internal)?
        .with_timezone(&Utc);
    (expires_at - now).to_std().map_err(AppError::internal)
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
    version_id: Option<String>,
}

async fn complete_backup(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(request): Json<CompleteRequest>,
) -> Result<Json<CompleteResponse>> {
    validate_uuid(&request.backup_id)?;
    let device = authenticate(&state, &headers).await?;
    let device_id = device.id;
    let backup_id = request.backup_id;
    let lookup_device_id = device_id.clone();
    let lookup_backup_id = backup_id.clone();
    let backup = run_db(state.database.clone(), move |database| {
        database.backup_for_device(&lookup_device_id, &lookup_backup_id)
    })
    .await?;

    if backup.status == "completed" {
        return Ok(Json(completion_response(backup)?));
    }
    let verified = state
        .b2
        .verify_object(&backup.object_key, backup.size_bytes, &backup.sha256)
        .await?;
    let completed = run_db(state.database, move |database| {
        database.mark_backup_completed(
            &device_id,
            &backup_id,
            verified.etag.as_deref(),
            verified.version_id.as_deref(),
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
    let device_id = device.id;
    let lookup_device_id = device_id.clone();
    let lookup_backup_id = backup_id.clone();
    let backup = run_db(state.database.clone(), move |database| {
        database.backup_for_device(&lookup_device_id, &lookup_backup_id)
    })
    .await?;

    validate_relay_headers(&headers, backup.size_bytes)?;
    if backup.status == "completed" {
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

    let _relay_slot =
        state
            .relay_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| AppError::RateLimited {
                retry_after_seconds: 5,
            })?;
    let admission_database = state.database.clone();
    let admission_device_id = device_id.clone();
    let admission_backup_id = backup_id.clone();
    let rate_capacity = state.rate_capacity;
    let refill_seconds = state.rate_refill.as_secs();
    run_db(admission_database, move |database| {
        database.begin_relay_attempt(
            &admission_device_id,
            &admission_backup_id,
            rate_capacity,
            refill_seconds,
        )
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
        .b2
        .upload_ciphertext(
            &backup.object_key,
            backup.size_bytes,
            &backup.sha256,
            spool.path(),
        )
        .await;
    spool.cleanup().await?;
    let verified = upload_result?;

    let completed = run_db(state.database, move |database| {
        database.mark_backup_completed(
            &device_id,
            &backup_id,
            verified.etag.as_deref(),
            verified.version_id.as_deref(),
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
    file.shutdown().await.map_err(AppError::internal)?;
    drop(file);
    Ok(spool)
}

fn completion_response(backup: BackupRecord) -> Result<CompleteResponse> {
    Ok(CompleteResponse {
        backup_id: backup.id,
        status: backup.status,
        completed_at: timestamp(
            backup.completed_at.ok_or_else(|| {
                AppError::Internal("completed backup has no timestamp".to_owned())
            })?,
        )?,
        etag: backup.etag,
        version_id: backup.version_id,
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
    let summary = run_db(state.database, move |database| {
        database.backup_summary(&lookup_id)
    })
    .await?;
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
    run_db(state.database.clone(), move |database| {
        database.authenticate(&token)
    })
    .await
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

async fn run_db<T, F>(database: Database, operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Database) -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || operation(&database))
        .await
        .map_err(AppError::internal)?
}
