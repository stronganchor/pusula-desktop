use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use age::x25519;
use chrono::{DateTime, Datelike, Duration as ChronoDuration, Utc};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, StatusCode,
};
use rusqlite::{backup::Backup, Connection, DatabaseName};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder as TempfileBuilder;
use tokio::sync::Mutex;
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
const B2_UPLOAD_HOST: &str = "s3.us-west-004.backblazeb2.com";
const B2_UPLOAD_PATH_PREFIX: &str = "/stronganchor-pusula-desktop-backups/backups/";
const UPLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const QUARANTINE_DIRECTORY: &str = "quarantine";
const REQUIRED_UPLOAD_HEADERS: [&str; 4] = [
    "content-length",
    "x-amz-content-sha256",
    "x-amz-meta-sha256",
    "x-amz-server-side-encryption",
];

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
    last_attempt_at: Option<String>,
    last_snapshot_at: Option<String>,
    last_remote_success_at: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectUploadOutcome {
    Uploaded,
    RelayRequired,
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
    allow_insecure_uploads: bool,
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
    method: String,
    upload_url: String,
    required_headers: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct CompleteRequest<'a> {
    backup_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct CompleteResponse {
    backup_id: String,
    status: String,
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
        Ok(Self {
            base_url,
            client,
            allow_insecure_uploads,
        })
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
        response
            .json()
            .await
            .map_err(|_| BackupError::InvalidGatewayResponse)
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
        response
            .json()
            .await
            .map_err(|_| BackupError::InvalidGatewayResponse)
    }

    async fn put_ciphertext(
        &self,
        queued: &QueuedBackup,
        reservation: &UploadUrlResponse,
    ) -> BackupResult<DirectUploadOutcome> {
        let (upload_url, headers) = self.validate_upload_request(queued, reservation)?;

        let file = tokio::fs::File::open(&queued.path).await?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
        let response = match self
            .client
            .put(upload_url)
            .headers(headers)
            .timeout(UPLOAD_REQUEST_TIMEOUT)
            .body(body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) if direct_upload_error_allows_relay(&error) => {
                return Ok(DirectUploadOutcome::RelayRequired);
            }
            Err(_) => return Err(BackupError::GatewayUnavailable),
        };
        ensure_success(response.status())?;
        Ok(DirectUploadOutcome::Uploaded)
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

    fn validate_upload_request(
        &self,
        queued: &QueuedBackup,
        reservation: &UploadUrlResponse,
    ) -> BackupResult<(Url, HeaderMap)> {
        if reservation.method != "PUT" {
            return Err(BackupError::InvalidGatewayResponse);
        }

        let upload_url =
            Url::parse(&reservation.upload_url).map_err(|_| BackupError::InvalidGatewayResponse)?;
        if !upload_url.username().is_empty()
            || upload_url.password().is_some()
            || upload_url.fragment().is_some()
        {
            return Err(BackupError::InvalidGatewayResponse);
        }
        if self.allow_insecure_uploads {
            if !matches!(upload_url.scheme(), "http" | "https") {
                return Err(BackupError::InvalidGatewayResponse);
            }
        } else if upload_url.scheme() != "https"
            || upload_url.host_str() != Some(B2_UPLOAD_HOST)
            || upload_url.port_or_known_default() != Some(443)
            || !upload_url.path().starts_with(B2_UPLOAD_PATH_PREFIX)
            || upload_url.path() == B2_UPLOAD_PATH_PREFIX
            || upload_url.query().is_none()
        {
            return Err(BackupError::InvalidGatewayResponse);
        }

        if reservation.required_headers.len() != REQUIRED_UPLOAD_HEADERS.len() {
            return Err(BackupError::InvalidGatewayResponse);
        }

        let mut headers = HeaderMap::new();
        for (name, value) in &reservation.required_headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| BackupError::InvalidGatewayResponse)?;
            if !REQUIRED_UPLOAD_HEADERS.contains(&name.as_str()) || headers.contains_key(&name) {
                return Err(BackupError::InvalidGatewayResponse);
            }
            let value =
                HeaderValue::from_str(value).map_err(|_| BackupError::InvalidGatewayResponse)?;
            headers.insert(name, value);
        }

        let expected_length = queued.metadata.size_bytes.to_string();
        if headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            != Some(expected_length.as_str())
        {
            return Err(BackupError::InvalidGatewayResponse);
        }
        if headers
            .get("x-amz-meta-sha256")
            .and_then(|value| value.to_str().ok())
            != Some(queued.metadata.sha256.as_str())
        {
            return Err(BackupError::InvalidGatewayResponse);
        }
        if headers
            .get("x-amz-content-sha256")
            .and_then(|value| value.to_str().ok())
            != Some("UNSIGNED-PAYLOAD")
        {
            return Err(BackupError::InvalidGatewayResponse);
        }
        if headers
            .get("x-amz-server-side-encryption")
            .and_then(|value| value.to_str().ok())
            != Some("AES256")
        {
            return Err(BackupError::InvalidGatewayResponse);
        }

        Ok((upload_url, headers))
    }

    async fn complete(&self, token: &str, backup_id: &str) -> BackupResult<()> {
        let response = self
            .client
            .post(self.endpoint("v1/backups/complete")?)
            .bearer_auth(token)
            .json(&CompleteRequest { backup_id })
            .send()
            .await
            .map_err(|_| BackupError::GatewayUnavailable)?;
        require_completed_response(response, backup_id).await
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
        response
            .json()
            .await
            .map_err(|_| BackupError::InvalidGatewayResponse)
    }
}

