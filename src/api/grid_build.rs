//! Tiled large-world grid build for Tauri Map (BWT-3.8).
//!
//! Wraps pure [`crate::gui::grid`] planner + `run_grid_build`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::api::convert::BrickModeDto;
use crate::api::dem_build::DemBuildRequest;
use crate::api::dem_predict::DemSourceDto;
use crate::gui::build::BlockType;
use crate::gui::dem_sources::DemSource;
use crate::gui::grid::{
    self, available_ram_bytes, estimate_grid, partition, run_grid_build, AnchorKind, GridMode,
    GridProgress, GridSettings, OutputOptions,
};
use crate::gui::imagery_sources::ImagerySource;
use crate::gui::tiles::BBoxLatLon;

fn map_dem_source(s: DemSourceDto) -> DemSource {
    match s {
        DemSourceDto::AwsTerrarium => DemSource::AwsTerrarium,
        DemSourceDto::MapboxTerrainRgb => DemSource::MapboxTerrainRgb,
        DemSourceDto::OpenTopography => DemSource::OpenTopography,
        DemSourceDto::OpenTopographyCop30 => DemSource::OpenTopographyCop30,
        DemSourceDto::Usgs3Dep => DemSource::Usgs3Dep,
    }
}

fn map_block_type(m: BrickModeDto) -> BlockType {
    match m {
        BrickModeDto::Default => BlockType::Brick,
        BrickModeDto::Tile => BlockType::Tile,
        BrickModeDto::SmoothTile => BlockType::SmoothTile,
        BrickModeDto::Stud => BlockType::Studded,
        BrickModeDto::Micro => BlockType::Micro,
    }
}

fn source_max_zoom(dem: DemSource, token: Option<&str>) -> u32 {
    match dem {
        DemSource::OpenTopography | DemSource::OpenTopographyCop30 | DemSource::Usgs3Dep => 15,
        other => crate::gui::dem_sources::tile_source_for(other, token)
            .map(|s| s.max_zoom())
            .unwrap_or(15),
    }
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.and_then(|t| {
        let t = t.trim().to_owned();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    })
}

/// How to subdivide the big bbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GridModeDto {
    /// Subdivide into ~tile_m metre tiles.
    #[default]
    Auto,
    /// Explicit cols × rows.
    Explicit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridBuildRequest {
    pub north: f64,
    pub south: f64,
    pub east: f64,
    pub west: f64,
    #[serde(default)]
    pub dem_source: DemSourceDto,
    #[serde(default = "default_tile_m")]
    pub tile_m: f64,
    #[serde(default)]
    pub mode: GridModeDto,
    #[serde(default = "default_cols")]
    pub cols: u32,
    #[serde(default = "default_rows")]
    pub rows: u32,
    #[serde(default = "default_studs")]
    pub studs_per_meter: f32,
    #[serde(default = "default_exag")]
    pub vertical_exaggeration: f32,
    #[serde(default = "default_name")]
    pub output_name: String,
    #[serde(default)]
    pub install: bool,
    #[serde(default)]
    pub overwrite: bool,
    /// Prefer stitched single world (true) vs individual tile saves only.
    #[serde(default = "default_true")]
    pub stitched: bool,
    #[serde(default)]
    pub individual: bool,
    #[serde(default)]
    pub mapbox_token: Option<String>,
    #[serde(default)]
    pub opentopo_key: Option<String>,
    #[serde(default)]
    pub brick_mode: BrickModeDto,
    #[serde(default)]
    pub glow: bool,
    #[serde(default)]
    pub no_collision: bool,
}

