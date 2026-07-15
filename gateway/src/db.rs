use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    crypto::{generate_secret, hash_secret, validate_name},
    error::{AppError, Result},
};

const INITIAL_MIGRATION: &str = include_str!("../migrations/0001_initial.sql");
const RELAY_ATTEMPT_MIGRATION: &str = include_str!("../migrations/0002_relay_attempted_at.sql");
const BACKUP_ADMISSION_MIGRATION: &str =
    include_str!("../migrations/0003_backup_admission_and_verification.sql");
const LOCAL_STORAGE_RETENTION_MIGRATION: &str =
    include_str!("../migrations/0004_local_storage_retention.sql");
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "initial", INITIAL_MIGRATION),
    (2, "relay_attempted_at", RELAY_ATTEMPT_MIGRATION),
    (
        3,
        "backup_admission_and_verification",
        BACKUP_ADMISSION_MIGRATION,
    ),
    (
        4,
        "local_storage_retention",
        LOCAL_STORAGE_RETENTION_MIGRATION,
    ),
];

const DAY_SECONDS: i64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
    #[default]
    Rolling,
    Daily,
    Monthly,
}

impl RetentionClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rolling => "rolling",
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }

    fn from_database(value: &str) -> rusqlite::Result<Self> {
        match value {
            "rolling" => Ok(Self::Rolling),
            "daily" => Ok(Self::Daily),
            "monthly" => Ok(Self::Monthly),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid retention class {other}"),
                )),
            )),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct AdmissionPolicy {
    pub rate_capacity: u32,
    pub rate_refill_seconds: u64,
    pub max_pending_per_device: u32,
    pub byte_quota_24h: u64,
    pub pending_max_age_seconds: u64,
    pub pending_cleanup_limit: u32,
    pub authorization_cleanup_limit: u32,
    pub daily_min_interval_seconds: u64,
    pub monthly_min_interval_seconds: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct RetentionPolicy {
    pub rolling_seconds: u64,
    pub daily_seconds: u64,
    pub monthly_seconds: u64,
    pub pending_max_age_seconds: u64,
    pub pending_cleanup_limit: u32,
    pub cleanup_limit: u32,
}

#[derive(Clone)]
pub struct Database {
    path: Arc<PathBuf>,
    pepper: Arc<[u8]>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnrollmentCredential {
    pub enrollment_id: String,
    pub enrollment_code: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceCredential {
    pub device_id: String,
    pub device_name: String,
    pub device_token: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedDevice {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupRecord {
    pub id: String,
    pub device_id: String,
    pub object_key: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub retention_class: RetentionClass,
    pub status: String,
    pub created_at: i64,
    pub upload_expires_at: i64,
    pub completed_at: Option<i64>,
    pub etag: Option<String>,
    pub version_id: Option<String>,
    pub verified_size_bytes: Option<u64>,
    pub verified_sha256: Option<String>,
    pub verified_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct BackupSummary {
    pub latest_completed: Option<BackupRecord>,
    pub active_pending: u64,
    pub expired_pending: u64,
}

#[derive(Debug, Clone)]
pub struct StoragePurgeCandidate {
    pub backup_id: String,
    pub object_key: String,
}

impl Database {
    pub fn new(path: impl Into<PathBuf>, pepper: Arc<[u8]>) -> Self {
        Self {
            path: Arc::new(path.into()),
            pepper,
        }
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    pub fn migrate(&self) -> Result<()> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(AppError::internal)?;
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AppError::internal)?;
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                    version INTEGER PRIMARY KEY NOT NULL,
                    name TEXT NOT NULL,
                    checksum TEXT NOT NULL,
                    applied_at INTEGER NOT NULL
                ) STRICT;",
            )
            .map_err(AppError::internal)?;

        for (version, name, sql) in MIGRATIONS {
            let checksum = migration_checksum(sql);
            let existing = transaction
                .query_row(
                    "SELECT checksum FROM schema_migrations WHERE version = ?1",
                    params![version],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(AppError::internal)?;
            match existing {
                Some(existing) if existing != checksum => {
                    return Err(AppError::Internal(format!(
                        "migration {version} checksum does not match the applied schema"
                    )));
                }
                Some(_) => {}
                None => {
                    transaction.execute_batch(sql).map_err(AppError::internal)?;
                    transaction
                        .execute(
                            "INSERT INTO schema_migrations(version, name, checksum, applied_at)
                             VALUES (?1, ?2, ?3, ?4)",
                            params![version, name, checksum, now_epoch()],
                        )
                        .map_err(AppError::internal)?;
                }
            }
        }
        transaction.commit().map_err(AppError::internal)
    }

    pub fn health_check(&self) -> Result<()> {
        self.connect()?
            .query_row("SELECT 1", [], |_| Ok(()))
            .map_err(AppError::internal)
    }

    pub fn issue_enrollment(
        &self,
        label: &str,
        valid_for_seconds: u64,
    ) -> Result<EnrollmentCredential> {
        let label = validate_name(label)?;
        if valid_for_seconds == 0 || valid_for_seconds > 30 * 24 * 60 * 60 {
            return Err(AppError::BadRequest(
                "enrollment lifetime must be between 1 second and 30 days",
            ));
        }
        let now = now_epoch();
        let expires_at = now
            .checked_add(i64::try_from(valid_for_seconds).map_err(AppError::internal)?)
            .ok_or_else(|| AppError::Internal("enrollment expiry overflow".to_owned()))?;
        let id = Uuid::new_v4().to_string();
        let code = generate_secret("pen_", 24)?;
        let hash = hash_secret(&self.pepper, b"enrollment-code", &code);
        self.connect()?
            .execute(
                "INSERT INTO enrollment_codes(id, label, code_hash, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, label, hash.as_slice(), now, expires_at],
            )
            .map_err(AppError::internal)?;
        Ok(EnrollmentCredential {
            enrollment_id: id,
            enrollment_code: code,
            expires_at,
        })
    }

    pub fn revoke_enrollment(&self, enrollment_id: &str) -> Result<()> {
        let changed = self
            .connect()?
            .execute(
                "UPDATE enrollment_codes
                 SET revoked_at = COALESCE(revoked_at, ?2)
                 WHERE id = ?1",
                params![enrollment_id, now_epoch()],
            )
            .map_err(AppError::internal)?;
        if changed == 0 {
            return Err(AppError::NotFound);
        }
        Ok(())
    }

    pub fn issue_device(&self, name: &str, rate_capacity: u32) -> Result<DeviceCredential> {
        validate_rate_capacity(rate_capacity)?;
        let now = now_epoch();
        let credential = self.new_device_credential(name, now)?;
        self.insert_device(&credential, rate_capacity, now)?;
        Ok(credential)
    }

    pub fn enroll_device(
        &self,
        enrollment_code: &str,
        device_name: &str,
        rate_capacity: u32,
    ) -> Result<DeviceCredential> {
        validate_rate_capacity(rate_capacity)?;
        if enrollment_code.len() < 20 || enrollment_code.len() > 100 {
            return Err(AppError::Unauthorized);
        }
        let device_name = validate_name(device_name)?;
        let now = now_epoch();
        let code_hash = hash_secret(&self.pepper, b"enrollment-code", enrollment_code);

        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AppError::internal)?;
        let enrollment_id = transaction
            .query_row(
                "SELECT id FROM enrollment_codes
                 WHERE code_hash = ?1
                   AND used_at IS NULL
                   AND revoked_at IS NULL
                   AND expires_at > ?2",
                params![code_hash.as_slice(), now],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(AppError::internal)?
            .ok_or(AppError::Unauthorized)?;

        // Do not spend entropy or construct a bearer credential until the
        // one-time code has been proven valid inside the write transaction.
        let credential = self.new_device_credential(&device_name, now)?;

        transaction
            .execute(
                "INSERT INTO devices(
                    id, name, token_hash, created_at,
                    upload_tokens, upload_tokens_updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?4)",
                params![
                    credential.device_id,
                    credential.device_name,
                    hash_secret(&self.pepper, b"device-token", &credential.device_token).as_slice(),
                    now,
                    f64::from(rate_capacity)
                ],
            )
            .map_err(AppError::internal)?;
        let changed = transaction
            .execute(
                "UPDATE enrollment_codes
                 SET used_at = ?2
                 WHERE id = ?1 AND used_at IS NULL AND revoked_at IS NULL",
                params![enrollment_id, now],
            )
            .map_err(AppError::internal)?;
        if changed != 1 {
            return Err(AppError::Unauthorized);
        }
        transaction.commit().map_err(AppError::internal)?;
        Ok(credential)
    }

    pub fn revoke_device(&self, device_id: &str) -> Result<()> {
        let changed = self
            .connect()?
            .execute(
                "UPDATE devices
                 SET revoked_at = COALESCE(revoked_at, ?2)
                 WHERE id = ?1",
                params![device_id, now_epoch()],
            )
            .map_err(AppError::internal)?;
        if changed == 0 {
            return Err(AppError::NotFound);
        }
        Ok(())
    }

    pub fn authenticate(&self, token: &str) -> Result<AuthenticatedDevice> {
        if !token.starts_with("pdt_") || token.len() < 30 || token.len() > 100 {
            return Err(AppError::Unauthorized);
        }
        let now = now_epoch();
        let token_hash = hash_secret(&self.pepper, b"device-token", token);
        let connection = self.connect()?;
        let device = connection
            .query_row(
                "SELECT id FROM devices WHERE token_hash = ?1 AND revoked_at IS NULL",
                params![token_hash.as_slice()],
                |row| Ok(AuthenticatedDevice { id: row.get(0)? }),
            )
            .optional()
            .map_err(AppError::internal)?
            .ok_or(AppError::Unauthorized)?;
        connection
            .execute(
                "UPDATE devices SET last_seen_at = ?2
                 WHERE id = ?1 AND (last_seen_at IS NULL OR last_seen_at < ?2 - 300)",
                params![device.id, now],
            )
            .map_err(AppError::internal)?;
        Ok(device)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reserve_or_reuse_backup(
        &self,
        device_id: &str,
        backup_id: &str,
        object_key: &str,
        size_bytes: u64,
        sha256: &str,
        retention_class: RetentionClass,
        upload_expires_at: i64,
        policy: AdmissionPolicy,
    ) -> Result<BackupRecord> {
        validate_admission_policy(policy)?;
        let database_size = i64::try_from(size_bytes).map_err(AppError::internal)?;
        let now = now_epoch();
        if upload_expires_at <= now {
            return Err(AppError::BadRequest("upload expiry must be in the future"));
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AppError::internal)?;
        let (stored_tokens, updated_at) = transaction
            .query_row(
                "SELECT upload_tokens, upload_tokens_updated_at
                 FROM devices WHERE id = ?1 AND revoked_at IS NULL",
                params![device_id],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(AppError::internal)?
            .ok_or(AppError::Unauthorized)?;

        let reusable = transaction
            .query_row(
                "SELECT id, device_id, object_key, size_bytes, sha256,
                        retention_class, status, created_at, upload_expires_at,
                        completed_at, etag, version_id, verified_size_bytes,
                        verified_sha256, verified_at
                 FROM backups
                 WHERE device_id = ?1 AND status = 'pending'
                   AND retention_class = ?2 AND size_bytes = ?3 AND sha256 = ?4
                   AND NOT EXISTS (
                       SELECT 1 FROM storage_purges p WHERE p.backup_id = backups.id
                   )
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
                params![device_id, retention_class.as_str(), database_size, sha256],
                map_backup,
            )
            .optional()
            .map_err(AppError::internal)?;
        let quota_window_start = now.saturating_sub(DAY_SECONDS);
        transaction
            .execute(
                "DELETE FROM upload_authorizations
                 WHERE id IN (
                     SELECT id FROM upload_authorizations
                     WHERE authorized_at <= ?1
                     ORDER BY authorized_at ASC, id ASC
                     LIMIT ?2
                 )",
                params![quota_window_start, policy.authorization_cleanup_limit],
            )
            .map_err(AppError::internal)?;

        if reusable.is_none() {
            let pending_count = transaction
                .query_row(
                    "SELECT COUNT(*) FROM backups
                     WHERE device_id = ?1 AND status = 'pending'
                       AND NOT EXISTS (
                           SELECT 1 FROM storage_purges p
                           WHERE p.backup_id = backups.id
                       )",
                    params![device_id],
                    |row| row.get::<_, u64>(0),
                )
                .map_err(AppError::internal)?;
            if pending_count >= u64::from(policy.max_pending_per_device) {
                return Err(AppError::RateLimited {
                    retry_after_seconds: 15 * 60,
                });
            }

            let minimum_interval = match retention_class {
                RetentionClass::Rolling => None,
                RetentionClass::Daily => Some(policy.daily_min_interval_seconds),
                RetentionClass::Monthly => Some(policy.monthly_min_interval_seconds),
            };
            if let Some(minimum_interval) = minimum_interval {
                let last_reserved = transaction
                    .query_row(
                        "SELECT MAX(created_at) FROM backups
                         WHERE device_id = ?1 AND retention_class = ?2
                           AND NOT EXISTS (
                               SELECT 1 FROM storage_purges p
                               WHERE p.backup_id = backups.id
                           )",
                        params![device_id, retention_class.as_str()],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .map_err(AppError::internal)?;
                if let Some(last_reserved) = last_reserved {
                    let next_allowed = last_reserved.saturating_add(
                        i64::try_from(minimum_interval).map_err(AppError::internal)?,
                    );
                    if now < next_allowed {
                        return Err(AppError::RateLimited {
                            retry_after_seconds: u64::try_from(next_allowed - now)
                                .map_err(AppError::internal)?
                                .max(1),
                        });
                    }
                }
            }
        }

        let (bytes_in_window, oldest_in_window) = transaction
            .query_row(
                "SELECT COALESCE(SUM(size_bytes), 0), MIN(authorized_at)
                 FROM upload_authorizations
                 WHERE device_id = ?1 AND authorized_at > ?2",
                params![device_id, quota_window_start],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .map_err(AppError::internal)?;
        if size_bytes > policy.byte_quota_24h
            || bytes_in_window.saturating_add(size_bytes) > policy.byte_quota_24h
        {
            let retry_after_seconds = oldest_in_window
                .map(|authorized_at| {
                    authorized_at
                        .saturating_add(DAY_SECONDS)
                        .saturating_sub(now)
                })
                .and_then(|seconds| u64::try_from(seconds).ok())
                .unwrap_or(60)
                .max(1);
            return Err(AppError::RateLimited {
                retry_after_seconds,
            });
        }

        let elapsed = now.saturating_sub(updated_at) as f64;
        let available = (stored_tokens + elapsed / policy.rate_refill_seconds as f64)
            .min(f64::from(policy.rate_capacity));
        if available < 1.0 {
            let retry_after = ((1.0 - available) * policy.rate_refill_seconds as f64).ceil() as u64;
            return Err(AppError::RateLimited {
                retry_after_seconds: retry_after.max(1),
            });
        }
        transaction
            .execute(
                "UPDATE devices
                 SET upload_tokens = ?2, upload_tokens_updated_at = ?3
                 WHERE id = ?1 AND revoked_at IS NULL",
                params![device_id, available - 1.0, now],
            )
            .map_err(AppError::internal)?;
        let backup = if let Some(mut backup) = reusable {
            transaction
                .execute(
                    "UPDATE backups SET upload_expires_at = ?3
                     WHERE id = ?1 AND device_id = ?2 AND status = 'pending'",
                    params![backup.id, device_id, upload_expires_at],
                )
                .map_err(AppError::internal)?;
            backup.upload_expires_at = upload_expires_at;
            backup
        } else {
            transaction
                .execute(
                    "INSERT INTO backups(
                        id, device_id, object_key, size_bytes, sha256,
                        retention_class, status, created_at, upload_expires_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
                    params![
                        backup_id,
                        device_id,
                        object_key,
                        database_size,
                        sha256,
                        retention_class.as_str(),
                        now,
                        upload_expires_at
                    ],
                )
                .map_err(AppError::internal)?;
            BackupRecord {
                id: backup_id.to_owned(),
                device_id: device_id.to_owned(),
                object_key: object_key.to_owned(),
                size_bytes,
                sha256: sha256.to_owned(),
                retention_class,
                status: "pending".to_owned(),
                created_at: now,
                upload_expires_at,
                completed_at: None,
                etag: None,
                version_id: None,
                verified_size_bytes: None,
                verified_sha256: None,
                verified_at: None,
            }
        };
        transaction
            .execute(
                "INSERT INTO upload_authorizations(
                    device_id, backup_id, size_bytes, authorized_at
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![device_id, backup.id, database_size, now],
            )
            .map_err(AppError::internal)?;
        transaction.commit().map_err(AppError::internal)?;
        Ok(backup)
    }

    /// Admit one relay write attempt for an existing pending reservation.
    /// Every attempt consumes one persisted token and one reservation-size
    /// entry in the rolling authorization ledger, including the first relay.
    pub fn begin_relay_attempt(
        &self,
        device_id: &str,
        backup_id: &str,
        policy: AdmissionPolicy,
    ) -> Result<()> {
        validate_admission_policy(policy)?;

        let now = now_epoch();
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AppError::internal)?;
        let state = transaction
            .query_row(
                "SELECT d.upload_tokens, d.upload_tokens_updated_at, b.size_bytes
                 FROM backups b
                 JOIN devices d ON d.id = b.device_id
                 WHERE b.id = ?1 AND b.device_id = ?2 AND b.status = 'pending'
                   AND d.revoked_at IS NULL",
                params![backup_id, device_id],
                |row| {
                    Ok((
                        row.get::<_, f64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(AppError::internal)?
            .ok_or(AppError::Conflict(
                "backup is no longer pending for this device",
            ))?;

        let quota_window_start = now.saturating_sub(DAY_SECONDS);
        transaction
            .execute(
                "DELETE FROM upload_authorizations
                 WHERE id IN (
                     SELECT id FROM upload_authorizations
                     WHERE authorized_at <= ?1
                     ORDER BY authorized_at ASC, id ASC
                     LIMIT ?2
                 )",
                params![quota_window_start, policy.authorization_cleanup_limit],
            )
            .map_err(AppError::internal)?;
        let (bytes_in_window, oldest_in_window) = transaction
            .query_row(
                "SELECT COALESCE(SUM(size_bytes), 0), MIN(authorized_at)
                 FROM upload_authorizations
                 WHERE device_id = ?1 AND authorized_at > ?2",
                params![device_id, quota_window_start],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .map_err(AppError::internal)?;
        if state.2 > policy.byte_quota_24h
            || bytes_in_window.saturating_add(state.2) > policy.byte_quota_24h
        {
            let retry_after_seconds = oldest_in_window
                .map(|authorized_at| {
                    authorized_at
                        .saturating_add(DAY_SECONDS)
                        .saturating_sub(now)
                })
                .and_then(|seconds| u64::try_from(seconds).ok())
                .unwrap_or(60)
                .max(1);
            return Err(AppError::RateLimited {
                retry_after_seconds,
            });
        }

        let elapsed = now.saturating_sub(state.1) as f64;
        let available = (state.0 + elapsed / policy.rate_refill_seconds as f64)
            .min(f64::from(policy.rate_capacity));
        if available < 1.0 {
            let retry_after = ((1.0 - available) * policy.rate_refill_seconds as f64).ceil() as u64;
            return Err(AppError::RateLimited {
                retry_after_seconds: retry_after.max(1),
            });
        }
        transaction
            .execute(
                "UPDATE devices
                 SET upload_tokens = ?2, upload_tokens_updated_at = ?3
                 WHERE id = ?1 AND revoked_at IS NULL",
                params![device_id, available - 1.0, now],
            )
            .map_err(AppError::internal)?;
        transaction
            .execute(
                "UPDATE backups SET relay_attempted_at = COALESCE(relay_attempted_at, ?3)
                 WHERE id = ?1 AND device_id = ?2 AND status = 'pending'",
                params![backup_id, device_id, now],
            )
            .map_err(AppError::internal)?;
        transaction
            .execute(
                "INSERT INTO upload_authorizations(
                    device_id, backup_id, size_bytes, authorized_at
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![device_id, backup_id, state.2, now],
            )
            .map_err(AppError::internal)?;
        transaction.commit().map_err(AppError::internal)
    }

    pub fn backup_for_device(&self, device_id: &str, backup_id: &str) -> Result<BackupRecord> {
        self.connect()?
            .query_row(
                "SELECT id, device_id, object_key, size_bytes, sha256,
                        retention_class, status, created_at, upload_expires_at,
                        completed_at, etag, version_id, verified_size_bytes,
                        verified_sha256, verified_at
                 FROM backups b WHERE id = ?1 AND device_id = ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM storage_purges p WHERE p.backup_id = b.id
                   )",
                params![backup_id, device_id],
                map_backup,
            )
            .optional()
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)
    }

    pub fn mark_backup_completed(
        &self,
        device_id: &str,
        backup_id: &str,
        etag: Option<&str>,
        version_id: &str,
        verified_size_bytes: u64,
        verified_sha256: &str,
    ) -> Result<BackupRecord> {
        validate_completion_evidence(version_id, verified_size_bytes, verified_sha256)?;
        let now = now_epoch();
        let database_verified_size =
            i64::try_from(verified_size_bytes).map_err(AppError::internal)?;
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AppError::internal)?;
        let changed = transaction
            .execute(
                "UPDATE backups
                 SET status = 'completed', completed_at = ?3, etag = ?4,
                     version_id = ?5, verified_size_bytes = ?6,
                     verified_sha256 = ?7, verified_at = ?3
                 WHERE id = ?1 AND device_id = ?2 AND status = 'pending'
                   AND size_bytes = ?6 AND sha256 = ?7",
                params![
                    backup_id,
                    device_id,
                    now,
                    etag,
                    version_id,
                    database_verified_size,
                    verified_sha256
                ],
            )
            .map_err(AppError::internal)?;
        let record = transaction
            .query_row(
                "SELECT id, device_id, object_key, size_bytes, sha256,
                        retention_class, status, created_at, upload_expires_at,
                        completed_at, etag, version_id, verified_size_bytes,
                        verified_sha256, verified_at
                 FROM backups WHERE id = ?1 AND device_id = ?2",
                params![backup_id, device_id],
                map_backup,
            )
            .optional()
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)?;

        if changed == 0 {
            if record.status == "completed"
                && record.version_id.as_deref() == Some(version_id)
                && record.verified_size_bytes == Some(verified_size_bytes)
                && record.verified_sha256.as_deref() == Some(verified_sha256)
            {
                transaction.commit().map_err(AppError::internal)?;
                return Ok(record);
            }
            if record.status == "pending" {
                return Err(AppError::Upstream(
                    "verified object did not match its reservation".to_owned(),
                ));
            }
            return Err(AppError::Conflict(
                "backup completion evidence conflicts with stored state",
            ));
        }

        transaction.commit().map_err(AppError::internal)?;
        Ok(record)
    }

    pub fn backup_summary(&self, device_id: &str) -> Result<BackupSummary> {
        let now = now_epoch();
        let connection = self.connect()?;
        let latest_completed = connection
            .query_row(
                "SELECT id, device_id, object_key, size_bytes, sha256,
                        retention_class, status, created_at, upload_expires_at,
                        completed_at, etag, version_id, verified_size_bytes,
                        verified_sha256, verified_at
                 FROM backups
                 WHERE device_id = ?1 AND status = 'completed'
                   AND NOT EXISTS (
                       SELECT 1 FROM storage_purges p WHERE p.backup_id = backups.id
                   )
                   AND version_id IS NOT NULL AND length(version_id) BETWEEN 1 AND 256
                   AND verified_size_bytes = size_bytes
                   AND verified_sha256 = sha256
                   AND verified_at IS NOT NULL
                 ORDER BY completed_at DESC LIMIT 1",
                params![device_id],
                map_backup,
            )
            .optional()
            .map_err(AppError::internal)?
            .filter(has_recovery_authority);
        let (active_pending, expired_pending) = connection
            .query_row(
                "SELECT
                    COALESCE(SUM(CASE WHEN upload_expires_at >= ?2 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN upload_expires_at < ?2 THEN 1 ELSE 0 END), 0)
                 FROM backups WHERE device_id = ?1 AND status = 'pending'",
                params![device_id, now],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
            )
            .map_err(AppError::internal)?;
        Ok(BackupSummary {
            latest_completed,
            active_pending,
            expired_pending,
        })
    }

    pub fn completed_backup(&self, backup_id: &str) -> Result<BackupRecord> {
        self.connect_read_only()?
            .query_row(
                "SELECT id, device_id, object_key, size_bytes, sha256,
                        retention_class, status, created_at, upload_expires_at,
                        completed_at, etag, version_id, verified_size_bytes,
                        verified_sha256, verified_at
                 FROM backups
                 WHERE id = ?1 AND status = 'completed'
                   AND NOT EXISTS (
                       SELECT 1 FROM storage_purges p WHERE p.backup_id = backups.id
                   )
                   AND version_id IS NOT NULL AND length(version_id) BETWEEN 1 AND 256
                   AND verified_size_bytes = size_bytes
                   AND verified_sha256 = sha256
                   AND verified_at IS NOT NULL",
                params![backup_id],
                map_backup,
            )
            .optional()
            .map_err(AppError::internal)?
            .filter(has_recovery_authority)
            .ok_or(AppError::NotFound)
    }

    pub fn list_completed_backups(&self, limit: u32) -> Result<Vec<BackupRecord>> {
        if !(1..=500).contains(&limit) {
            return Err(AppError::BadRequest("limit must be between 1 and 500"));
        }
        let connection = self.connect_read_only()?;
        let mut statement = connection
            .prepare(
                "SELECT id, device_id, object_key, size_bytes, sha256,
                        retention_class, status, created_at, upload_expires_at,
                        completed_at, etag, version_id, verified_size_bytes,
                        verified_sha256, verified_at
                 FROM backups
                 WHERE status = 'completed'
                   AND NOT EXISTS (
                       SELECT 1 FROM storage_purges p WHERE p.backup_id = backups.id
                   )
                   AND version_id IS NOT NULL AND length(version_id) BETWEEN 1 AND 256
                   AND verified_size_bytes = size_bytes
                   AND verified_sha256 = sha256
                   AND verified_at IS NOT NULL
                 ORDER BY completed_at DESC, id DESC
                 LIMIT ?1",
            )
            .map_err(AppError::internal)?;
        let rows = statement
            .query_map(params![limit], map_backup)
            .map_err(AppError::internal)?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(AppError::internal)?
            .into_iter()
            .filter(has_recovery_authority)
            .collect())
    }

    pub fn claim_storage_purges(
        &self,
        policy: RetentionPolicy,
    ) -> Result<Vec<StoragePurgeCandidate>> {
        validate_retention_policy(policy)?;
        let now = now_epoch();
        let rolling_cutoff = retention_cutoff(now, policy.rolling_seconds)?;
        let daily_cutoff = retention_cutoff(now, policy.daily_seconds)?;
        let monthly_cutoff = retention_cutoff(now, policy.monthly_seconds)?;
        let pending_cutoff = retention_cutoff(now, policy.pending_max_age_seconds)?;
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AppError::internal)?;

        let unfinished = transaction
            .query_row(
                "SELECT COUNT(*) FROM storage_purges WHERE completed_at IS NULL",
                [],
                |row| row.get::<_, u32>(0),
            )
            .map_err(AppError::internal)?;
        let mut remaining = policy.cleanup_limit.saturating_sub(unfinished);
        if remaining > 0 {
            let pending_limit = remaining.min(policy.pending_cleanup_limit);
            transaction
                .execute(
                    "INSERT INTO storage_purges(backup_id, started_at)
                     SELECT b.id, ?1
                     FROM backups b
                     WHERE b.status = 'pending'
                       AND b.upload_expires_at < ?2
                       AND NOT EXISTS (
                           SELECT 1 FROM storage_purges existing
                           WHERE existing.backup_id = b.id
                       )
                     ORDER BY b.upload_expires_at ASC, b.id ASC
                     LIMIT ?3",
                    params![now, pending_cutoff, pending_limit],
                )
                .map_err(AppError::internal)?;
            let unfinished_after_pending = transaction
                .query_row(
                    "SELECT COUNT(*) FROM storage_purges WHERE completed_at IS NULL",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .map_err(AppError::internal)?;
            remaining = policy
                .cleanup_limit
                .saturating_sub(unfinished_after_pending);
        }
        if remaining > 0 {
            transaction
                .execute(
                    "INSERT INTO storage_purges(backup_id, started_at)
                     SELECT b.id, ?1
                     FROM backups b
                     WHERE b.status = 'completed'
                       AND b.completed_at IS NOT NULL
                       AND NOT EXISTS (
                           SELECT 1 FROM storage_purges existing
                           WHERE existing.backup_id = b.id
                       )
                       AND b.completed_at < CASE b.retention_class
                           WHEN 'rolling' THEN ?2
                           WHEN 'daily' THEN ?3
                           WHEN 'monthly' THEN ?4
                       END
                       AND b.id <> (
                           SELECT newest.id
                           FROM backups newest
                           WHERE newest.device_id = b.device_id
                             AND newest.retention_class = b.retention_class
                             AND newest.status = 'completed'
                             AND newest.completed_at IS NOT NULL
                             AND NOT EXISTS (
                                 SELECT 1 FROM storage_purges newest_purge
                                 WHERE newest_purge.backup_id = newest.id
                             )
                           ORDER BY newest.completed_at DESC, newest.id DESC
                           LIMIT 1
                       )
                     ORDER BY b.completed_at ASC, b.id ASC
                     LIMIT ?5",
                    params![now, rolling_cutoff, daily_cutoff, monthly_cutoff, remaining],
                )
                .map_err(AppError::internal)?;
        }

        let candidates = {
            let mut statement = transaction
                .prepare(
                    "SELECT p.backup_id, b.object_key
                     FROM storage_purges p
                     JOIN backups b ON b.id = p.backup_id
                     WHERE p.completed_at IS NULL
                     ORDER BY p.started_at ASC, p.backup_id ASC
                     LIMIT ?1",
                )
                .map_err(AppError::internal)?;
            let rows = statement
                .query_map(params![policy.cleanup_limit], |row| {
                    Ok(StoragePurgeCandidate {
                        backup_id: row.get(0)?,
                        object_key: row.get(1)?,
                    })
                })
                .map_err(AppError::internal)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(AppError::internal)?
        };
        transaction.commit().map_err(AppError::internal)?;
        Ok(candidates)
    }

    pub fn finish_storage_purge(&self, backup_id: &str) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AppError::internal)?;
        let status = transaction
            .query_row(
                "SELECT b.status
                 FROM storage_purges p
                 JOIN backups b ON b.id = p.backup_id
                 WHERE p.backup_id = ?1 AND p.completed_at IS NULL",
                params![backup_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(AppError::internal)?
            .ok_or(AppError::NotFound)?;
        if status == "pending" {
            transaction
                .execute(
                    "DELETE FROM storage_purges
                     WHERE backup_id = ?1 AND completed_at IS NULL",
                    params![backup_id],
                )
                .map_err(AppError::internal)?;
            let changed = transaction
                .execute(
                    "DELETE FROM backups WHERE id = ?1 AND status = 'pending'",
                    params![backup_id],
                )
                .map_err(AppError::internal)?;
            if changed != 1 {
                return Err(AppError::Conflict(
                    "stale pending backup changed during storage cleanup",
                ));
            }
        } else if status == "completed" {
            transaction
                .execute(
                    "UPDATE storage_purges
                 SET completed_at = COALESCE(completed_at, ?2)
                 WHERE backup_id = ?1",
                    params![backup_id, now_epoch()],
                )
                .map_err(AppError::internal)?;
        } else {
            return Err(AppError::Conflict("storage purge status was invalid"));
        }
        transaction.commit().map_err(AppError::internal)?;
        Ok(())
    }

    fn new_device_credential(&self, name: &str, now: i64) -> Result<DeviceCredential> {
        Ok(DeviceCredential {
            device_id: Uuid::new_v4().to_string(),
            device_name: validate_name(name)?,
            device_token: generate_secret("pdt_", 32)?,
            created_at: now,
        })
    }

    fn insert_device(
        &self,
        credential: &DeviceCredential,
        rate_capacity: u32,
        now: i64,
    ) -> Result<()> {
        let token_hash = hash_secret(&self.pepper, b"device-token", &credential.device_token);
        self.connect()?
            .execute(
                "INSERT INTO devices(
                    id, name, token_hash, created_at,
                    upload_tokens, upload_tokens_updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?4)",
                params![
                    credential.device_id,
                    credential.device_name,
                    token_hash.as_slice(),
                    now,
                    f64::from(rate_capacity)
                ],
            )
            .map_err(AppError::internal)?;
        Ok(())
    }

    fn connect(&self) -> Result<Connection> {
        let connection = Connection::open(self.path.as_ref()).map_err(AppError::internal)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA busy_timeout = 5000;",
            )
            .map_err(AppError::internal)?;
        Ok(connection)
    }

    fn connect_read_only(&self) -> Result<Connection> {
        let connection = Connection::open_with_flags(
            self.path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(AppError::internal)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA query_only = ON;
                 PRAGMA busy_timeout = 1000;",
            )
            .map_err(AppError::internal)?;
        Ok(connection)
    }
}

fn map_backup(row: &rusqlite::Row<'_>) -> rusqlite::Result<BackupRecord> {
    let size_bytes = row.get::<_, i64>(3)?;
    let verified_size_bytes = row
        .get::<_, Option<i64>>(12)?
        .map(|value| {
            u64::try_from(value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    12,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    Ok(BackupRecord {
        id: row.get(0)?,
        device_id: row.get(1)?,
        object_key: row.get(2)?,
        size_bytes: u64::try_from(size_bytes).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        sha256: row.get(4)?,
        retention_class: RetentionClass::from_database(&row.get::<_, String>(5)?)?,
        status: row.get(6)?,
        created_at: row.get(7)?,
        upload_expires_at: row.get(8)?,
        completed_at: row.get(9)?,
        etag: row.get(10)?,
        version_id: row.get(11)?,
        verified_size_bytes,
        verified_sha256: row.get(13)?,
        verified_at: row.get(14)?,
    })
}

fn has_recovery_authority(backup: &BackupRecord) -> bool {
    backup.status == "completed"
        && backup.version_id.as_deref().is_some_and(|version_id| {
            !version_id.is_empty()
                && version_id.len() <= 256
                && !version_id.chars().any(char::is_control)
        })
        && backup.verified_size_bytes == Some(backup.size_bytes)
        && backup.verified_sha256.as_deref() == Some(backup.sha256.as_str())
        && backup.verified_at.is_some()
}

fn migration_checksum(sql: &str) -> String {
    // Git checkouts may materialize text files with CRLF on Windows while the
    // production AlmaLinux build uses LF. Line endings are not SQL semantics;
    // canonicalizing them keeps immutable migration evidence portable.
    let canonical = sql.replace("\r\n", "\n").replace('\r', "\n");
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

pub fn now_epoch() -> i64 {
    Utc::now().timestamp()
}

fn validate_rate_capacity(rate_capacity: u32) -> Result<()> {
    if !(1..=60).contains(&rate_capacity) {
        return Err(AppError::BadRequest(
            "rate capacity must be between 1 and 60",
        ));
    }
    Ok(())
}

fn validate_admission_policy(policy: AdmissionPolicy) -> Result<()> {
    validate_rate_capacity(policy.rate_capacity)?;
    if policy.rate_refill_seconds == 0 || policy.rate_refill_seconds > 3600 {
        return Err(AppError::BadRequest(
            "rate refill must be between 1 and 3600 seconds",
        ));
    }
    if policy.max_pending_per_device == 0
        || policy.pending_cleanup_limit == 0
        || policy.authorization_cleanup_limit == 0
        || policy.byte_quota_24h == 0
        || policy.pending_max_age_seconds == 0
        || policy.daily_min_interval_seconds < 20 * 60 * 60
        || policy.monthly_min_interval_seconds < 25 * 24 * 60 * 60
    {
        return Err(AppError::BadRequest("admission policy is invalid"));
    }
    Ok(())
}

fn validate_retention_policy(policy: RetentionPolicy) -> Result<()> {
    if policy.rolling_seconds == 0
        || policy.daily_seconds < policy.rolling_seconds
        || policy.monthly_seconds < policy.daily_seconds
        || policy.pending_max_age_seconds == 0
        || policy.pending_cleanup_limit == 0
        || policy.pending_cleanup_limit > 1000
        || policy.cleanup_limit == 0
        || policy.cleanup_limit > 1000
    {
        return Err(AppError::BadRequest("retention policy is invalid"));
    }
    Ok(())
}

fn retention_cutoff(now: i64, retention_seconds: u64) -> Result<i64> {
    let retention_seconds = i64::try_from(retention_seconds).map_err(AppError::internal)?;
    Ok(now.saturating_sub(retention_seconds))
}

fn validate_completion_evidence(
    version_id: &str,
    verified_size_bytes: u64,
    verified_sha256: &str,
) -> Result<()> {
    let version_is_valid = !version_id.is_empty()
        && version_id.len() <= 256
        && !version_id.chars().any(char::is_control);
    let hash_is_valid = verified_sha256.len() == 64
        && verified_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !version_is_valid || verified_size_bytes == 0 || !hash_is_valid {
        return Err(AppError::Upstream(
            "storage completion evidence was invalid".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn database() -> (TempDir, Database) {
        let directory = TempDir::new().unwrap();
        let database = Database::new(
            directory.path().join("gateway.sqlite3"),
            Arc::from(b"test-only-pepper-at-least-32-bytes".as_slice()),
        );
        database.migrate().unwrap();
        (directory, database)
    }

    fn admission_policy() -> AdmissionPolicy {
        AdmissionPolicy {
            rate_capacity: 5,
            rate_refill_seconds: 60,
            max_pending_per_device: 8,
            byte_quota_24h: 1024,
            pending_max_age_seconds: 30 * 24 * 60 * 60,
            pending_cleanup_limit: 100,
            authorization_cleanup_limit: 500,
            daily_min_interval_seconds: 20 * 60 * 60,
            monthly_min_interval_seconds: 25 * 24 * 60 * 60,
        }
    }

    #[test]
    fn migration_is_idempotent() {
        let (_directory, database) = database();
        database.migrate().unwrap();
        database.health_check().unwrap();
        let connection = database.connect().unwrap();
        let mut statement = connection
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap();
        let versions = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(versions, vec![1, 2, 3, 4]);
    }

    #[test]
    fn migration_upgrades_an_existing_v1_database_and_remains_idempotent() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("gateway.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (
                    version INTEGER PRIMARY KEY NOT NULL,
                    name TEXT NOT NULL,
                    checksum TEXT NOT NULL,
                    applied_at INTEGER NOT NULL
                ) STRICT;",
            )
            .unwrap();
        connection.execute_batch(INITIAL_MIGRATION).unwrap();
        connection
            .execute(
                "INSERT INTO schema_migrations(version, name, checksum, applied_at)
                 VALUES (1, 'initial', ?1, ?2)",
                params![migration_checksum(INITIAL_MIGRATION), now_epoch()],
            )
            .unwrap();
        drop(connection);

        let database = Database::new(
            path,
            Arc::from(b"test-only-pepper-at-least-32-bytes".as_slice()),
        );
        database.migrate().unwrap();
        database.migrate().unwrap();
        let connection = database.connect().unwrap();
        let relay_column_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('backups')
                 WHERE name = 'relay_attempted_at'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let migration_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(relay_column_count, 1);
        let retention_column_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('backups')
                 WHERE name = 'retention_class'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let verification_column_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('backups')
                 WHERE name IN ('verified_size_bytes', 'verified_sha256', 'verified_at')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let authorization_table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'upload_authorizations'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let purge_table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'storage_purges'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migration_count, 4);
        assert_eq!(retention_column_count, 1);
        assert_eq!(verification_column_count, 3);
        assert_eq!(authorization_table_count, 1);
        assert_eq!(purge_table_count, 1);
    }

    #[test]
    fn changed_applied_migration_checksum_is_rejected() {
        let (_directory, database) = database();
        database
            .connect()
            .unwrap()
            .execute(
                "UPDATE schema_migrations SET checksum = 'tampered' WHERE version = 2",
                [],
            )
            .unwrap();
        let error = database.migrate().unwrap_err();
        assert!(matches!(error, AppError::Internal(message) if message.contains("migration 2")));
    }

    #[test]
    fn migration_checksum_is_stable_across_checkout_line_endings() {
        assert_eq!(
            migration_checksum("CREATE TABLE t(x);\nSELECT 1;\n"),
            migration_checksum("CREATE TABLE t(x);\r\nSELECT 1;\r\n")
        );
    }

    #[test]
    fn enrollment_is_one_time_and_tokens_can_be_revoked() {
        let (_directory, database) = database();
        let enrollment = database.issue_enrollment("test install", 60).unwrap();
        let device = database
            .enroll_device(&enrollment.enrollment_code, "front desk", 5)
            .unwrap();
        assert!(database
            .enroll_device(&enrollment.enrollment_code, "other", 5)
            .is_err());
        assert_eq!(
            database.authenticate(&device.device_token).unwrap().id,
            device.device_id
        );
        database.revoke_device(&device.device_id).unwrap();
        assert!(database.authenticate(&device.device_token).is_err());
    }

    #[test]
    fn revoked_and_expired_enrollment_codes_are_rejected() {
        let (_directory, database) = database();
        let revoked = database.issue_enrollment("revoked", 60).unwrap();
        database.revoke_enrollment(&revoked.enrollment_id).unwrap();
        assert!(database
            .enroll_device(&revoked.enrollment_code, "device", 5)
            .is_err());

        let expired = database.issue_enrollment("expired", 60).unwrap();
        database
            .connect()
            .unwrap()
            .execute(
                "UPDATE enrollment_codes
                 SET created_at = ?2, expires_at = ?3 WHERE id = ?1",
                params![expired.enrollment_id, now_epoch() - 2, now_epoch() - 1],
            )
            .unwrap();
        assert!(database
            .enroll_device(&expired.enrollment_code, "device", 5)
            .is_err());
    }

    #[test]
    fn upload_rate_limit_is_persisted() {
        let (_directory, database) = database();
        let device = database.issue_device("test", 1).unwrap();
        database
            .reserve_or_reuse_backup(
                &device.device_id,
                &Uuid::new_v4().to_string(),
                "backups/test/one.age",
                10,
                &"ab".repeat(32),
                RetentionClass::Rolling,
                now_epoch() + 900,
                AdmissionPolicy {
                    rate_capacity: 1,
                    ..admission_policy()
                },
            )
            .unwrap();
        let error = database
            .reserve_or_reuse_backup(
                &device.device_id,
                &Uuid::new_v4().to_string(),
                "backups/test/two.age",
                10,
                &"cd".repeat(32),
                RetentionClass::Rolling,
                now_epoch() + 900,
                AdmissionPolicy {
                    rate_capacity: 1,
                    ..admission_policy()
                },
            )
            .unwrap_err();
        assert!(matches!(error, AppError::RateLimited { .. }));
    }

    #[test]
    fn identical_pending_daily_and_monthly_reservations_reuse_rows_but_charge_authorization() {
        let (_directory, database) = database();
        for retention_class in [RetentionClass::Daily, RetentionClass::Monthly] {
            let device = database.issue_device(retention_class.as_str(), 5).unwrap();
            let first_id = Uuid::new_v4().to_string();
            let first = database
                .reserve_or_reuse_backup(
                    &device.device_id,
                    &first_id,
                    &format!(
                        "backups/{}/{}/first.age",
                        retention_class.as_str(),
                        device.device_id
                    ),
                    100,
                    &"aa".repeat(32),
                    retention_class,
                    now_epoch() + 900,
                    admission_policy(),
                )
                .unwrap();
            let renewed_expiry = now_epoch() + 1800;
            let reused = database
                .reserve_or_reuse_backup(
                    &device.device_id,
                    &Uuid::new_v4().to_string(),
                    &format!(
                        "backups/{}/{}/replacement.age",
                        retention_class.as_str(),
                        device.device_id
                    ),
                    100,
                    &"aa".repeat(32),
                    retention_class,
                    renewed_expiry,
                    admission_policy(),
                )
                .unwrap();
            assert_eq!(reused.id, first.id);
            assert_eq!(reused.object_key, first.object_key);
            assert_eq!(reused.upload_expires_at, renewed_expiry);

            let tokens: f64 = database
                .connect()
                .unwrap()
                .query_row(
                    "SELECT upload_tokens FROM devices WHERE id = ?1",
                    params![device.device_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert!((3.0..4.0).contains(&tokens));

            let error = database
                .reserve_or_reuse_backup(
                    &device.device_id,
                    &Uuid::new_v4().to_string(),
                    &format!(
                        "backups/{}/{}/different.age",
                        retention_class.as_str(),
                        device.device_id
                    ),
                    100,
                    &"bb".repeat(32),
                    retention_class,
                    now_epoch() + 900,
                    admission_policy(),
                )
                .unwrap_err();
            assert!(matches!(error, AppError::RateLimited { .. }));

            let interval = match retention_class {
                RetentionClass::Daily => admission_policy().daily_min_interval_seconds,
                RetentionClass::Monthly => admission_policy().monthly_min_interval_seconds,
                RetentionClass::Rolling => unreachable!(),
            };
            database
                .connect()
                .unwrap()
                .execute(
                    "UPDATE backups SET created_at = ?2 WHERE id = ?1",
                    params![first.id, now_epoch() - i64::try_from(interval).unwrap() - 1],
                )
                .unwrap();
            database
                .reserve_or_reuse_backup(
                    &device.device_id,
                    &Uuid::new_v4().to_string(),
                    &format!(
                        "backups/{}/{}/after-interval.age",
                        retention_class.as_str(),
                        device.device_id
                    ),
                    100,
                    &"cc".repeat(32),
                    retention_class,
                    now_epoch() + 900,
                    admission_policy(),
                )
                .unwrap();
        }
    }

    #[test]
    fn reservation_refresh_charges_persisted_quota_across_database_reopen() {
        let (directory, database) = database();
        let device = database.issue_device("quota", 5).unwrap();
        let policy = AdmissionPolicy {
            byte_quota_24h: 1200,
            ..admission_policy()
        };
        let first = database
            .reserve_or_reuse_backup(
                &device.device_id,
                &Uuid::new_v4().to_string(),
                "backups/rolling/quota/one.age",
                600,
                &"11".repeat(32),
                RetentionClass::Rolling,
                now_epoch() + 900,
                policy,
            )
            .unwrap();

        let reopened = Database::new(
            directory.path().join("gateway.sqlite3"),
            Arc::from(b"test-only-pepper-at-least-32-bytes".as_slice()),
        );
        let resigned = reopened
            .reserve_or_reuse_backup(
                &device.device_id,
                &Uuid::new_v4().to_string(),
                "backups/rolling/quota/two.age",
                600,
                &"11".repeat(32),
                RetentionClass::Rolling,
                now_epoch() + 900,
                policy,
            )
            .unwrap();
        assert_eq!(resigned.id, first.id);
        let (authorizations, bytes): (u64, u64) = reopened
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*), SUM(size_bytes) FROM upload_authorizations
                 WHERE device_id = ?1",
                params![device.device_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((authorizations, bytes), (2, 1200));

        let error = reopened
            .reserve_or_reuse_backup(
                &device.device_id,
                &Uuid::new_v4().to_string(),
                "backups/rolling/quota/three.age",
                600,
                &"11".repeat(32),
                RetentionClass::Rolling,
                now_epoch() + 900,
                policy,
            )
            .unwrap_err();
        assert!(matches!(error, AppError::RateLimited { .. }));
    }

    #[test]
    fn pending_cleanup_claim_is_bounded_and_frees_one_slot_after_unlink() {
        let (_directory, database) = database();
        let device = database.issue_device("cleanup", 5).unwrap();
        let policy = AdmissionPolicy {
            max_pending_per_device: 2,
            pending_max_age_seconds: 24 * 60 * 60,
            pending_cleanup_limit: 1,
            authorization_cleanup_limit: 1,
            ..admission_policy()
        };
        for index in 0..2 {
            database
                .reserve_or_reuse_backup(
                    &device.device_id,
                    &Uuid::new_v4().to_string(),
                    &format!("backups/rolling/cleanup/{index}.age"),
                    10,
                    &format!("{index:02}").repeat(32),
                    RetentionClass::Rolling,
                    now_epoch() + 900,
                    policy,
                )
                .unwrap();
        }
        database
            .connect()
            .unwrap()
            .execute(
                "UPDATE backups SET created_at = ?2, upload_expires_at = ?3
                 WHERE device_id = ?1 AND status = 'pending'",
                params![
                    device.device_id,
                    now_epoch() - 3 * 24 * 60 * 60,
                    now_epoch() - 2 * 24 * 60 * 60
                ],
            )
            .unwrap();
        database
            .connect()
            .unwrap()
            .execute(
                "UPDATE upload_authorizations SET authorized_at = ?1",
                params![now_epoch() - 2 * 24 * 60 * 60],
            )
            .unwrap();

        let claimed = database
            .claim_storage_purges(RetentionPolicy {
                rolling_seconds: 14 * DAY_SECONDS as u64,
                daily_seconds: 60 * DAY_SECONDS as u64,
                monthly_seconds: 400 * DAY_SECONDS as u64,
                pending_max_age_seconds: DAY_SECONDS as u64,
                pending_cleanup_limit: 1,
                cleanup_limit: 1,
            })
            .unwrap();
        assert_eq!(claimed.len(), 1);
        database
            .finish_storage_purge(&claimed[0].backup_id)
            .unwrap();

        database
            .reserve_or_reuse_backup(
                &device.device_id,
                &Uuid::new_v4().to_string(),
                "backups/rolling/cleanup/new.age",
                10,
                &"ff".repeat(32),
                RetentionClass::Rolling,
                now_epoch() + 900,
                policy,
            )
            .unwrap();
        let pending: u64 = database
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM backups
                 WHERE device_id = ?1 AND status = 'pending'",
                params![device.device_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 2);
        let stale_authorizations: u64 = database
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM upload_authorizations
                 WHERE authorized_at <= ?1",
                params![now_epoch() - DAY_SECONDS],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_authorizations, 1);
    }

    #[test]
    fn completed_admin_records_include_body_verification_evidence() {
        let (_directory, database) = database();
        let device = database.issue_device("admin", 5).unwrap();
        let backup_id = Uuid::new_v4().to_string();
        database
            .reserve_or_reuse_backup(
                &device.device_id,
                &backup_id,
                "backups/rolling/admin/one.age",
                10,
                &"ab".repeat(32),
                RetentionClass::Rolling,
                now_epoch() + 900,
                admission_policy(),
            )
            .unwrap();
        database
            .mark_backup_completed(
                &device.device_id,
                &backup_id,
                Some("etag"),
                "version-1",
                10,
                &"ab".repeat(32),
            )
            .unwrap();
        let listed = database.list_completed_backups(10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].version_id.as_deref(), Some("version-1"));
        assert_eq!(listed[0].verified_size_bytes, Some(10));
        let expected_sha256 = "ab".repeat(32);
        assert_eq!(
            listed[0].verified_sha256.as_deref(),
            Some(expected_sha256.as_str())
        );
        assert!(listed[0].verified_at.is_some());
        assert_eq!(database.completed_backup(&backup_id).unwrap().id, backup_id);
    }

    #[test]
    fn completion_evidence_must_match_reservation_and_is_idempotent_only_when_exact() {
        let (_directory, database) = database();
        let device = database.issue_device("completion guard", 5).unwrap();
        let backup_id = Uuid::new_v4().to_string();
        let expected_sha256 = "ab".repeat(32);
        database
            .reserve_or_reuse_backup(
                &device.device_id,
                &backup_id,
                "backups/rolling/completion/guard.age",
                10,
                &expected_sha256,
                RetentionClass::Rolling,
                now_epoch() + 900,
                admission_policy(),
            )
            .unwrap();

        for error in [
            database
                .mark_backup_completed(
                    &device.device_id,
                    &backup_id,
                    None,
                    "version-1",
                    11,
                    &expected_sha256,
                )
                .unwrap_err(),
            database
                .mark_backup_completed(
                    &device.device_id,
                    &backup_id,
                    None,
                    "version-1",
                    10,
                    &"cd".repeat(32),
                )
                .unwrap_err(),
            database
                .mark_backup_completed(
                    &device.device_id,
                    &backup_id,
                    None,
                    "",
                    10,
                    &expected_sha256,
                )
                .unwrap_err(),
            database
                .mark_backup_completed(
                    &device.device_id,
                    &backup_id,
                    None,
                    "version-1",
                    10,
                    &"AB".repeat(32),
                )
                .unwrap_err(),
        ] {
            assert!(matches!(error, AppError::Upstream(_)));
            assert_eq!(
                database
                    .backup_for_device(&device.device_id, &backup_id)
                    .unwrap()
                    .status,
                "pending"
            );
        }

        let completed = database
            .mark_backup_completed(
                &device.device_id,
                &backup_id,
                Some("etag-1"),
                "version-1",
                10,
                &expected_sha256,
            )
            .unwrap();
        let repeated = database
            .mark_backup_completed(
                &device.device_id,
                &backup_id,
                Some("etag-1"),
                "version-1",
                10,
                &expected_sha256,
            )
            .unwrap();
        assert_eq!(repeated.completed_at, completed.completed_at);
        assert!(matches!(
            database
                .mark_backup_completed(
                    &device.device_id,
                    &backup_id,
                    Some("etag-2"),
                    "version-2",
                    10,
                    &expected_sha256,
                )
                .unwrap_err(),
            AppError::Conflict(_)
        ));
    }

    #[test]
    fn legacy_metadata_only_completion_is_excluded_from_verified_recovery_views() {
        let (_directory, database) = database();
        let device = database.issue_device("legacy completion", 5).unwrap();
        let backup_id = Uuid::new_v4().to_string();
        database
            .reserve_or_reuse_backup(
                &device.device_id,
                &backup_id,
                "backups/rolling/legacy/completion.age",
                10,
                &"ab".repeat(32),
                RetentionClass::Rolling,
                now_epoch() + 900,
                admission_policy(),
            )
            .unwrap();
        database
            .connect()
            .unwrap()
            .execute(
                "UPDATE backups
                 SET status = 'completed', completed_at = ?2, version_id = 'legacy-version'
                 WHERE id = ?1",
                params![backup_id, now_epoch()],
            )
            .unwrap();

        assert!(database
            .backup_summary(&device.device_id)
            .unwrap()
            .latest_completed
            .is_none());
        assert!(matches!(
            database.completed_backup(&backup_id).unwrap_err(),
            AppError::NotFound
        ));
        assert!(database.list_completed_backups(10).unwrap().is_empty());
    }

    fn insert_completed_for_retention(
        database: &Database,
        device_id: &str,
        retention: RetentionClass,
        completed_at: i64,
        suffix: &str,
    ) -> (String, String) {
        let backup_id = Uuid::new_v4().to_string();
        let object_key = format!(
            "backups/{}/{device_id}/{suffix}.sqlite3.age",
            retention.as_str()
        );
        let sha256 = hex::encode(Sha256::digest(suffix.as_bytes()));
        database
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO backups(
                    id, device_id, object_key, size_bytes, sha256,
                    status, created_at, upload_expires_at, completed_at,
                    version_id, retention_class, verified_size_bytes,
                    verified_sha256, verified_at
                 ) VALUES (?1, ?2, ?3, 10, ?4, 'completed', ?5, ?6, ?7,
                           ?8, ?9, 10, ?4, ?7)",
                params![
                    backup_id,
                    device_id,
                    object_key,
                    sha256,
                    completed_at - 10,
                    completed_at + 890,
                    completed_at,
                    format!("fs-sha256-{sha256}"),
                    retention.as_str()
                ],
            )
            .unwrap();
        (backup_id, object_key)
    }

    #[test]
    fn retention_is_bounded_resumable_and_excludes_claimed_recovery_rows() {
        let (_directory, database) = database();
        let device = database.issue_device("retention", 5).unwrap();
        let now = now_epoch();
        let (oldest_id, oldest_key) = insert_completed_for_retention(
            &database,
            &device.device_id,
            RetentionClass::Rolling,
            now - 30 * DAY_SECONDS,
            "oldest",
        );
        let (older_id, _) = insert_completed_for_retention(
            &database,
            &device.device_id,
            RetentionClass::Rolling,
            now - 20 * DAY_SECONDS,
            "older",
        );
        let (newest_id, _) = insert_completed_for_retention(
            &database,
            &device.device_id,
            RetentionClass::Rolling,
            now - 15 * DAY_SECONDS,
            "newest",
        );
        let policy = RetentionPolicy {
            rolling_seconds: 14 * DAY_SECONDS as u64,
            daily_seconds: 60 * DAY_SECONDS as u64,
            monthly_seconds: 400 * DAY_SECONDS as u64,
            pending_max_age_seconds: 30 * DAY_SECONDS as u64,
            pending_cleanup_limit: 100,
            cleanup_limit: 1,
        };

        let first = database.claim_storage_purges(policy).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].backup_id, oldest_id);
        assert_eq!(first[0].object_key, oldest_key);
        assert!(matches!(
            database.completed_backup(&oldest_id),
            Err(AppError::NotFound)
        ));
        assert!(matches!(
            database.backup_for_device(&device.device_id, &oldest_id),
            Err(AppError::NotFound)
        ));

        let interrupted_retry = database.claim_storage_purges(policy).unwrap();
        assert_eq!(interrupted_retry[0].backup_id, oldest_id);
        database.finish_storage_purge(&oldest_id).unwrap();
        let second = database.claim_storage_purges(policy).unwrap();
        assert_eq!(second[0].backup_id, older_id);
        database.finish_storage_purge(&older_id).unwrap();

        assert!(database.claim_storage_purges(policy).unwrap().is_empty());
        assert_eq!(database.completed_backup(&newest_id).unwrap().id, newest_id);
    }

    #[test]
    fn retention_uses_independent_class_boundaries() {
        let (_directory, database) = database();
        let device = database.issue_device("class retention", 5).unwrap();
        let now = now_epoch();
        for (retention, expired_days, current_days) in [
            (RetentionClass::Rolling, 15, 13),
            (RetentionClass::Daily, 61, 59),
            (RetentionClass::Monthly, 401, 399),
        ] {
            insert_completed_for_retention(
                &database,
                &device.device_id,
                retention,
                now - expired_days * DAY_SECONDS,
                &format!("{}-expired", retention.as_str()),
            );
            insert_completed_for_retention(
                &database,
                &device.device_id,
                retention,
                now - current_days * DAY_SECONDS,
                &format!("{}-current", retention.as_str()),
            );
        }
        let candidates = database
            .claim_storage_purges(RetentionPolicy {
                rolling_seconds: 14 * DAY_SECONDS as u64,
                daily_seconds: 60 * DAY_SECONDS as u64,
                monthly_seconds: 400 * DAY_SECONDS as u64,
                pending_max_age_seconds: 30 * DAY_SECONDS as u64,
                pending_cleanup_limit: 100,
                cleanup_limit: 10,
            })
            .unwrap();
        assert_eq!(candidates.len(), 3);
        assert!(candidates
            .iter()
            .all(|candidate| candidate.object_key.contains("-expired")));
    }

    #[tokio::test]
    async fn interrupted_retention_claim_is_resumed_through_object_unlink() {
        use crate::storage::{prune_retention, LocalObjectStore};

        let (directory, database) = database();
        let storage = LocalObjectStore::new(directory.path().join("objects"), 0).unwrap();
        let device = database.issue_device("retention unlink", 5).unwrap();
        let now = now_epoch();
        let mut records = Vec::new();
        for (index, completed_at) in [now - 30 * DAY_SECONDS, now - 20 * DAY_SECONDS]
            .into_iter()
            .enumerate()
        {
            let body = format!("ciphertext-{index}").into_bytes();
            let hash = hex::encode(Sha256::digest(&body));
            let backup_id = Uuid::new_v4().to_string();
            let object_key = format!(
                "backups/rolling/{}/2026/07/15/{backup_id}.sqlite3.age",
                device.device_id
            );
            database
                .reserve_or_reuse_backup(
                    &device.device_id,
                    &backup_id,
                    &object_key,
                    body.len() as u64,
                    &hash,
                    RetentionClass::Rolling,
                    now + 900,
                    AdmissionPolicy {
                        byte_quota_24h: 1024 * 1024,
                        ..admission_policy()
                    },
                )
                .unwrap();
            let spool = directory.path().join(format!("spool-{index}.age"));
            tokio::fs::write(&spool, &body).await.unwrap();
            let verified = storage
                .store_verified_spool(&object_key, body.len() as u64, &hash, &spool)
                .await
                .unwrap();
            database
                .mark_backup_completed(
                    &device.device_id,
                    &backup_id,
                    None,
                    &verified.version_id,
                    verified.size_bytes,
                    &verified.sha256,
                )
                .unwrap();
            database
                .connect()
                .unwrap()
                .execute(
                    "UPDATE backups
                     SET created_at = ?2, upload_expires_at = ?3,
                         completed_at = ?4, verified_at = ?4
                     WHERE id = ?1",
                    params![
                        backup_id,
                        completed_at - 10,
                        completed_at + 890,
                        completed_at
                    ],
                )
                .unwrap();
            records.push((backup_id, object_key));
        }
        let policy = RetentionPolicy {
            rolling_seconds: 14 * DAY_SECONDS as u64,
            daily_seconds: 60 * DAY_SECONDS as u64,
            monthly_seconds: 400 * DAY_SECONDS as u64,
            pending_max_age_seconds: 30 * DAY_SECONDS as u64,
            pending_cleanup_limit: 100,
            cleanup_limit: 10,
        };

        let claimed = database.claim_storage_purges(policy).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].backup_id, records[0].0);
        assert!(storage.root().join(&records[0].1).exists());

        assert_eq!(
            prune_retention(database.clone(), storage.clone(), policy)
                .await
                .unwrap(),
            1
        );
        assert!(!storage.root().join(&records[0].1).exists());
        assert!(storage.root().join(&records[1].1).exists());
        let completed: i64 = database
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM storage_purges
                 WHERE backup_id = ?1 AND completed_at IS NOT NULL",
                params![records[0].0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(completed, 1);
    }

    #[tokio::test]
    async fn stale_pending_cleanup_unlinks_before_deleting_its_database_row() {
        use crate::storage::{prune_retention, LocalObjectStore};

        let (directory, database) = database();
        let storage = LocalObjectStore::new(directory.path().join("objects"), 0).unwrap();
        let device = database.issue_device("stale pending", 5).unwrap();
        let body = b"published-before-completion";
        let hash = hex::encode(Sha256::digest(body));
        let backup_id = Uuid::new_v4().to_string();
        let object_key = format!(
            "backups/rolling/{}/2026/07/15/{backup_id}.sqlite3.age",
            device.device_id
        );
        database
            .reserve_or_reuse_backup(
                &device.device_id,
                &backup_id,
                &object_key,
                body.len() as u64,
                &hash,
                RetentionClass::Rolling,
                now_epoch() + 900,
                admission_policy(),
            )
            .unwrap();
        database
            .connect()
            .unwrap()
            .execute(
                "UPDATE backups
                 SET created_at = ?2, upload_expires_at = ?3
                 WHERE id = ?1",
                params![
                    backup_id,
                    now_epoch() - 3 * DAY_SECONDS,
                    now_epoch() - 2 * DAY_SECONDS
                ],
            )
            .unwrap();
        let spool = directory.path().join("stale-pending-spool.age");
        tokio::fs::write(&spool, body).await.unwrap();
        storage
            .store_verified_spool(&object_key, body.len() as u64, &hash, &spool)
            .await
            .unwrap();
        assert!(storage.root().join(&object_key).exists());

        let removed = prune_retention(
            database.clone(),
            storage.clone(),
            RetentionPolicy {
                rolling_seconds: 14 * DAY_SECONDS as u64,
                daily_seconds: 60 * DAY_SECONDS as u64,
                monthly_seconds: 400 * DAY_SECONDS as u64,
                pending_max_age_seconds: DAY_SECONDS as u64,
                pending_cleanup_limit: 10,
                cleanup_limit: 10,
            },
        )
        .await
        .unwrap();
        assert_eq!(removed, 1);
        assert!(!storage.root().join(&object_key).exists());
        assert!(matches!(
            database.backup_for_device(&device.device_id, &backup_id),
            Err(AppError::NotFound)
        ));
        let purge_rows: u64 = database
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM storage_purges WHERE backup_id = ?1",
                params![backup_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(purge_rows, 0);
    }
}
