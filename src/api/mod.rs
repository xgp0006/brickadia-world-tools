//! Public, UI-agnostic command surface for shells (egui today, Tauri tomorrow).
//!
//! Keep this free of egui/eframe/walkers. Progress is a plain callback so either
//! shell can map it to channels or IPC events.

pub mod convert;
pub mod dem_predict;
pub mod install;

#[cfg(feature = "dem")]
pub mod dem_build;
#[cfg(feature = "dem")]
pub mod sculpt;

pub use convert::{
    BrickModeDto, ConvertProgress, ConvertRequest, ConvertResult, convert_heightmap,
};
pub use dem_predict::{DemPredictRequest, DemPredictResult, DemSourceDto, predict_dem_cells};
pub use install::{
    BRICKADIA_APP_ID, brickadia_saved_dir, brickadia_worlds_dir, builds_dir, install_save,
    install_save_ext, is_prefix_missing, unique_save_path,
};

#[cfg(feature = "dem")]
pub use dem_build::{DemBuildProgress, DemBuildRequest, DemBuildResult, dem_fetch_build};
#[cfg(feature = "dem")]
pub use sculpt::{
    SculptCreateBlankRequest, SculptExportRequest, SculptExportResult, SculptLoadPngRequest,
    SculptPreview, SculptProgress, SculptSessionInfo, SculptStrokeRequest, SculptToolDto,
    sculpt_apply_stroke, sculpt_close, sculpt_create_blank, sculpt_export, sculpt_info,
    sculpt_load_png, sculpt_preview, sculpt_undo,
};

/// Crate version string for shell "about" / health checks.
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