fn direct_upload_error_allows_relay(error: &reqwest::Error) -> bool {
    error.status().is_none()
        && !error.is_body()
        && (error.is_timeout() || error.is_connect() || error.is_request())
}

async fn require_completed_response(
    response: reqwest::Response,
    expected_backup_id: &str,
) -> BackupResult<()> {
    ensure_success(response.status())?;
    let completion = response
        .json::<CompleteResponse>()
        .await
        .map_err(|_| BackupError::InvalidGatewayResponse)?;
    if completion.backup_id != expected_backup_id || completion.status != "completed" {
        return Err(BackupError::InvalidGatewayResponse);
    }
    Ok(())
}

fn ensure_success(status: StatusCode) -> BackupResult<()> {
    if status.is_success() {
        Ok(())
    } else {
        Err(BackupError::GatewayRejected(status.as_u16()))
    }
}

#[derive(Clone)]
pub struct BackupService {
    database: Database,
    queue_dir: PathBuf,
    state_path: PathBuf,
    recipient: x25519::Recipient,
    gateway: GatewayClient,
    token_store: Arc<dyn TokenStore>,
    operation_lock: Arc<Mutex<()>>,
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
            operation_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn enroll(
        &self,
        enrollment_code: String,
        device_name: String,
    ) -> BackupResult<BackupEnrollment> {
        let _guard = self.operation_lock.lock().await;
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
        let mut state = self.read_state()?;
        state.device_id = Some(enrollment.device_id.clone());
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
        let _guard = self.operation_lock.lock().await;
        self.create_and_flush(retention_class).await
    }

    pub async fn prepare_for_update(&self) -> BackupResult<BackupRunReport> {
        self.backup_now(RetentionClass::Rolling).await
    }

    pub async fn prepare_for_destructive_import(&self) -> BackupResult<BackupRunReport> {
        let _guard = self.operation_lock.lock().await;
        let database = self.database.clone();
        let queue_dir = self.queue_dir.clone();
        let recipient = self.recipient.clone();
        let queued = tokio::task::spawn_blocking(move || {
            create_encrypted_snapshot(
                &database,
                &queue_dir,
                &recipient,
                RetentionClass::Rolling,
                true,
            )
        })
        .await
        .map_err(|_| BackupError::Task)??;

        self.prune_queue()?;
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

        let mut state = self.read_state().unwrap_or_default();
        state.last_attempt_at = Some(Utc::now().to_rfc3339());
        state.last_snapshot_at = Some(queued.metadata.created_at.clone());
        let _ = self.write_state(&state);

        Ok(BackupRunReport {
            encrypted_snapshot_created: true,
            safe_to_continue: true,
            retention_class: RetentionClass::Rolling,
            created_at: queued.metadata.created_at,
            uploaded_count: 0,
            pending_count,
            local_recovery_count,
            queue_healthy: quarantined_file_count == 0,
            quarantined_file_count,
            remote_result: RemoteResult::LocalRecovery,
        })
    }