fn default_tile_m() -> f64 {
    750.0
}
fn default_cols() -> u32 {
    2
}
fn default_rows() -> u32 {
    2
}
fn default_studs() -> f32 {
    4.0
}
fn default_exag() -> f32 {
    1.0
}
fn default_name() -> String {
    "grid-build".into()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridEstimateDto {
    pub tile_count: u32,
    pub total_cells: u64,
    pub est_bricks: u64,
    pub peak_mesh_bytes: u64,
    pub est_seconds: f64,
    pub fits_ram: bool,
    pub over_cell_budget: bool,
    pub zoom: u32,
    pub cell_m: f64,
    pub cols: u32,
    pub rows: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridBuildProgress {
    pub phase: String,
    pub frac: f32,
    pub tiles_done: u32,
    pub tiles_total: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridBuildResult {
    pub paths: Vec<PathBuf>,
    pub installed_paths: Vec<PathBuf>,
    pub brick_count: u64,
    pub tile_count: u32,
    pub warnings: Vec<String>,
}

fn resolve_tokens(req: &GridBuildRequest) -> (Option<String>, Option<String>) {
    let mut mapbox = nonempty(req.mapbox_token.clone());
    let mut opentopo = nonempty(req.opentopo_key.clone());
    if mapbox.is_none() || opentopo.is_none() {
        if let Ok(cfg) = crate::gui::config::Config::load() {
            if mapbox.is_none() {
                mapbox = nonempty(cfg.mapbox_token);
            }
            if opentopo.is_none() {
                opentopo = nonempty(cfg.opentopo_api_key);
            }
        }
    }
    (mapbox, opentopo)
}

fn plan_and_settings(
    req: &GridBuildRequest,
) -> Result<(grid::GridPlan, GridSettings), String> {
    if !(req.north > req.south && req.east >= req.west) {
        return Err("bbox must have north>south and east>=west".into());
    }
    let tile_m = req.tile_m.clamp(100.0, 50_000.0);
    let mode = match req.mode {
        GridModeDto::Auto => GridMode::AutoSubdivide { tile_m },
        GridModeDto::Explicit => GridMode::Explicit {
            tile_m,
            cols: req.cols.max(1),
            rows: req.rows.max(1),
            anchor: AnchorKind::NwCorner,
        },
    };
    let dem = map_dem_source(req.dem_source);
    let (mapbox, opentopo) = resolve_tokens(req);
    let max_z = source_max_zoom(dem, mapbox.as_deref());
    let mut plan = partition(
        BBoxLatLon {
            north: req.north,
            south: req.south,
            east: req.east,
            west: req.west,
        },
        mode,
        dem,
        max_z,
        req.studs_per_meter.clamp(0.5, 32.0),
        req.vertical_exaggeration.clamp(0.25, 8.0),
        matches!(req.brick_mode, BrickModeDto::Micro),
    );
    plan.name = req.output_name.clone();

    let stitched = req.stitched || !req.individual;
    let individual = req.individual;
    let output = OutputOptions {
        brdb: true,
        brz: false,
        stitched,
        individual: individual || !stitched,
        install_to_brickadia: req.install,
    };

    let settings = GridSettings {
        dem_source: dem,
        imagery_source: ImagerySource::None,
        mapbox_token: mapbox,
        opentopo_key: opentopo,
        block_type: map_block_type(req.brick_mode),
        glow: req.glow,
        no_collision: req.no_collision,
        output,
        overwrite: req.overwrite,
    };
    Ok((plan, settings))
}

/// Pure pre-commit estimate (no network).
pub fn grid_estimate(req: GridBuildRequest) -> Result<GridEstimateDto, String> {
    let (plan, settings) = plan_and_settings(&req)?;
    let ram = available_ram_bytes().unwrap_or(16 * 1024 * 1024 * 1024);
    let est = estimate_grid(&plan, &settings, ram);
    Ok(GridEstimateDto {
        tile_count: est.tile_count,
        total_cells: est.total_cells,
        est_bricks: est.est_bricks,
        peak_mesh_bytes: est.peak_mesh_bytes,
        est_seconds: est.est_seconds,
        fits_ram: est.fits_ram,
        over_cell_budget: est.over_cell_budget,
        zoom: plan.zoom,
        cell_m: plan.cell_m,
        cols: plan.cols,
        rows: plan.rows,
    })
}

/// Run tiled fetch + mesh + write. Emits progress via callback.
pub fn grid_fetch_build(
    req: GridBuildRequest,
    progress: impl Fn(GridBuildProgress) + Send + Sync + 'static,
    cancel: impl Fn() -> bool + Send + Sync + 'static,
) -> Result<GridBuildResult, String> {
    let (plan, settings) = plan_and_settings(&req)?;
    if plan.tiles.is_empty() {
        return Err("grid plan has zero tiles — enlarge the box or lower tile size".into());
    }

    let tiles_total = plan.tiles.len() as u32;
    let progress_state = Arc::new(Mutex::new(GridProgress::new(tiles_total)));
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_cb = Arc::new(cancel);
    let progress_cb = Arc::new(progress);

    // Poll thread: mirror GridProgress into UI events.
    let ps = Arc::clone(&progress_state);
    let cf = Arc::clone(&cancel_flag);
    let pcb = Arc::clone(&progress_cb);
    let ccb = Arc::clone(&cancel_cb);
    let poll = std::thread::spawn(move || {
        while !cf.load(Ordering::Relaxed) {
            if ccb() {
                cf.store(true, Ordering::Relaxed);
                break;
            }
            if let Ok(p) = ps.lock() {
                let phase = p.phase.label().to_string();
                let frac = if p.tiles_total == 0 {
                    0.0
                } else {
                    (p.tiles_done as f32 / p.tiles_total as f32).clamp(0.0, 1.0)
                };
                pcb(GridBuildProgress {
                    phase,
                    frac,
                    tiles_done: p.tiles_done,
                    tiles_total: p.tiles_total,
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });

    let outcome =
        run_grid_build(plan, settings, Arc::clone(&progress_state), Arc::clone(&cancel_flag))
            .map_err(|e| e.to_string())?;

    cancel_flag.store(true, Ordering::Relaxed);
    let _ = poll.join();

    progress_cb(GridBuildProgress {
        phase: "Finished".into(),
        frac: 1.0,
        tiles_done: tiles_total,
        tiles_total,
    });

    Ok(GridBuildResult {
        paths: outcome.written,
        installed_paths: outcome.installed,
        brick_count: outcome.brick_count as u64,
        tile_count: outcome.tiles,
        warnings: outcome.warnings,
    })
}

/// Convenience: single-box dem request → grid with 1×1 explicit (debug).
#[allow(dead_code)]
pub fn grid_from_dem_request(req: DemBuildRequest, tile_m: f64) -> GridBuildRequest {
    GridBuildRequest {
        north: req.north,
        south: req.south,
        east: req.east,
        west: req.west,
        dem_source: req.dem_source,
        tile_m,
        mode: GridModeDto::Auto,
        cols: 2,
        rows: 2,
        studs_per_meter: req.studs_per_meter,
        vertical_exaggeration: req.vertical_exaggeration,
        output_name: req.output_name,
        install: req.install,
        overwrite: req.overwrite,
        stitched: true,
        individual: false,
        mapbox_token: req.mapbox_token,
        opentopo_key: req.opentopo_key,
        brick_mode: req.brick_mode,
        glow: req.glow,
        no_collision: req.no_collision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_small_auto_grid() {
        let est = grid_estimate(GridBuildRequest {
            north: 40.05,
            south: 40.0,
            east: -105.0,
            west: -105.05,
            dem_source: DemSourceDto::AwsTerrarium,
            tile_m: 1000.0,
            mode: GridModeDto::Auto,
            cols: 2,
            rows: 2,
            studs_per_meter: 4.0,
            vertical_exaggeration: 1.0,
            output_name: "t".into(),
            install: false,
            overwrite: false,
            stitched: true,
            individual: false,
            mapbox_token: None,
            opentopo_key: None,
            brick_mode: BrickModeDto::Tile,
            glow: false,
            no_collision: false,
        })
        .expect("estimate");
        assert!(est.tile_count >= 1);
        assert!(est.cols >= 1 && est.rows >= 1);
    }
}
