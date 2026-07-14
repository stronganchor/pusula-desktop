mod api;
pub mod db;
mod error;
pub mod models;

use std::path::PathBuf;

pub use db::Database;
pub use models::{DatabaseStatus, ExportBundle, ExportSummary, ImportSummary};
use serde_json::Value;
use tauri::{Manager, State};

struct DbState {
    database: Database,
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let database_path = app
                .path()
                .app_local_data_dir()
                .map_err(|error| error.to_string())?
                .join("data")
                .join("pusula.sqlite3");
            let database =
                Database::initialize(database_path).map_err(|error| error.to_string())?;
            app.manage(DbState { database });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            api_request,
            export_data,
            import_data,
            export_data_file,
            import_data_file,
            database_status,
        ])
        .run(tauri::generate_context!())
        .expect("Pusula başlatılamadı");
}
