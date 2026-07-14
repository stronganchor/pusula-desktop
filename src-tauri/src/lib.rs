mod api;
mod backup;
pub mod db;
mod error;
pub mod models;

use std::{path::PathBuf, time::Duration};

use backup::{
    BackupEnrollment, BackupRunReport, BackupService, BackupStatusReport, RetentionClass,
};
pub use db::Database;
pub use models::{DatabaseStatus, ExportBundle, ExportSummary, ImportSummary};
use serde_json::Value;
use tauri::{Manager, State};

struct DbState {
    database: Database,
}

struct BackupState {
    service: BackupService,
}

#[tauri::command]
fn api_request(
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
    state
        .database
        .api_request(&path, method.as_deref(), body)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn export_data(state: State<'_, DbState>) -> Result<ExportBundle, String> {
    state
        .database
        .export_data()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn import_data(
    state: State<'_, DbState>,
    bundle: ExportBundle,
    replace: Option<bool>,
) -> Result<ImportSummary, String> {
    state
        .database
        .import_data(bundle, replace.unwrap_or(false))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn export_data_file(
    state: State<'_, DbState>,
    path: String,
    overwrite: Option<bool>,
) -> Result<ExportSummary, String> {
    state
        .database
        .export_data_file(&PathBuf::from(path), overwrite.unwrap_or(false))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn import_data_file(
    state: State<'_, DbState>,
    path: String,
    replace: Option<bool>,
) -> Result<ImportSummary, String> {
    state
        .database
        .import_data_file(&PathBuf::from(path), replace.unwrap_or(false))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn database_status(state: State<'_, DbState>) -> Result<DatabaseStatus, String> {
    state.database.status().map_err(|error| error.to_string())
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
async fn prepare_for_update(state: State<'_, BackupState>) -> Result<BackupRunReport, String> {
    let service = state.service.clone();
    service
        .prepare_for_update()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn prepare_for_destructive_import(
    state: State<'_, BackupState>,
) -> Result<BackupRunReport, String> {
    let service = state.service.clone();
    service
        .prepare_for_destructive_import()
        .await
        .map_err(|error| error.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
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
            app.manage(DbState { database });
            app.manage(BackupState {
                service: backup_service.clone(),
            });
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
            backup_enroll,
            backup_now,
            backup_status,
            prepare_for_update,
            prepare_for_destructive_import,
        ])
        .run(tauri::generate_context!())
        .expect("Pusula başlatılamadı");
}
