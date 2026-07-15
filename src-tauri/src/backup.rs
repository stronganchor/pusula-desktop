use std::{
    fs::{self, File},
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use age::x25519;
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveDate, Utc};
use reqwest::{Client, StatusCode};
use rusqlite::{backup::Backup, Connection, DatabaseName};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder as TempfileBuilder;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::io::ReaderStream;
use url::Url;
use uuid::Uuid;

use crate::db::Database;

const GATEWAY_URL: &str = "https://pusula-backup.stronganchortech.com/";
const RECOVERY_RECIPIENT: &str = "age1ht9yu6avu79sxq0w3s68t9gh3u853q7q9aehhjw7h2w68zw0yq2qpyv059";
const CREDENTIAL_SERVICE_SUFFIX: &str = ".backup";
const CREDENTIAL_ACCOUNT: &str = "device-token";
const QUEUE_METADATA_VERSION: u32 = 1;
const SCHEDULE_INTERVAL_HOURS: i64 = 24;
const FAILURE_RETRY_HOURS: i64 = 6;
const LOCAL_RECOVERY_LIMIT: usize = 3;
const UPLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const QUARANTINE_DIRECTORY: &str = "quarantine";
const GATEWAY_RELAY_TRANSPORT: &str = "gateway_relay";

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("backup filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("backup database snapshot failed: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("backup metadata is invalid: {0}")]
    Json(#[from] serde_json::Error),

    #[error("backup encryption failed")]
    Encryption,

    #[error("backup gateway is unavailable")]
    GatewayUnavailable,

    #[error("backup gateway rejected the request ({0})")]
    GatewayRejected(u16),

    #[error("backup gateway returned an invalid response")]
    InvalidGatewayResponse,

    #[error("backup credential storage is unavailable")]
    CredentialStore,

    #[error("backup configuration is invalid")]
    Configuration,

    #[error("backup task could not be completed")]
    Task,
}

type BackupResult<T> = Result<T, BackupError>;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
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

    fn pending_limit(self) -> usize {
        match self {
            Self::Rolling => 14,
            Self::Daily => 8,
            Self::Monthly => 4,
        }
    }
}

