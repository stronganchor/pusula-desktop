mod api;
mod backup;
pub mod db;
mod error;
pub mod models;

use std::{
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use backup::{
    BackupEnrollment, BackupRunReport, BackupService, BackupStatusReport, RetentionClass,
};
pub use db::Database;
pub use models::{DatabaseStatus, ExportBundle, ExportSummary, ImportSummary};
use serde_json::Value;
use tauri::{Manager, State};
use tokio::sync::{Mutex, OwnedRwLockWriteGuard, RwLock};

struct DbState {
    database: Database,
    maintenance_gate: Arc<RwLock<()>>,
}

struct BackupState {
    service: BackupService,
}

const UPDATE_MAINTENANCE_LEASE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

struct UpdateMaintenanceLease {
    id: u64,
    _guard: OwnedRwLockWriteGuard<()>,
}

struct UpdateState {
    maintenance_lease: Arc<Mutex<Option<UpdateMaintenanceLease>>>,
    next_lease_id: AtomicU64,
}

impl UpdateState {
    fn new() -> Self {
        Self {
            maintenance_lease: Arc::new(Mutex::new(None)),
            next_lease_id: AtomicU64::new(1),
        }
    }

    async fn has_active_lease(&self) -> bool {
        self.maintenance_lease.lock().await.is_some()
    }

    async fn retain_guard_until(
        &self,
        guard: OwnedRwLockWriteGuard<()>,
        timeout: Duration,
    ) -> Result<(), String> {
        let lease_id = self.next_lease_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut current = self.maintenance_lease.lock().await;
            if current.is_some() {
                return Err("Güncelleme hazırlığı zaten etkin.".to_owned());
            }
            *current = Some(UpdateMaintenanceLease {
                id: lease_id,
                _guard: guard,
            });
        }

        // A renderer reload or crash cannot leave the database locked for the
        // rest of the process lifetime. A unique lease id prevents an older
        // watchdog from releasing a newer update attempt.
        let maintenance_lease = self.maintenance_lease.clone();
        let watchdog = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let mut current = maintenance_lease.lock().await;
            if current.as_ref().map(|lease| lease.id) == Some(lease_id) {
                current.take();
            }
        });
        drop(watchdog);
        Ok(())
    }

    async fn cancel(&self) {
        self.maintenance_lease.lock().await.take();
    }
}

async fn run_database_task<T, F>(operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| "Veritabanı işlemi beklenmedik biçimde durdu.".to_owned())?
}

async fn run_import_operation<T, Recovery, RecoveryFuture, Import, ImportFuture>(
    replace: bool,
    recovery: Recovery,
    import: Import,
) -> Result<T, String>
where
    Recovery: FnOnce() -> RecoveryFuture,
    RecoveryFuture: Future<Output = Result<BackupRunReport, String>>,
    Import: FnOnce() -> ImportFuture,
    ImportFuture: Future<Output = Result<T, String>>,
{
    if replace {
        let report = recovery().await?;
        if !report.encrypted_snapshot_created || !report.safe_to_continue {
            return Err(
                "Şifreli yerel kurtarma anlık görüntüsü doğrulanamadığı için içe aktarma engellendi."
                    .to_owned(),
            );
        }
    }
    import().await
}

