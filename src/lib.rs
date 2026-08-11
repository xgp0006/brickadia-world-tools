pub mod api;
pub mod map;
pub mod opt;
pub mod util;

/// DEM pipeline + (when `gui` is also enabled) the egui shell modules.
/// Available under `feature = "dem"` so Tauri can fetch/build without egui.
#[cfg(feature = "dem")]
pub mod gui;
