#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    db::{Database, RetentionPolicy},
    error::{AppError, Result},
};

const VERSION_PREFIX: &str = "fs-sha256-";

#[derive(Clone)]
pub struct LocalObjectStore {
    root: Arc<PathBuf>,
    min_free_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifiedObject {
    pub etag: Option<String>,
    pub version_id: String,
    pub size_bytes: u64,
    pub sha256: String,
}

impl LocalObjectStore {
    pub fn new(root: impl Into<PathBuf>, min_free_bytes: u64) -> Result<Self> {
        let root = root.into();
        validate_absolute_path(&root)?;
        let root_was_created = !root.exists();
        fs::create_dir_all(&root).map_err(AppError::internal)?;
        validate_directory(&root)?;
        set_private_directory_permissions(&root)?;
        if root_was_created {
            sync_directory(&root)?;
            if let Some(parent) = root.parent() {
                sync_directory(parent)?;
            }
        }
        Ok(Self {
            root: Arc::new(root),
            min_free_bytes,
        })
    }

    pub fn open_existing(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        validate_absolute_path(&root)?;
        validate_directory(&root)?;
        Ok(Self {
            root: Arc::new(root),
            min_free_bytes: 0,
        })
    }

    pub fn root(&self) -> &Path {
        self.root.as_ref()
    }

    pub fn ensure_capacity(&self, incoming_bytes: u64) -> Result<()> {
        let required = incoming_bytes.checked_add(self.min_free_bytes).ok_or(
            AppError::ServiceUnavailable {
                retry_after_seconds: 3600,
            },
        )?;
        let available = fs2::available_space(self.root.as_ref()).map_err(AppError::internal)?;
        if available < required {
            return Err(AppError::ServiceUnavailable {
                retry_after_seconds: 3600,
            });
        }
        Ok(())
    }

    pub async fn verify_object_if_present(
        &self,
        object_key: &str,
        expected_size: u64,
        expected_sha256: &str,
    ) -> Result<Option<VerifiedObject>> {
        let path = self.object_path(object_key)?;
        verify_path_if_present(&path, expected_size, expected_sha256).await
    }

    pub async fn store_verified_spool(
        &self,
        object_key: &str,
        expected_size: u64,
        expected_sha256: &str,
        spool_path: &Path,
    ) -> Result<VerifiedObject> {
        verify_path_if_present(spool_path, expected_size, expected_sha256)
            .await?
            .ok_or_else(|| AppError::Internal("verified relay spool disappeared".to_owned()))?;
        let final_path = self.prepare_object_parent(object_key)?;

        if let Some(existing) =
            verify_path_if_present(&final_path, expected_size, expected_sha256).await?
        {
            return Ok(existing);
        }

        match tokio::fs::hard_link(spool_path, &final_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return verify_path_if_present(&final_path, expected_size, expected_sha256)
                    .await?
                    .ok_or_else(|| {
                        AppError::Upstream(
                            "immutable storage object disappeared during publication".to_owned(),
                        )
                    });
            }
            Err(error) => {
                return Err(AppError::Internal(format!(
                    "local object publication failed; relay spool and object root must share one filesystem: {error}"
                )));
            }
        }
        set_private_file_permissions(&final_path).await?;
        if let Some(parent) = final_path.parent() {
            sync_directory(parent)?;
        }
        verify_path_if_present(&final_path, expected_size, expected_sha256)
            .await?
            .ok_or_else(|| AppError::Upstream("published storage object disappeared".to_owned()))
    }

    pub async fn download_verified(
        &self,
        object_key: &str,
        version_id: &str,
        expected_size: u64,
        expected_sha256: &str,
        output_path: &Path,
    ) -> Result<VerifiedObject> {
        validate_version_id(version_id, expected_sha256)?;
        let source_path = self.object_path(object_key)?;
        validate_regular_file(&source_path)?;
        let mut source = open_read_only(&source_path).await?;
        let source_size = source.metadata().await.map_err(AppError::internal)?.len();
        if source_size != expected_size {
            return Err(size_mismatch(expected_size, source_size));
        }

        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut output = options
            .open(output_path)
            .await
            .map_err(AppError::internal)?;
        let result = async {
            let mut received = 0_u64;
            let mut hasher = Sha256::new();
            let mut buffer = vec![0_u8; 64 * 1024];
            loop {
                let count = source.read(&mut buffer).await.map_err(AppError::internal)?;
                if count == 0 {
                    break;
                }
                received = received
                    .checked_add(u64::try_from(count).map_err(AppError::internal)?)
                    .ok_or_else(|| AppError::Upstream("stored object size overflow".to_owned()))?;
                if received > expected_size {
                    return Err(AppError::Upstream(
                        "stored object exceeded the reserved size".to_owned(),
                    ));
                }
                hasher.update(&buffer[..count]);
                output
                    .write_all(&buffer[..count])
                    .await
                    .map_err(AppError::internal)?;
            }
            verify_actual(
                received,
                &hex::encode(hasher.finalize()),
                expected_size,
                expected_sha256,
            )?;
            output.flush().await.map_err(AppError::internal)?;
            output.sync_all().await.map_err(AppError::internal)?;
            Ok(verified(expected_size, expected_sha256))
        }
        .await;
        drop(output);
        if result.is_err() {
            remove_partial_output(output_path).await?;
        }
        result
    }

