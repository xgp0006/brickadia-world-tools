//! Tauri shell for Brickadia World Tools.
//! Phase 2: Convert · Phase 3: Map · Phase 4: Sculpt MVP.

use heightmap::api::{
    self, ConvertProgress, ConvertRequest, ConvertResult, DemBuildProgress, DemBuildRequest,
    DemBuildResult, DemPredictRequest, DemPredictResult, GridBuildProgress, GridBuildRequest,
    GridBuildResult, GridEstimateDto, SculptCreateBlankRequest, SculptExportRequest,
    SculptExportResult, SculptFromDemRequest, SculptLayerBoxRequest, SculptLayersExportResult,
    SculptLayersInfo, SculptLoadPngRequest, SculptPaletteInfo, SculptPreview, SculptProgress,
    SculptSessionInfo, SculptStrokeRequest, SculptZoneAddRectRequest, SculptZonesInfo,
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

/// Fetch Map bbox DEM → new sculpt session (egui "Send to Sculpt").
/// Emits `sculpt:progress` while fetching.
#[tauri::command]
async fn sculpt_from_dem(
    app: AppHandle,
    request: SculptFromDemRequest,
) -> Result<SculptSessionInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        api::sculpt_from_dem(
            request,
            move |p: SculptProgress| {
                let _ = app.emit("sculpt:progress", &p);
            },
            || false,
        )
    })
    .await
    .map_err(|e| format!("sculpt_from_dem task failed: {e}"))?
}

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

#[tauri::command]
fn sculpt_palette(session_id: u64) -> Result<SculptPaletteInfo, String> {
    api::sculpt_palette(session_id)
}

#[tauri::command]
fn sculpt_zone_add_rect(request: SculptZoneAddRectRequest) -> Result<SculptZonesInfo, String> {
    api::sculpt_zone_add_rect(request)
}

#[tauri::command]
fn sculpt_zone_clear(session_id: u64) -> Result<SculptZonesInfo, String> {
    api::sculpt_zone_clear(session_id)
}

#[tauri::command]
fn sculpt_zones_info(session_id: u64) -> Result<SculptZonesInfo, String> {
    api::sculpt_zones_info(session_id)
}

#[tauri::command]
fn sculpt_layers_info(session_id: u64) -> Result<SculptLayersInfo, String> {
    api::sculpt_layers_info(session_id)
}

#[tauri::command]
fn sculpt_layer_add(session_id: u64) -> Result<SculptLayersInfo, String> {
    api::sculpt_layer_add(session_id)
}

#[tauri::command]
fn sculpt_layer_set_active(session_id: u64, index: usize) -> Result<SculptLayersInfo, String> {
    api::sculpt_layer_set_active(session_id, index)
}

#[tauri::command]
fn sculpt_layer_paint_box(request: SculptLayerBoxRequest) -> Result<SculptLayersInfo, String> {
    api::sculpt_layer_paint_box(request)
}

/// Multi-save layer export. Emits `sculpt:progress`.
#[tauri::command]
async fn sculpt_export_layers(
    app: AppHandle,
    request: SculptExportRequest,
) -> Result<SculptLayersExportResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        api::sculpt_export_layers(
            request,
            move |p: SculptProgress| {
                let _ = app.emit("sculpt:progress", &p);
            },
            || false,
        )
    })
    .await
    .map_err(|e| format!("sculpt layer export task failed: {e}"))?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // MapLibre attribution / any <a href="https://…"> must not navigate the
        // WebView away from the app — open in the system browser instead.
        .plugin(
            tauri::plugin::Builder::<tauri::Wry, ()>::new("external-links")
                .on_navigation(|_webview, url| {
                    let scheme = url.scheme();
                    if scheme == "http" || scheme == "https" || scheme == "mailto" {
                        let _ = tauri_plugin_opener::open_url(url.as_str(), None::<&str>);
                        return false;
                    }
                    // Allow app assets, tauri, and Vite dev server.
                    true
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            core_version,
            builds_dir,
            convert_build,
            dem_predict,
            dem_fetch_build,
            grid_estimate,
            grid_fetch_build,
            install_save,
            sculpt_from_dem,
            sculpt_create_blank,
            sculpt_load_png,
            sculpt_close,
            sculpt_info,
            sculpt_preview,
            sculpt_apply_stroke,
            sculpt_undo,
            sculpt_export,
            sculpt_palette,
            sculpt_zone_add_rect,
            sculpt_zone_clear,
            sculpt_zones_info,
            sculpt_layers_info,
            sculpt_layer_add,
            sculpt_layer_set_active,
            sculpt_layer_paint_box,
            sculpt_export_layers,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