impl FromStr for RetentionClass {
    type Err = BackupError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "rolling" => Ok(Self::Rolling),
            "daily" => Ok(Self::Daily),
            "monthly" => Ok(Self::Monthly),
            _ => Err(BackupError::Configuration),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupEnrollment {
    pub device_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupRunReport {
    pub encrypted_snapshot_created: bool,
    pub safe_to_continue: bool,
    pub retention_class: RetentionClass,
    pub created_at: String,
    pub uploaded_count: usize,
    pub pending_count: usize,
    pub local_recovery_count: usize,
    pub queue_healthy: bool,
    pub quarantined_file_count: usize,
    pub remote_result: RemoteResult,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteResult {
    Uploaded,
    QueuedOffline,
    NotEnrolled,
    LocalRecovery,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
pub struct RemoteBackupSummary {
    pub backup_id: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub completed_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
pub struct RemoteBackupStatus {
    pub device_id: String,
    pub server_time: String,
    pub active_pending_uploads: u64,
    pub expired_pending_uploads: u64,
    pub latest_completed: Option<RemoteBackupSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupStatusReport {
    pub enrolled: bool,
    pub needs_reenrollment: bool,
    pub device_id: Option<String>,
    pub pending_count: usize,
    pub local_recovery_count: usize,
    pub queue_healthy: bool,
    pub quarantined_file_count: usize,
    pub last_attempt_at: Option<String>,
    pub last_snapshot_at: Option<String>,
    pub last_remote_success_at: Option<String>,
    pub next_scheduled_at: Option<String>,
    pub remote: Option<RemoteBackupStatus>,
    pub gateway_reachable: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct BackupScheduleState {
    device_id: Option<String>,
    needs_reenrollment: bool,
    last_attempt_at: Option<String>,
    last_snapshot_at: Option<String>,
    last_remote_success_at: Option<String>,
    last_daily_period: Option<String>,
    last_monthly_period: Option<String>,
    next_remote_retry_at: Option<String>,
    next_scheduled_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct QueueMetadata {
    format_version: u32,
    created_at: String,
    retention_class: RetentionClass,
    size_bytes: u64,
    sha256: String,
    #[serde(default)]
    local_recovery: bool,
    #[serde(default)]
    scheduled_period: Option<String>,
    upload: Option<PendingUpload>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PendingUpload {
    backup_id: String,
    stage: UploadStage,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum UploadStage {
    Reserved,
    RelayPending,
    Uploaded,
}

#[derive(Debug, Clone)]
struct QueuedBackup {
    path: PathBuf,
    metadata_path: PathBuf,
    metadata: QueueMetadata,
}

trait TokenStore: Send + Sync {
    fn load(&self) -> BackupResult<Option<String>>;
    fn store(&self, token: &str) -> BackupResult<()>;
    fn delete(&self) -> BackupResult<()>;
}

#[derive(Debug)]
struct PlatformTokenStore {
    service: String,
}

impl PlatformTokenStore {
    fn for_app_identifier(app_identifier: &str) -> BackupResult<Self> {
        if app_identifier.trim().is_empty() || app_identifier.contains(char::is_whitespace) {
            return Err(BackupError::Configuration);
        }
        Ok(Self {
            service: format!("{app_identifier}{CREDENTIAL_SERVICE_SUFFIX}"),
        })
    }
}

#[cfg(windows)]
impl PlatformTokenStore {
    fn entry(&self) -> BackupResult<keyring::Entry> {
        keyring::Entry::new(&self.service, CREDENTIAL_ACCOUNT)
            .map_err(|_| BackupError::CredentialStore)
    }
}

#[cfg(windows)]
impl TokenStore for PlatformTokenStore {
    fn load(&self) -> BackupResult<Option<String>> {
        match self.entry()?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(BackupError::CredentialStore),
        }
    }

    fn store(&self, token: &str) -> BackupResult<()> {
        self.entry()?
            .set_password(token)
            .map_err(|_| BackupError::CredentialStore)
    }

    fn delete(&self) -> BackupResult<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(BackupError::CredentialStore),
        }
    }
}

#[cfg(not(windows))]
impl TokenStore for PlatformTokenStore {
    fn load(&self) -> BackupResult<Option<String>> {
        Ok(None)
    }

    fn store(&self, _token: &str) -> BackupResult<()> {
        Err(BackupError::CredentialStore)
    }

    fn delete(&self) -> BackupResult<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct GatewayClient {
    base_url: Url,
    client: Client,
}

#[derive(Debug, Serialize)]
struct EnrollRequest<'a> {
    enrollment_code: &'a str,
    device_name: &'a str,
}

#[derive(Debug, Deserialize)]
struct EnrollResponse {
    device_id: String,
    device_token: String,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct UploadUrlRequest<'a> {
    content_length: u64,
    sha256: &'a str,
    retention_class: RetentionClass,
}

#[derive(Debug, Clone, Deserialize)]
struct UploadUrlResponse {
    backup_id: String,
    retention_class: RetentionClass,
    transport: String,
}

#[derive(Debug, Serialize)]
struct CompleteRequest<'a> {
    backup_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct CompleteResponse {
    backup_id: String,
    status: String,
    completed_at: String,
    etag: Option<String>,
    version_id: String,
}

#[derive(Debug, Deserialize)]
struct GatewayErrorEnvelope {
    error: GatewayErrorBody,
}

#[derive(Debug, Deserialize)]
struct GatewayErrorBody {
    code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionProbe {
    Completed,
    ObjectNotPresent,
    BindingNotFound,
}

impl GatewayClient {
    fn production() -> BackupResult<Self> {
        Self::new(GATEWAY_URL, false, Duration::from_secs(45))
    }

    fn new(base_url: &str, allow_insecure_uploads: bool, timeout: Duration) -> BackupResult<Self> {
        let base_url = Url::parse(base_url).map_err(|_| BackupError::Configuration)?;
        if base_url.scheme() != "https" && !allow_insecure_uploads {
            return Err(BackupError::Configuration);
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10).min(timeout))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| BackupError::Configuration)?;
        Ok(Self { base_url, client })
    }

    fn endpoint(&self, path: &str) -> BackupResult<Url> {
        self.base_url
            .join(path)
            .map_err(|_| BackupError::Configuration)
    }

    async fn enroll(
        &self,
        enrollment_code: &str,
        device_name: &str,
    ) -> BackupResult<EnrollResponse> {
        let response = self
            .client
            .post(self.endpoint("v1/enroll")?)
            .json(&EnrollRequest {
                enrollment_code,
                device_name,
            })
            .send()
            .await
            .map_err(|_| BackupError::GatewayUnavailable)?;
        ensure_success(response.status())?;
        let enrollment: EnrollResponse = response
            .json()
            .await
            .map_err(|_| BackupError::InvalidGatewayResponse)?;
        validate_backup_id(&enrollment.device_id)?;
        Ok(enrollment)
    }

    async fn reserve_upload(
        &self,
        token: &str,
        queued: &QueuedBackup,
    ) -> BackupResult<UploadUrlResponse> {
        let response = self
            .client
            .post(self.endpoint("v1/backups/upload-url")?)
            .bearer_auth(token)
            .json(&UploadUrlRequest {
                content_length: queued.metadata.size_bytes,
                sha256: &queued.metadata.sha256,
                retention_class: queued.metadata.retention_class,
            })
            .send()
            .await
            .map_err(|_| BackupError::GatewayUnavailable)?;
        ensure_success(response.status())?;
        let reservation: UploadUrlResponse = response
            .json()
            .await
            .map_err(|_| BackupError::InvalidGatewayResponse)?;
        validate_backup_id(&reservation.backup_id)?;
        if reservation.retention_class != queued.metadata.retention_class {
            return Err(BackupError::InvalidGatewayResponse);
        }
        if reservation.transport != GATEWAY_RELAY_TRANSPORT {
            return Err(BackupError::InvalidGatewayResponse);
        }
        Ok(reservation)
    }

    async fn relay_ciphertext(
        &self,
        token: &str,
        queued: &QueuedBackup,
        backup_id: &str,
    ) -> BackupResult<()> {
        validate_backup_id(backup_id)?;
        let endpoint = self.endpoint(&format!("v1/backups/relay/{backup_id}"))?;
        let file = tokio::fs::File::open(&queued.path).await?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
        let response = self
            .client
            .put(endpoint)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header(
                reqwest::header::CONTENT_LENGTH,
                queued.metadata.size_bytes.to_string(),
            )
            .timeout(UPLOAD_REQUEST_TIMEOUT)
            .body(body)
            .send()
            .await
            .map_err(|_| BackupError::GatewayUnavailable)?;
        require_completed_response(response, backup_id).await
    }

    async fn complete(&self, token: &str, backup_id: &str) -> BackupResult<CompletionProbe> {
        validate_backup_id(backup_id)?;
        let response = self
            .client
            .post(self.endpoint("v1/backups/complete")?)
            .bearer_auth(token)
            .json(&CompleteRequest { backup_id })
            .timeout(UPLOAD_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|_| BackupError::GatewayUnavailable)?;
        completion_probe_response(response, backup_id).await
    }

    async fn status(&self, token: &str) -> BackupResult<RemoteBackupStatus> {
        let response = self
            .client
            .get(self.endpoint("v1/backups/status")?)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| BackupError::GatewayUnavailable)?;
        ensure_success(response.status())?;
        let status: RemoteBackupStatus = response
            .json()
            .await
            .map_err(|_| BackupError::InvalidGatewayResponse)?;
        validate_backup_id(&status.device_id)?;
        if let Some(latest) = status.latest_completed.as_ref() {
            validate_backup_id(&latest.backup_id)?;
        }
        Ok(status)
    }
}

async fn require_completed_response(
    response: reqwest::Response,
    expected_backup_id: &str,
) -> BackupResult<()> {
    match completion_probe_response(response, expected_backup_id).await? {
        CompletionProbe::Completed => Ok(()),
        CompletionProbe::ObjectNotPresent => Err(BackupError::GatewayRejected(409)),
        CompletionProbe::BindingNotFound => Err(BackupError::GatewayRejected(404)),
    }
}

async fn completion_probe_response(
    response: reqwest::Response,
    expected_backup_id: &str,
) -> BackupResult<CompletionProbe> {
    let status = response.status();
    if status.is_success() {
        let completion = response
            .json::<CompleteResponse>()
            .await
            .map_err(|_| BackupError::InvalidGatewayResponse)?;
        if completion.backup_id != expected_backup_id || completion.status != "completed" {
            return Err(BackupError::InvalidGatewayResponse);
        }
        DateTime::parse_from_rfc3339(&completion.completed_at)
            .map_err(|_| BackupError::InvalidGatewayResponse)?;
        if completion.version_id.is_empty()
            || completion.version_id.len() > 256
            || completion.version_id.chars().any(char::is_control)
            || completion
                .etag
                .as_deref()
                .is_some_and(|etag| etag.len() > 256 || etag.chars().any(char::is_control))
        {
            return Err(BackupError::InvalidGatewayResponse);
        }
        return Ok(CompletionProbe::Completed);
    }
    if matches!(status, StatusCode::NOT_FOUND | StatusCode::CONFLICT) {
        let error = response
            .json::<GatewayErrorEnvelope>()
            .await
            .map_err(|_| BackupError::InvalidGatewayResponse)?;
        return match (status, error.error.code.as_str()) {
            (StatusCode::CONFLICT, "object_not_present") => Ok(CompletionProbe::ObjectNotPresent),
            (StatusCode::NOT_FOUND, "not_found") => Ok(CompletionProbe::BindingNotFound),
            _ => Err(BackupError::GatewayRejected(status.as_u16())),
        };
    }
    Err(BackupError::GatewayRejected(status.as_u16()))
}

fn ensure_success(status: StatusCode) -> BackupResult<()> {
    if status.is_success() {
        Ok(())
    } else {
        Err(BackupError::GatewayRejected(status.as_u16()))
    }
}

fn gateway_auth_rejected(error: &BackupError) -> bool {
    matches!(error, BackupError::GatewayRejected(401 | 403))
}

#[derive(Clone)]
pub struct BackupService {
    database: Database,
    queue_dir: PathBuf,
    state_path: PathBuf,
    recipient: x25519::Recipient,
    gateway: GatewayClient,
    token_store: Arc<dyn TokenStore>,
    snapshot_lock: Arc<Mutex<()>>,
    flush_lock: Arc<Mutex<()>>,
    state_lock: Arc<Mutex<()>>,
}

impl BackupService {
    pub fn production(
        database: Database,
        app_data_dir: PathBuf,
        app_identifier: &str,
    ) -> BackupResult<Self> {
        Self::new(
            database,
            app_data_dir.join("backup-queue"),
            app_data_dir.join("backup-state.json"),
            RECOVERY_RECIPIENT,
            GatewayClient::production()?,
            Arc::new(PlatformTokenStore::for_app_identifier(app_identifier)?),
        )
    }

    fn new(
        database: Database,
        queue_dir: PathBuf,
        state_path: PathBuf,
        recipient: &str,
        gateway: GatewayClient,
        token_store: Arc<dyn TokenStore>,
    ) -> BackupResult<Self> {
        let recipient = recipient
            .parse::<x25519::Recipient>()
            .map_err(|_| BackupError::Configuration)?;
        fs::create_dir_all(&queue_dir)?;
        if let Some(parent) = state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Self {
            database,
            queue_dir,
            state_path,
            recipient,
            gateway,
            token_store,
            snapshot_lock: Arc::new(Mutex::new(())),
            flush_lock: Arc::new(Mutex::new(())),
            state_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn enroll(
        &self,
        enrollment_code: String,
        device_name: String,
    ) -> BackupResult<BackupEnrollment> {
        let _guard = self.flush_lock.lock().await;
        let enrollment_code = enrollment_code.trim();
        let device_name = device_name.trim();
        if enrollment_code.is_empty()
            || enrollment_code.len() > 512
            || device_name.is_empty()
            || device_name.len() > 100
        {
            return Err(BackupError::Configuration);
        }

        let enrollment = self.gateway.enroll(enrollment_code, device_name).await?;
        self.token_store.store(&enrollment.device_token)?;
        let _state_guard = self.state_lock.lock().await;
        let mut state = self.read_state()?;
        state.device_id = Some(enrollment.device_id.clone());
        state.needs_reenrollment = false;
        state.next_remote_retry_at = Some(Utc::now().to_rfc3339());
        state.next_scheduled_at = Some(Utc::now().to_rfc3339());
        if let Err(error) = self.write_state(&state) {
            let _ = self.token_store.delete();
            return Err(error);
        }

        Ok(BackupEnrollment {
            device_id: enrollment.device_id,
            created_at: enrollment.created_at,
        })
    }

    pub async fn backup_now(
        &self,
        retention_class: RetentionClass,
    ) -> BackupResult<BackupRunReport> {
        let permit = self.reserve_snapshot().await;
        let queued = self
            .create_local_snapshot_locked(retention_class, false, None)
            .await?;
        self.record_snapshot(&queued, Utc::now()).await?;
        drop(permit);
        let (uploaded_count, remote_result) = self.flush_existing_queue().await?;
        self.snapshot_report(&queued, uploaded_count, remote_result)
    }

    #[cfg(test)]
    async fn prepare_for_update(&self) -> BackupResult<BackupRunReport> {
        let permit = self.reserve_snapshot().await;
        self.prepare_for_update_reserved(permit).await
    }

    pub(crate) async fn reserve_snapshot(&self) -> OwnedMutexGuard<()> {
        self.snapshot_lock.clone().lock_owned().await
    }

    pub(crate) async fn prepare_for_update_reserved(
        &self,
        permit: OwnedMutexGuard<()>,
    ) -> BackupResult<BackupRunReport> {
        let queued = self
            .create_local_snapshot_locked(RetentionClass::Rolling, false, None)
            .await?;
        self.record_snapshot(&queued, Utc::now()).await?;
        drop(permit);
        self.prune_queue_if_flush_idle()?;
        let remote_result = if self.token_store.load()?.is_some() {
            RemoteResult::QueuedOffline
        } else {
            RemoteResult::NotEnrolled
        };
        self.snapshot_report(&queued, 0, remote_result)
    }

    #[cfg(test)]
    pub async fn prepare_for_destructive_import(&self) -> BackupResult<BackupRunReport> {
        let permit = self.reserve_snapshot().await;
        self.prepare_for_destructive_import_reserved(permit).await
    }

    pub(crate) async fn prepare_for_destructive_import_reserved(
        &self,
        permit: OwnedMutexGuard<()>,
    ) -> BackupResult<BackupRunReport> {
        let queued = self
            .create_local_snapshot_locked(RetentionClass::Rolling, true, None)
            .await?;
        self.record_snapshot(&queued, Utc::now()).await?;
        drop(permit);
        self.prune_queue_if_flush_idle()?;
        self.snapshot_report(&queued, 0, RemoteResult::LocalRecovery)
    }

    fn prune_queue_if_flush_idle(&self) -> BackupResult<()> {
        let Ok(_guard) = self.flush_lock.try_lock() else {
            return Ok(());
        };
        self.prune_queue()
    }

    async fn create_local_snapshot_locked(
        &self,
        retention_class: RetentionClass,
        local_recovery: bool,
        scheduled_period: Option<String>,
    ) -> BackupResult<QueuedBackup> {
        let database = self.database.clone();
        let queue_dir = self.queue_dir.clone();
        let recipient = self.recipient.clone();
        let queued = tokio::task::spawn_blocking(move || {
            create_encrypted_snapshot(
                &database,
                &queue_dir,
                &recipient,
                retention_class,
                local_recovery,
                scheduled_period,
            )
        })
        .await
        .map_err(|_| BackupError::Task)??;
        Ok(queued)
    }

    async fn record_snapshot(
        &self,
        queued: &QueuedBackup,
        attempted_at: DateTime<Utc>,
    ) -> BackupResult<()> {
        let _state_guard = self.state_lock.lock().await;
        let mut state = self.read_state()?;
        state.last_attempt_at = Some(attempted_at.to_rfc3339());
        state.last_snapshot_at = Some(queued.metadata.created_at.clone());
        if !queued.metadata.local_recovery {
            state.next_remote_retry_at = Some(attempted_at.to_rfc3339());
        }
        if let Some(period) = queued.metadata.scheduled_period.as_deref() {
            if let Some(value) = period.strip_prefix("daily:") {
                state.last_daily_period = Some(value.to_owned());
            } else if let Some(value) = period.strip_prefix("monthly:") {
                state.last_monthly_period = Some(value.to_owned());
            }
        }
        state.next_scheduled_at =
            Some((attempted_at + ChronoDuration::hours(SCHEDULE_INTERVAL_HOURS)).to_rfc3339());
        self.write_state(&state)
    }

    fn snapshot_report(
        &self,
        queued: &QueuedBackup,
        uploaded_count: usize,
        remote_result: RemoteResult,
    ) -> BackupResult<BackupRunReport> {
        let queue = self.list_queue()?;
        let pending_count = queue
            .iter()
            .filter(|item| !item.metadata.local_recovery)
            .count();
        let local_recovery_count = queue
            .iter()
            .filter(|item| item.metadata.local_recovery)
            .count();
        let quarantined_file_count = self.quarantined_file_count()?;
        Ok(BackupRunReport {
            encrypted_snapshot_created: true,
            safe_to_continue: true,
            retention_class: queued.metadata.retention_class,
            created_at: queued.metadata.created_at.clone(),
            uploaded_count,
            pending_count,
            local_recovery_count,
            queue_healthy: quarantined_file_count == 0,
            quarantined_file_count,
            remote_result,
        })
    }

    async fn flush_existing_queue(&self) -> BackupResult<(usize, RemoteResult)> {
        let _flush_guard = self.flush_lock.lock().await;
        // Retention mutates the same queue that a remote flush consumes. Keep it
        // in the serialized flush lane so a stalled upload can never race local
        // pruning, while snapshot creation and update preflight remain network-free.
        self.prune_queue()?;
        let attempted_at = Utc::now();
        let token = match self.token_store.load()? {
            Some(token) => token,
            None => {
                let _state_guard = self.state_lock.lock().await;
                let mut state = self.read_state()?;
                state.next_remote_retry_at = None;
                state.next_scheduled_at = Some(
                    (attempted_at + ChronoDuration::hours(SCHEDULE_INTERVAL_HOURS)).to_rfc3339(),
                );
                self.write_state(&state)?;
                return Ok((0, RemoteResult::NotEnrolled));
            }
        };
        {
            let _state_guard = self.state_lock.lock().await;
            let mut state = self.read_state()?;
            state.last_attempt_at = Some(attempted_at.to_rfc3339());
            self.write_state(&state)?;
        }

        match self.flush_queue(&token).await {
            Ok(uploaded_count) => {
                let pending = self
                    .list_queue()?
                    .iter()
                    .any(|item| !item.metadata.local_recovery);
                let _state_guard = self.state_lock.lock().await;
                let mut state = self.read_state()?;
                if uploaded_count > 0 {
                    state.last_remote_success_at = Some(Utc::now().to_rfc3339());
                }
                state.next_remote_retry_at = pending.then(|| {
                    (attempted_at + ChronoDuration::hours(FAILURE_RETRY_HOURS)).to_rfc3339()
                });
                state.next_scheduled_at = Some(
                    (attempted_at
                        + ChronoDuration::hours(if pending {
                            FAILURE_RETRY_HOURS
                        } else {
                            SCHEDULE_INTERVAL_HOURS
                        }))
                    .to_rfc3339(),
                );
                self.write_state(&state)?;
                Ok((
                    uploaded_count,
                    if uploaded_count > 0 {
                        RemoteResult::Uploaded
                    } else {
                        RemoteResult::QueuedOffline
                    },
                ))
            }
            Err(error) if gateway_auth_rejected(&error) => {
                self.token_store.delete()?;
                let _state_guard = self.state_lock.lock().await;
                let mut state = self.read_state()?;
                state.needs_reenrollment = true;
                state.next_remote_retry_at = None;
                state.next_scheduled_at = Some(
                    (attempted_at + ChronoDuration::hours(SCHEDULE_INTERVAL_HOURS)).to_rfc3339(),
                );
                self.write_state(&state)?;
                Ok((0, RemoteResult::NotEnrolled))
            }
            Err(_) => {
                let retry_at = attempted_at + ChronoDuration::hours(FAILURE_RETRY_HOURS);
                let _state_guard = self.state_lock.lock().await;
                let mut state = self.read_state()?;
                state.next_remote_retry_at = Some(retry_at.to_rfc3339());
                state.next_scheduled_at = Some(retry_at.to_rfc3339());
                self.write_state(&state)?;
                Ok((0, RemoteResult::QueuedOffline))
            }
        }
    }

    async fn flush_queue(&self, token: &str) -> BackupResult<usize> {
        let mut uploaded_count = 0;
        for mut queued in self.list_queue()? {
            if queued.metadata.local_recovery {
                continue;
            }
            if verify_queued_backup(&queued).is_err() {
                self.quarantine_backup(&queued, "ciphertext")?;
                continue;
            }
            let mut binding_resets = 0_u8;
            loop {
                let pending = if let Some(pending) = queued.metadata.upload.clone() {
                    pending
                } else {
                    let reserved = self.gateway.reserve_upload(token, &queued).await?;
                    validate_backup_id(&reserved.backup_id)?;
                    let pending = PendingUpload {
                        backup_id: reserved.backup_id.clone(),
                        stage: UploadStage::RelayPending,
                    };
                    queued.metadata.upload = Some(pending.clone());
                    write_json_atomic(&queued.metadata_path, &queued.metadata)?;
                    pending
                };

                if validate_backup_id(&pending.backup_id).is_err() {
                    // A malformed local sidecar must never be sent to the
                    // gateway, but the verified ciphertext remains useful.
                    queued.metadata.upload = None;
                    write_json_atomic(&queued.metadata_path, &queued.metadata)?;
                    continue;
                }

                match self.gateway.complete(token, &pending.backup_id).await? {
                    CompletionProbe::Completed => {
                        remove_queued_backup(&queued)?;
                        uploaded_count += 1;
                        break;
                    }
                    CompletionProbe::BindingNotFound => {
                        binding_resets += 1;
                        queued.metadata.upload = None;
                        write_json_atomic(&queued.metadata_path, &queued.metadata)?;
                        if binding_resets > 1 {
                            return Err(BackupError::GatewayRejected(404));
                        }
                        continue;
                    }
                    CompletionProbe::ObjectNotPresent => {
                        // Reserved and Uploaded are legacy direct-B2 stages.
                        // Persist the relay-only intent before any network body
                        // leaves the machine so a lost response is resumable.
                        if pending.stage != UploadStage::RelayPending {
                            queued.metadata.upload = Some(PendingUpload {
                                backup_id: pending.backup_id.clone(),
                                stage: UploadStage::RelayPending,
                            });
                            write_json_atomic(&queued.metadata_path, &queued.metadata)?;
                        }
                        match self
                            .gateway
                            .relay_ciphertext(token, &queued, &pending.backup_id)
                            .await
                        {
                            Ok(()) => {
                                remove_queued_backup(&queued)?;
                                uploaded_count += 1;
                                break;
                            }
                            Err(BackupError::GatewayRejected(404)) => {
                                binding_resets += 1;
                                queued.metadata.upload = None;
                                write_json_atomic(&queued.metadata_path, &queued.metadata)?;
                                if binding_resets > 1 {
                                    return Err(BackupError::GatewayRejected(404));
                                }
                                continue;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
        }
        Ok(uploaded_count)
    }

    pub async fn status(&self) -> BackupResult<BackupStatusReport> {
        // Queue files can be removed or rebound during a flush. Serialize the
        // status snapshot with those mutations, but not with local DB snapshots.
        let _flush_guard = self.flush_lock.lock().await;
        let queue = self.list_queue()?;
        let pending_count = queue
            .iter()
            .filter(|item| !item.metadata.local_recovery)
            .count();
        let local_recovery_count = queue
            .iter()
            .filter(|item| item.metadata.local_recovery)
            .count();
        let quarantined_file_count = self.quarantined_file_count()?;
        let mut state = {
            let _state_guard = self.state_lock.lock().await;
            self.read_state()?
        };
        let token = self.token_store.load()?;
        let mut enrolled = token.is_some();
        let (remote, gateway_reachable) = if let Some(token) = token {
            match self.gateway.status(&token).await {
                Ok(remote) => {
                    // The authenticated gateway response is authoritative.
                    // Reconcile a missing/stale local state file after a crash
                    // between keyring and state persistence (or state restore).
                    state.device_id = Some(remote.device_id.clone());
                    state.needs_reenrollment = false;
                    let _state_guard = self.state_lock.lock().await;
                    self.write_state(&state)?;
                    (Some(remote), Some(true))
                }
                Err(error) if gateway_auth_rejected(&error) => {
                    self.token_store.delete()?;
                    enrolled = false;
                    state.needs_reenrollment = true;
                    state.next_remote_retry_at = None;
                    let _state_guard = self.state_lock.lock().await;
                    self.write_state(&state)?;
                    (None, Some(true))
                }
                Err(BackupError::InvalidGatewayResponse) => {
                    return Err(BackupError::InvalidGatewayResponse);
                }
                Err(_) => (None, Some(false)),
            }
        } else {
            (None, None)
        };
        Ok(BackupStatusReport {
            enrolled,
            needs_reenrollment: state.needs_reenrollment,
            device_id: state.device_id,
            pending_count,
            local_recovery_count,
            queue_healthy: quarantined_file_count == 0,
            quarantined_file_count,
            last_attempt_at: state.last_attempt_at,
            last_snapshot_at: state.last_snapshot_at,
            last_remote_success_at: state.last_remote_success_at,
            next_scheduled_at: state.next_scheduled_at,
            remote,
            gateway_reachable,
        })
    }

    pub async fn run_scheduled_if_due(&self) {
        let _ = self
            .run_scheduled_at(Utc::now(), Local::now().date_naive())
            .await;
    }

    async fn run_scheduled_at(
        &self,
        now: DateTime<Utc>,
        business_date: NaiveDate,
    ) -> BackupResult<()> {
        let daily_period = business_date.format("%Y-%m-%d").to_string();
        let monthly_period = business_date.format("%Y-%m").to_string();
        let daily_key = format!("daily:{daily_period}");
        let monthly_key = format!("monthly:{monthly_period}");
        let permit = self.reserve_snapshot().await;
        let mut state = {
            let _state_guard = self.state_lock.lock().await;
            self.read_state()?
        };
        let queued = self.list_queue()?;
        let daily_materialized = state.last_daily_period.as_deref() == Some(&daily_period)
            || queued
                .iter()
                .any(|item| item.metadata.scheduled_period.as_deref() == Some(daily_key.as_str()));
        let monthly_materialized = state.last_monthly_period.as_deref() == Some(&monthly_period)
            || queued.iter().any(|item| {
                item.metadata.scheduled_period.as_deref() == Some(monthly_key.as_str())
            });
        let mut created = false;

        if !daily_materialized {
            let snapshot = self
                .create_local_snapshot_locked(RetentionClass::Daily, false, Some(daily_key.clone()))
                .await?;
            self.record_snapshot(&snapshot, now).await?;
            state.last_daily_period = Some(daily_period.clone());
            created = true;
        } else if state.last_daily_period.as_deref() != Some(&daily_period) {
            state.last_daily_period = Some(daily_period.clone());
        }

        if !monthly_materialized {
            let snapshot = self
                .create_local_snapshot_locked(
                    RetentionClass::Monthly,
                    false,
                    Some(monthly_key.clone()),
                )
                .await?;
            self.record_snapshot(&snapshot, now).await?;
            state.last_monthly_period = Some(monthly_period.clone());
            created = true;
        } else if state.last_monthly_period.as_deref() != Some(&monthly_period) {
            state.last_monthly_period = Some(monthly_period.clone());
        }

        {
            let _state_guard = self.state_lock.lock().await;
            let mut persisted = self.read_state()?;
            persisted.last_daily_period = state.last_daily_period;
            persisted.last_monthly_period = state.last_monthly_period;
            self.write_state(&persisted)?;
            state = persisted;
        }
        drop(permit);

        let pending = self
            .list_queue()?
            .iter()
            .any(|item| !item.metadata.local_recovery);
        let retry_due = state
            .next_remote_retry_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|retry| retry.with_timezone(&Utc) <= now)
            .unwrap_or(pending);
        if created || (pending && retry_due) {
            let _ = self.flush_existing_queue().await?;
        }
        Ok(())
    }

    fn read_state(&self) -> BackupResult<BackupScheduleState> {
        match fs::read(&self.state_path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(BackupScheduleState::default())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn write_state(&self, state: &BackupScheduleState) -> BackupResult<()> {
        write_json_atomic(&self.state_path, state)
    }

    fn list_queue(&self) -> BackupResult<Vec<QueuedBackup>> {
        let mut queued = Vec::new();
        for entry in fs::read_dir(&self.queue_dir)? {
            let path = entry?.path();
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !file_name.ends_with(".sqlite3.age") {
                continue;
            }
            let metadata_path = queue_metadata_path(&path);
            let metadata = match fs::read(&metadata_path) {
                Ok(bytes) => match serde_json::from_slice::<QueueMetadata>(&bytes) {
                    Ok(metadata) if metadata.format_version == QUEUE_METADATA_VERSION => metadata,
                    Ok(_) | Err(_) => {
                        quarantine_file(&metadata_path, &self.queue_dir, "metadata")?;
                        match reconstruct_queue_metadata(&path, &metadata_path) {
                            Ok(metadata) => metadata,
                            Err(_) => {
                                self.quarantine_paths(&path, &metadata_path, "unreadable")?;
                                continue;
                            }
                        }
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match reconstruct_queue_metadata(&path, &metadata_path) {
                        Ok(metadata) => metadata,
                        Err(_) => {
                            self.quarantine_paths(&path, &metadata_path, "unreadable")?;
                            continue;
                        }
                    }
                }
                Err(error) => return Err(error.into()),
            };
            queued.push(QueuedBackup {
                path,
                metadata_path,
                metadata,
            });
        }
        queued.sort_by(|left, right| left.metadata.created_at.cmp(&right.metadata.created_at));
        Ok(queued)
    }

    fn quarantine_backup(&self, queued: &QueuedBackup, reason: &str) -> BackupResult<()> {
        self.quarantine_paths(&queued.path, &queued.metadata_path, reason)
    }

    fn quarantine_paths(
        &self,
        ciphertext_path: &Path,
        metadata_path: &Path,
        reason: &str,
    ) -> BackupResult<()> {
        quarantine_file(ciphertext_path, &self.queue_dir, reason)?;
        quarantine_file(metadata_path, &self.queue_dir, reason)?;
        Ok(())
    }

    fn quarantined_file_count(&self) -> BackupResult<usize> {
        let quarantine_dir = self.queue_dir.join(QUARANTINE_DIRECTORY);
        let entries = match fs::read_dir(quarantine_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let mut count = 0;
        for entry in entries {
            if entry?.file_type()?.is_file() {
                count += 1;
            }
        }
        Ok(count)
    }

    fn prune_queue(&self) -> BackupResult<()> {
        let queued = self.list_queue()?;
        for retention in [
            RetentionClass::Rolling,
            RetentionClass::Daily,
            RetentionClass::Monthly,
        ] {
            let matching: Vec<&QueuedBackup> = queued
                .iter()
                .filter(|item| {
                    !item.metadata.local_recovery && item.metadata.retention_class == retention
                })
                .collect();
            let remove_count = matching.len().saturating_sub(retention.pending_limit());
            for item in matching.into_iter().take(remove_count) {
                remove_queued_backup(item)?;
            }
        }
        let local_recoveries: Vec<&QueuedBackup> = queued
            .iter()
            .filter(|item| item.metadata.local_recovery)
            .collect();
        let remove_count = local_recoveries.len().saturating_sub(LOCAL_RECOVERY_LIMIT);
        for item in local_recoveries.into_iter().take(remove_count) {
            remove_queued_backup(item)?;
        }
        Ok(())
    }
}

fn validate_backup_id(value: &str) -> BackupResult<()> {
    let parsed = Uuid::parse_str(value).map_err(|_| BackupError::InvalidGatewayResponse)?;
    if parsed.to_string() != value {
        return Err(BackupError::InvalidGatewayResponse);
    }
    Ok(())
}

fn create_encrypted_snapshot(
    database: &Database,
    queue_dir: &Path,
    recipient: &x25519::Recipient,
    retention_class: RetentionClass,
    local_recovery: bool,
    scheduled_period: Option<String>,
) -> BackupResult<QueuedBackup> {
    fs::create_dir_all(queue_dir)?;
    let created_at = Utc::now();
    let snapshot_kind = if local_recovery {
        "local-recovery"
    } else {
        retention_class.as_str()
    };
    let file_name = format!(
        "backup-{}-{}-{}.sqlite3.age",
        created_at.format("%Y%m%dT%H%M%SZ"),
        Uuid::new_v4(),
        snapshot_kind
    );
    let final_path = queue_dir.join(file_name);
    let mut partial = TempfileBuilder::new()
        .prefix("pusula-backup-")
        .suffix(".partial")
        .tempfile_in(queue_dir)?;

    let encryptor = age::Encryptor::with_recipients(std::iter::once(recipient as _))
        .map_err(|_| BackupError::Encryption)?;
    let mut encrypted_writer = encryptor
        .wrap_output(partial.as_file_mut())
        .map_err(|_| BackupError::Encryption)?;

    // SQLite's online backup API takes one consistent snapshot into an in-memory
    // connection. Serializing that connection lets age consume the bytes directly;
    // plaintext is never placed in a filesystem temporary file.
    let source = Connection::open(database.path())?;
    let mut destination = Connection::open_in_memory()?;
    let backup = Backup::new(&source, &mut destination)?;
    backup.run_to_completion(128, Duration::from_millis(5), None)?;
    drop(backup);
    let integrity: String =
        destination.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(BackupError::Database(rusqlite::Error::InvalidQuery));
    }
    let serialized = destination.serialize(DatabaseName::Main)?;
    let mut plaintext_reader = &serialized[..];
    std::io::copy(&mut plaintext_reader, &mut encrypted_writer)?;
    let encrypted_file = encrypted_writer
        .finish()
        .map_err(|_| BackupError::Encryption)?;
    encrypted_file.flush()?;
    encrypted_file.sync_all()?;

    let size_bytes = partial.as_file().metadata()?.len();
    let sha256 = sha256_file(partial.path())?;
    let metadata_path = queue_metadata_path(&final_path);
    let metadata = QueueMetadata {
        format_version: QUEUE_METADATA_VERSION,
        created_at: created_at.to_rfc3339(),
        retention_class,
        size_bytes,
        sha256,
        local_recovery,
        scheduled_period,
        upload: None,
    };
    // The queue scanner discovers work by ciphertext filename. Publish the
    // durable metadata first, then atomically rename the ciphertext into view;
    // a concurrent flush can therefore never reconstruct an incomplete item.
    write_json_atomic(&metadata_path, &metadata)?;
    if let Err(error) = partial.persist(&final_path) {
        let _ = fs::remove_file(&metadata_path);
        return Err(BackupError::Io(error.error));
    }
    sync_parent_directory(queue_dir)?;

    Ok(QueuedBackup {
        path: final_path,
        metadata_path,
        metadata,
    })
}

fn verify_queued_backup(queued: &QueuedBackup) -> BackupResult<()> {
    let metadata = fs::metadata(&queued.path)?;
    if metadata.len() != queued.metadata.size_bytes
        || sha256_file(&queued.path)? != queued.metadata.sha256
    {
        return Err(BackupError::Encryption);
    }
    Ok(())
}

fn sha256_file(path: &Path) -> BackupResult<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn queue_metadata_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".json");
    PathBuf::from(value)
}

fn reconstruct_queue_metadata(path: &Path, metadata_path: &Path) -> BackupResult<QueueMetadata> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(BackupError::Configuration)?;
    let local_recovery = file_name.ends_with("-local-recovery.sqlite3.age");
    let retention_class = if local_recovery {
        RetentionClass::Rolling
    } else {
        [
            RetentionClass::Rolling,
            RetentionClass::Daily,
            RetentionClass::Monthly,
        ]
        .into_iter()
        .find(|retention| file_name.ends_with(&format!("-{}.sqlite3.age", retention.as_str())))
        .ok_or(BackupError::Configuration)?
    };
    let file_metadata = fs::metadata(path)?;
    let created_at = DateTime::<Utc>::from(file_metadata.modified()?);
    let metadata = QueueMetadata {
        format_version: QUEUE_METADATA_VERSION,
        created_at: created_at.to_rfc3339(),
        retention_class,
        size_bytes: file_metadata.len(),
        sha256: sha256_file(path)?,
        local_recovery,
        scheduled_period: None,
        upload: None,
    };
    write_json_atomic(metadata_path, &metadata)?;
    Ok(metadata)
}

fn quarantine_file(path: &Path, queue_dir: &Path, reason: &str) -> BackupResult<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let original_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(BackupError::Configuration)?;
    let quarantine_dir = queue_dir.join(QUARANTINE_DIRECTORY);
    fs::create_dir_all(&quarantine_dir)?;
    let target = quarantine_dir.join(format!(
        "{}-{}-{}-{}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        reason,
        Uuid::new_v4(),
        original_name
    ));
    fs::rename(path, &target)?;
    sync_parent_directory(&quarantine_dir)?;
    sync_parent_directory(queue_dir)?;
    Ok(true)
}

fn remove_queued_backup(queued: &QueuedBackup) -> BackupResult<()> {
    match fs::remove_file(&queued.path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match fs::remove_file(&queued.metadata_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> BackupResult<()> {
    let parent = path.parent().ok_or(BackupError::Configuration)?;
    fs::create_dir_all(parent)?;
    let mut temporary = TempfileBuilder::new()
        .prefix("pusula-metadata-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    serde_json::to_writer(temporary.as_file_mut(), value)?;
    temporary.as_file_mut().write_all(b"\n")?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| BackupError::Io(error.error))?;
    sync_parent_directory(parent)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> BackupResult<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> BackupResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex as StdMutex,
        },
        time::Duration,
    };

    use age::x25519;
    use axum::{
        body::Bytes,
        extract::{Path as AxumPath, State},
        http::{HeaderMap as AxumHeaders, StatusCode as AxumStatus},
        routing::{get, post, put},
        Json, Router,
    };
    use rusqlite::params;
    use serde_json::{json, Value};
    use tempfile::TempDir;
    use tokio::sync::Notify;

    use super::*;

    #[derive(Default)]
    struct MemoryTokenStore {
        token: StdMutex<Option<String>>,
    }

    impl MemoryTokenStore {
        fn with_token(token: &str) -> Self {
            Self {
                token: StdMutex::new(Some(token.to_owned())),
            }
        }
    }

    impl TokenStore for MemoryTokenStore {
        fn load(&self) -> BackupResult<Option<String>> {
            Ok(self.token.lock().expect("token lock").clone())
        }

        fn store(&self, token: &str) -> BackupResult<()> {
            *self.token.lock().expect("token lock") = Some(token.to_owned());
            Ok(())
        }

        fn delete(&self) -> BackupResult<()> {
            *self.token.lock().expect("token lock") = None;
            Ok(())
        }
    }

    fn seeded_database(temp: &TempDir) -> Database {
        let database = Database::initialize(temp.path().join("pusula.sqlite3")).expect("database");
        let connection = database.connect().expect("connection");
        connection
            .execute(
                "INSERT INTO customers(id, name, registration_date) VALUES (?1, ?2, ?3)",
                params![17, "Offline Customer", "2026-07-14"],
            )
            .expect("seed customer");
        connection
            .execute(
                "INSERT INTO sales(id, customer_id, date, total_kurus, description) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![31, 17, "2026-07-14", 12345_i64, "Offline sale"],
            )
            .expect("seed sale");
        database
    }

    fn test_service(
        temp: &TempDir,
        database: Database,
        identity: &x25519::Identity,
        gateway_url: &str,
        token_store: Arc<dyn TokenStore>,
    ) -> BackupService {
        BackupService::new(
            database,
            temp.path().join("queue"),
            temp.path().join("state.json"),
            &identity.to_public().to_string(),
            GatewayClient::new(gateway_url, true, Duration::from_secs(2)).expect("gateway"),
            token_store,
        )
        .expect("service")
    }

    #[test]
    fn credential_namespace_tracks_the_tauri_app_identifier() {
        let production =
            PlatformTokenStore::for_app_identifier("com.stronganchor.pusula").expect("production");
        let isolated = PlatformTokenStore::for_app_identifier(
            "com.stronganchor.pusula.invalid-signature-test.0123456789abcdef",
        )
        .expect("isolated");

        assert_eq!(production.service, "com.stronganchor.pusula.backup");
        assert_eq!(
            isolated.service,
            "com.stronganchor.pusula.invalid-signature-test.0123456789abcdef.backup"
        );
        assert_ne!(production.service, isolated.service);
        assert!(PlatformTokenStore::for_app_identifier("").is_err());
        assert!(PlatformTokenStore::for_app_identifier("invalid identifier").is_err());
    }

    #[test]
    fn definitive_gateway_auth_rejections_are_distinct_from_transient_failures() {
        assert!(gateway_auth_rejected(&BackupError::GatewayRejected(401)));
        assert!(gateway_auth_rejected(&BackupError::GatewayRejected(403)));
        assert!(!gateway_auth_rejected(&BackupError::GatewayRejected(404)));
        assert!(!gateway_auth_rejected(&BackupError::GatewayUnavailable));
    }

    #[test]
    fn gateway_identifiers_must_be_canonical_lowercase_uuids() {
        assert!(validate_backup_id("00000000-0000-4000-8000-000000000001").is_ok());
        assert!(validate_backup_id("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA").is_err());
        assert!(validate_backup_id("00000000000040008000000000000001").is_err());
        assert!(validate_backup_id("not-a-uuid").is_err());
    }

    #[tokio::test]
    async fn scheduler_persists_business_periods_without_duplicate_artifacts_and_catches_up_month()
    {
        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            "http://127.0.0.1:9/",
            Arc::new(MemoryTokenStore::default()),
        );
        let first_now = DateTime::parse_from_rfc3339("2026-07-15T05:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let first_date = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();

        service
            .run_scheduled_at(first_now, first_date)
            .await
            .expect("first periods");
        let first_queue = service.list_queue().expect("first queue");
        assert_eq!(first_queue.len(), 2);
        assert!(first_queue
            .iter()
            .any(|item| { item.metadata.scheduled_period.as_deref() == Some("daily:2026-07-15") }));
        assert!(first_queue
            .iter()
            .any(|item| { item.metadata.scheduled_period.as_deref() == Some("monthly:2026-07") }));
        let state = service.read_state().expect("state");
        assert_eq!(state.last_daily_period.as_deref(), Some("2026-07-15"));
        assert_eq!(state.last_monthly_period.as_deref(), Some("2026-07"));

        service
            .run_scheduled_at(first_now + ChronoDuration::hours(6), first_date)
            .await
            .expect("same period retry");
        assert_eq!(service.list_queue().expect("same queue").len(), 2);

        let next_day = NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        service
            .run_scheduled_at(first_now + ChronoDuration::days(1), next_day)
            .await
            .expect("next day");
        assert_eq!(service.list_queue().expect("next-day queue").len(), 3);

        let missed_month_day = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap();
        service
            .run_scheduled_at(first_now + ChronoDuration::days(21), missed_month_day)
            .await
            .expect("missed month catch-up");
        let caught_up = service.list_queue().expect("catch-up queue");
        assert_eq!(caught_up.len(), 5);
        assert!(caught_up
            .iter()
            .any(|item| { item.metadata.scheduled_period.as_deref() == Some("monthly:2026-08") }));
        service
            .run_scheduled_at(
                first_now + ChronoDuration::days(21) + ChronoDuration::hours(6),
                missed_month_day,
            )
            .await
            .expect("catch-up replay");
        assert_eq!(service.list_queue().expect("deduplicated queue").len(), 5);
    }

    #[derive(Clone, Default)]
    struct RetryOnlyState {
        reserve_attempts: Arc<AtomicUsize>,
    }

    async fn mock_retry_reject_reserve(
        State(state): State<RetryOnlyState>,
    ) -> (AxumStatus, Json<Value>) {
        state.reserve_attempts.fetch_add(1, Ordering::SeqCst);
        (
            AxumStatus::SERVICE_UNAVAILABLE,
            Json(json!({"error": "temporarily_unavailable"})),
        )
    }

    #[tokio::test]
    async fn six_hour_remote_retry_flushes_existing_queue_without_new_scheduled_snapshots() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let state = RetryOnlyState::default();
        let app = Router::new()
            .route("/v1/backups/upload-url", post(mock_retry_reject_reserve))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });

        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            &format!("{origin}/"),
            Arc::new(MemoryTokenStore::with_token("retry-device-token")),
        );
        let business_date = Local::now().date_naive();
        service
            .run_scheduled_at(Utc::now(), business_date)
            .await
            .expect("first scheduled attempt");
        assert_eq!(state.reserve_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(service.list_queue().expect("first queue").len(), 2);
        let retry_at = DateTime::parse_from_rfc3339(
            service
                .read_state()
                .expect("state")
                .next_remote_retry_at
                .as_deref()
                .expect("retry time"),
        )
        .unwrap()
        .with_timezone(&Utc);

        service
            .run_scheduled_at(retry_at - ChronoDuration::seconds(1), business_date)
            .await
            .expect("not due");
        assert_eq!(state.reserve_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(service.list_queue().expect("unchanged queue").len(), 2);

        service
            .run_scheduled_at(retry_at + ChronoDuration::seconds(1), business_date)
            .await
            .expect("retry due");
        assert_eq!(state.reserve_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(service.list_queue().expect("retry queue").len(), 2);

        server.abort();
    }

    async fn mock_rejected_device_status() -> (AxumStatus, Json<Value>) {
        (
            AxumStatus::UNAUTHORIZED,
            Json(json!({"error": "invalid_device_token"})),
        )
    }

    #[tokio::test]
    async fn definitive_device_rejection_deletes_token_and_exposes_reenrollment_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let app = Router::new().route("/v1/backups/status", get(mock_rejected_device_status));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });

        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let token_store = Arc::new(MemoryTokenStore::with_token("revoked-device-token"));
        let service = test_service(
            &temp,
            database,
            &identity,
            &format!("{origin}/"),
            token_store.clone(),
        );
        let status = service.status().await.expect("status");
        assert!(!status.enrolled);
        assert!(status.needs_reenrollment);
        assert_eq!(status.gateway_reachable, Some(true));
        assert_eq!(token_store.load().expect("token"), None);
        assert!(service.read_state().expect("state").needs_reenrollment);

        server.abort();
    }

    async fn mock_authoritative_device_status() -> Json<Value> {
        Json(json!({
            "device_id": "00000000-0000-4000-8000-000000000081",
            "server_time": "2026-07-15T12:00:00Z",
            "active_pending_uploads": 0,
            "expired_pending_uploads": 0,
            "latest_completed": null
        }))
    }

    async fn mock_noncanonical_device_status() -> Json<Value> {
        Json(json!({
            "device_id": "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
            "server_time": "2026-07-15T12:00:00Z",
            "active_pending_uploads": 0,
            "expired_pending_uploads": 0,
            "latest_completed": null
        }))
    }

    #[tokio::test]
    async fn authenticated_status_reconciles_authoritative_canonical_device_identity() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let app = Router::new().route("/v1/backups/status", get(mock_authoritative_device_status));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });
        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            &format!("{origin}/"),
            Arc::new(MemoryTokenStore::with_token("valid-device-token")),
        );
        let mut stale = service.read_state().expect("state");
        stale.device_id = Some("00000000-0000-4000-8000-000000000099".to_owned());
        stale.needs_reenrollment = true;
        service.write_state(&stale).expect("stale state");

        let status = service.status().await.expect("authoritative status");
        assert_eq!(
            status.device_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000081")
        );
        assert!(!status.needs_reenrollment);
        let persisted = service.read_state().expect("reconciled state");
        assert_eq!(persisted.device_id, status.device_id);
        assert!(!persisted.needs_reenrollment);

        server.abort();
    }

    #[tokio::test]
    async fn successful_status_with_noncanonical_device_id_is_rejected_without_state_change() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let app = Router::new().route("/v1/backups/status", get(mock_noncanonical_device_status));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });
        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            &format!("{origin}/"),
            Arc::new(MemoryTokenStore::with_token("valid-device-token")),
        );
        let before = service.read_state().expect("state");
        assert!(matches!(
            service.status().await,
            Err(BackupError::InvalidGatewayResponse)
        ));
        assert_eq!(
            serde_json::to_value(service.read_state().expect("unchanged state")).unwrap(),
            serde_json::to_value(before).unwrap()
        );