    pub async fn remove_object(&self, object_key: &str) -> Result<()> {
        let path = self.object_path(object_key)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(AppError::Upstream(
                    "storage object is not a regular file".to_owned(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(AppError::internal(error)),
        }
        tokio::fs::remove_file(&path)
            .await
            .map_err(AppError::internal)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(())
    }

    fn object_path(&self, object_key: &str) -> Result<PathBuf> {
        validate_object_key(object_key)?;
        Ok(self.root.join(object_key))
    }

    fn prepare_object_parent(&self, object_key: &str) -> Result<PathBuf> {
        let final_path = self.object_path(object_key)?;
        let parent = final_path
            .parent()
            .ok_or_else(|| AppError::Internal("object key has no parent".to_owned()))?;
        let relative = parent
            .strip_prefix(self.root.as_ref())
            .map_err(AppError::internal)?;
        let mut current = self.root.as_ref().clone();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                return Err(AppError::Internal(
                    "generated storage path was invalid".to_owned(),
                ));
            };
            current.push(component);
            match fs::create_dir(&current) {
                Ok(()) => {
                    set_private_directory_permissions(&current)?;
                    sync_directory(&current)?;
                    if let Some(parent) = current.parent() {
                        sync_directory(parent)?;
                    }
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    validate_directory(&current)?;
                }
                Err(error) => return Err(AppError::internal(error)),
            }
        }
        Ok(final_path)
    }
}

pub async fn prune_retention(
    database: Database,
    storage: LocalObjectStore,
    policy: RetentionPolicy,
) -> Result<u64> {
    let claim_database = database.clone();
    let candidates =
        tokio::task::spawn_blocking(move || claim_database.claim_storage_purges(policy))
            .await
            .map_err(AppError::internal)??;
    let mut completed = 0_u64;
    for candidate in candidates {
        storage.remove_object(&candidate.object_key).await?;
        let finish_database = database.clone();
        let backup_id = candidate.backup_id;
        tokio::task::spawn_blocking(move || finish_database.finish_storage_purge(&backup_id))
            .await
            .map_err(AppError::internal)??;
        completed = completed.saturating_add(1);
    }
    Ok(completed)
}

fn validate_absolute_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(AppError::BadRequest(
            "PUSULA_GATEWAY_OBJECT_ROOT must be an absolute normalized path",
        ));
    }
    Ok(())
}

fn validate_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(AppError::internal)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::Internal(
            "local object storage path is not a private directory".to_owned(),
        ));
    }
    Ok(())
}

fn validate_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            AppError::NotFound
        } else {
            AppError::internal(error)
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::Upstream(
            "storage object is not a regular file".to_owned(),
        ));
    }
    Ok(())
}

async fn verify_path_if_present(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<Option<VerifiedObject>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::internal(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::Upstream(
            "storage object is not a regular file".to_owned(),
        ));
    }
    if metadata.len() != expected_size {
        return Err(size_mismatch(expected_size, metadata.len()));
    }
    let mut file = open_read_only(path).await?;
    let mut received = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).await.map_err(AppError::internal)?;
        if count == 0 {
            break;
        }
        received = received
            .checked_add(u64::try_from(count).map_err(AppError::internal)?)
            .ok_or_else(|| AppError::Upstream("stored object size overflow".to_owned()))?;
        if received > expected_size {
            return Err(AppError::Upstream(
                "stored object exceeded the reserved size".to_owned(),
            ));
        }
        hasher.update(&buffer[..count]);
    }
    let actual_sha256 = hex::encode(hasher.finalize());
    verify_actual(received, &actual_sha256, expected_size, expected_sha256)?;
    Ok(Some(verified(received, &actual_sha256)))
}

async fn open_read_only(path: &Path) -> Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    options.open(path).await.map_err(AppError::internal)
}

