//! Tauri shell for Brickadia World Tools.
//! Phase 1–2: Convert pipeline via `heightmap::api`. Map/Sculpt later.

use heightmap::api::{self, ConvertRequest, ConvertResult};

#[tauri::command]
fn core_version() -> String {
    api::CORE_VERSION.to_string()
}

/// Convert heightmap (+ optional colormap) → `.brdb` / `.brz`.
/// Same path as the egui Convert tab worker.
#[tauri::command]
fn convert_build(request: ConvertRequest) -> Result<ConvertResult, String> {
    api::convert_heightmap(request, |_| {}, || false)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![core_version, convert_build])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