        server.abort();
    }

    #[derive(Clone, Default)]
    struct StalledFlushState {
        attempts: Arc<AtomicUsize>,
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    async fn mock_stalled_reserve(
        State(state): State<StalledFlushState>,
    ) -> (AxumStatus, Json<Value>) {
        state.attempts.fetch_add(1, Ordering::SeqCst);
        state.entered.notify_one();
        state.release.notified().await;
        (
            AxumStatus::SERVICE_UNAVAILABLE,
            Json(json!({"error": "released_test_stall"})),
        )
    }

    #[tokio::test]
    async fn update_preflight_snapshots_locally_while_remote_flush_is_stalled() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let state = StalledFlushState::default();
        let app = Router::new()
            .route("/v1/backups/upload-url", post(mock_stalled_reserve))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });

        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database.clone(),
            &identity,
            &format!("{origin}/"),
            Arc::new(MemoryTokenStore::with_token("stalled-device-token")),
        );
        let flushing_service = service.clone();
        let flushing =
            tokio::spawn(async move { flushing_service.backup_now(RetentionClass::Rolling).await });
        tokio::time::timeout(Duration::from_secs(2), state.entered.notified())
            .await
            .expect("remote request entered");

        database
            .connect()
            .expect("write connection")
            .execute(
                "INSERT INTO customers(id, name, registration_date) VALUES (99, 'During upload', '2026-07-15')",
                [],
            )
            .expect("local write remains available");
        let preflight = tokio::time::timeout(Duration::from_secs(1), service.prepare_for_update())
            .await
            .expect("preflight must not wait for gateway")
            .expect("local snapshot");
        assert!(preflight.encrypted_snapshot_created);
        assert_eq!(preflight.remote_result, RemoteResult::QueuedOffline);
        assert_eq!(state.attempts.load(Ordering::SeqCst), 1);
        let mut snapshots_with_write = 0;
        for (index, queued) in service
            .list_queue()
            .expect("preflight queue")
            .iter()
            .enumerate()
        {
            let plaintext = age::decrypt(
                &identity,
                &fs::read(&queued.path).expect("queued ciphertext"),
            )
            .expect("decrypt snapshot");
            let restored_path = temp.path().join(format!("preflight-{index}.sqlite3"));
            fs::write(&restored_path, plaintext).expect("write restored test snapshot");
            let restored = Connection::open(restored_path).expect("restored snapshot");
            let count: i64 = restored
                .query_row("SELECT COUNT(*) FROM customers WHERE id = 99", [], |row| {
                    row.get(0)
                })
                .expect("written customer count");
            snapshots_with_write += usize::from(count == 1);
        }
        assert_eq!(snapshots_with_write, 1);

        state.release.notify_one();
        let flush_report = tokio::time::timeout(Duration::from_secs(2), flushing)
            .await
            .expect("flush task completed")
            .expect("flush join")
            .expect("local backup report");
        assert_eq!(flush_report.remote_result, RemoteResult::QueuedOffline);
        assert_eq!(service.list_queue().expect("queue").len(), 2);

        server.abort();
    }

    const STALE_BACKUP_ID: &str = "00000000-0000-4000-8000-000000000071";
    const REBOUND_BACKUP_ID: &str = "00000000-0000-4000-8000-000000000072";

    #[derive(Clone, Default)]
    struct StaleBindingState {
        reserve_attempts: Arc<AtomicUsize>,
        uploaded_sha256: Arc<StdMutex<Option<String>>>,
    }

    async fn mock_stale_reserve(
        State(state): State<StaleBindingState>,
        Json(body): Json<Value>,
    ) -> (AxumStatus, Json<Value>) {
        let attempt = state.reserve_attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt == 1 {
            return (
                AxumStatus::SERVICE_UNAVAILABLE,
                Json(json!({"error": "force_binding_inspection"})),
            );
        }
        assert!(body["content_length"].as_u64().is_some());
        assert_eq!(body["sha256"].as_str().expect("sha256").len(), 64);
        (
            AxumStatus::OK,
            Json(json!({
                "backup_id": REBOUND_BACKUP_ID,
                "retention_class": "rolling",
                "transport": "gateway_relay",
                "expires_at": "2026-07-15T12:15:00Z"
            })),
        )
    }

    async fn mock_stale_complete(
        State(state): State<StaleBindingState>,
        Json(body): Json<Value>,
    ) -> (AxumStatus, Json<Value>) {
        let backup_id = body["backup_id"].as_str().unwrap_or_default();
        if backup_id == STALE_BACKUP_ID {
            return (
                AxumStatus::NOT_FOUND,
                Json(json!({"error": {"code": "not_found", "message": "not found"}})),
            );
        }
        if state
            .uploaded_sha256
            .lock()
            .expect("uploaded sha")
            .is_none()
        {
            return (
                AxumStatus::CONFLICT,
                Json(json!({"error": {"code": "object_not_present", "message": "not present"}})),
            );
        }
        (
            AxumStatus::OK,
            Json(json!({
                "backup_id": backup_id,
                "status": "completed",
                "completed_at": "2026-07-15T12:01:00Z",
                "etag": "rebound-etag",
                "version_id": "rebound-version-1"
            })),
        )
    }

    async fn mock_stale_relay(
        State(state): State<StaleBindingState>,
        AxumPath(backup_id): AxumPath<String>,
        body: Bytes,
    ) -> (AxumStatus, Json<Value>) {
        assert_eq!(backup_id, REBOUND_BACKUP_ID);
        *state.uploaded_sha256.lock().expect("uploaded sha") =
            Some(format!("{:x}", Sha256::digest(&body)));
        (
            AxumStatus::OK,
            Json(json!({
                "backup_id": backup_id,
                "status": "completed",
                "completed_at": "2026-07-15T12:01:00Z",
                "etag": null,
                "version_id": "rebound-version-1"
            })),
        )
    }

    async fn exercise_stale_binding(stage: UploadStage) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let state = StaleBindingState::default();
        let app = Router::new()
            .route("/v1/backups/upload-url", post(mock_stale_reserve))
            .route("/v1/backups/relay/{backup_id}", put(mock_stale_relay))
            .route("/v1/backups/complete", post(mock_stale_complete))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });

        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            &format!("{origin}/"),
            Arc::new(MemoryTokenStore::with_token("stale-device-token")),
        );
        service
            .prepare_for_update()
            .await
            .expect("queued local snapshot");
        let mut queued = service.list_queue().expect("queue").remove(0);
        let original_ciphertext = fs::read(&queued.path).expect("ciphertext");
        let original_sha256 = format!("{:x}", Sha256::digest(&original_ciphertext));
        queued.metadata.upload = Some(PendingUpload {
            backup_id: STALE_BACKUP_ID.to_owned(),
            stage,
        });
        write_json_atomic(&queued.metadata_path, &queued.metadata).expect("stale binding");

        let first_error = service
            .flush_queue("stale-device-token")
            .await
            .expect_err("the first re-reservation is forced to fail");
        assert!(
            matches!(first_error, BackupError::GatewayRejected(503)),
            "unexpected first re-reservation error: {first_error:?}"
        );
        let rebound = service.list_queue().expect("preserved queue").remove(0);
        assert!(rebound.metadata.upload.is_none());
        assert_eq!(
            fs::read(&rebound.path).expect("preserved ciphertext"),
            original_ciphertext
        );

        assert_eq!(
            service
                .flush_queue("stale-device-token")
                .await
                .expect("re-reserved upload"),
            1
        );
        assert!(service.list_queue().expect("empty queue").is_empty());
        assert_eq!(
            state
                .uploaded_sha256
                .lock()
                .expect("uploaded sha")
                .as_deref(),
            Some(original_sha256.as_str())
        );
        assert_eq!(state.reserve_attempts.load(Ordering::SeqCst), 2);

        server.abort();
    }

    #[tokio::test]
    async fn stale_uploaded_404_clears_only_binding_then_re_reserves_same_ciphertext() {
        exercise_stale_binding(UploadStage::Uploaded).await;
    }

    #[tokio::test]
    async fn stale_relay_pending_404_clears_only_binding_then_re_reserves_same_ciphertext() {
        exercise_stale_binding(UploadStage::RelayPending).await;
    }

    async fn mock_mismatched_retention_reserve(
        State(_state): State<StaleBindingState>,
        Json(_body): Json<Value>,
    ) -> Json<Value> {
        Json(json!({
            "backup_id": REBOUND_BACKUP_ID,
            "retention_class": "monthly",
            "transport": "gateway_relay",
            "expires_at": "2026-07-15T12:15:00Z"
        }))
    }

    #[tokio::test]
    async fn reservation_retention_class_must_match_the_queued_lifecycle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let state = StaleBindingState::default();
        let app = Router::new()
            .route(
                "/v1/backups/upload-url",
                post(mock_mismatched_retention_reserve),
            )
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });
        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            &format!("{origin}/"),
            Arc::new(MemoryTokenStore::with_token("device-token")),
        );
        service.prepare_for_update().await.expect("local snapshot");
        assert!(matches!(
            service.flush_queue("device-token").await,
            Err(BackupError::InvalidGatewayResponse)
        ));
        assert!(service.list_queue().expect("queue")[0]
            .metadata
            .upload
            .is_none());
        server.abort();
    }

    #[tokio::test]
    async fn noncanonical_backup_id_in_local_metadata_is_never_sent_and_only_binding_is_cleared() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let retry = RetryOnlyState::default();
        let app = Router::new()
            .route("/v1/backups/upload-url", post(mock_retry_reject_reserve))
            .with_state(retry);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });
        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            &format!("{origin}/"),
            Arc::new(MemoryTokenStore::with_token("device-token")),
        );
        service.prepare_for_update().await.expect("local snapshot");
        let mut queued = service.list_queue().expect("queue").remove(0);
        queued.metadata.upload = Some(PendingUpload {
            backup_id: "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".to_owned(),
            stage: UploadStage::Uploaded,
        });
        write_json_atomic(&queued.metadata_path, &queued.metadata).expect("metadata");
        assert!(matches!(
            service.flush_queue("device-token").await,
            Err(BackupError::GatewayRejected(503))
        ));
        let preserved = service.list_queue().expect("preserved queue").remove(0);
        assert!(preserved.metadata.upload.is_none());
        assert!(preserved.path.exists());
        server.abort();
    }

    #[tokio::test]
    async fn failed_network_leaves_decryptable_consistent_sqlite_queued() {
        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            "http://127.0.0.1:9/",
            Arc::new(MemoryTokenStore::with_token("device_test_token")),
        );

        let report = service
            .prepare_for_update()
            .await
            .expect("local backup succeeds while offline");
        assert!(report.encrypted_snapshot_created);
        assert!(report.safe_to_continue);
        assert_eq!(report.remote_result, RemoteResult::QueuedOffline);
        assert_eq!(report.pending_count, 1);

        let queued = service.list_queue().expect("queue");
        let ciphertext = fs::read(&queued[0].path).expect("ciphertext");
        assert!(!ciphertext.starts_with(b"SQLite format 3"));
        let plaintext = age::decrypt(&identity, &ciphertext).expect("decrypt");
        let restored_path = temp.path().join("restored.sqlite3");
        fs::write(&restored_path, plaintext).expect("restore file");
        let restored = Connection::open(restored_path).expect("restored database");
        let integrity: String = restored
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .expect("integrity");
        let customers: i64 = restored
            .query_row("SELECT COUNT(*) FROM customers", [], |row| row.get(0))
            .expect("customer count");
        let total_kurus: i64 = restored
            .query_row("SELECT SUM(total_kurus) FROM sales", [], |row| row.get(0))
            .expect("financial total");
        assert_eq!(integrity, "ok");
        assert_eq!(customers, 1);
        assert_eq!(total_kurus, 12345);
    }

    #[derive(Clone, Default)]
    struct MockObservations {
        enrolled: Arc<StdMutex<bool>>,
        relayed_with_bearer: Arc<StdMutex<bool>>,
        completed: Arc<StdMutex<bool>>,
    }

    async fn mock_enroll(
        State(state): State<MockObservations>,
        Json(body): Json<Value>,
    ) -> (AxumStatus, Json<Value>) {
        assert_eq!(body["enrollment_code"], "one-time-code");
        assert_eq!(body["device_name"], "Front Desk");
        *state.enrolled.lock().expect("enrolled") = true;
        (
            AxumStatus::CREATED,
            Json(json!({
                "device_id": "00000000-0000-4000-8000-000000000001",
                "device_token": "secret-device-token",
                "created_at": "2026-07-14T12:00:00Z"
            })),
        )
    }

    async fn mock_reserve(
        State(_state): State<MockObservations>,
        headers: AxumHeaders,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer secret-device-token")
        );
        assert!(body["content_length"].as_u64().is_some());
        assert_eq!(body["sha256"].as_str().expect("sha").len(), 64);
        Json(json!({
            "backup_id": "00000000-0000-4000-8000-000000000002",
            "retention_class": "rolling",
            "transport": "gateway_relay",
            "expires_at": "2026-07-14T12:15:00Z"
        }))
    }

    async fn mock_gateway_relay(
        State(state): State<MockObservations>,
        AxumPath(backup_id): AxumPath<String>,
        headers: AxumHeaders,
        body: Bytes,
    ) -> (AxumStatus, Json<Value>) {
        assert_eq!(backup_id, "00000000-0000-4000-8000-000000000002");
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer secret-device-token")
        );
        assert!(headers.get("cookie").is_none());
        assert_eq!(
            headers
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/octet-stream")
        );
        assert!(!body.starts_with(b"SQLite format 3"));
        *state.relayed_with_bearer.lock().expect("relayed") = true;
        *state.completed.lock().expect("completed") = true;
        (
            AxumStatus::OK,
            Json(json!({
                "backup_id": backup_id,
                "status": "completed",
                "completed_at": "2026-07-14T12:01:00Z",
                "etag": null,
                "version_id": "test-version-1"
            })),
        )
    }

    async fn mock_complete(
        State(state): State<MockObservations>,
        headers: AxumHeaders,
        Json(body): Json<Value>,
    ) -> (AxumStatus, Json<Value>) {
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer secret-device-token")
        );
        assert_eq!(body["backup_id"], "00000000-0000-4000-8000-000000000002");
        if !*state.relayed_with_bearer.lock().expect("relayed") {
            return (
                AxumStatus::CONFLICT,
                Json(json!({"error": {"code": "object_not_present", "message": "not present"}})),
            );
        }
        *state.completed.lock().expect("completed") = true;
        (
            AxumStatus::OK,
            Json(json!({
                "backup_id": body["backup_id"],
                "status": "completed",
                "completed_at": "2026-07-14T12:01:00Z",
                "etag": "test-etag",
                "version_id": "test-version-1"
            })),
        )
    }

    async fn mock_status(headers: AxumHeaders) -> Json<Value> {
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer secret-device-token")
        );
        Json(json!({
            "device_id": "00000000-0000-4000-8000-000000000001",
            "server_time": "2026-07-14T12:02:00Z",
            "active_pending_uploads": 0,
            "expired_pending_uploads": 0,
            "latest_completed": {
                "backup_id": "00000000-0000-4000-8000-000000000002",
                "size_bytes": 100,
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "completed_at": "2026-07-14T12:01:00Z"
            }
        }))
    }

    #[tokio::test]
    async fn gateway_flow_stores_token_outside_state_and_sends_ciphertext_only_to_gateway() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let observations = MockObservations::default();
        let app = Router::new()
            .route("/v1/enroll", post(mock_enroll))
            .route("/v1/backups/upload-url", post(mock_reserve))
            .route("/v1/backups/relay/{backup_id}", put(mock_gateway_relay))
            .route("/v1/backups/complete", post(mock_complete))
            .route("/v1/backups/status", get(mock_status))
            .with_state(observations.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });

        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let token_store = Arc::new(MemoryTokenStore::default());
        let service = test_service(
            &temp,
            database,
            &identity,
            &format!("{origin}/"),
            token_store.clone(),
        );

        let enrollment = service
            .enroll("one-time-code".to_owned(), "Front Desk".to_owned())
            .await
            .expect("enroll");
        assert_eq!(enrollment.device_id, "00000000-0000-4000-8000-000000000001");
        let state_contents = fs::read_to_string(&service.state_path).expect("state");
        assert!(!state_contents.contains("secret-device-token"));
        assert_eq!(
            token_store.load().expect("token"),
            Some("secret-device-token".to_owned())
        );

        let report = service
            .backup_now(RetentionClass::Rolling)
            .await
            .expect("backup");
        assert_eq!(report.remote_result, RemoteResult::Uploaded);
        assert_eq!(report.pending_count, 0);
        let status = service.status().await.expect("status");
        assert!(status.remote.is_some());
        assert!(*observations.enrolled.lock().expect("enrolled"));
        assert!(*observations.relayed_with_bearer.lock().expect("relayed"));
        assert!(*observations.completed.lock().expect("completed"));

        server.abort();
    }

    #[derive(Clone, Default)]
    struct RelayMockState {
        reserve_count: Arc<StdMutex<usize>>,
        relay_count: Arc<StdMutex<usize>>,
        complete_count: Arc<StdMutex<usize>>,
        object_versions: Arc<StdMutex<usize>>,
        fail_first_relay: Arc<StdMutex<bool>>,
        invalid_successes_before_valid: Arc<StdMutex<usize>>,
        expected_size: Arc<StdMutex<u64>>,
        expected_sha256: Arc<StdMutex<String>>,
    }

    async fn mock_relay_reserve(
        State(state): State<RelayMockState>,
        headers: AxumHeaders,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer relay-device-token")
        );
        let size = body["content_length"].as_u64().expect("content length");
        let sha256 = body["sha256"].as_str().expect("sha256").to_owned();
        assert_eq!(body["retention_class"], "rolling");
        *state.reserve_count.lock().expect("reserve count") += 1;
        *state.expected_size.lock().expect("expected size") = size;
        *state.expected_sha256.lock().expect("expected sha256") = sha256;

        Json(json!({
            "backup_id": "00000000-0000-4000-8000-000000000022",
            "retention_class": "rolling",
            "transport": "gateway_relay",
            "expires_at": "2026-07-15T12:15:00Z"
        }))
    }

    async fn mock_relay_upload(
        State(state): State<RelayMockState>,
        AxumPath(backup_id): AxumPath<String>,
        headers: AxumHeaders,
        body: Bytes,
    ) -> (AxumStatus, Json<Value>) {
        assert_eq!(backup_id, "00000000-0000-4000-8000-000000000022");
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer relay-device-token")
        );
        assert_eq!(
            headers
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/octet-stream")
        );
        let expected_size = *state.expected_size.lock().expect("expected size");
        assert_eq!(body.len() as u64, expected_size);
        assert_eq!(
            headers
                .get("content-length")
                .and_then(|value| value.to_str().ok()),
            Some(expected_size.to_string().as_str())
        );
        assert!(headers.get("transfer-encoding").is_none());
        assert!(body.starts_with(b"age-encryption.org/v1"));
        assert!(!body.starts_with(b"SQLite format 3"));
        assert_eq!(
            format!("{:x}", Sha256::digest(&body)),
            *state.expected_sha256.lock().expect("expected sha256")
        );

        let attempt = {
            let mut relay_count = state.relay_count.lock().expect("relay count");
            *relay_count += 1;
            *relay_count
        };
        {
            let mut versions = state.object_versions.lock().expect("object versions");
            if *versions == 0 {
                *versions = 1;
            }
        }
        let fail_first = *state.fail_first_relay.lock().expect("fail first relay");
        if fail_first && attempt == 1 {
            return (
                AxumStatus::BAD_GATEWAY,
                Json(json!({"error": "upstream_unavailable"})),
            );
        }
        let invalid_successes = *state
            .invalid_successes_before_valid
            .lock()
            .expect("invalid successes");
        if attempt <= invalid_successes {
            return (
                AxumStatus::OK,
                Json(json!({
                    "backup_id": backup_id,
                    "status": "completed",
                    "completed_at": "2026-07-15T12:01:00Z",
                    "etag": "invalid-relay-etag",
                    "version_id": null
                })),
            );
        }

        (
            AxumStatus::OK,
            Json(json!({
                "backup_id": backup_id,
                "status": "completed",
                "completed_at": "2026-07-15T12:01:00Z",
                "etag": "relay-etag",
                "version_id": "relay-version-1"
            })),
        )
    }

    async fn mock_relay_complete(
        State(state): State<RelayMockState>,
        Json(body): Json<Value>,
    ) -> (AxumStatus, Json<Value>) {
        *state.complete_count.lock().expect("complete count") += 1;
        if *state.object_versions.lock().expect("object versions") == 0 {
            return (
                AxumStatus::CONFLICT,
                Json(json!({"error": {"code": "object_not_present", "message": "not present"}})),
            );
        }
        (
            AxumStatus::OK,
            Json(json!({
                "backup_id": body["backup_id"],
                "status": "completed",
                "completed_at": "2026-07-15T12:01:00Z",
                "etag": "relay-etag",
                "version_id": "relay-version-1"
            })),
        )
    }

    #[tokio::test]
    async fn lost_relay_response_is_confirmed_without_a_second_object_version() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let state = RelayMockState::default();
        *state.fail_first_relay.lock().expect("fail first relay") = true;
        let app = Router::new()
            .route("/v1/backups/upload-url", post(mock_relay_reserve))
            .route("/v1/backups/relay/{backup_id}", put(mock_relay_upload))
            .route("/v1/backups/complete", post(mock_relay_complete))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });

        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let first_service = test_service(
            &temp,
            database.clone(),
            &identity,
            &format!("{origin}/"),
            Arc::new(MemoryTokenStore::with_token("relay-device-token")),
        );
        let report = first_service
            .backup_now(RetentionClass::Rolling)
            .await
            .expect("local backup remains successful");
        assert_eq!(report.remote_result, RemoteResult::QueuedOffline);
        assert_eq!(report.pending_count, 1);
        let queued = first_service.list_queue().expect("queued backup");
        let pending = queued[0].metadata.upload.as_ref().expect("pending upload");
        assert_eq!(pending.stage, UploadStage::RelayPending);
        assert_eq!(pending.backup_id, "00000000-0000-4000-8000-000000000022");
        let sidecar = fs::read_to_string(&queued[0].metadata_path).expect("sidecar");
        assert!(sidecar.contains("\"stage\":\"relay_pending\""));
        drop(first_service);

        let restarted_service = test_service(
            &temp,
            database,
            &identity,
            &format!("{origin}/"),
            Arc::new(MemoryTokenStore::with_token("relay-device-token")),
        );
        assert_eq!(
            restarted_service
                .flush_queue("relay-device-token")
                .await
                .expect("completion confirmation"),
            1
        );
        assert!(restarted_service
            .list_queue()
            .expect("empty queue")
            .is_empty());
        assert_eq!(*state.reserve_count.lock().expect("reserve count"), 1);
        assert_eq!(*state.relay_count.lock().expect("relay count"), 1);
        assert_eq!(*state.object_versions.lock().expect("object versions"), 1);
        assert_eq!(*state.complete_count.lock().expect("complete count"), 2);

        server.abort();
    }

    #[tokio::test]
    async fn relay_requires_durable_version_evidence_before_deleting_queue() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let state = RelayMockState::default();
        *state
            .invalid_successes_before_valid
            .lock()
            .expect("invalid successes") = 1;
        let app = Router::new()
            .route("/v1/backups/upload-url", post(mock_relay_reserve))
            .route("/v1/backups/relay/{backup_id}", put(mock_relay_upload))
            .route("/v1/backups/complete", post(mock_relay_complete))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });

        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            &format!("{origin}/"),
            Arc::new(MemoryTokenStore::with_token("relay-device-token")),
        );
        let report = service
            .backup_now(RetentionClass::Rolling)
            .await
            .expect("local backup remains successful");
        assert_eq!(report.remote_result, RemoteResult::QueuedOffline);
        assert_eq!(service.list_queue().expect("first queued retry").len(), 1);

        assert_eq!(
            service
                .flush_queue("relay-device-token")
                .await
                .expect("valid completion retry"),
            1
        );
        assert!(service.list_queue().expect("empty queue").is_empty());
        assert_eq!(*state.reserve_count.lock().expect("reserve count"), 1);
        assert_eq!(*state.relay_count.lock().expect("relay count"), 1);
        assert_eq!(*state.object_versions.lock().expect("object versions"), 1);
        assert_eq!(*state.complete_count.lock().expect("complete count"), 2);

        server.abort();
    }

    #[test]
    fn upload_timeout_covers_the_gateway_proxy_window_without_becoming_unbounded() {
        assert!(UPLOAD_REQUEST_TIMEOUT >= Duration::from_secs(15 * 60));
        assert!(UPLOAD_REQUEST_TIMEOUT < Duration::from_secs(60 * 60));
    }

    #[test]
    fn reservation_schema_requires_the_relay_transport_and_no_external_url() {
        let reservation: UploadUrlResponse = serde_json::from_value(json!({
            "backup_id": "00000000-0000-4000-8000-000000000002",
            "retention_class": "rolling",
            "transport": "gateway_relay",
            "expires_at": "2026-07-14T12:15:00Z"
        }))
        .expect("relay reservation");
        assert_eq!(reservation.transport, GATEWAY_RELAY_TRANSPORT);

        assert!(serde_json::from_value::<UploadUrlResponse>(json!({
            "backup_id": "00000000-0000-4000-8000-000000000002",
            "retention_class": "rolling",
            "method": "PUT",
            "upload_url": "https://object-storage.example/file"
        }))
        .is_err());
    }

    #[tokio::test]
    async fn malformed_sidecar_is_reconstructed_without_stalling_healthy_backups() {
        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            "http://127.0.0.1:9/",
            Arc::new(MemoryTokenStore::default()),
        );

        service
            .backup_now(RetentionClass::Rolling)
            .await
            .expect("first backup");
        let first = service.list_queue().expect("first queue").remove(0);
        fs::write(&first.metadata_path, b"{broken").expect("corrupt sidecar");

        let status = service.status().await.expect("degraded status");
        assert_eq!(status.pending_count, 1);
        assert!(!status.queue_healthy);
        assert_eq!(status.quarantined_file_count, 1);
        assert!(first.path.exists());
        assert!(first.metadata_path.exists());

        service
            .backup_now(RetentionClass::Rolling)
            .await
            .expect("second backup");
        assert_eq!(service.list_queue().expect("healthy queue").len(), 2);
    }

    #[tokio::test]
    async fn corrupt_ciphertext_is_quarantined_without_blocking_the_queue() {
        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            "http://127.0.0.1:9/",
            Arc::new(MemoryTokenStore::default()),
        );

        service
            .backup_now(RetentionClass::Rolling)
            .await
            .expect("backup");
        let queued = service.list_queue().expect("queue").remove(0);
        fs::write(&queued.path, b"corrupt").expect("corrupt ciphertext");

        assert_eq!(service.flush_queue("unused-token").await.expect("flush"), 0);
        assert!(service.list_queue().expect("empty queue").is_empty());
        assert_eq!(service.quarantined_file_count().expect("quarantine"), 2);
    }

    #[tokio::test]
    async fn destructive_import_recovery_snapshots_remain_local_and_are_bounded() {
        let temp = TempDir::new().expect("temp");
        let database = seeded_database(&temp);
        let identity = x25519::Identity::generate();
        let service = test_service(
            &temp,
            database,
            &identity,
            "http://127.0.0.1:9/",
            Arc::new(MemoryTokenStore::with_token("unused-token")),
        );

        for _ in 0..4 {
            let report = service
                .prepare_for_destructive_import()
                .await
                .expect("local recovery");
            assert_eq!(report.remote_result, RemoteResult::LocalRecovery);
            assert!(report.safe_to_continue);
        }

        assert_eq!(service.flush_queue("unused-token").await.expect("flush"), 0);
        let queue = service.list_queue().expect("queue");
        assert_eq!(queue.len(), LOCAL_RECOVERY_LIMIT);
        assert!(queue.iter().all(|item| item.metadata.local_recovery));
    }
}
