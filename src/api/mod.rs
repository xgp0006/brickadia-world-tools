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
pub mod grid_build;
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
pub use grid_build::{
    GridBuildProgress, GridBuildRequest, GridBuildResult, GridEstimateDto, GridModeDto,
    grid_estimate, grid_fetch_build,
};
#[cfg(feature = "dem")]
pub use sculpt::{
    SculptCreateBlankRequest, SculptExportRequest, SculptExportResult, SculptLayerBoxRequest,
    SculptLayerInfo, SculptLayerPartResult, SculptLayersExportResult, SculptLayersInfo,
    SculptLoadPngRequest, SculptPaletteInfo, SculptPreview, SculptProgress, SculptSessionInfo,
    SculptStrokeRequest, SculptToolDto, SculptZoneAddRectRequest, SculptZonesInfo, StampKindDto,
    ZoneModeDto, sculpt_apply_stroke, sculpt_close, sculpt_create_blank, sculpt_export,
    sculpt_export_layers, sculpt_info, sculpt_layer_add, sculpt_layer_paint_box,
    sculpt_layer_set_active, sculpt_layers_info, sculpt_load_png, sculpt_palette, sculpt_preview,
    sculpt_undo, sculpt_zone_add_rect, sculpt_zone_clear, sculpt_zones_info,
};

/// Crate version string for shell "about" / health checks.
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
