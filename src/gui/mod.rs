mod app;
pub mod logger;
pub use app::*;
pub mod util;

/// HTTP User-Agent for every outbound request (tile fetch, geocode, DEM REST).
/// Derived from the crate version so it can never drift from Cargo.toml.
pub(crate) const USER_AGENT: &str = concat!(
    "heightmap2brz/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/xgp0006/brickadia-heightmap-tools)"
);

mod build;
mod config;
mod coords;
mod dem_sources;
mod geocode;
mod imagery_sources;
mod map_tab;
mod theme;
mod tiles;
pub use theme::install_theme;
