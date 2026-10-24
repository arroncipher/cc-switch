use crate::proxy::traffic_recorder::{TrafficExport, TrafficRecordingConfig};
use crate::store::AppState;
use std::path::PathBuf;
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
pub async fn set_traffic_recording_config(
    state: tauri::State<'_, AppState>,
    config: TrafficRecordingConfig,
) -> Result<(), String> {
    state
        .proxy_service
        .configure_traffic_recording(config)
        .await
}

#[tauri::command]
pub(crate) async fn export_traffic_recordings(
    state: tauri::State<'_, AppState>,
    target: PathBuf,
) -> Result<TrafficExport, String> {
    state.proxy_service.export_traffic_recordings(target).await
}

#[tauri::command]
pub async fn open_traffic_recording_directory(app: tauri::AppHandle) -> Result<(), String> {
    let directory = crate::settings::get_settings()
        .traffic_recording
        .directory
        .unwrap_or_else(|| crate::config::get_app_config_dir().join("raw-traffic"));
    if !directory.is_dir() {
        return Err("Recording directory does not exist yet".into());
    }
    app.opener()
        .open_path(directory.to_string_lossy().to_string(), None::<String>)
        .map_err(|e| e.to_string())
}