    async fn create_and_flush(
        &self,
        retention_class: RetentionClass,
    ) -> BackupResult<BackupRunReport> {
        let attempted_at = Utc::now();
        let mut state = self.read_state().unwrap_or_default();
        state.last_attempt_at = Some(attempted_at.to_rfc3339());
        state.next_scheduled_at =
            Some((attempted_at + ChronoDuration::hours(FAILURE_RETRY_HOURS)).to_rfc3339());
        let _ = self.write_state(&state);

        let database = self.database.clone();
        let queue_dir = self.queue_dir.clone();
        let recipient = self.recipient.clone();
        let queued = tokio::task::spawn_blocking(move || {
            create_encrypted_snapshot(&database, &queue_dir, &recipient, retention_class, false)
        })
        .await
        .map_err(|_| BackupError::Task)??;

        // Reaching this point means the SQLite snapshot has been encrypted, flushed,
        // fsynced, and atomically placed in the persistent queue. Remote failures below
        // are therefore status, not a reason to block an import, update, or local write.
        let _ = self.prune_queue();
        state.last_snapshot_at = Some(queued.metadata.created_at.clone());

        let enrolled = self.token_store.load().ok().flatten();
        let (uploaded_count, remote_result) = if let Some(token) = enrolled {
            match self.flush_queue(&token).await {
                Ok(uploaded_count) if uploaded_count > 0 => {
                    state.last_remote_success_at = Some(Utc::now().to_rfc3339());
                    (uploaded_count, RemoteResult::Uploaded)
                }
                Ok(_) => (0, RemoteResult::QueuedOffline),
                Err(_) => (0, RemoteResult::QueuedOffline),
            }
        } else {
            (0, RemoteResult::NotEnrolled)
        };

        let next_attempt_hours = if remote_result == RemoteResult::QueuedOffline {
            FAILURE_RETRY_HOURS
        } else {
            SCHEDULE_INTERVAL_HOURS
        };
        state.next_scheduled_at =
            Some((attempted_at + ChronoDuration::hours(next_attempt_hours)).to_rfc3339());
        let _ = self.write_state(&state);
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
            retention_class,
            created_at: queued.metadata.created_at,
            uploaded_count,
            pending_count,
            local_recovery_count,
            queue_healthy: quarantined_file_count == 0,
            quarantined_file_count,
            remote_result,
        })
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

            if let Some(pending) = &queued.metadata.upload {
                match pending.stage {
                    UploadStage::Uploaded => {
                        self.gateway.complete(token, &pending.backup_id).await?;
                        remove_queued_backup(&queued)?;
                        uploaded_count += 1;
                        continue;
                    }
                    UploadStage::RelayPending => {
                        self.gateway
                            .relay_ciphertext(token, &queued, &pending.backup_id)
                            .await?;
                        remove_queued_backup(&queued)?;
                        uploaded_count += 1;
                        continue;
                    }
                    UploadStage::Reserved => {}
                }
            }

            let reservation = self.gateway.reserve_upload(token, &queued).await?;
            validate_backup_id(&reservation.backup_id)?;
            queued.metadata.upload = Some(PendingUpload {
                backup_id: reservation.backup_id.clone(),
                stage: UploadStage::Reserved,
            });
            write_json_atomic(&queued.metadata_path, &queued.metadata)?;

            if self.gateway.put_ciphertext(&queued, &reservation).await?
                == DirectUploadOutcome::RelayRequired
            {
                queued.metadata.upload = Some(PendingUpload {
                    backup_id: reservation.backup_id.clone(),
                    stage: UploadStage::RelayPending,
                });
                write_json_atomic(&queued.metadata_path, &queued.metadata)?;

                self.gateway
                    .relay_ciphertext(token, &queued, &reservation.backup_id)
                    .await?;
                remove_queued_backup(&queued)?;
                uploaded_count += 1;
                continue;
            }
            queued.metadata.upload = Some(PendingUpload {
                backup_id: reservation.backup_id.clone(),
                stage: UploadStage::Uploaded,
            });
            write_json_atomic(&queued.metadata_path, &queued.metadata)?;

            self.gateway.complete(token, &reservation.backup_id).await?;
            remove_queued_backup(&queued)?;
            uploaded_count += 1;
        }
        Ok(uploaded_count)
    }

    pub async fn status(&self) -> BackupResult<BackupStatusReport> {
        let state = self.read_state()?;
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
        let token = self.token_store.load().ok().flatten();
        let enrolled = token.is_some();
        let (remote, gateway_reachable) = if let Some(token) = token {
            match self.gateway.status(&token).await {
                Ok(remote) => (Some(remote), Some(true)),
                Err(_) => (None, Some(false)),
            }
        } else {
            (None, None)
        };
        Ok(BackupStatusReport {
            enrolled,
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
        let due = self
            .read_state()
            .ok()
            .and_then(|state| state.next_scheduled_at)
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|next| next.with_timezone(&Utc) <= Utc::now())
            .unwrap_or(true);
        if !due {
            return;
        }

        let now = Utc::now();
        let retention = if now.day() == 1 {
            RetentionClass::Monthly
        } else {
            RetentionClass::Daily
        };
        let _ = self.backup_now(retention).await;
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
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| BackupError::InvalidGatewayResponse)
}

