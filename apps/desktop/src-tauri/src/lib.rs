//! Tauri shell for Brickadia World Tools.
//! Phase 2: Convert with progress, install-to-Worlds, dialogs.

use heightmap::api::{
    self, ConvertProgress, ConvertRequest, ConvertResult, DemPredictRequest, DemPredictResult,
};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter};

#[tauri::command]
fn core_version() -> String {
    api::CORE_VERSION.to_string()
}

/// Staging builds directory (`~/.local/share/heightmap2brz/builds`).
#[tauri::command]
fn builds_dir() -> Result<String, String> {
    api::builds_dir().map(|p| p.display().to_string())
}

/// Pure DEM resolution prediction (no network) for Map UI scaffolding.
#[tauri::command]
fn dem_predict(request: DemPredictRequest) -> Result<DemPredictResult, String> {
    api::predict_dem_cells(request)
}

/// Convert heightmap (+ optional colormap) → `.brdb` / `.brz`.
/// Emits `convert:progress` with [`ConvertProgress`] during the run.
/// Optional install into Brickadia Worlds is soft-fail (see result fields).
#[tauri::command]
async fn convert_build(
    app: AppHandle,
    request: ConvertRequest,
) -> Result<ConvertResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        api::convert_heightmap(
            request,
            |p: ConvertProgress| {
                let _ = app.emit("convert:progress", &p);
            },
            || false,
        )
    })
    .await
    .map_err(|e| format!("convert task failed: {e}"))?
}

/// Install an existing save into Brickadia Worlds/Prefabs.
#[tauri::command]
fn install_save(path: String, overwrite: bool) -> Result<String, String> {
    api::install_save(PathBuf::from(path).as_path(), overwrite)
        .map(|p| p.display().to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            core_version,
            builds_dir,
            convert_build,
            dem_predict,
            install_save,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
