use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    crypto::{generate_secret, hash_secret, validate_name},
    error::{AppError, Result},
};

const INITIAL_MIGRATION: &str = include_str!("../migrations/0001_initial.sql");

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

#[derive(Debug, Clone)]
pub struct BackupRecord {
    pub id: String,
    pub object_key: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub status: String,
    pub created_at: i64,
    pub upload_expires_at: i64,
    pub completed_at: Option<i64>,
    pub etag: Option<String>,
    pub version_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BackupSummary {
    pub latest_completed: Option<BackupRecord>,
    pub active_pending: u64,
    pub expired_pending: u64,
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

        let checksum = hex::encode(Sha256::digest(INITIAL_MIGRATION.as_bytes()));
        let existing = transaction
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(AppError::internal)?;
        match existing {
            Some(existing) if existing != checksum => {
                return Err(AppError::Internal(
                    "migration 1 checksum does not match the applied schema".to_owned(),
                ));
            }
            Some(_) => {}
            None => {
                transaction
                    .execute_batch(INITIAL_MIGRATION)
                    .map_err(AppError::internal)?;
                transaction
                    .execute(
                        "INSERT INTO schema_migrations(version, name, checksum, applied_at)
                         VALUES (1, 'initial', ?1, ?2)",
                        params![checksum, now_epoch()],
                    )
                    .map_err(AppError::internal)?;
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
        let now = now_epoch();
        let code_hash = hash_secret(&self.pepper, b"enrollment-code", enrollment_code);
        let credential = self.new_device_credential(device_name, now)?;

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
    pub fn reserve_backup(
        &self,
        device_id: &str,
        backup_id: &str,
        object_key: &str,
        size_bytes: u64,
        sha256: &str,
        upload_expires_at: i64,
        rate_capacity: u32,
        refill_seconds: u64,
    ) -> Result<BackupRecord> {
        validate_rate_capacity(rate_capacity)?;
        if refill_seconds == 0 || refill_seconds > 3600 {
            return Err(AppError::BadRequest(
                "rate refill must be between 1 and 3600 seconds",
            ));
        }
        let size_bytes = i64::try_from(size_bytes).map_err(AppError::internal)?;
        let now = now_epoch();
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

        let elapsed = now.saturating_sub(updated_at) as f64;
        let available =
            (stored_tokens + elapsed / refill_seconds as f64).min(f64::from(rate_capacity));
        if available < 1.0 {
            let retry_after = ((1.0 - available) * refill_seconds as f64).ceil() as u64;
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
                "INSERT INTO backups(
                    id, device_id, object_key, size_bytes, sha256,
                    status, created_at, upload_expires_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
                params![
                    backup_id,
                    device_id,
                    object_key,
                    size_bytes,
                    sha256,
                    now,
                    upload_expires_at
                ],
            )
            .map_err(AppError::internal)?;
        transaction.commit().map_err(AppError::internal)?;

        Ok(BackupRecord {
            id: backup_id.to_owned(),
            object_key: object_key.to_owned(),
            size_bytes: size_bytes as u64,
            sha256: sha256.to_owned(),
            status: "pending".to_owned(),
            created_at: now,
            upload_expires_at,
            completed_at: None,
            etag: None,
            version_id: None,
        })
    }

    pub fn backup_for_device(&self, device_id: &str, backup_id: &str) -> Result<BackupRecord> {
        self.connect()?
            .query_row(
                "SELECT id, object_key, size_bytes, sha256, status, created_at,
                        upload_expires_at, completed_at, etag, version_id
                 FROM backups WHERE id = ?1 AND device_id = ?2",
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
        version_id: Option<&str>,
    ) -> Result<BackupRecord> {
        let now = now_epoch();
        let connection = self.connect()?;
        connection
            .execute(
                "UPDATE backups
                 SET status = 'completed', completed_at = ?3, etag = ?4, version_id = ?5
                 WHERE id = ?1 AND device_id = ?2 AND status = 'pending'",
                params![backup_id, device_id, now, etag, version_id],
            )
            .map_err(AppError::internal)?;
        self.backup_for_device(device_id, backup_id)
    }

    pub fn backup_summary(&self, device_id: &str) -> Result<BackupSummary> {
        let now = now_epoch();
        let connection = self.connect()?;
        let latest_completed = connection
            .query_row(
                "SELECT id, object_key, size_bytes, sha256, status, created_at,
                        upload_expires_at, completed_at, etag, version_id
                 FROM backups
                 WHERE device_id = ?1 AND status = 'completed'
                 ORDER BY completed_at DESC LIMIT 1",
                params![device_id],
                map_backup,
            )
            .optional()
            .map_err(AppError::internal)?;
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
}

fn map_backup(row: &rusqlite::Row<'_>) -> rusqlite::Result<BackupRecord> {
    let size_bytes = row.get::<_, i64>(2)?;
    Ok(BackupRecord {
        id: row.get(0)?,
        object_key: row.get(1)?,
        size_bytes: u64::try_from(size_bytes).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        sha256: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
        upload_expires_at: row.get(6)?,
        completed_at: row.get(7)?,
        etag: row.get(8)?,
        version_id: row.get(9)?,
    })
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

    #[test]
    fn migration_is_idempotent() {
        let (_directory, database) = database();
        database.migrate().unwrap();
        database.health_check().unwrap();
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
            .reserve_backup(
                &device.device_id,
                &Uuid::new_v4().to_string(),
                "backups/test/one.age",
                10,
                &"ab".repeat(32),
                now_epoch() + 900,
                1,
                60,
            )
            .unwrap();
        let error = database
            .reserve_backup(
                &device.device_id,
                &Uuid::new_v4().to_string(),
                "backups/test/two.age",
                10,
                &"cd".repeat(32),
                now_epoch() + 900,
                1,
                60,
            )
            .unwrap_err();
        assert!(matches!(error, AppError::RateLimited { .. }));
    }
}
