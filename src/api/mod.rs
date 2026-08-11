//! Public, UI-agnostic command surface for shells (egui today, Tauri tomorrow).
//!
//! Keep this free of egui/eframe/walkers. Progress is a plain callback so either
//! shell can map it to channels or IPC events.

pub mod convert;
pub mod dem_predict;

pub use convert::{
    BrickModeDto, ConvertProgress, ConvertRequest, ConvertResult, convert_heightmap,
};
pub use dem_predict::{DemPredictRequest, DemPredictResult, predict_dem_cells};

/// Crate version string for shell "about" / health checks.
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