fn verify_actual(
    actual_size: u64,
    actual_sha256: &str,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<()> {
    if actual_size != expected_size {
        return Err(size_mismatch(expected_size, actual_size));
    }
    if actual_sha256 != expected_sha256 {
        return Err(AppError::Upstream(
            "stored object body checksum did not match the reservation".to_owned(),
        ));
    }
    Ok(())
}

fn verified(size_bytes: u64, sha256: &str) -> VerifiedObject {
    VerifiedObject {
        etag: None,
        version_id: format!("{VERSION_PREFIX}{sha256}"),
        size_bytes,
        sha256: sha256.to_owned(),
    }
}

fn validate_version_id(version_id: &str, expected_sha256: &str) -> Result<()> {
    if version_id != format!("{VERSION_PREFIX}{expected_sha256}") {
        return Err(AppError::Conflict(
            "storage version does not match the completed backup",
        ));
    }
    Ok(())
}

fn size_mismatch(expected: u64, actual: u64) -> AppError {
    AppError::Upstream(format!(
        "stored object size mismatch (expected {expected}, got {actual})"
    ))
}

fn validate_object_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > 900
        || key.starts_with('/')
        || key.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(AppError::Internal(
            "generated storage object key was invalid".to_owned(),
        ));
    }
    Ok(())
}

async fn remove_partial_output(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::internal(error)),
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(AppError::internal)
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
async fn set_private_file_permissions(path: &Path) -> Result<()> {
    tokio::fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .await
        .map_err(AppError::internal)
}

#[cfg(not(unix))]
async fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(AppError::internal)
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn checksum(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    #[tokio::test]
    async fn immutable_store_is_idempotent_and_exactly_downloadable() {
        let directory = TempDir::new().unwrap();
        let store = LocalObjectStore::new(directory.path().join("objects"), 0).unwrap();
        let spool = directory.path().join("spool.age");
        let body = b"encrypted-ciphertext";
        tokio::fs::write(&spool, body).await.unwrap();
        let hash = checksum(body);
        let key = "backups/rolling/device/2026/07/15/backup.sqlite3.age";

        let first = store
            .store_verified_spool(key, body.len() as u64, &hash, &spool)
            .await
            .unwrap();
        let second = store
            .store_verified_spool(key, body.len() as u64, &hash, &spool)
            .await
            .unwrap();
        assert_eq!(first.version_id, format!("fs-sha256-{hash}"));
        assert_eq!(first.version_id, second.version_id);

        let output = directory.path().join("recovery.age");
        store
            .download_verified(key, &first.version_id, body.len() as u64, &hash, &output)
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(output).await.unwrap(), body);
    }

    #[tokio::test]
    async fn conflicting_immutable_object_is_rejected() {
        let directory = TempDir::new().unwrap();
        let store = LocalObjectStore::new(directory.path().join("objects"), 0).unwrap();
        let first_spool = directory.path().join("first.age");
        let second_spool = directory.path().join("second.age");
        tokio::fs::write(&first_spool, b"first").await.unwrap();
        tokio::fs::write(&second_spool, b"other").await.unwrap();
        let key = "backups/rolling/device/2026/07/15/backup.sqlite3.age";
        store
            .store_verified_spool(key, 5, &checksum(b"first"), &first_spool)
            .await
            .unwrap();
        let error = store
            .store_verified_spool(key, 5, &checksum(b"other"), &second_spool)
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::Upstream(_)));
    }

    #[test]
    fn capacity_reserve_fails_closed_on_overflow() {
        let directory = TempDir::new().unwrap();
        let store = LocalObjectStore::new(directory.path().join("objects"), u64::MAX).unwrap();
        assert!(matches!(
            store.ensure_capacity(1),
            Err(AppError::ServiceUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn failed_download_removes_partial_output() {
        let directory = TempDir::new().unwrap();
        let store = LocalObjectStore::new(directory.path().join("objects"), 0).unwrap();
        let spool = directory.path().join("spool.age");
        let body = b"original-ciphertext";
        tokio::fs::write(&spool, body).await.unwrap();
        let hash = checksum(body);
        let key = "backups/rolling/device/2026/07/15/backup.sqlite3.age";
        let stored = store
            .store_verified_spool(key, body.len() as u64, &hash, &spool)
            .await
            .unwrap();
        tokio::fs::write(store.object_path(key).unwrap(), b"tampered-ciphertext")
            .await
            .unwrap();
        let output = directory.path().join("partial.age");
        assert!(store
            .download_verified(key, &stored.version_id, body.len() as u64, &hash, &output,)
            .await
            .is_err());
        assert!(!output.exists());
    }

    #[test]
    fn object_keys_reject_traversal_and_absolute_paths() {
        for key in ["../escape.age", "/absolute.age", "backups//empty.age"] {
            assert!(validate_object_key(key).is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn object_verification_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new().unwrap();
        let store = LocalObjectStore::new(directory.path().join("objects"), 0).unwrap();
        let target = directory.path().join("outside.age");
        tokio::fs::write(&target, b"ciphertext").await.unwrap();
        let key = "backups/rolling/device/2026/07/15/backup.sqlite3.age";
        let object_path = store.prepare_object_parent(key).unwrap();
        symlink(&target, &object_path).unwrap();
        assert!(matches!(
            store
                .verify_object_if_present(key, 10, &checksum(b"ciphertext"))
                .await,
            Err(AppError::Upstream(_))
        ));
    }
}