#[tauri::command]
async fn api_request(
    state: State<'_, DbState>,
    path: String,
    method: Option<String>,
    body: Option<Value>,
) -> Result<Value, String> {
    let body = body.map(|value| match value {
        Value::String(serialized) => {
            serde_json::from_str(&serialized).unwrap_or(Value::String(serialized))
        }
        other => other,
    });
    let database = state.database.clone();
    let _guard = state.maintenance_gate.clone().read_owned().await;
    run_database_task(move || {
        database
            .api_request(&path, method.as_deref(), body)
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn export_data(state: State<'_, DbState>) -> Result<ExportBundle, String> {
    let database = state.database.clone();
    let _guard = state.maintenance_gate.clone().read_owned().await;
    run_database_task(move || database.export_data().map_err(|error| error.to_string())).await
}

#[tauri::command]
async fn import_data(
    database_state: State<'_, DbState>,
    backup_state: State<'_, BackupState>,
    bundle: ExportBundle,
    replace: Option<bool>,
) -> Result<ImportSummary, String> {
    let replace = replace.unwrap_or(false);
    let database = database_state.database.clone();
    let backup = backup_state.service.clone();
    let _guard = database_state.maintenance_gate.clone().write_owned().await;
    run_import_operation(
        replace,
        move || async move {
            backup
                .prepare_for_destructive_import()
                .await
                .map_err(|error| error.to_string())
        },
        move || async move {
            run_database_task(move || {
                database
                    .import_data(bundle, replace)
                    .map_err(|error| error.to_string())
            })
            .await
        },
    )
    .await
}

#[tauri::command]
async fn export_data_file(
    state: State<'_, DbState>,
    path: String,
    overwrite: Option<bool>,
) -> Result<ExportSummary, String> {
    let database = state.database.clone();
    let _guard = state.maintenance_gate.clone().read_owned().await;
    run_database_task(move || {
        database
            .export_data_file(&PathBuf::from(path), overwrite.unwrap_or(false))
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn import_data_file(
    database_state: State<'_, DbState>,
    backup_state: State<'_, BackupState>,
    path: String,
    replace: Option<bool>,
) -> Result<ImportSummary, String> {
    let replace = replace.unwrap_or(false);
    let database = database_state.database.clone();
    let backup = backup_state.service.clone();
    let _guard = database_state.maintenance_gate.clone().write_owned().await;
    run_import_operation(
        replace,
        move || async move {
            backup
                .prepare_for_destructive_import()
                .await
                .map_err(|error| error.to_string())
        },
        move || async move {
            run_database_task(move || {
                database
                    .import_data_file(&PathBuf::from(path), replace)
                    .map_err(|error| error.to_string())
            })
            .await
        },
    )
    .await
}

#[tauri::command]
async fn database_status(state: State<'_, DbState>) -> Result<DatabaseStatus, String> {
    let database = state.database.clone();
    let _guard = state.maintenance_gate.clone().read_owned().await;
    run_database_task(move || database.status().map_err(|error| error.to_string())).await
}

#[tauri::command]
async fn acknowledge_empty_start(state: State<'_, DbState>) -> Result<(), String> {
    let database = state.database.clone();
    let _guard = state.maintenance_gate.clone().read_owned().await;
    run_database_task(move || {
        database
            .acknowledge_empty_start()
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn acknowledge_import_verification(
    state: State<'_, DbState>,
    summary: ImportSummary,
) -> Result<(), String> {
    let database = state.database.clone();
    let _guard = state.maintenance_gate.clone().write_owned().await;
    run_database_task(move || {
        database
            .acknowledge_import_verification(&summary)
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn backup_enroll(
    state: State<'_, BackupState>,
    enrollment_code: String,
    device_name: String,
) -> Result<BackupEnrollment, String> {
    let service = state.service.clone();
    service
        .enroll(enrollment_code, device_name)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn backup_now(
    state: State<'_, BackupState>,
    retention_class: Option<String>,
) -> Result<BackupRunReport, String> {
    let retention_class = retention_class
        .as_deref()
        .unwrap_or("rolling")
        .parse::<RetentionClass>()
        .map_err(|error| error.to_string())?;
    let service = state.service.clone();
    service
        .backup_now(retention_class)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn backup_status(state: State<'_, BackupState>) -> Result<BackupStatusReport, String> {
    let service = state.service.clone();
    service.status().await.map_err(|error| error.to_string())
}

#[tauri::command]
async fn prepare_for_update(
    database_state: State<'_, DbState>,
    backup_state: State<'_, BackupState>,
    update_state: State<'_, UpdateState>,
) -> Result<BackupRunReport, String> {
    if update_state.has_active_lease().await {
        return Err("Güncelleme hazırlığı zaten etkin.".to_owned());
    }

    // Wait for every active database operation, then retain the exclusive
    // guard across the frontend's installer call. No new business write can
    // land between this snapshot and process replacement.
    let maintenance_guard = database_state.maintenance_gate.clone().write_owned().await;
    let report = backup_state
        .service
        .clone()
        .prepare_for_update()
        .await
        .map_err(|error| error.to_string())?;
    if !report.encrypted_snapshot_created || !report.safe_to_continue {
        return Err("Güncelleme öncesi şifreli yedek doğrulanamadı.".to_owned());
    }
    update_state
        .retain_guard_until(maintenance_guard, UPDATE_MAINTENANCE_LEASE_TIMEOUT)
        .await?;
    Ok(report)
}

#[tauri::command]
async fn cancel_prepared_update(state: State<'_, UpdateState>) -> Result<(), String> {
    state.cancel().await;
    Ok(())
}

#[tauri::command]
async fn prepare_for_destructive_import(
    database_state: State<'_, DbState>,
    backup_state: State<'_, BackupState>,
) -> Result<BackupRunReport, String> {
    let service = backup_state.service.clone();
    let _guard = database_state.maintenance_gate.clone().write_owned().await;
    service
        .prepare_for_destructive_import()
        .await
        .map_err(|error| error.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let app_data_dir = app
                .path()
                .app_local_data_dir()
                .map_err(|error| error.to_string())?;
            let database_path = app_data_dir.join("data").join("pusula.sqlite3");
            let database =
                Database::initialize(database_path).map_err(|error| error.to_string())?;
            let backup_service = BackupService::production(database.clone(), app_data_dir)
                .map_err(|error| error.to_string())?;
            app.manage(DbState {
                database,
                maintenance_gate: Arc::new(RwLock::new(())),
            });
            app.manage(BackupState {
                service: backup_service.clone(),
            });
            app.manage(UpdateState::new());
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                loop {
                    backup_service.run_scheduled_if_due().await;
                    tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            api_request,
            export_data,
            import_data,
            export_data_file,
            import_data_file,
            database_status,
            acknowledge_empty_start,
            acknowledge_import_verification,
            backup_enroll,
            backup_now,
            backup_status,
            prepare_for_update,
            cancel_prepared_update,
            prepare_for_destructive_import,
        ])
        .run(tauri::generate_context!())
        .expect("Pusula başlatılamadı");
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex,
    };

    use super::*;
    use crate::backup::RemoteResult;

    fn successful_recovery_report() -> BackupRunReport {
        BackupRunReport {
            encrypted_snapshot_created: true,
            safe_to_continue: true,
            retention_class: RetentionClass::Rolling,
            created_at: "2026-07-14T12:00:00Z".to_owned(),
            uploaded_count: 0,
            pending_count: 1,
            local_recovery_count: 1,
            queue_healthy: true,
            quarantined_file_count: 0,
            remote_result: RemoteResult::LocalRecovery,
        }
    }

    #[tokio::test]
    async fn replacement_stops_before_import_when_recovery_fails() {
        let applied = Arc::new(AtomicBool::new(false));
        let import_applied = applied.clone();
        let result = run_import_operation(
            true,
            || async { Err("local fsync failed".to_owned()) },
            move || async move {
                import_applied.store(true, Ordering::SeqCst);
                Ok::<_, String>(())
            },
        )
        .await;

        assert!(result.is_err());
        assert!(!applied.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn replacement_applies_only_after_verified_local_recovery() {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let recovery_events = events.clone();
        let import_events = events.clone();
        let result = run_import_operation(
            true,
            move || async move {
                recovery_events.lock().unwrap().push("recovery");
                Ok(successful_recovery_report())
            },
            move || async move {
                import_events.lock().unwrap().push("import");
                Ok::<_, String>(17)
            },
        )
        .await
        .unwrap();

        assert_eq!(result, 17);
        assert_eq!(*events.lock().unwrap(), vec!["recovery", "import"]);
    }

    #[tokio::test]
    async fn prepared_update_blocks_database_operations_until_cancelled() {
        let gate = Arc::new(RwLock::new(()));
        let update_state = UpdateState::new();
        update_state
            .retain_guard_until(gate.clone().write_owned().await, Duration::from_secs(60))
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(25), gate.clone().read_owned())
                .await
                .is_err()
        );

        update_state.cancel().await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), gate.read_owned())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn abandoned_update_lease_releases_database_automatically() {
        let gate = Arc::new(RwLock::new(()));
        let update_state = UpdateState::new();
        update_state
            .retain_guard_until(gate.clone().write_owned().await, Duration::from_millis(25))
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(5), gate.clone().read_owned())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), gate.read_owned())
                .await
                .is_ok()
        );
    }
}
