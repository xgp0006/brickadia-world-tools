//! GUI shell (egui) and shared DEM pipeline modules.
//!
//! Modules that need only `feature = "dem"` (tiles, sources, build, config)
//! live here historically; they are free of egui/walkers so Tauri can depend
//! on `heightmap` with `features = ["dem"]` without pulling the full GUI.

/// HTTP User-Agent for every outbound request (tile fetch, geocode, DEM REST).
/// Derived from the crate version so it can never drift from Cargo.toml.
pub(crate) const USER_AGENT: &str = concat!(
    "heightmap2brz/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/xgp0006/brickadia-heightmap-tools)"
);

// --- dem feature: pure fetch/mesh/write (no egui) ---
#[cfg(feature = "dem")]
pub(crate) mod build;
#[cfg(feature = "dem")]
pub(crate) mod config;
#[cfg(feature = "dem")]
pub(crate) mod dem_sources;
#[cfg(feature = "dem")]
pub(crate) mod imagery_sources;
#[cfg(feature = "dem")]
pub(crate) mod tiles;
#[cfg(feature = "dem")]
pub(crate) mod scale;
// Pure grid planner/orchestrator (no egui) — Tauri Map Grid mode.
#[cfg(feature = "dem")]
pub(crate) mod grid;

// --- gui feature: egui shell ---
#[cfg(feature = "gui")]
mod app;
#[cfg(feature = "gui")]
pub mod logger;
#[cfg(feature = "gui")]
pub mod util;
#[cfg(feature = "gui")]
pub use app::*;

#[cfg(feature = "gui")]
mod coords;
#[cfg(feature = "gui")]
mod geocode;
#[cfg(feature = "gui")]
mod grid_ui;
#[cfg(feature = "gui")]
mod map_tab;
#[cfg(feature = "gui")]
mod preview_source;
// Pure heightfield/brush/tools compile under `dem` (Tauri Sculpt API).
// egui tab + paint/zones convert path stay `gui`-gated inside the module.
#[cfg(feature = "dem")]
pub(crate) mod sculpt;
#[cfg(feature = "gui")]
mod theme;
#[cfg(feature = "gui")]
mod zones;
#[cfg(feature = "gui")]
pub use theme::install_theme;