fn create_encrypted_snapshot(
    database: &Database,
    queue_dir: &Path,
    recipient: &x25519::Recipient,
    retention_class: RetentionClass,
    local_recovery: bool,
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

    partial
        .persist(&final_path)
        .map_err(|error| BackupError::Io(error.error))?;
    sync_parent_directory(queue_dir)?;

    let size_bytes = fs::metadata(&final_path)?.len();
    let sha256 = sha256_file(&final_path)?;
    let metadata_path = queue_metadata_path(&final_path);
    let metadata = QueueMetadata {
        format_version: QUEUE_METADATA_VERSION,
        created_at: created_at.to_rfc3339(),
        retention_class,
        size_bytes,
        sha256,
        local_recovery,
        upload: None,
    };
    if let Err(error) = write_json_atomic(&metadata_path, &metadata) {
        let _ = fs::remove_file(&final_path);
        return Err(error);
    }

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
        sync::{Arc, Mutex as StdMutex},
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
        uploaded_without_bearer: Arc<StdMutex<bool>>,
        completed: Arc<StdMutex<bool>>,
        origin: Arc<StdMutex<String>>,
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
        State(state): State<MockObservations>,
        headers: AxumHeaders,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer secret-device-token")
        );
        let size = body["content_length"].as_u64().expect("size");
        let sha = body["sha256"].as_str().expect("sha");
        let origin = state.origin.lock().expect("origin").clone();
        Json(json!({
            "backup_id": "00000000-0000-4000-8000-000000000002",
            "retention_class": "rolling",
            "method": "PUT",
            "upload_url": format!("{origin}/object"),
            "required_headers": {
                "content-length": size.to_string(),
                "x-amz-content-sha256": "UNSIGNED-PAYLOAD",
                "x-amz-meta-sha256": sha,
                "x-amz-server-side-encryption": "AES256"
            },
            "expires_at": "2026-07-14T12:15:00Z"
        }))
    }

    async fn mock_upload(
        State(state): State<MockObservations>,
        headers: AxumHeaders,
        body: Bytes,
    ) -> AxumStatus {
        assert!(headers.get("authorization").is_none());
        assert!(headers.get("cookie").is_none());
        assert_eq!(
            headers
                .get("x-amz-server-side-encryption")
                .and_then(|value| value.to_str().ok()),
            Some("AES256")
        );
        assert!(!body.starts_with(b"SQLite format 3"));
        *state.uploaded_without_bearer.lock().expect("uploaded") = true;
        AxumStatus::OK
    }

    async fn mock_complete(
        State(state): State<MockObservations>,
        headers: AxumHeaders,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer secret-device-token")
        );
        assert_eq!(body["backup_id"], "00000000-0000-4000-8000-000000000002");
        *state.completed.lock().expect("completed") = true;
        Json(json!({
            "backup_id": body["backup_id"],
            "status": "completed",
            "completed_at": "2026-07-14T12:01:00Z",
            "etag": "test-etag",
            "version_id": null
        }))
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
    async fn gateway_flow_stores_token_outside_state_and_never_sends_it_to_object_put() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let observations = MockObservations::default();
        *observations.origin.lock().expect("origin") = origin.clone();
        let app = Router::new()
            .route("/v1/enroll", post(mock_enroll))
            .route("/v1/backups/upload-url", post(mock_reserve))
            .route("/object", put(mock_upload))
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
        assert!(*observations
            .uploaded_without_bearer
            .lock()
            .expect("uploaded"));
        assert!(*observations.completed.lock().expect("completed"));

        server.abort();
    }

    #[derive(Clone, Default)]
    struct RelayMockState {
        direct_upload_url: Arc<StdMutex<String>>,
        reserve_count: Arc<StdMutex<usize>>,
        relay_count: Arc<StdMutex<usize>>,
        complete_count: Arc<StdMutex<usize>>,
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
        *state.expected_sha256.lock().expect("expected sha256") = sha256.clone();
        let direct_upload_url = state
            .direct_upload_url
            .lock()
            .expect("direct upload url")
            .clone();

        Json(json!({
            "backup_id": "00000000-0000-4000-8000-000000000022",
            "retention_class": "rolling",
            "method": "PUT",
            "upload_url": direct_upload_url,
            "required_headers": {
                "content-length": size.to_string(),
                "x-amz-content-sha256": "UNSIGNED-PAYLOAD",
                "x-amz-meta-sha256": sha256,
                "x-amz-server-side-encryption": "AES256"
            },
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
            let (response_backup_id, response_status) = if attempt == 1 {
                ("00000000-0000-4000-8000-000000000099", "completed")
            } else {
                (backup_id.as_str(), "pending")
            };
            return (
                AxumStatus::OK,
                Json(json!({
                    "backup_id": response_backup_id,
                    "status": response_status,
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
                "version_id": null
            })),
        )
    }

    async fn mock_relay_complete(State(state): State<RelayMockState>) -> (AxumStatus, Json<Value>) {
        *state.complete_count.lock().expect("complete count") += 1;
        (
            AxumStatus::OK,
            Json(json!({"status": "unexpected_complete"})),
        )
    }

    async fn mock_direct_http_rejection(_body: Bytes) -> AxumStatus {
        AxumStatus::SERVICE_UNAVAILABLE
    }

    #[tokio::test]
    async fn tls_transport_failure_relay_is_ciphertext_only_and_retries_same_reservation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let state = RelayMockState::default();
        *state.direct_upload_url.lock().expect("direct upload url") =
            format!("https://{}/object", listener.local_addr().expect("address"));
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
                .expect("relay retry"),
            1
        );
        assert!(restarted_service
            .list_queue()
            .expect("empty queue")
            .is_empty());
        assert_eq!(*state.reserve_count.lock().expect("reserve count"), 1);
        assert_eq!(*state.relay_count.lock().expect("relay count"), 2);
        assert_eq!(*state.complete_count.lock().expect("complete count"), 0);

        server.abort();
    }

    #[tokio::test]
    async fn direct_b2_http_status_does_not_trigger_relay() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let state = RelayMockState::default();
        *state.direct_upload_url.lock().expect("direct upload url") = format!("{origin}/object");
        let app = Router::new()
            .route("/v1/backups/upload-url", post(mock_relay_reserve))
            .route("/object", put(mock_direct_http_rejection))
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
        assert_eq!(report.pending_count, 1);
        let queued = service.list_queue().expect("queued backup");
        assert_eq!(
            queued[0].metadata.upload.as_ref().expect("pending").stage,
            UploadStage::Reserved
        );
        assert_eq!(*state.reserve_count.lock().expect("reserve count"), 1);
        assert_eq!(*state.relay_count.lock().expect("relay count"), 0);
        assert_eq!(*state.complete_count.lock().expect("complete count"), 0);

        server.abort();
    }

    #[tokio::test]
    async fn relay_requires_matching_completed_response_before_deleting_queue() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let state = RelayMockState::default();
        *state.direct_upload_url.lock().expect("direct upload url") =
            format!("https://{}/object", listener.local_addr().expect("address"));
        *state
            .invalid_successes_before_valid
            .lock()
            .expect("invalid successes") = 2;
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

        assert!(matches!(
            service.flush_queue("relay-device-token").await,
            Err(BackupError::InvalidGatewayResponse)
        ));
        assert_eq!(service.list_queue().expect("second queued retry").len(), 1);

        assert_eq!(
            service
                .flush_queue("relay-device-token")
                .await
                .expect("valid completion retry"),
            1
        );
        assert!(service.list_queue().expect("empty queue").is_empty());
        assert_eq!(*state.reserve_count.lock().expect("reserve count"), 1);
        assert_eq!(*state.relay_count.lock().expect("relay count"), 3);
        assert_eq!(*state.complete_count.lock().expect("complete count"), 0);

        server.abort();
    }

    #[test]
    fn upload_timeout_covers_the_gateway_proxy_window_without_becoming_unbounded() {
        assert!(UPLOAD_REQUEST_TIMEOUT >= Duration::from_secs(15 * 60));
        assert!(UPLOAD_REQUEST_TIMEOUT < Duration::from_secs(60 * 60));
    }

    fn queued_for_upload(temp: &TempDir) -> QueuedBackup {
        let path = temp.path().join("test.sqlite3.age");
        QueuedBackup {
            metadata_path: queue_metadata_path(&path),
            path,
            metadata: QueueMetadata {
                format_version: QUEUE_METADATA_VERSION,
                created_at: "2026-07-14T12:00:00Z".to_owned(),
                retention_class: RetentionClass::Rolling,
                size_bytes: 3,
                sha256: "a".repeat(64),
                local_recovery: false,
                upload: None,
            },
        }
    }

    fn valid_production_reservation() -> UploadUrlResponse {
        UploadUrlResponse {
            backup_id: "00000000-0000-4000-8000-000000000002".to_owned(),
            method: "PUT".to_owned(),
            upload_url: concat!(
                "https://s3.us-west-004.backblazeb2.com/",
                "stronganchor-pusula-desktop-backups/backups/rolling/",
                "device/file.sqlite3.age?X-Amz-Signature=test"
            )
            .to_owned(),
            required_headers: BTreeMap::from([
                ("content-length".to_owned(), "3".to_owned()),
                (
                    "x-amz-content-sha256".to_owned(),
                    "UNSIGNED-PAYLOAD".to_owned(),
                ),
                ("x-amz-meta-sha256".to_owned(), "a".repeat(64)),
                (
                    "x-amz-server-side-encryption".to_owned(),
                    "AES256".to_owned(),
                ),
            ]),
        }
    }

    #[test]
    fn production_uploads_are_pinned_to_the_expected_b2_destination_and_headers() {
        let temp = TempDir::new().expect("temp");
        let queued = queued_for_upload(&temp);
        let gateway = GatewayClient::new(
            "https://pusula-backup.stronganchortech.com/",
            false,
            Duration::from_secs(2),
        )
        .expect("gateway");
        gateway
            .validate_upload_request(&queued, &valid_production_reservation())
            .expect("valid production upload");

        for invalid_url in [
            "https://evil.example/stronganchor-pusula-desktop-backups/backups/file?sig=x",
            "https://s3.us-west-004.backblazeb2.com/other-bucket/backups/file?sig=x",
            "https://s3.us-west-004.backblazeb2.com:444/stronganchor-pusula-desktop-backups/backups/file?sig=x",
            "https://s3.us-west-004.backblazeb2.com/stronganchor-pusula-desktop-backups/backups/file",
            "http://s3.us-west-004.backblazeb2.com/stronganchor-pusula-desktop-backups/backups/file?sig=x",
        ] {
            let mut reservation = valid_production_reservation();
            reservation.upload_url = invalid_url.to_owned();
            assert!(matches!(
                gateway.validate_upload_request(&queued, &reservation),
                Err(BackupError::InvalidGatewayResponse)
            ));
        }

        let mut extra_header = valid_production_reservation();
        extra_header
            .required_headers
            .insert("authorization".to_owned(), "secret".to_owned());
        assert!(matches!(
            gateway.validate_upload_request(&queued, &extra_header),
            Err(BackupError::InvalidGatewayResponse)
        ));

        for (header, invalid_value) in [
            ("content-length", "4"),
            ("x-amz-content-sha256", "payload-hash"),
            ("x-amz-meta-sha256", "wrong"),
            ("x-amz-server-side-encryption", "none"),
        ] {
            let mut reservation = valid_production_reservation();
            reservation
                .required_headers
                .insert(header.to_owned(), invalid_value.to_owned());
            assert!(matches!(
                gateway.validate_upload_request(&queued, &reservation),
                Err(BackupError::InvalidGatewayResponse)
            ));
        }
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
