//! Tauri shell for Brickadia World Tools.
//! Phase 2: Convert · Phase 3: Map · Phase 4: Sculpt MVP.

use heightmap::api::{
    self, ConvertProgress, ConvertRequest, ConvertResult, DemBuildProgress, DemBuildRequest,
    DemBuildResult, DemPredictRequest, DemPredictResult, GridBuildProgress, GridBuildRequest,
    GridBuildResult, GridEstimateDto, SculptCreateBlankRequest, SculptExportRequest,
    SculptExportResult, SculptLoadPngRequest, SculptPreview, SculptProgress, SculptSessionInfo,
    SculptStrokeRequest,
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

/// Fetch DEM for bbox → mesh → write `.brdb` (optional install).
/// Emits `build:progress` with `{ phase, frac }` during the run.
#[tauri::command]
async fn dem_fetch_build(
    app: AppHandle,
    request: DemBuildRequest,
) -> Result<DemBuildResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        api::dem_fetch_build(
            request,
            move |p: DemBuildProgress| {
                let _ = app.emit("build:progress", &p);
            },
            || false,
        )
    })
    .await
    .map_err(|e| format!("dem build task failed: {e}"))?
}

/// Install an existing save into Brickadia Worlds/Prefabs.
#[tauri::command]
fn install_save(path: String, overwrite: bool) -> Result<String, String> {
    api::install_save(PathBuf::from(path).as_path(), overwrite)
        .map(|p| p.display().to_string())
}

/// Pre-commit grid estimate (no network).
#[tauri::command]
fn grid_estimate(request: GridBuildRequest) -> Result<GridEstimateDto, String> {
    api::grid_estimate(request)
}

/// Tiled DEM fetch + mesh + write. Emits `grid:progress`.
#[tauri::command]
async fn grid_fetch_build(
    app: AppHandle,
    request: GridBuildRequest,
) -> Result<GridBuildResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        api::grid_fetch_build(
            request,
            move |p: GridBuildProgress| {
                let _ = app.emit("grid:progress", &p);
            },
            || false,
        )
    })
    .await
    .map_err(|e| format!("grid build task failed: {e}"))?
}

// ── Sculpt (Phase 4 MVP) ────────────────────────────────────────────────────

#[tauri::command]
fn sculpt_create_blank(request: SculptCreateBlankRequest) -> Result<SculptSessionInfo, String> {
    api::sculpt_create_blank(request)
}

#[tauri::command]
fn sculpt_load_png(request: SculptLoadPngRequest) -> Result<SculptSessionInfo, String> {
    api::sculpt_load_png(request)
}

#[tauri::command]
fn sculpt_close(session_id: u64) -> Result<(), String> {
    api::sculpt_close(session_id)
}

#[tauri::command]
fn sculpt_info(session_id: u64) -> Result<SculptSessionInfo, String> {
    api::sculpt_info(session_id)
}

#[tauri::command]
fn sculpt_preview(session_id: u64) -> Result<SculptPreview, String> {
    api::sculpt_preview(session_id)
}

#[tauri::command]
fn sculpt_apply_stroke(request: SculptStrokeRequest) -> Result<SculptSessionInfo, String> {
    api::sculpt_apply_stroke(request)
}

#[tauri::command]
fn sculpt_undo(session_id: u64) -> Result<SculptSessionInfo, String> {
    api::sculpt_undo(session_id)
}

/// Mesh session → `.brdb`. Emits `sculpt:progress` during the run.
#[tauri::command]
async fn sculpt_export(
    app: AppHandle,
    request: SculptExportRequest,
) -> Result<SculptExportResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        api::sculpt_export(
            request,
            move |p: SculptProgress| {
                let _ = app.emit("sculpt:progress", &p);
            },
            || false,
        )
    })
    .await
    .map_err(|e| format!("sculpt export task failed: {e}"))?
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
            dem_fetch_build,
            grid_estimate,
            grid_fetch_build,
            install_save,
            sculpt_create_blank,
            sculpt_load_png,
            sculpt_close,
            sculpt_info,
            sculpt_preview,
            sculpt_apply_stroke,
            sculpt_undo,
            sculpt_export,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
